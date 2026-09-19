#include <sqlite3.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#ifndef _WIN32
#include <pthread.h>
#include <unistd.h>
#endif

typedef struct rebuild_context {
    sqlite3 *db;
    int rc;
    char *error;
} rebuild_context;

typedef struct writer_sample {
    sqlite3_int64 wait_ms;
    sqlite3_int64 hold_ms;
    sqlite3_int64 total_ms;
} writer_sample;

typedef struct checkpoint_sample {
    int busy;
    int log_frames;
    int checkpointed_frames;
} checkpoint_sample;

static void fail(const char *message) {
    fprintf(stderr, "%s\n", message);
    exit(1);
}

static void require(int condition, const char *message) {
    if (!condition) {
        fail(message);
    }
}

static void load_extension(
    sqlite3 *db,
    const char *path,
    const char *entrypoint
) {
    char *error = NULL;
    require(
        sqlite3_enable_load_extension(db, 1) == SQLITE_OK,
        "failed to enable SQLite extension loading"
    );
    int rc = sqlite3_load_extension(db, path, entrypoint, &error);
    if (rc != SQLITE_OK) {
        fprintf(
            stderr,
            "sqlite3_load_extension(%s) failed: %s\n",
            entrypoint,
            error == NULL ? "unknown" : error
        );
        sqlite3_free(error);
        exit(1);
    }
    sqlite3_free(error);
}

static sqlite3_int64 scalar_int64(sqlite3 *db, const char *sql) {
    sqlite3_stmt *statement = NULL;
    require(sqlite3_prepare_v2(db, sql, -1, &statement, NULL) == SQLITE_OK, "prepare failed");
    require(sqlite3_step(statement) == SQLITE_ROW, "scalar returned no row");
    sqlite3_int64 value = sqlite3_column_int64(statement, 0);
    sqlite3_finalize(statement);
    return value;
}

static sqlite3_int64 monotonic_millis(void) {
    struct timespec value;
    require(clock_gettime(CLOCK_MONOTONIC, &value) == 0, "clock_gettime failed");
    return (sqlite3_int64)value.tv_sec * 1000 + value.tv_nsec / 1000000;
}

#ifndef _WIN32
static void *run_rebuild(void *user_data) {
    rebuild_context *context = (rebuild_context *)user_data;
    context->rc = sqlite3_exec(
        context->db,
        "SELECT lithograph("
            "'CALL db.index.semantic.rebuild(''sleep_sem'',''branch/main'')'"
        ");",
        NULL,
        NULL,
        &context->error
    );
    return NULL;
}
#endif

static void remove_database_files(
    const char *database,
    const char *wal_path,
    const char *shm_path
) {
    remove(database);
    remove(wal_path);
    remove(shm_path);
}

#ifndef _WIN32
static sqlite3 *open_semantic_connection(
    const char *database,
    const char *provider,
    const char *lithograph,
    const char *failure
) {
    sqlite3 *db = NULL;
    require(sqlite3_open(database, &db) == SQLITE_OK, failure);
    load_extension(db, provider, "sqlite3_syntheticembedding_init");
    load_extension(db, lithograph, "sqlite3_lithograph_init");
    return db;
}

static void seed_concurrency_fixture(sqlite3 *db) {
    char *error = NULL;
    require(
        sqlite3_exec(
            db,
            "PRAGMA journal_mode=WAL;"
            "SELECT lithograph_init();"
            "SELECT lithograph("
                "'CREATE (:SleepDoc {text:''alpha''}), (:SleepDoc {text:''beta''}) FINISH'"
            ");"
            "SELECT lithograph("
                "'CALL db.index.semantic.createNodeIndex("
                    "''sleep_sem'', [''SleepDoc''], ''text'', "
                    "{provider:''synthetic-a'', providerConfig:{sleep_ms:1000}, "
                    "dimensions:4, similarity:''cosine''}"
                ")'"
            ");",
            NULL,
            NULL,
            &error
        ) == SQLITE_OK,
        error == NULL ? "failed to seed semantic concurrency fixture" : error
    );
    sqlite3_free(error);
}

