#include <sqlite3.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#ifdef _WIN32
#include <windows.h>
#endif

enum {
    WARM_CONNECTIONS = 5,
    REOPEN_ITERATIONS = 100,
};

typedef struct timings {
    uint64_t *values;
    size_t len;
    size_t cap;
} timings;

typedef struct summary {
    uint64_t p50;
    uint64_t p95;
    uint64_t max;
    uint64_t failures;
} summary;

typedef struct reopen_summary {
    summary query;
    summary open_load;
} reopen_summary;

typedef struct adapter_report {
    summary rows_adjacency;
    summary rows_indexed;
    reopen_summary rows_adjacency_reopen;
    reopen_summary rows_indexed_reopen;
    summary sql_tx;
} adapter_report;

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

static void timing_push(timings *values, uint64_t micros) {
    if (values->len == values->cap) {
        size_t next = values->cap == 0 ? 32 : values->cap * 2;
        uint64_t *buffer = (uint64_t *)realloc(values->values, next * sizeof(uint64_t));
        require(buffer != NULL, "failed to grow timing buffer");
        values->values = buffer;
        values->cap = next;
    }
    values->values[values->len++] = micros;
}

static int compare_u64(const void *left, const void *right) {
    uint64_t a = *(const uint64_t *)left;
    uint64_t b = *(const uint64_t *)right;
    return (a > b) - (a < b);
}

static summary timing_summary(timings *values, uint64_t failures) {
    if (values->len == 0) {
        return (summary){.failures = failures};
    }
    qsort(values->values, values->len, sizeof(uint64_t), compare_u64);
    size_t p50 = (values->len - 1) / 2;
    size_t p95 = (size_t)(0.95 * (double)(values->len - 1));
    return (summary){
        .p50 = values->values[p50],
        .p95 = values->values[p95],
        .max = values->values[values->len - 1],
        .failures = failures,
    };
}

static int try_load_extension(sqlite3 *db, const char *path) {
    char *error = NULL;
    int rc = sqlite3_enable_load_extension(db, 1);
    if (rc == SQLITE_OK) {
        rc = sqlite3_load_extension(db, path, "sqlite3_lithograph_init", &error);
    }
    sqlite3_free(error);
    return rc;
}

static int open_loaded(const char *database, const char *extension, sqlite3 **db) {
    int rc = sqlite3_open(database, db);
    if (rc != SQLITE_OK) {
        return rc;
    }
    rc = try_load_extension(*db, extension);
    if (rc != SQLITE_OK) {
        sqlite3_close(*db);
        *db = NULL;
    }
    return rc;
}

static uint64_t target_scale_id(sqlite3 *db) {
    sqlite3_stmt *statement = NULL;
    require(
        sqlite3_prepare_v2(
            db,
            "SELECT max(properties.int_value) "
            "FROM main._lithograph_cp_properties AS properties "
            "JOIN main._lithograph_prop_keys AS keys ON keys.id=properties.key_id "
            "WHERE keys.name='scaleId'",
            -1,
            &statement,
            NULL
        ) == SQLITE_OK,
        "failed to prepare scale target lookup"
    );
    require(sqlite3_step(statement) == SQLITE_ROW, "scale target lookup returned no row");
    uint64_t maximum = (uint64_t)sqlite3_column_int64(statement, 0);
    sqlite3_finalize(statement);
    return maximum / 2 + 1;
}

static int consume_rows(sqlite3 *db, const char *sql, uint64_t expected_rows) {
    sqlite3_stmt *statement = NULL;
    int rc = sqlite3_prepare_v2(db, sql, -1, &statement, NULL);
    if (rc != SQLITE_OK) {
        return rc;
    }
    uint64_t rows = 0;
    while ((rc = sqlite3_step(statement)) == SQLITE_ROW) {
        rows += 1;
    }
    int finalize_rc = sqlite3_finalize(statement);
    if (rc == SQLITE_DONE && finalize_rc == SQLITE_OK && rows != expected_rows) {
        return SQLITE_CORRUPT;
    }
    if (rc == SQLITE_DONE && finalize_rc != SQLITE_OK) {
        return finalize_rc;
    }
    return rc == SQLITE_DONE ? SQLITE_OK : rc;
}

