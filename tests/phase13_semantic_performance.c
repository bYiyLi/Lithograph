#if !defined(_WIN32) && !defined(_POSIX_C_SOURCE)
#define _POSIX_C_SOURCE 200809L
#endif

#include <sqlite3.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#ifdef _WIN32
#include <windows.h>
#else
#include <dlfcn.h>
#endif

#include "lithograph.h"

typedef int (*execute_fn)(
    sqlite3 *, const char *, size_t, const char *, size_t, const char *, size_t,
    lithograph_event_callback_v1, void *, char **
);
typedef void (*free_fn)(void *);

typedef struct query_sample {
    uint64_t started_ns;
    uint64_t first_row_ns;
    uint64_t finished_ns;
    uint64_t rows;
} query_sample;

typedef struct rebuild_sample {
    sqlite3_int64 indexed_entities;
    sqlite3_int64 embedded_texts;
    sqlite3_int64 cache_hits;
    uint64_t elapsed_micros;
} rebuild_sample;

typedef struct cache_stats {
    sqlite3_int64 used_bytes;
    sqlite3_int64 entries;
    sqlite3_int64 spaces;
} cache_stats;

typedef struct hnsw_stats {
    sqlite3_int64 caches;
    sqlite3_int64 entries;
} hnsw_stats;

typedef struct performance_report {
    query_sample cold;
    query_sample warm_local;
    query_sample warm_persistent;
    rebuild_sample cold_rebuild;
    rebuild_sample warm_rebuild;
    cache_stats persistent_cache;
    hnsw_stats hnsw_after_cold;
    hnsw_stats hnsw_after_local_warm;
    hnsw_stats hnsw_after_cold_rebuild;
    hnsw_stats hnsw_after_warm_rebuild;
    hnsw_stats hnsw_after_persistent_query;
    sqlite3_int64 cold_validate;
    sqlite3_int64 cold_batches;
    sqlite3_int64 cold_inputs;
    sqlite3_int64 cold_persistent;
    sqlite3_int64 local_validate;
    sqlite3_int64 local_batches;
    sqlite3_int64 local_inputs;
    sqlite3_int64 rebuild_batches;
    sqlite3_int64 rebuild_inputs;
    sqlite3_int64 warm_rebuild_batches;
    sqlite3_int64 warm_rebuild_inputs;
    sqlite3_int64 persistent_validate;
    sqlite3_int64 persistent_batches;
    sqlite3_int64 persistent_inputs;
} performance_report;

#ifdef _WIN32
typedef HMODULE library_handle;
static library_handle open_library(const char *path) { return LoadLibraryA(path); }
static FARPROC load_symbol(library_handle library, const char *name) {
    return GetProcAddress(library, name);
}
static void close_library(library_handle library) { FreeLibrary(library); }
#else
typedef void *library_handle;
static library_handle open_library(const char *path) {
    return dlopen(path, RTLD_NOW | RTLD_LOCAL);
}
static void *load_symbol(library_handle library, const char *name) {
    return dlsym(library, name);
}
static void close_library(library_handle library) { dlclose(library); }
#endif

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