static void wait_for_provider_call(sqlite3 *db) {
    int active = 0;
    for (int attempt = 0; attempt < 600; ++attempt) {
        if (scalar_int64(db, "SELECT synthetic_embedding_active_calls()") > 0) {
            active = 1;
            break;
        }
        usleep(5000);
    }
    require(active, "semantic rebuild never entered the synthetic Provider call");
}

static writer_sample run_concurrent_writer(sqlite3 *db) {
    char *error = NULL;
    const sqlite3_int64 started = monotonic_millis();
    int rc = sqlite3_exec(db, "BEGIN IMMEDIATE;", NULL, NULL, &error);
    const sqlite3_int64 acquired = monotonic_millis();
    if (rc != SQLITE_OK) {
        fprintf(
            stderr,
            "concurrent writer could not acquire write ownership: %s\n",
            error == NULL ? "unknown" : error
        );
    }
    sqlite3_free(error);
    error = NULL;
    require(
        rc == SQLITE_OK,
        "semantic rebuild Provider stage held SQLite single-writer ownership"
    );

    rc = sqlite3_exec(
        db,
        "SELECT lithograph('CREATE (:ConcurrentWriter {value:1}) FINISH');",
        NULL,
        NULL,
        &error
    );
    if (rc != SQLITE_OK) {
        fprintf(
            stderr,
            "concurrent writer failed during Provider call: %s\n",
            error == NULL ? "unknown" : error
        );
    }
    sqlite3_free(error);
    error = NULL;
    if (rc != SQLITE_OK) {
        sqlite3_exec(db, "ROLLBACK;", NULL, NULL, NULL);
    }
    require(rc == SQLITE_OK, "concurrent writer failed after acquiring ownership");

    rc = sqlite3_exec(db, "COMMIT;", NULL, NULL, &error);
    const sqlite3_int64 finished = monotonic_millis();
    if (rc != SQLITE_OK) {
        fprintf(
            stderr,
            "concurrent writer commit failed: %s\n",
            error == NULL ? "unknown" : error
        );
    }
    sqlite3_free(error);
    require(rc == SQLITE_OK, "concurrent writer commit failed");
    writer_sample sample = {
        .wait_ms = acquired - started,
        .hold_ms = finished - acquired,
        .total_ms = finished - started,
    };
    require(
        sample.wait_ms < 700 && sample.total_ms < 700,
        "concurrent writer was delayed until the 1000 ms Provider call completed"
    );
    return sample;
}

static checkpoint_sample observe_wal_checkpoint(sqlite3 *db) {
    sqlite3_stmt *statement = NULL;
    require(
        sqlite3_prepare_v2(
            db,
            "PRAGMA wal_checkpoint(PASSIVE);",
            -1,
            &statement,
            NULL
        ) == SQLITE_OK,
        "failed to prepare WAL checkpoint observation"
    );
    require(
        sqlite3_step(statement) == SQLITE_ROW,
        "WAL checkpoint observation returned no row"
    );
    checkpoint_sample sample = {
        .busy = sqlite3_column_int(statement, 0),
        .log_frames = sqlite3_column_int(statement, 1),
        .checkpointed_frames = sqlite3_column_int(statement, 2),
    };
    require(
        sqlite3_finalize(statement) == SQLITE_OK,
        "failed to finalize WAL checkpoint observation"
    );
    require(
        sample.busy >= 0
            && sample.log_frames >= 0
            && sample.checkpointed_frames >= 0,
        "WAL checkpoint observation returned invalid counters"
    );
    return sample;
}

