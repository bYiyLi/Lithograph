#include <sqlite3.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#ifdef _WIN32
#include <windows.h>
#else
#include <unistd.h>
#endif

typedef struct latency_samples {
    uint64_t *values;
    size_t len;
    size_t cap;
} latency_samples;

typedef struct reader_context {
    const char *database;
    const char *extension;
    uint64_t deadline_ns;
    uint64_t target_id;
    uint64_t operations;
    uint64_t rows;
    uint64_t busy;
    uint64_t failures;
    latency_samples latency;
} reader_context;

typedef struct writer_context {
    const char *database;
    const char *extension;
    uint64_t deadline_ns;
    uint64_t duration_ns;
    unsigned readers;
    uint64_t writes;
    uint64_t maintenance;
    uint64_t tag_moves;
    uint64_t gc_successes;
    uint64_t rebuild_successes;
    uint64_t busy;
    uint64_t failures;
    uint64_t peak_wal_bytes;
    latency_samples latency;
    latency_samples writer_wait;
    latency_samples writer_hold;
} writer_context;

static uint64_t wal_size(const char *database);

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

static void sleep_millis(unsigned millis) {
#ifdef _WIN32
    Sleep(millis);
#else
    struct timespec value = {
        .tv_sec = (time_t)(millis / 1000),
        .tv_nsec = (long)(millis % 1000) * 1000000L,
    };
    while (nanosleep(&value, &value) != 0) {
    }
#endif
}

static void latency_push(latency_samples *samples, uint64_t micros) {
    if (samples->len == samples->cap) {
        size_t next = samples->cap == 0 ? 4096 : samples->cap * 2;
        uint64_t *values = (uint64_t *)realloc(samples->values, next * sizeof(uint64_t));
        require(values != NULL, "failed to grow latency sample buffer");
        samples->values = values;
        samples->cap = next;
    }
    samples->values[samples->len++] = micros;
}

static int compare_u64(const void *left, const void *right) {
    uint64_t a = *(const uint64_t *)left;
    uint64_t b = *(const uint64_t *)right;
    return (a > b) - (a < b);
}

static uint64_t latency_percentile(latency_samples *samples, double percentile) {
    if (samples->len == 0) {
        return 0;
    }
    qsort(samples->values, samples->len, sizeof(uint64_t), compare_u64);
    size_t index = (size_t)(percentile * (double)(samples->len - 1));
    return samples->values[index];
}

static void open_loaded(const char *database, const char *extension, sqlite3 **out) {
    sqlite3 *db = NULL;
    require(sqlite3_open(database, &db) == SQLITE_OK, "failed to open stress database");
    require(sqlite3_busy_timeout(db, 1000) == SQLITE_OK, "failed to set busy timeout");
    require(sqlite3_enable_load_extension(db, 1) == SQLITE_OK, "failed to enable extension loading");
    char *error = NULL;
    int rc = sqlite3_load_extension(db, extension, "sqlite3_lithograph_init", &error);
    if (rc != SQLITE_OK) {
        fprintf(stderr, "sqlite3_load_extension failed: %s\n", error == NULL ? "unknown" : error);
        sqlite3_free(error);
        sqlite3_close(db);
        exit(1);
    }
    sqlite3_free(error);
    *out = db;
}

