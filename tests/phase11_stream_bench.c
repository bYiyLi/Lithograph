#include <sqlite3.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#ifdef _WIN32
#include <windows.h>
#endif

typedef struct sample_set {
    uint64_t *micros;
    uint64_t *first_micros;
    unsigned count;
    uint64_t expected_rows;
    uint64_t failures;
} sample_set;

typedef struct fixture_counts {
    unsigned char commit[32];
    uint64_t nodes;
    uint64_t hub_degree;
} fixture_counts;

static void fail(const char *message) {
    fprintf(stderr, "%s\n", message);
    exit(1);
}

static void require(int condition, const char *message) {
    if (!condition) {
        fail(message);
    }
}

static uint64_t monotonic_ns(void) {
#ifdef _WIN32
    LARGE_INTEGER frequency;
    LARGE_INTEGER counter;
    QueryPerformanceFrequency(&frequency);
    QueryPerformanceCounter(&counter);
    return (uint64_t)((counter.QuadPart * 1000000000ULL) / frequency.QuadPart);
#else
    struct timespec value;
    require(clock_gettime(CLOCK_MONOTONIC, &value) == 0, "clock_gettime failed");
    return (uint64_t)value.tv_sec * 1000000000ULL + (uint64_t)value.tv_nsec;
#endif
}

static void load_extension(sqlite3 *db, const char *path) {
    char *error = NULL;
    require(sqlite3_enable_load_extension(db, 1) == SQLITE_OK, "failed to enable extension loading");
    int rc = sqlite3_load_extension(db, path, "sqlite3_lithograph_init", &error);
    if (rc != SQLITE_OK) {
        fprintf(stderr, "sqlite3_load_extension failed: %s\n", error == NULL ? "unknown" : error);
        sqlite3_free(error);
        exit(1);
    }
    sqlite3_free(error);
}

static fixture_counts inspect_checkpoint_fixture(sqlite3 *db) {
    fixture_counts result = {0};
    sqlite3_stmt *statement = NULL;
    const char *metadata_sql =
        "SELECT checkpoints.commit_id, CAST(json_extract(entry.value,'$[1]') AS INTEGER) "
        "FROM main._lithograph_branches AS branch "
        "JOIN main._lithograph_checkpoints AS checkpoints ON checkpoints.commit_id=branch.commit_id "
        "JOIN main._lithograph_labels AS dictionary ON dictionary.name='ScaleNode' "
        "JOIN json_each(CAST(checkpoints.metadata AS TEXT),'$.labels') AS entry "
        "WHERE branch.name='main' AND CAST(json_extract(entry.value,'$[0]') AS INTEGER)=dictionary.id";
    require(sqlite3_prepare_v2(db, metadata_sql, -1, &statement, NULL) == SQLITE_OK,
            "failed to inspect checkpoint fixture");
    require(sqlite3_step(statement) == SQLITE_ROW, "main Branch is not pinned to the scale checkpoint");
    const void *commit = sqlite3_column_blob(statement, 0);
    require(commit != NULL && sqlite3_column_bytes(statement, 0) == 32, "scale checkpoint id is invalid");
    memcpy(result.commit, commit, 32);
    sqlite3_int64 nodes = sqlite3_column_int64(statement, 1);
    require(nodes > 0, "ScaleNode checkpoint cardinality is invalid");
    result.nodes = (uint64_t)nodes;
    sqlite3_finalize(statement);

    const char *hub_sql =
        "SELECT labels.node_id, types.id FROM main._lithograph_cp_labels AS labels "
        "JOIN main._lithograph_labels AS dictionary ON dictionary.id=labels.label_id "
        "JOIN main._lithograph_rel_types AS types ON types.name='SCALE_LINK' "
        "WHERE labels.commit_id=?1 AND dictionary.name='ScaleHub' LIMIT 1";
    require(sqlite3_prepare_v2(db, hub_sql, -1, &statement, NULL) == SQLITE_OK, "failed to locate scale hub");
    require(sqlite3_bind_blob(statement, 1, result.commit, 32, SQLITE_STATIC) == SQLITE_OK,
            "failed to bind scale checkpoint");
    require(sqlite3_step(statement) == SQLITE_ROW, "scale hub is missing from checkpoint");
    sqlite3_int64 hub = sqlite3_column_int64(statement, 0);
    sqlite3_int64 type_id = sqlite3_column_int64(statement, 1);
    sqlite3_finalize(statement);

    const char *degree_sql =
        "SELECT count(*) FROM main._lithograph_cp_relationships INDEXED BY _lithograph_cp_rel_out "
        "WHERE commit_id=?1 AND source_id=?2 AND type_id=?3";
    require(sqlite3_prepare_v2(db, degree_sql, -1, &statement, NULL) == SQLITE_OK,
            "failed to count scale hub degree");
    require(sqlite3_bind_blob(statement, 1, result.commit, 32, SQLITE_STATIC) == SQLITE_OK,
            "failed to bind scale checkpoint");
    require(sqlite3_bind_int64(statement, 2, hub) == SQLITE_OK, "failed to bind scale hub");
    require(sqlite3_bind_int64(statement, 3, type_id) == SQLITE_OK, "failed to bind relationship type");
    require(sqlite3_step(statement) == SQLITE_ROW, "scale hub degree returned no row");
    sqlite3_int64 hub_degree = sqlite3_column_int64(statement, 0);
    require(hub_degree > 0, "scale hub degree is invalid");
    result.hub_degree = (uint64_t)hub_degree;
    sqlite3_finalize(statement);
    return result;
}