static void verify_concurrency_result(
    sqlite3 *writer_db,
    rebuild_context *context,
    writer_sample writer,
    checkpoint_sample checkpoint
) {
    if (context->rc != SQLITE_OK) {
        fprintf(
            stderr,
            "semantic rebuild failed: %s\n",
            context->error == NULL ? "unknown" : context->error
        );
    }
    require(context->rc == SQLITE_OK, "semantic rebuild failed after concurrent writer");
    sqlite3_free(context->error);
    require(
        scalar_int64(writer_db, "SELECT synthetic_embedding_active_calls()") == 0,
        "synthetic Provider active-call counter did not return to zero"
    );
    require(
        scalar_int64(
            writer_db,
            "SELECT synthetic_embedding_non_autocommit_calls()"
        ) == 0,
        "semantic rebuild called Provider while its SQLite connection was in a transaction"
    );
    require(
        scalar_int64(
            writer_db,
            "SELECT count(*) FROM main._lithograph_embedding_cache"
        ) == 2,
        "semantic rebuild did not atomically publish two deduplicated cache entries"
    );
    require(
        scalar_int64(
            writer_db,
            "SELECT json_extract(lithograph("
                "'MATCH (n:ConcurrentWriter) RETURN count(n)'"
            "), '$.rows[0][0]')"
        ) == 1,
        "concurrent graph writer was not committed"
    );
    printf(
        "{\"schemaVersion\":1,\"providerSleepMs\":1000,"
        "\"writerWaitMs\":%lld,\"writerHoldMs\":%lld,\"writerTotalMs\":%lld,"
        "\"walCheckpoint\":{\"busy\":%d,\"logFrames\":%d,\"checkpointedFrames\":%d},"
        "\"nonAutocommitProviderCalls\":0,\"persistentCacheEntries\":2}\n",
        (long long)writer.wait_ms,
        (long long)writer.hold_ms,
        (long long)writer.total_ms,
        checkpoint.busy,
        checkpoint.log_frames,
        checkpoint.checkpointed_frames
    );
}
#endif

int main(int argc, char **argv) {
#ifdef _WIN32
    (void)argc;
    (void)argv;
    puts("Phase 13 semantic concurrency smoke is covered by Unix CI");
    return 0;
#else
    if (argc != 4) {
        fprintf(
            stderr,
            "usage: %s <lithograph-extension> <synthetic-provider> <database-path>\n",
            argv[0]
        );
        return 2;
    }
    const char *lithograph = argv[1];
    const char *provider = argv[2];
    const char *database = argv[3];
    char wal_path[4096];
    char shm_path[4096];
    sqlite3_snprintf((int)sizeof(wal_path), wal_path, "%s-wal", database);
    sqlite3_snprintf((int)sizeof(shm_path), shm_path, "%s-shm", database);
    remove_database_files(database, wal_path, shm_path);

    sqlite3 *rebuild_db = open_semantic_connection(
        database,
        provider,
        lithograph,
        "failed to open rebuild database"
    );
    seed_concurrency_fixture(rebuild_db);
    sqlite3 *writer_db = open_semantic_connection(
        database,
        provider,
        lithograph,
        "failed to open writer database"
    );
    sqlite3_busy_timeout(writer_db, 0);

    rebuild_context context = {
        .db = rebuild_db,
        .rc = SQLITE_ERROR,
        .error = NULL,
    };
    pthread_t rebuild_thread;
    require(
        pthread_create(&rebuild_thread, NULL, run_rebuild, &context) == 0,
        "failed to start semantic rebuild thread"
    );
    wait_for_provider_call(writer_db);
    const writer_sample writer = run_concurrent_writer(writer_db);
    require(
        scalar_int64(writer_db, "SELECT synthetic_embedding_active_calls()") > 0,
        "Provider call completed before WAL/resource observation"
    );
    const checkpoint_sample checkpoint = observe_wal_checkpoint(writer_db);
    require(
        pthread_join(rebuild_thread, NULL) == 0,
        "failed to join semantic rebuild thread"
    );
    verify_concurrency_result(writer_db, &context, writer, checkpoint);

    require(sqlite3_close(writer_db) == SQLITE_OK, "failed to close writer database");
    require(sqlite3_close(rebuild_db) == SQLITE_OK, "failed to close rebuild database");
    remove_database_files(database, wal_path, shm_path);
    return 0;
#endif
}
