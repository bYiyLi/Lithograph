#if !defined(_WIN32) && !defined(_POSIX_C_SOURCE)
#define _POSIX_C_SOURCE 200809L
#endif

#include <sqlite3.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <time.h>

typedef struct query_sample {
    uint64_t started_ns;
    uint64_t first_row_ns;
    uint64_t finished_ns;
    uint64_t rows;
} query_sample;

typedef struct rebuild_sample {
    sqlite3_int64 indexed_entities;
    sqlite3_int64 embedded_texts;
    uint64_t elapsed_micros;
} rebuild_sample;

typedef struct hnsw_stats {
    sqlite3_int64 caches;
    sqlite3_int64 entries;
} hnsw_stats;

typedef struct provider_stats {
    sqlite3_int64 validate_calls;
    sqlite3_int64 batch_calls;
    sqlite3_int64 inputs;
} provider_stats;

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
    struct timespec value;
    require(clock_gettime(CLOCK_MONOTONIC, &value) == 0, "clock_gettime failed");
    return (uint64_t)value.tv_sec * UINT64_C(1000000000) + (uint64_t)value.tv_nsec;
}

static void load_extension(sqlite3 *db, const char *path, const char *entrypoint) {
    char *error = NULL;
    require(sqlite3_enable_load_extension(db, 1) == SQLITE_OK, "failed to enable extension loading");
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

static void exec_sql(sqlite3 *db, const char *sql, const char *failure) {
    char *error = NULL;
    int rc = sqlite3_exec(db, sql, NULL, NULL, &error);
    if (rc != SQLITE_OK) {
        fprintf(stderr, "%s: %s\n", failure, error == NULL ? "unknown" : error);
    }
    sqlite3_free(error);
    require(rc == SQLITE_OK, failure);
}

static sqlite3_int64 scalar_int64(sqlite3 *db, const char *sql) {
    sqlite3_stmt *statement = NULL;
    require(sqlite3_prepare_v2(db, sql, -1, &statement, NULL) == SQLITE_OK, "prepare failed");
    require(sqlite3_step(statement) == SQLITE_ROW, "scalar returned no row");
    sqlite3_int64 value = sqlite3_column_int64(statement, 0);
    require(sqlite3_finalize(statement) == SQLITE_OK, "scalar finalize failed");
    return value;
}

static void reset_provider(sqlite3 *db) {
    require(
        scalar_int64(db, "SELECT synthetic_embedding_reset('synthetic-a')") == 1,
        "failed to reset synthetic provider counters"
    );
}

static provider_stats read_provider_stats(sqlite3 *db) {
    provider_stats stats = {
        .validate_calls = scalar_int64(
            db,
            "SELECT synthetic_embedding_validate_calls('synthetic-a')"
        ),
        .batch_calls = scalar_int64(
            db,
            "SELECT synthetic_embedding_embed_calls('synthetic-a')"
        ),
        .inputs = scalar_int64(
            db,
            "SELECT synthetic_embedding_embed_inputs('synthetic-a')"
        ),
    };
    return stats;
}

static query_sample run_rows_query(sqlite3 *db) {
    const char *sql =
        "SELECT data FROM lithograph_rows("
        "'CALL db.index.semantic.queryNodes(''perf_sem'',''0'',{limit:8}) "
        "YIELD node, score RETURN node.id, score'"
        ") WHERE event='row'";
    sqlite3_stmt *statement = NULL;
    query_sample sample = {.started_ns = monotonic_ns()};
    require(sqlite3_prepare_v2(db, sql, -1, &statement, NULL) == SQLITE_OK,
            "Semantic rows query prepare failed");
    int rc;
    while ((rc = sqlite3_step(statement)) == SQLITE_ROW) {
        if (sample.first_row_ns == 0) {
            sample.first_row_ns = monotonic_ns();
        }
        sample.rows += 1;
    }
    sample.finished_ns = monotonic_ns();
    int finalize_rc = sqlite3_finalize(statement);
    require(rc == SQLITE_DONE, "Semantic rows query execution failed");
    require(finalize_rc == SQLITE_OK, "Semantic rows query finalize failed");
    require(sample.rows == 8, "Semantic rows query returned unexpected row count");
    require(sample.first_row_ns >= sample.started_ns, "Semantic rows query emitted no first row");
    return sample;
}

static rebuild_sample run_rebuild(sqlite3 *db) {
    const char *sql =
        "SELECT "
        "json_extract(result,'$.rows[0][0]'),"
        "json_extract(result,'$.rows[0][1]') "
        "FROM (SELECT lithograph("
        "'CALL db.index.semantic.rebuild(''perf_sem'',''branch/main'') "
        "YIELD indexedEntities, embeddedTexts "
        "RETURN indexedEntities, embeddedTexts'"
        ") AS result)";
    sqlite3_stmt *statement = NULL;
    const uint64_t started = monotonic_ns();
    require(sqlite3_prepare_v2(db, sql, -1, &statement, NULL) == SQLITE_OK,
            "rebuild prepare failed");
    require(sqlite3_step(statement) == SQLITE_ROW, "rebuild returned no row");
    rebuild_sample sample = {
        .indexed_entities = sqlite3_column_int64(statement, 0),
        .embedded_texts = sqlite3_column_int64(statement, 1),
        .elapsed_micros = (monotonic_ns() - started) / 1000,
    };
    require(sqlite3_finalize(statement) == SQLITE_OK, "rebuild finalize failed");
    return sample;
}

static hnsw_stats read_hnsw_stats(sqlite3 *db) {
    sqlite3_int64 table_exists = scalar_int64(
        db,
        "SELECT count(*) FROM sqlite_temp_master "
        "WHERE type='table' AND name='_lithograph_vector_cache_meta'"
    );
    if (table_exists == 0) {
        return (hnsw_stats){0, 0};
    }
    return (hnsw_stats){
        .caches = scalar_int64(
            db,
            "SELECT count(*) FROM temp._lithograph_vector_cache_meta WHERE complete=1"
        ),
        .entries = scalar_int64(
            db,
            "SELECT COALESCE(sum(entry_count),0) "
            "FROM temp._lithograph_vector_cache_meta WHERE complete=1"
        ),
    };
}

static sqlite3 *open_fixture(
    const char *database,
    const char *provider,
    const char *lithograph
) {
    sqlite3 *db = NULL;
    require(sqlite3_open(database, &db) == SQLITE_OK,
            "failed to open Phase 13 performance database");
    load_extension(db, provider, "sqlite3_syntheticembedding_init");
    load_extension(db, lithograph, "sqlite3_lithograph_init");
    return db;
}

static void seed_fixture(sqlite3 *db) {
    exec_sql(
        db,
        "SELECT lithograph_init();"
        "SELECT lithograph("
            "'UNWIND range(0,63) AS i "
            "CREATE (:PerfDoc {id:i, text:toString(i % 16)}) FINISH'"
        ");"
        "SELECT lithograph("
            "'CALL db.index.semantic.createNodeIndex("
                "''perf_sem'', [''PerfDoc''], ''text'', "
                "{provider:''synthetic-a'', providerConfig:{}, "
                "dimensions:4, similarity:''cosine''}"
            ")'"
        ");",
        "failed to seed Phase 13 performance fixture"
    );
}

static uint64_t first_row_micros(query_sample sample) {
    return (sample.first_row_ns - sample.started_ns) / 1000;
}

static uint64_t full_consume_micros(query_sample sample) {
    return (sample.finished_ns - sample.started_ns) / 1000;
}

static void print_query(
    const char *name,
    query_sample sample,
    provider_stats provider,
    hnsw_stats hnsw,
    int trailing
) {
    printf(
        "\"%s\":{\"rows\":%llu,\"firstRowMicros\":%llu,\"fullConsumeMicros\":%llu,"
        "\"validateCalls\":%lld,\"providerBatchCalls\":%lld,\"providerInputs\":%lld,"
        "\"tempHnswCaches\":%lld,\"tempHnswEntries\":%lld}%s",
        name,
        (unsigned long long)sample.rows,
        (unsigned long long)first_row_micros(sample),
        (unsigned long long)full_consume_micros(sample),
        (long long)provider.validate_calls,
        (long long)provider.batch_calls,
        (long long)provider.inputs,
        (long long)hnsw.caches,
        (long long)hnsw.entries,
        trailing ? "," : ""
    );
}

static void print_rebuild(
    const char *name,
    rebuild_sample sample,
    provider_stats provider,
    hnsw_stats hnsw,
    int trailing
) {
    printf(
        "\"%s\":{\"indexedEntities\":%lld,\"embeddedTexts\":%lld,"
        "\"providerBatchCalls\":%lld,\"providerInputs\":%lld,\"elapsedMicros\":%llu,"
        "\"tempHnswCaches\":%lld,\"tempHnswEntries\":%lld}%s",
        name,
        (long long)sample.indexed_entities,
        (long long)sample.embedded_texts,
        (long long)provider.batch_calls,
        (long long)provider.inputs,
        (unsigned long long)sample.elapsed_micros,
        (long long)hnsw.caches,
        (long long)hnsw.entries,
        trailing ? "," : ""
    );
}

int main(int argc, char **argv) {
    if (argc != 4) {
        fprintf(stderr, "usage: %s <lithograph-extension> <synthetic-provider> <database-path>\n", argv[0]);
        return 2;
    }
    const char *lithograph = argv[1];
    const char *provider = argv[2];
    const char *database = argv[3];
    remove(database);

    sqlite3 *db = open_fixture(database, provider, lithograph);
    seed_fixture(db);
    require(
        scalar_int64(
            db,
            "SELECT count(*) FROM main.sqlite_schema "
            "WHERE type='table' AND name='_lithograph_embedding_cache'"
        ) == 0,
        "Phase 15 performance fixture unexpectedly contains the removed Core embedding cache"
    );

    reset_provider(db);
    require(read_hnsw_stats(db).caches == 0, "cold fixture unexpectedly has TEMP HNSW");
    query_sample cold = run_rows_query(db);
    provider_stats cold_provider = read_provider_stats(db);
    hnsw_stats cold_hnsw = read_hnsw_stats(db);
    require(cold_provider.batch_calls > 0 && cold_provider.inputs > 0,
            "cold Semantic query did not call Provider");
    require(cold_hnsw.caches == 1 && cold_hnsw.entries == 64,
            "cold Semantic query did not build one complete TEMP HNSW");

    reset_provider(db);
    query_sample warm_local = run_rows_query(db);
    provider_stats warm_local_provider = read_provider_stats(db);
    hnsw_stats warm_local_hnsw = read_hnsw_stats(db);
    require(warm_local_provider.batch_calls > 0 && warm_local_provider.inputs > 0,
            "same-connection Semantic query did not embed its query text");
    require(warm_local_hnsw.caches == 1 && warm_local_hnsw.entries == 64,
            "same-connection Semantic query lost the TEMP HNSW");

    reset_provider(db);
    rebuild_sample rebuild = run_rebuild(db);
    provider_stats rebuild_provider = read_provider_stats(db);
    hnsw_stats rebuild_hnsw = read_hnsw_stats(db);
    require(rebuild.indexed_entities == 64, "rebuild indexed entity count differs");
    require(rebuild_provider.batch_calls > 0 && rebuild_provider.inputs > 0,
            "Semantic rebuild did not call Provider without a Core persistent cache");
    require(rebuild_hnsw.caches == 1 && rebuild_hnsw.entries == 64,
            "Semantic rebuild lost its TEMP HNSW");

    require(sqlite3_close(db) == SQLITE_OK, "failed to close first performance connection");

    db = open_fixture(database, provider, lithograph);
    require(read_hnsw_stats(db).caches == 0, "reopened connection unexpectedly inherited TEMP HNSW");
    reset_provider(db);
    query_sample reopened = run_rows_query(db);
    provider_stats reopened_provider = read_provider_stats(db);
    hnsw_stats reopened_hnsw = read_hnsw_stats(db);
    require(reopened_provider.batch_calls > 0 && reopened_provider.inputs > 0,
            "new-connection Semantic query did not rebuild Provider-derived state");
    require(reopened_hnsw.caches == 1 && reopened_hnsw.entries == 64,
            "new-connection Semantic query did not rebuild one complete TEMP HNSW");

    reset_provider(db);
    rebuild_sample repeated_rebuild = run_rebuild(db);
    provider_stats repeated_provider = read_provider_stats(db);
    hnsw_stats repeated_hnsw = read_hnsw_stats(db);
    require(repeated_rebuild.indexed_entities == 64, "repeated rebuild indexed entity count differs");
    require(repeated_provider.batch_calls > 0 && repeated_provider.inputs > 0,
            "repeated rebuild unexpectedly depended on removed Core cache");
    require(repeated_hnsw.caches == 1 && repeated_hnsw.entries == 64,
            "repeated rebuild lost the TEMP HNSW");

    printf(
        "{\"schemaVersion\":2,\"documents\":64,\"uniqueSourceTexts\":16,"
        "\"duplicateRate\":0.75,\"corePersistentEmbeddingCache\":false,"
    );
    print_query("coldQuery", cold, cold_provider, cold_hnsw, 1);
    print_query("sameConnectionQuery", warm_local, warm_local_provider, warm_local_hnsw, 1);
    print_rebuild("sameConnectionRebuild", rebuild, rebuild_provider, rebuild_hnsw, 1);
    print_query("newConnectionQuery", reopened, reopened_provider, reopened_hnsw, 1);
    print_rebuild(
        "newConnectionRebuild",
        repeated_rebuild,
        repeated_provider,
        repeated_hnsw,
        0
    );
    printf("}\n");

    require(sqlite3_close(db) == SQLITE_OK, "failed to close performance connection");
    remove(database);
    return 0;
}