static int consume_rows(
    sqlite3 *db,
    const char *sql,
    uint64_t expected_rows,
    uint64_t started,
    uint64_t *first_micros
) {
    sqlite3_stmt *statement = NULL;
    int rc = sqlite3_prepare_v2(db, sql, -1, &statement, NULL);
    if (rc != SQLITE_OK) {
        return rc;
    }
    uint64_t rows = 0;
    while ((rc = sqlite3_step(statement)) == SQLITE_ROW) {
        if (rows == 0) {
            *first_micros = (monotonic_ns() - started) / 1000;
        }
        rows += 1;
    }
    int finalize_rc = sqlite3_finalize(statement);
    if (rc != SQLITE_DONE) {
        return rc;
    }
    if (finalize_rc != SQLITE_OK) {
        return finalize_rc;
    }
    return rows == expected_rows ? SQLITE_OK : SQLITE_CORRUPT;
}

static sample_set run_rows_samples(
    sqlite3 *db,
    const char *sql,
    uint64_t expected_rows,
    unsigned samples
) {
    sample_set result = {
        .micros = (uint64_t *)calloc(samples, sizeof(uint64_t)),
        .first_micros = (uint64_t *)calloc(samples, sizeof(uint64_t)),
        .count = samples,
        .expected_rows = expected_rows,
    };
    require(result.micros != NULL && result.first_micros != NULL, "failed to allocate rows samples");
    for (unsigned index = 0; index < samples; index++) {
        uint64_t started = monotonic_ns();
        int rc = consume_rows(db, sql, expected_rows, started, &result.first_micros[index]);
        result.micros[index] = (monotonic_ns() - started) / 1000;
        result.failures += (uint64_t)(rc != SQLITE_OK);
    }
    return result;
}

static int compare_u64(const void *left, const void *right) {
    uint64_t a = *(const uint64_t *)left;
    uint64_t b = *(const uint64_t *)right;
    return (a > b) - (a < b);
}