static int step_reader_query(sqlite3 *db, const char *sql, uint64_t expected_rows) {
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

static const char *reader_query(unsigned selector, uint64_t target_id, char *buffer, size_t size) {
    if (selector == 0) {
        return "SELECT data FROM lithograph_rows('MATCH (:ScaleLowDegree)-[:SCALE_LINK]->(m) RETURN 1') WHERE event='row'";
    }
    if (selector == 1) {
        int written = snprintf(
            buffer,
            size,
            "SELECT data FROM lithograph_rows('MATCH (n:ScaleNode) WHERE n.scaleId = %llu RETURN n.scaleId') WHERE event='row'",
            (unsigned long long)target_id
        );
        require(written > 0 && (size_t)written < size, "indexed reader SQL overflow");
        return buffer;
    }
    return "SELECT data FROM lithograph_rows('MATCH (:ScaleLowDegree)-[:SCALE_LINK]->(m) RETURN 1','{}','{\"graphView\":{\"requireAllLabels\":[\"ScaleNode\"]}}') WHERE event='row'";
}

static void classify_result(int rc, uint64_t *busy, uint64_t *failures) {
    if (rc == SQLITE_OK) {
        return;
    }
    if (rc == SQLITE_BUSY || rc == SQLITE_LOCKED) {
        *busy += 1;
    } else {
        *failures += 1;
    }
}

static void *reader_main(void *data) {
    reader_context *context = (reader_context *)data;
    sqlite3 *db = NULL;
    open_loaded(context->database, context->extension, &db);
    char sql[512];
    while (monotonic_ns() < context->deadline_ns) {
        unsigned selector = (unsigned)(context->operations % 3);
        const char *query = reader_query(selector, context->target_id, sql, sizeof(sql));
        uint64_t started = monotonic_ns();
        int rc = step_reader_query(db, query, 1);
        latency_push(&context->latency, (monotonic_ns() - started) / 1000);
        classify_result(rc, &context->busy, &context->failures);
        if (rc == SQLITE_OK) {
            context->rows += 1;
        }
        context->operations += 1;
    }
    sqlite3_close(db);
    return NULL;
}

static int exec_sql(sqlite3 *db, const char *sql) {
    char *error = NULL;
    int rc = sqlite3_exec(db, sql, NULL, NULL, &error);
    sqlite3_free(error);
    return rc;
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

static int writer_create(
    writer_context *context,
    sqlite3 *db,
    uint64_t ordinal
) {
    char query[256];
    int written = snprintf(
        query,
        sizeof(query),
        "CREATE (:Phase11Mixed {value:%llu}) FINISH",
        (unsigned long long)ordinal
    );
    require(written > 0 && (size_t)written < sizeof(query), "writer create query overflow");

    uint64_t wait_started = monotonic_ns();
    int rc = execute_scalar(db, "SELECT lithograph_tx_begin('{}')");
    latency_push(&context->writer_wait, (monotonic_ns() - wait_started) / 1000);
    if (rc != SQLITE_OK) {
        return rc;
    }

    uint64_t hold_started = monotonic_ns();
    rc = execute_lithograph(db, query);
    if (rc != SQLITE_OK) {
        (void)execute_scalar(db, "SELECT lithograph_tx_abort()");
        latency_push(&context->writer_hold, (monotonic_ns() - hold_started) / 1000);
        return rc;
    }
    rc = execute_scalar(db, "SELECT lithograph_tx_commit()");
    if (rc != SQLITE_OK) {
        (void)execute_scalar(db, "SELECT lithograph_tx_abort()");
    }
    latency_push(&context->writer_hold, (monotonic_ns() - hold_started) / 1000);
    return rc;
}

static int writer_tag(sqlite3 *db, unsigned readers, int create) {
    char sql[512];
    const char *procedure = create ? "create" : "move";
    int written = snprintf(
        sql,
        sizeof(sql),
        "SELECT lithograph('CALL lithograph.tag.%s(''phase11-mixed-%u'',''branch/main'') YIELD name RETURN name');",
        procedure,
        readers
    );
    require(written > 0 && (size_t)written < sizeof(sql), "writer tag SQL overflow");
    return exec_sql(db, sql);
}

static int writer_gc(sqlite3 *db) {
    return exec_sql(db, "SELECT lithograph('CALL lithograph.gc() YIELD commits RETURN commits');");
}

static int writer_rebuild(sqlite3 *db) {
    return exec_sql(
        db,
        "SELECT lithograph('CALL lithograph.index.rebuild(''scale_id'',''branch/main'') YIELD name RETURN name');"
    );
}

static void cleanup_writer_tag(sqlite3 *db, unsigned readers) {
    char sql[512];
    int written = snprintf(
        sql,
        sizeof(sql),
        "SELECT lithograph('CALL lithograph.tag.delete(''phase11-mixed-%u'') YIELD name RETURN name');",
        readers
    );
    require(written > 0 && (size_t)written < sizeof(sql), "writer tag cleanup SQL overflow");
    (void)exec_sql(db, sql);
}

static void writer_maintenance(writer_context *context, sqlite3 *db, int *rebuilt) {
    if (context->writes > 0 && context->writes % 50 == 0) {
        int rc = writer_tag(db, context->readers, 0);
        classify_result(rc, &context->busy, &context->failures);
        context->tag_moves += (uint64_t)(rc == SQLITE_OK);
        context->maintenance += 1;
    }
    if (context->writes > 0 && context->writes % 100 == 0) {
        int rc = writer_gc(db);
        classify_result(rc, &context->busy, &context->failures);
        context->gc_successes += (uint64_t)(rc == SQLITE_OK);
        context->maintenance += 1;
    }
    uint64_t remaining = context->deadline_ns > monotonic_ns() ? context->deadline_ns - monotonic_ns() : 0;
    if (!*rebuilt && remaining <= context->duration_ns / 2) {
        int rc = writer_rebuild(db);
        classify_result(rc, &context->busy, &context->failures);
        context->rebuild_successes += (uint64_t)(rc == SQLITE_OK);
        context->maintenance += 1;
        *rebuilt = rc == SQLITE_OK;
    }
}

static void *writer_main(void *data) {
    writer_context *context = (writer_context *)data;
    sqlite3 *db = NULL;
    open_loaded(context->database, context->extension, &db);
    classify_result(writer_tag(db, context->readers, 1), &context->busy, &context->failures);
    int rebuilt = 0;
    while (monotonic_ns() < context->deadline_ns) {
        uint64_t started = monotonic_ns();
        int rc = writer_create(context, db, context->writes + 1);
        latency_push(&context->latency, (monotonic_ns() - started) / 1000);
        classify_result(rc, &context->busy, &context->failures);
        if (rc == SQLITE_OK) {
            context->writes += 1;
        }
        writer_maintenance(context, db, &rebuilt);
        uint64_t wal = wal_size(context->database);
        if (wal > context->peak_wal_bytes) {
            context->peak_wal_bytes = wal;
        }
        sleep_millis(100);
    }
    sqlite3_close(db);
    return NULL;
}

static uint64_t file_size(const char *path) {
    FILE *file = fopen(path, "rb");
    if (file == NULL) {
        return 0;
    }
    require(fseek(file, 0, SEEK_END) == 0, "failed to seek stress file");
    long size = ftell(file);
    fclose(file);
    return size < 0 ? 0 : (uint64_t)size;
}

static uint64_t wal_size(const char *database) {
    char path[2048];
    int written = snprintf(path, sizeof(path), "%s-wal", database);
    require(written > 0 && (size_t)written < sizeof(path), "WAL path overflow");
    return file_size(path);
}

static uint64_t read_target_id(sqlite3 *db) {
    sqlite3_stmt *statement = NULL;
    require(
        sqlite3_prepare_v2(
            db,
            "SELECT CAST(json_extract(CAST(metadata AS TEXT),'$.nodes') AS INTEGER) "
            "FROM main._lithograph_checkpoints ORDER BY created_at DESC LIMIT 1",
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

static void ensure_format3_and_index(sqlite3 *db) {
    sqlite3_stmt *statement = NULL;
    require(
        sqlite3_prepare_v2(db, "SELECT storage_format FROM main._lithograph_meta WHERE id=1", -1, &statement, NULL) == SQLITE_OK,
        "failed to inspect stress storage format"
    );
    require(sqlite3_step(statement) == SQLITE_ROW, "stress storage format returned no row");
    require(sqlite3_column_int(statement, 0) == 3, "mixed stress database must already be format 3");
    sqlite3_finalize(statement);
    require(
        sqlite3_prepare_v2(
            db,
            "SELECT EXISTS(SELECT 1 FROM main._lithograph_index_generations WHERE complete=1 AND entry_count>0)",
            -1,
            &statement,
            NULL
        ) == SQLITE_OK,
        "failed to inspect persistent index generation"
    );
    require(sqlite3_step(statement) == SQLITE_ROW, "persistent index generation lookup returned no row");
    require(sqlite3_column_int(statement, 0) == 1, "mixed stress database must already have a ready persistent generation");
    sqlite3_finalize(statement);
}

static void aggregate_readers(
    reader_context *contexts,
    unsigned readers,
    uint64_t *operations,
    uint64_t *rows,
    uint64_t *busy,
    uint64_t *failures,
    uint64_t *p95
) {
    for (unsigned index = 0; index < readers; index++) {
        *operations += contexts[index].operations;
        *rows += contexts[index].rows;
        *busy += contexts[index].busy;
        *failures += contexts[index].failures;
        uint64_t value = latency_percentile(&contexts[index].latency, 0.95);
        if (value > *p95) {
            *p95 = value;
        }
        free(contexts[index].latency.values);
    }
}

int main(int argc, char **argv) {
    if (argc != 5) {
        fprintf(stderr, "usage: phase11-mixed-stress <extension> <database> <readers> <duration-seconds>\n");
        return 2;
    }
    const char *extension = argv[1];
    const char *database = argv[2];
    unsigned readers = (unsigned)strtoul(argv[3], NULL, 10);
    uint64_t duration_seconds = (uint64_t)strtoull(argv[4], NULL, 10);
    require(readers > 0 && readers <= 64, "reader count must be 1..64");
    require(duration_seconds > 0, "duration must be positive");

    sqlite3 *setup = NULL;
    open_loaded(database, extension, &setup);
    uint64_t target_id = read_target_id(setup);
    ensure_format3_and_index(setup);
    cleanup_writer_tag(setup, readers);
    sqlite3_close(setup);

    uint64_t duration_ns = duration_seconds * 1000000000ULL;
    uint64_t deadline = monotonic_ns() + duration_ns;
    reader_context *contexts = (reader_context *)calloc(readers, sizeof(reader_context));
    pthread_t *threads = (pthread_t *)calloc(readers, sizeof(pthread_t));
    require(contexts != NULL && threads != NULL, "failed to allocate reader state");
    for (unsigned index = 0; index < readers; index++) {
        contexts[index].database = database;
        contexts[index].extension = extension;
        contexts[index].deadline_ns = deadline;
        contexts[index].target_id = target_id;
        require(pthread_create(&threads[index], NULL, reader_main, &contexts[index]) == 0, "failed to start reader thread");
    }
    writer_context writer = {
        .database = database,
        .extension = extension,
        .deadline_ns = deadline,
        .duration_ns = duration_ns,
        .readers = readers,
    };
    pthread_t writer_thread;
    require(pthread_create(&writer_thread, NULL, writer_main, &writer) == 0, "failed to start writer thread");
    for (unsigned index = 0; index < readers; index++) {
        require(pthread_join(threads[index], NULL) == 0, "failed to join reader thread");
    }
    require(pthread_join(writer_thread, NULL) == 0, "failed to join writer thread");

    uint64_t reader_operations = 0;
    uint64_t reader_rows = 0;
    uint64_t reader_busy = 0;
    uint64_t reader_failures = 0;
    uint64_t reader_p95 = 0;
    aggregate_readers(
        contexts,
        readers,
        &reader_operations,
        &reader_rows,
        &reader_busy,
        &reader_failures,
        &reader_p95
    );
    uint64_t writer_p95 = latency_percentile(&writer.latency, 0.95);
    uint64_t writer_wait_p95 = latency_percentile(&writer.writer_wait, 0.95);
    uint64_t writer_hold_p95 = latency_percentile(&writer.writer_hold, 0.95);
    uint64_t final_wal_bytes = wal_size(database);
    printf(
        "{\"readers\":%u,\"durationSeconds\":%llu,\"readerOperations\":%llu,\"readerRows\":%llu,\"readerBusy\":%llu,\"readerFailures\":%llu,\"readerP95Micros\":%llu,\"writerWrites\":%llu,\"writerMaintenance\":%llu,\"tagMoves\":%llu,\"gcSuccesses\":%llu,\"rebuildSuccesses\":%llu,\"writerBusy\":%llu,\"writerFailures\":%llu,\"writerP95Micros\":%llu,\"writerWaitP95Micros\":%llu,\"writerHoldP95Micros\":%llu,\"peakWalBytes\":%llu,\"finalWalBytes\":%llu}\n",
        readers,
        (unsigned long long)duration_seconds,
        (unsigned long long)reader_operations,
        (unsigned long long)reader_rows,
        (unsigned long long)reader_busy,
        (unsigned long long)reader_failures,
        (unsigned long long)reader_p95,
        (unsigned long long)writer.writes,
        (unsigned long long)writer.maintenance,
        (unsigned long long)writer.tag_moves,
        (unsigned long long)writer.gc_successes,
        (unsigned long long)writer.rebuild_successes,
        (unsigned long long)writer.busy,
        (unsigned long long)writer.failures,
        (unsigned long long)writer_p95,
        (unsigned long long)writer_wait_p95,
        (unsigned long long)writer_hold_p95,
        (unsigned long long)writer.peak_wal_bytes,
        (unsigned long long)final_wal_bytes
    );
    require(reader_failures == 0, "mixed stress reader failures were observed");
    require(writer.failures == 0, "mixed stress writer failures were observed");
    require(writer.tag_moves > 0, "mixed stress did not complete a Tag move");
    require(writer.gc_successes > 0, "mixed stress did not complete canonical GC");
    require(writer.rebuild_successes > 0, "mixed stress did not complete Index rebuild");
    free(writer.latency.values);
    free(writer.writer_wait.values);
    free(writer.writer_hold.values);
    free(threads);
    free(contexts);
    return 0;
}