static void load_extension(
    sqlite3 *db,
    const char *path,
    const char *entrypoint
) {
    char *error = NULL;
    require(
        sqlite3_enable_load_extension(db, 1) == SQLITE_OK,
        "failed to enable extension loading"
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

static int query_callback(
    void *user_data,
    lithograph_event_kind_v1 kind,
    const unsigned char *json,
    size_t json_len
) {
    (void)json;
    (void)json_len;
    query_sample *sample = (query_sample *)user_data;
    if (kind == LITHOGRAPH_EVENT_ROW_V1) {
        if (sample->first_row_ns == 0) {
            sample->first_row_ns = monotonic_ns();
        }
        sample->rows += 1;
    }
    return 0;
}

static query_sample run_native_query(
    sqlite3 *db,
    execute_fn execute,
    free_fn lithograph_free
) {
    const char *query =
        "CALL db.index.semantic.queryNodes('perf_sem','0',{limit:8}) "
        "YIELD node, score RETURN node.id, score";
    query_sample sample = {.started_ns = monotonic_ns()};
    char *error = NULL;
    int rc = execute(
        db,
        query,
        strlen(query),
        "{}",
        2,
        "{}",
        2,
        query_callback,
        &sample,
        &error
    );
    sample.finished_ns = monotonic_ns();
    if (rc != SQLITE_OK && error != NULL) {
        fprintf(stderr, "Semantic Native query failed: %s\n", error);
    }
    lithograph_free(error);
    require(rc == SQLITE_OK, "Semantic Native query failed");
    require(sample.rows == 8, "Semantic Native query returned unexpected row count");
    require(sample.first_row_ns >= sample.started_ns, "Semantic Native query emitted no first row");
    return sample;
}

static rebuild_sample run_rebuild(sqlite3 *db) {
    const char *sql =
        "SELECT "
        "json_extract(result,'$.rows[0][0]'),"
        "json_extract(result,'$.rows[0][1]'),"
        "json_extract(result,'$.rows[0][2]') "
        "FROM (SELECT lithograph("
        "'CALL db.index.semantic.rebuild(''perf_sem'',''branch/main'') "
        "YIELD indexedEntities, embeddedTexts, cacheHits "
        "RETURN indexedEntities, embeddedTexts, cacheHits'"
        ") AS result)";
    sqlite3_stmt *statement = NULL;
    const uint64_t started = monotonic_ns();
    require(sqlite3_prepare_v2(db, sql, -1, &statement, NULL) == SQLITE_OK, "rebuild prepare failed");
    require(sqlite3_step(statement) == SQLITE_ROW, "rebuild returned no row");
    rebuild_sample sample = {
        .indexed_entities = sqlite3_column_int64(statement, 0),
        .embedded_texts = sqlite3_column_int64(statement, 1),
        .cache_hits = sqlite3_column_int64(statement, 2),
        .elapsed_micros = (monotonic_ns() - started) / 1000,
    };
    require(sqlite3_finalize(statement) == SQLITE_OK, "rebuild finalize failed");
    return sample;
}

static cache_stats read_cache_stats(sqlite3 *db) {
    const char *sql =
        "SELECT "
        "json_extract(result,'$.rows[0][0]'),"
        "json_extract(result,'$.rows[0][1]'),"
        "json_extract(result,'$.rows[0][2]') "
        "FROM (SELECT lithograph("
        "'CALL db.index.semantic.cache.stats() "
        "YIELD usedBytes, entries, spaces RETURN usedBytes, entries, spaces'"
        ") AS result)";
    sqlite3_stmt *statement = NULL;
    require(sqlite3_prepare_v2(db, sql, -1, &statement, NULL) == SQLITE_OK, "cache stats prepare failed");
    require(sqlite3_step(statement) == SQLITE_ROW, "cache stats returned no row");
    cache_stats stats = {
        .used_bytes = sqlite3_column_int64(statement, 0),
        .entries = sqlite3_column_int64(statement, 1),
        .spaces = sqlite3_column_int64(statement, 2),
    };
    require(sqlite3_finalize(statement) == SQLITE_OK, "cache stats finalize failed");
    return stats;
}

static hnsw_stats read_hnsw_stats(sqlite3 *db) {
    sqlite3_int64 table_exists = scalar_int64(
        db,
        "SELECT count(*) FROM sqlite_temp_master "
        "WHERE type='table' AND name='_lithograph_vector_cache_meta'"
    );
    if (table_exists == 0) {
        hnsw_stats empty = {0, 0};
        return empty;
    }
    hnsw_stats stats = {
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
    return stats;
}

static sqlite3 *open_fixture(
    const char *database,
    const char *provider,
    const char *lithograph
) {
    sqlite3 *db = NULL;
    require(sqlite3_open(database, &db) == SQLITE_OK, "failed to open Phase 13 performance database");
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

static void print_report(const performance_report *report) {
    printf("{\"schemaVersion\":1,\"documents\":64,\"uniqueSourceTexts\":16,\"duplicateRate\":0.75,");
    printf(
        "\"coldQuery\":{\"rows\":%llu,\"firstRowMicros\":%llu,\"fullConsumeMicros\":%llu,"
        "\"validateCalls\":%lld,\"providerBatchCalls\":%lld,\"providerInputs\":%lld,"
        "\"persistentCacheEntries\":%lld,\"tempHnswCaches\":%lld,\"tempHnswEntries\":%lld},",
        (unsigned long long)report->cold.rows,
        (unsigned long long)first_row_micros(report->cold),
        (unsigned long long)full_consume_micros(report->cold),
        (long long)report->cold_validate,
        (long long)report->cold_batches,
        (long long)report->cold_inputs,
        (long long)report->cold_persistent,
        (long long)report->hnsw_after_cold.caches,
        (long long)report->hnsw_after_cold.entries
    );
    printf(
        "\"sameConnectionWarmQuery\":{\"firstRowMicros\":%llu,\"fullConsumeMicros\":%llu,"
        "\"validateCalls\":%lld,\"providerBatchCalls\":%lld,\"providerInputs\":%lld,"
        "\"tempHnswCaches\":%lld,\"tempHnswEntries\":%lld},",
        (unsigned long long)first_row_micros(report->warm_local),
        (unsigned long long)full_consume_micros(report->warm_local),
        (long long)report->local_validate,
        (long long)report->local_batches,
        (long long)report->local_inputs,
        (long long)report->hnsw_after_local_warm.caches,
        (long long)report->hnsw_after_local_warm.entries
    );
    printf(
        "\"coldRebuild\":{\"indexedEntities\":%lld,\"embeddedTexts\":%lld,\"cacheHits\":%lld,"
        "\"providerBatchCalls\":%lld,\"providerInputs\":%lld,\"elapsedMicros\":%llu,"
        "\"tempHnswCaches\":%lld,\"tempHnswEntries\":%lld},",
        (long long)report->cold_rebuild.indexed_entities,
        (long long)report->cold_rebuild.embedded_texts,
        (long long)report->cold_rebuild.cache_hits,
        (long long)report->rebuild_batches,
        (long long)report->rebuild_inputs,
        (unsigned long long)report->cold_rebuild.elapsed_micros,
        (long long)report->hnsw_after_cold_rebuild.caches,
        (long long)report->hnsw_after_cold_rebuild.entries
    );
    printf(
        "\"persistentCache\":{\"usedBytes\":%lld,\"entries\":%lld,\"spaces\":%lld},",
        (long long)report->persistent_cache.used_bytes,
        (long long)report->persistent_cache.entries,
        (long long)report->persistent_cache.spaces
    );
    printf(
        "\"newConnectionWarmQuery\":{\"firstRowMicros\":%llu,\"fullConsumeMicros\":%llu,"
        "\"validateCalls\":%lld,\"providerBatchCalls\":%lld,\"providerInputs\":%lld,"
        "\"tempHnswCaches\":%lld,\"tempHnswEntries\":%lld},",
        (unsigned long long)first_row_micros(report->warm_persistent),
        (unsigned long long)full_consume_micros(report->warm_persistent),
        (long long)report->persistent_validate,
        (long long)report->persistent_batches,
        (long long)report->persistent_inputs,
        (long long)report->hnsw_after_persistent_query.caches,
        (long long)report->hnsw_after_persistent_query.entries
    );
    printf(
        "\"warmRebuild\":{\"embeddedTexts\":%lld,\"cacheHits\":%lld,"
        "\"providerBatchCalls\":%lld,\"providerInputs\":%lld,\"elapsedMicros\":%llu,"
        "\"tempHnswCaches\":%lld,\"tempHnswEntries\":%lld},"
        "\"semanticSearchMode\":\"temp-hnsw-v1\",\"tempHnswBuilds\":2}\n",
        (long long)report->warm_rebuild.embedded_texts,
        (long long)report->warm_rebuild.cache_hits,
        (long long)report->warm_rebuild_batches,
        (long long)report->warm_rebuild_inputs,
        (unsigned long long)report->warm_rebuild.elapsed_micros,
        (long long)report->hnsw_after_warm_rebuild.caches,
        (long long)report->hnsw_after_warm_rebuild.entries
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
    library_handle library = open_library(lithograph);
    require(library != NULL, "failed to open Lithograph Native library");
#ifdef _WIN32
    FARPROC execute_symbol = load_symbol(library, "lithograph_v1_execute");
    FARPROC free_symbol = load_symbol(library, "lithograph_v1_free");
    execute_fn execute = NULL;
    free_fn lithograph_free = NULL;
    memcpy(&execute, &execute_symbol, sizeof(execute));
    memcpy(&lithograph_free, &free_symbol, sizeof(lithograph_free));
#else
    execute_fn execute = (execute_fn)load_symbol(library, "lithograph_v1_execute");
    free_fn lithograph_free = (free_fn)load_symbol(library, "lithograph_v1_free");
#endif
    require(execute != NULL && lithograph_free != NULL, "missing Lithograph Native symbols");

    reset_provider(db);
    hnsw_stats hnsw_before_cold = read_hnsw_stats(db);
    query_sample cold = run_native_query(db, execute, lithograph_free);
    hnsw_stats hnsw_after_cold = read_hnsw_stats(db);
    sqlite3_int64 cold_validate = scalar_int64(db, "SELECT synthetic_embedding_validate_calls('synthetic-a')");
    sqlite3_int64 cold_batches = scalar_int64(db, "SELECT synthetic_embedding_embed_calls('synthetic-a')");
    sqlite3_int64 cold_inputs = scalar_int64(db, "SELECT synthetic_embedding_embed_inputs('synthetic-a')");
    sqlite3_int64 cold_persistent = scalar_int64(db, "SELECT count(*) FROM main._lithograph_embedding_cache");
    require(cold_persistent == 0, "ordinary Semantic query persisted Embeddings");
    require(
        hnsw_before_cold.caches == 0
            && hnsw_after_cold.caches == 1
            && hnsw_after_cold.entries == 64,
        "cold Semantic query did not materialize one complete TEMP HNSW"
    );

    reset_provider(db);
    query_sample warm_local = run_native_query(db, execute, lithograph_free);
    hnsw_stats hnsw_after_local_warm = read_hnsw_stats(db);
    sqlite3_int64 local_validate = scalar_int64(db, "SELECT synthetic_embedding_validate_calls('synthetic-a')");
    sqlite3_int64 local_batches = scalar_int64(db, "SELECT synthetic_embedding_embed_calls('synthetic-a')");
    sqlite3_int64 local_inputs = scalar_int64(db, "SELECT synthetic_embedding_embed_inputs('synthetic-a')");
    require(local_batches == 0 && local_inputs == 0, "same-connection query LRU missed");
    require(
        hnsw_after_local_warm.caches == 1 && hnsw_after_local_warm.entries == 64,
        "same-connection warm query did not reuse the TEMP HNSW"
    );

    reset_provider(db);
    rebuild_sample cold_rebuild = run_rebuild(db);
    hnsw_stats hnsw_after_cold_rebuild = read_hnsw_stats(db);
    sqlite3_int64 rebuild_batches = scalar_int64(db, "SELECT synthetic_embedding_embed_calls('synthetic-a')");
    sqlite3_int64 rebuild_inputs = scalar_int64(db, "SELECT synthetic_embedding_embed_inputs('synthetic-a')");
    cache_stats stats = read_cache_stats(db);
    require(cold_rebuild.indexed_entities == 64, "rebuild indexed entity count differs");
    require(cold_rebuild.embedded_texts == 16, "rebuild did not deduplicate exact source text");
    require(cold_rebuild.cache_hits == 0, "cold rebuild unexpectedly reported persistent hits");
    require(rebuild_inputs == 16, "cold rebuild Provider input count differs from unique source count");
    require(stats.entries == 16 && stats.spaces == 1 && stats.used_bytes > 0, "persistent cache stats differ");
    require(
        hnsw_after_cold_rebuild.caches == 1 && hnsw_after_cold_rebuild.entries == 64,
        "cold rebuild changed or lost the existing TEMP HNSW"
    );
    require(sqlite3_close(db) == SQLITE_OK, "failed to close cold performance connection");

    db = open_fixture(database, provider, lithograph);
    hnsw_stats hnsw_before_reopen_rebuild = read_hnsw_stats(db);
    reset_provider(db);
    rebuild_sample warm_rebuild = run_rebuild(db);
    hnsw_stats hnsw_after_warm_rebuild = read_hnsw_stats(db);
    sqlite3_int64 warm_rebuild_batches = scalar_int64(db, "SELECT synthetic_embedding_embed_calls('synthetic-a')");
    sqlite3_int64 warm_rebuild_inputs = scalar_int64(db, "SELECT synthetic_embedding_embed_inputs('synthetic-a')");
    require(warm_rebuild.embedded_texts == 0 && warm_rebuild.cache_hits == 16, "warm rebuild cache accounting differs");
    require(warm_rebuild_batches == 0 && warm_rebuild_inputs == 0, "warm rebuild called Provider");
    require(
        hnsw_before_reopen_rebuild.caches == 0
            && hnsw_after_warm_rebuild.caches == 1
            && hnsw_after_warm_rebuild.entries == 64,
        "persistent-warm rebuild did not materialize one complete TEMP HNSW"
    );

    reset_provider(db);
    query_sample warm_persistent = run_native_query(db, execute, lithograph_free);
    hnsw_stats hnsw_after_persistent_query = read_hnsw_stats(db);
    sqlite3_int64 persistent_validate = scalar_int64(db, "SELECT synthetic_embedding_validate_calls('synthetic-a')");
    sqlite3_int64 persistent_batches = scalar_int64(db, "SELECT synthetic_embedding_embed_calls('synthetic-a')");
    sqlite3_int64 persistent_inputs = scalar_int64(db, "SELECT synthetic_embedding_embed_inputs('synthetic-a')");
    require(persistent_batches == 0 && persistent_inputs == 0, "persistent warm query called Provider");
    require(
        hnsw_after_persistent_query.caches == 1 && hnsw_after_persistent_query.entries == 64,
        "persistent warm query did not reuse rebuild's TEMP HNSW"
    );

    performance_report report = {
        .cold = cold,
        .warm_local = warm_local,
        .warm_persistent = warm_persistent,
        .cold_rebuild = cold_rebuild,
        .warm_rebuild = warm_rebuild,
        .persistent_cache = stats,
        .hnsw_after_cold = hnsw_after_cold,
        .hnsw_after_local_warm = hnsw_after_local_warm,
        .hnsw_after_cold_rebuild = hnsw_after_cold_rebuild,
        .hnsw_after_warm_rebuild = hnsw_after_warm_rebuild,
        .hnsw_after_persistent_query = hnsw_after_persistent_query,
        .cold_validate = cold_validate,
        .cold_batches = cold_batches,
        .cold_inputs = cold_inputs,
        .cold_persistent = cold_persistent,
        .local_validate = local_validate,
        .local_batches = local_batches,
        .local_inputs = local_inputs,
        .rebuild_batches = rebuild_batches,
        .rebuild_inputs = rebuild_inputs,
        .warm_rebuild_batches = warm_rebuild_batches,
        .warm_rebuild_inputs = warm_rebuild_inputs,
        .persistent_validate = persistent_validate,
        .persistent_batches = persistent_batches,
        .persistent_inputs = persistent_inputs,
    };
    print_report(&report);

    require(sqlite3_close(db) == SQLITE_OK, "failed to close warm performance connection");
    close_library(library);
    remove(database);
    return 0;
}