static int execute_scalar(sqlite3 *db, const char *sql) {
    sqlite3_stmt *statement = NULL;
    int rc = sqlite3_prepare_v2(db, sql, -1, &statement, NULL);
    if (rc != SQLITE_OK) {
        return rc;
    }
    rc = sqlite3_step(statement);
    if (rc == SQLITE_ROW) {
        rc = sqlite3_step(statement);
    }
    int finalize_rc = sqlite3_finalize(statement);
    if (rc != SQLITE_DONE) {
        return rc;
    }
    return finalize_rc;
}

static int execute_lithograph(sqlite3 *db, const char *query) {
    sqlite3_stmt *statement = NULL;
    int rc = sqlite3_prepare_v2(db, "SELECT lithograph(?1)", -1, &statement, NULL);
    if (rc != SQLITE_OK) {
        return rc;
    }
    rc = sqlite3_bind_text(statement, 1, query, -1, SQLITE_TRANSIENT);
    if (rc == SQLITE_OK) {
        rc = sqlite3_step(statement);
    }
    if (rc == SQLITE_ROW) {
        rc = sqlite3_step(statement);
    }
    int finalize_rc = sqlite3_finalize(statement);
    if (rc != SQLITE_DONE) {
        return rc;
    }
    return finalize_rc;
}

static summary benchmark_rows_warm_connections(
    const char *extension,
    const char *database,
    const char *query,
    unsigned iterations
) {
    timings values = {0};
    uint64_t failures = 0;
    for (unsigned connection_index = 0; connection_index < WARM_CONNECTIONS; connection_index++) {
        sqlite3 *db = NULL;
        require(open_loaded(database, extension, &db) == SQLITE_OK, "failed to open warm rows connection");
        require(consume_rows(db, query, 1) == SQLITE_OK, "rows warmup failed");
        for (unsigned index = 0; index < iterations; index++) {
            uint64_t started = monotonic_ns();
            int rc = consume_rows(db, query, 1);
            timing_push(&values, (monotonic_ns() - started) / 1000);
            failures += (uint64_t)(rc != SQLITE_OK);
        }
        sqlite3_close(db);
    }
    summary result = timing_summary(&values, failures);
    free(values.values);
    return result;
}

static reopen_summary benchmark_rows_reopen(
    const char *extension,
    const char *database,
    const char *query
) {
    timings query_values = {0};
    timings open_values = {0};
    uint64_t failures = 0;
    for (unsigned index = 0; index < REOPEN_ITERATIONS; index++) {
        sqlite3 *db = NULL;
        uint64_t open_started = monotonic_ns();
        int rc = open_loaded(database, extension, &db);
        timing_push(&open_values, (monotonic_ns() - open_started) / 1000);
        if (rc == SQLITE_OK) {
            uint64_t query_started = monotonic_ns();
            rc = consume_rows(db, query, 1);
            timing_push(&query_values, (monotonic_ns() - query_started) / 1000);
            sqlite3_close(db);
        }
        failures += (uint64_t)(rc != SQLITE_OK);
    }
    reopen_summary result = {
        .query = timing_summary(&query_values, failures),
        .open_load = timing_summary(&open_values, failures),
    };
    free(query_values.values);
    free(open_values.values);
    return result;
}

static int sql_tx_iteration(sqlite3 *db, unsigned ordinal) {
    int rc = execute_scalar(db, "SELECT lithograph_tx_begin('{}')");
    if (rc != SQLITE_OK) {
        return rc;
    }

    char create[256];
    unsigned value = 900000000U + ordinal * 2U;
    int written = snprintf(
        create,
        sizeof(create),
        "CREATE (:Phase11TxProbe {value:%u}) FINISH",
        value
    );
    require(written > 0 && (size_t)written < sizeof(create), "SQL create query overflow");
    rc = execute_lithograph(db, create);
    if (rc != SQLITE_OK) {
        (void)execute_scalar(db, "SELECT lithograph_tx_abort()");
        return rc;
    }

    char update[320];
    written = snprintf(
        update,
        sizeof(update),
        "MATCH (n:Phase11TxProbe) WHERE n.value=%u SET n.value=%u FINISH",
        value,
        value + 1U
    );
    require(written > 0 && (size_t)written < sizeof(update), "SQL update query overflow");
    rc = execute_lithograph(db, update);
    if (rc != SQLITE_OK) {
        (void)execute_scalar(db, "SELECT lithograph_tx_abort()");
        return rc;
    }
    return execute_scalar(db, "SELECT lithograph_tx_commit()");
}

