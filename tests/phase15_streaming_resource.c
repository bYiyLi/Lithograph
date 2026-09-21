#include <sqlite3.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#ifdef _WIN32
#include <windows.h>
#endif

static void fail(sqlite3 *db, const char *message) {
    fprintf(stderr, "%s: %s\n", message, db == NULL ? "no database" : sqlite3_errmsg(db));
    exit(1);
}

static void require(sqlite3 *db, int condition, const char *message) {
    if (!condition) {
        fail(db, message);
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
    if (clock_gettime(CLOCK_MONOTONIC, &value) != 0) {
        return 0;
    }
    return (uint64_t)value.tv_sec * 1000000000ULL + (uint64_t)value.tv_nsec;
#endif
}

static void sleep_micros(uint64_t micros) {
    if (micros == 0) {
        return;
    }
#ifdef _WIN32
    Sleep((DWORD)((micros + 999ULL) / 1000ULL));
#else
    struct timespec request = {
        .tv_sec = (time_t)(micros / 1000000ULL),
        .tv_nsec = (long)((micros % 1000000ULL) * 1000ULL),
    };
    while (nanosleep(&request, &request) != 0) {
    }
#endif
}

static void load_extension(sqlite3 *db, const char *path) {
    char *error = NULL;
    require(db, sqlite3_enable_load_extension(db, 1) == SQLITE_OK, "failed to enable extension loading");
    int rc = sqlite3_load_extension(db, path, "sqlite3_lithograph_init", &error);
    if (rc != SQLITE_OK) {
        fprintf(stderr, "sqlite3_load_extension failed: %s\n", error == NULL ? "unknown" : error);
        sqlite3_free(error);
        exit(1);
    }
    sqlite3_free(error);
}

static void execute(sqlite3 *db, const char *sql) {
    char *error = NULL;
    int rc = sqlite3_exec(db, sql, NULL, NULL, &error);
    if (rc != SQLITE_OK) {
        fprintf(stderr, "SQL failed (%s): %s\n", sql, error == NULL ? sqlite3_errmsg(db) : error);
        sqlite3_free(error);
        exit(1);
    }
    sqlite3_free(error);
}

static void branch_head(sqlite3 *db, char output[65]) {
    sqlite3_stmt *statement = NULL;
    require(db,
            sqlite3_prepare_v2(
                db,
                "SELECT lower(hex(commit_id)) FROM main._lithograph_branches WHERE name='main'",
                -1,
                &statement,
                NULL
            ) == SQLITE_OK,
            "failed to prepare branch head");
    require(db, sqlite3_step(statement) == SQLITE_ROW, "main branch head is missing");
    const unsigned char *text = sqlite3_column_text(statement, 0);
    require(db, text != NULL && sqlite3_column_bytes(statement, 0) == 64, "main branch head is invalid");
    memcpy(output, text, 64);
    output[64] = '\0';
    require(db, sqlite3_finalize(statement) == SQLITE_OK, "failed to finalize branch head");
}

typedef struct resource_options {
    const char *extension;
    const char *database;
    const char *mode;
    const char *csv_uri;
    uint64_t requested_rows;
    uint64_t delay_micros;
    int read_only;
    int baseline;
    int early;
    int slow;
} resource_options;

typedef struct resource_metrics {
    uint64_t started;
    uint64_t columns_micros;
    uint64_t first_row_micros;
    uint64_t rows;
    uint64_t summary_micros;
} resource_metrics;

static int valid_mode(const char *mode) {
    return strcmp(mode, "read") == 0 || strcmp(mode, "tx") == 0 ||
        strcmp(mode, "txcsv") == 0 ||
        strcmp(mode, "baseline") == 0 || strcmp(mode, "fast") == 0 ||
        strcmp(mode, "slow") == 0 || strcmp(mode, "early") == 0;
}

static resource_options parse_options(int argc, char **argv) {
    if (argc != 6) {
        fprintf(stderr,
                "usage: phase15-streaming-resource <extension> <database> <read|tx|txcsv|baseline|fast|slow|early> <rows> <delay-micros>\n");
        exit(2);
    }
    resource_options options = {
        .extension = argv[1],
        .database = argv[2],
        .mode = argv[3],
        .csv_uri = getenv("LITHOGRAPH_PHASE15_CSV_URI"),
        .requested_rows = strtoull(argv[4], NULL, 10),
        .delay_micros = strtoull(argv[5], NULL, 10),
    };
    require(NULL, options.requested_rows > 0 && options.requested_rows <= 10000000ULL,
            "rows must be 1..10000000");
    require(NULL, valid_mode(options.mode),
            "mode must be read, tx, txcsv, baseline, fast, slow, or early");
    if (strcmp(options.mode, "txcsv") == 0) {
        require(NULL, options.csv_uri != NULL && options.csv_uri[0] != '\0',
                "txcsv mode requires LITHOGRAPH_PHASE15_CSV_URI");
    }
    options.read_only = strcmp(options.mode, "read") == 0 || strcmp(options.mode, "tx") == 0 ||
        strcmp(options.mode, "txcsv") == 0;
    options.baseline = strcmp(options.mode, "baseline") == 0;
    options.early = strcmp(options.mode, "early") == 0;
    options.slow = strcmp(options.mode, "slow") == 0;
    return options;
}

static char *build_cypher(const resource_options *options) {
    if (strcmp(options->mode, "read") == 0) {
        return sqlite3_mprintf(
            "MATCH (n:ScaleNode) WHERE n.scaleId > 0 AND n.scaleId <= %llu RETURN n.scaleId",
            (unsigned long long)options->requested_rows
        );
    }
    if (strcmp(options->mode, "tx") == 0) {
        return sqlite3_mprintf(
            "MATCH (n:ScaleNode) WHERE n.scaleId > 0 AND n.scaleId <= %llu "
            "CALL (n) { RETURN 1 AS one } IN TRANSACTIONS OF 4096 ROWS RETURN one",
            (unsigned long long)options->requested_rows
        );
    }
    if (strcmp(options->mode, "txcsv") == 0) {
        return sqlite3_mprintf(
            "LOAD CSV FROM '%q' AS row "
            "CALL (row) { RETURN 1 AS one } IN TRANSACTIONS OF 4096 ROWS RETURN one",
            options->csv_uri
        );
    }
    if (options->baseline) {
        return sqlite3_mprintf(
            "MATCH (n:ScaleNode) WHERE n.scaleId > 0 AND n.scaleId <= %llu "
            "SET n.phase15Probe = n.scaleId FINISH",
            (unsigned long long)options->requested_rows
        );
    }
    return sqlite3_mprintf(
        "MATCH (n:ScaleNode) WHERE n.scaleId > 0 AND n.scaleId <= %llu "
        "SET n.phase15Probe = n.scaleId RETURN n.scaleId",
        (unsigned long long)options->requested_rows
    );
}

static sqlite3_stmt *prepare_stream(sqlite3 *db, const resource_options *options) {
    char *cypher = build_cypher(options);
    require(db, cypher != NULL, "failed to allocate Cypher");
    char *sql = sqlite3_mprintf("SELECT event,data FROM lithograph_rows('%q')", cypher);
    sqlite3_free(cypher);
    require(db, sql != NULL, "failed to allocate rows SQL");
    sqlite3_stmt *statement = NULL;
    require(db, sqlite3_prepare_v2(db, sql, -1, &statement, NULL) == SQLITE_OK,
            "failed to prepare write stream");
    sqlite3_free(sql);
    return statement;
}

static int consume_stream(
    sqlite3 *db,
    sqlite3_stmt *statement,
    const resource_options *options,
    resource_metrics *metrics
) {
    metrics->started = monotonic_ns();
    int rc = sqlite3_step(statement);
    require(db, rc == SQLITE_ROW, "stream did not return columns event");
    metrics->columns_micros = (monotonic_ns() - metrics->started) / 1000ULL;
    const unsigned char *event = sqlite3_column_text(statement, 0);
    require(db, event != NULL && strcmp((const char *)event, "columns") == 0,
            "stream did not begin with columns");
    while ((rc = sqlite3_step(statement)) == SQLITE_ROW) {
        event = sqlite3_column_text(statement, 0);
        require(db, event != NULL, "stream event is NULL");
        if (strcmp((const char *)event, "row") == 0) {
            metrics->rows += 1;
            if (metrics->rows == 1) {
                metrics->first_row_micros = (monotonic_ns() - metrics->started) / 1000ULL;
                if (options->early) {
                    break;
                }
            }
            if (options->slow) {
                sleep_micros(options->delay_micros);
            }
        } else if (strcmp((const char *)event, "summary") == 0) {
            metrics->summary_micros = (monotonic_ns() - metrics->started) / 1000ULL;
        }
    }
    return rc;
}

static void finish_stream(
    sqlite3 *db,
    sqlite3_stmt *statement,
    const resource_options *options,
    const resource_metrics *metrics,
    int rc,
    const char before[65]
) {
    char after[65];
    if (options->early) {
        require(db, metrics->rows == 1, "early-close mode did not observe exactly one row");
        require(db, sqlite3_finalize(statement) == SQLITE_OK,
                "failed to finalize early-close stream");
        branch_head(db, after);
        require(db, strcmp(before, after) == 0,
                "early-close did not rollback the unpublished write");
        return;
    }
    require(db, rc == SQLITE_DONE, "write stream did not finish");
    require(db, metrics->rows == (options->baseline ? 0 : options->requested_rows),
            "stream row count differs from requested rows");
    require(db, metrics->summary_micros != 0, "stream did not expose summary");
    require(db, sqlite3_finalize(statement) == SQLITE_OK, "failed to finalize stream");
}

static void report_metrics(
    const resource_options *options,
    const resource_metrics *metrics,
    uint64_t total_micros
) {
    uint64_t writer_hold_micros = options->read_only ? 0 : total_micros;
    printf(
        "{\"mode\":\"%s\",\"requestedRows\":%llu,\"rowsObserved\":%llu,"
        "\"delayMicros\":%llu,\"columnsMicros\":%llu,\"firstRowMicros\":%llu,"
        "\"summaryMicros\":%llu,\"totalMicros\":%llu,\"writerHoldMicros\":%llu}\n",
        options->mode,
        (unsigned long long)options->requested_rows,
        (unsigned long long)metrics->rows,
        (unsigned long long)options->delay_micros,
        (unsigned long long)metrics->columns_micros,
        (unsigned long long)metrics->first_row_micros,
        (unsigned long long)metrics->summary_micros,
        (unsigned long long)total_micros,
        (unsigned long long)writer_hold_micros
    );
}

int main(int argc, char **argv) {
    resource_options options = parse_options(argc, argv);
    sqlite3 *db = NULL;
    require(db, sqlite3_open(options.database, &db) == SQLITE_OK,
            "failed to open resource database");
    load_extension(db, options.extension);
    char before[65];
    char after[65];
    branch_head(db, before);
    if (!options.read_only) {
        execute(db, "BEGIN");
    }
    sqlite3_stmt *statement = prepare_stream(db, &options);
    resource_metrics metrics = {0};
    int rc = consume_stream(db, statement, &options, &metrics);
    finish_stream(db, statement, &options, &metrics, rc, before);
    if (!options.read_only) {
        execute(db, "ROLLBACK");
    }
    branch_head(db, after);
    require(db, strcmp(before, after) == 0, "resource workload changed the branch head");
    uint64_t total_micros = (monotonic_ns() - metrics.started) / 1000ULL;
    report_metrics(&options, &metrics, total_micros);
    require(db, sqlite3_close(db) == SQLITE_OK, "failed to close resource database");
    return 0;
}