static void print_samples(const char *name, sample_set *samples, int trailing) {
    qsort(samples->micros, samples->count, sizeof(uint64_t), compare_u64);
    qsort(samples->first_micros, samples->count, sizeof(uint64_t), compare_u64);
    unsigned median_index = samples->count / 2;
    uint64_t median = samples->micros[median_index];
    uint64_t maximum = samples->micros[samples->count - 1];
    uint64_t first_median = samples->first_micros[median_index];
    uint64_t first_maximum = samples->first_micros[samples->count - 1];
    printf(
        "\"%s\":{\"expectedRows\":%llu,\"failures\":%llu,\"firstRowMedianMicros\":%llu,\"firstRowMaxMicros\":%llu,\"medianMicros\":%llu,\"maxMicros\":%llu,\"samplesMicros\":[",
        name,
        (unsigned long long)samples->expected_rows,
        (unsigned long long)samples->failures,
        (unsigned long long)first_median,
        (unsigned long long)first_maximum,
        (unsigned long long)median,
        (unsigned long long)maximum
    );
    for (unsigned index = 0; index < samples->count; index++) {
        printf("%s%llu", index == 0 ? "" : ",", (unsigned long long)samples->micros[index]);
    }
    printf("]}%s", trailing ? "," : "");
}

int main(int argc, char **argv) {
    if (argc != 4 && argc != 5) {
        fprintf(stderr, "usage: phase11-stream-bench <extension> <database> <samples> [all|rows-label|rows-hub|rows-tx]\n");
        return 2;
    }
    const char *extension = argv[1];
    const char *database = argv[2];
    unsigned samples = (unsigned)strtoul(argv[3], NULL, 10);
    const char *mode = argc == 5 ? argv[4] : "all";
    require(samples >= 3 && samples <= 16, "stream samples must be 3..16");

    sqlite3 *db = NULL;
    require(sqlite3_open(database, &db) == SQLITE_OK, "failed to open stream database");
    load_extension(db, extension);
    fixture_counts fixture = inspect_checkpoint_fixture(db);

    const char *label_sql =
        "SELECT data FROM lithograph_rows('MATCH (:ScaleNode) RETURN 1') WHERE event='row'";
    const char *hub_sql =
        "SELECT data FROM lithograph_rows('MATCH (:ScaleHub)-[:SCALE_LINK]->(m) RETURN 1') WHERE event='row'";
    const char *tx_sql =
        "SELECT data FROM lithograph_rows('MATCH (n:ScaleNode) CALL (n) { RETURN 1 AS one } IN TRANSACTIONS OF 4096 ROWS RETURN one') WHERE event='row'";
    sample_set rows_label = {0};
    sample_set rows_hub = {0};
    sample_set rows_tx = {0};
    int run_all = strcmp(mode, "all") == 0;
    if (run_all || strcmp(mode, "rows-label") == 0) {
        rows_label = run_rows_samples(db, label_sql, fixture.nodes, samples);
    }
    if (run_all || strcmp(mode, "rows-hub") == 0) {
        rows_hub = run_rows_samples(db, hub_sql, fixture.hub_degree, samples);
    }
    if (strcmp(mode, "rows-tx") == 0) {
        rows_tx = run_rows_samples(db, tx_sql, fixture.nodes, samples);
    }

    printf("{\"samples\":%u,\"mode\":\"%s\",", samples, mode);
    if (run_all) {
        print_samples("rowsLabelScan", &rows_label, 1);
        print_samples("rowsHubScan", &rows_hub, 0);
    } else if (strcmp(mode, "rows-label") == 0) {
        print_samples("rowsLabelScan", &rows_label, 0);
    } else if (strcmp(mode, "rows-hub") == 0) {
        print_samples("rowsHubScan", &rows_hub, 0);
    } else if (strcmp(mode, "rows-tx") == 0) {
        print_samples("rowsTransactionSubquery", &rows_tx, 0);
    } else {
        fail("unknown stream benchmark mode");
    }
    printf("}\n");
    uint64_t failures = rows_label.failures + rows_hub.failures;
    failures += rows_tx.failures;
    free(rows_label.micros);
    free(rows_label.first_micros);
    free(rows_hub.micros);
    free(rows_hub.first_micros);
    free(rows_tx.micros);
    free(rows_tx.first_micros);
    sqlite3_close(db);
    require(failures == 0, "stream benchmark observed failures");
    return 0;
}