static summary benchmark_sql_tx(sqlite3 *db, unsigned iterations) {
    timings values = {0};
    uint64_t failures = 0;
    for (unsigned index = 0; index < iterations; index++) {
        uint64_t started = monotonic_ns();
        int rc = sql_tx_iteration(db, index + 1);
        timing_push(&values, (monotonic_ns() - started) / 1000);
        failures += (uint64_t)(rc != SQLITE_OK);
    }
    summary result = timing_summary(&values, failures);
    free(values.values);
    return result;
}

static void print_summary(const char *name, summary value, int trailing) {
    printf(
        "\"%s\":{\"p50Micros\":%llu,\"p95Micros\":%llu,\"maxMicros\":%llu,\"failures\":%llu}%s",
        name,
        (unsigned long long)value.p50,
        (unsigned long long)value.p95,
        (unsigned long long)value.max,
        (unsigned long long)value.failures,
        trailing ? "," : ""
    );
}

static void print_adapter_report(const adapter_report *report, unsigned warm_iterations) {
    printf(
        "{\"warmConnections\":%u,\"warmIterationsPerConnection\":%u,\"reopenIterations\":%u,",
        WARM_CONNECTIONS,
        warm_iterations,
        REOPEN_ITERATIONS
    );
    print_summary("rowsAdjacencyWarm", report->rows_adjacency, 1);
    print_summary("rowsIndexedEqualityWarm", report->rows_indexed, 1);
    print_summary("rowsAdjacencyReopen", report->rows_adjacency_reopen.query, 1);
    print_summary("rowsAdjacencyOpenLoad", report->rows_adjacency_reopen.open_load, 1);
    print_summary("rowsIndexedEqualityReopen", report->rows_indexed_reopen.query, 1);
    print_summary("rowsIndexedEqualityOpenLoad", report->rows_indexed_reopen.open_load, 1);
    print_summary("sqlExplicitTransaction", report->sql_tx, 0);
    printf("}\n");
}

static int adapter_report_ok(const adapter_report *report) {
    return report->rows_adjacency.failures == 0
        && report->rows_indexed.failures == 0
        && report->rows_adjacency_reopen.query.failures == 0
        && report->rows_indexed_reopen.query.failures == 0
        && report->sql_tx.failures == 0;
}

int main(int argc, char **argv) {
    if (argc != 5) {
        fprintf(stderr, "usage: phase11-adapter-bench <extension> <database> <read-iterations> <tx-iterations>\n");
        return 2;
    }
    const char *extension = argv[1];
    const char *database = argv[2];
    unsigned read_iterations = (unsigned)strtoul(argv[3], NULL, 10);
    unsigned tx_iterations = (unsigned)strtoul(argv[4], NULL, 10);
    require(read_iterations > 0 && tx_iterations > 0, "benchmark iterations must be positive");

    sqlite3 *db = NULL;
    require(open_loaded(database, extension, &db) == SQLITE_OK, "failed to open adapter benchmark database");
    uint64_t target = target_scale_id(db);
    char indexed_sql[512];
    int written = snprintf(
        indexed_sql,
        sizeof(indexed_sql),
        "SELECT data FROM lithograph_rows("
        "'MATCH (n:ScaleNode) WHERE n.scaleId = %llu RETURN n.scaleId'"
        ") WHERE event='row'",
        (unsigned long long)target
    );
    require(written > 0 && (size_t)written < sizeof(indexed_sql), "indexed benchmark SQL overflow");
    const char *adjacency_sql =
        "SELECT data FROM lithograph_rows("
        "'MATCH (:ScaleLowDegree)-[:SCALE_LINK]->(m) RETURN 1'"
        ") WHERE event='row'";

    adapter_report report = {
        .rows_adjacency = benchmark_rows_warm_connections(
            extension, database, adjacency_sql, read_iterations
        ),
        .rows_indexed = benchmark_rows_warm_connections(
            extension, database, indexed_sql, read_iterations
        ),
        .rows_adjacency_reopen = benchmark_rows_reopen(extension, database, adjacency_sql),
        .rows_indexed_reopen = benchmark_rows_reopen(extension, database, indexed_sql),
        .sql_tx = benchmark_sql_tx(db, tx_iterations),
    };
    print_adapter_report(&report, read_iterations);
    require(adapter_report_ok(&report), "adapter benchmark observed failures");
    sqlite3_close(db);
    return 0;
}
