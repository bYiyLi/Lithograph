#include <sqlite3.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#ifdef _WIN32
#include <windows.h>
#else
#include <dlfcn.h>
#endif

#include "lithograph.h"

typedef int (*tx_begin_fn)(sqlite3 *, const char *, size_t, char **, char **);
typedef int (*tx_execute_fn)(
    sqlite3 *, const char *, size_t, const char *, size_t, const char *, size_t,
    lithograph_event_callback_v1, void *, char **
);
typedef int (*tx_commit_fn)(sqlite3 *, char **, char **);
typedef void (*free_fn)(void *);

#ifdef _WIN32
typedef HMODULE library_handle;
static library_handle open_library(const char *path) { return LoadLibraryA(path); }
static FARPROC load_symbol(library_handle library, const char *name) {
    return GetProcAddress(library, name);
}
static void close_library(library_handle library) { FreeLibrary(library); }
#else
typedef void *library_handle;
static library_handle open_library(const char *path) { return dlopen(path, RTLD_NOW | RTLD_LOCAL); }
static void *load_symbol(library_handle library, const char *name) { return dlsym(library, name); }
static void close_library(library_handle library) { dlclose(library); }
#endif

static void fail(const char *message) {
    fprintf(stderr, "%s\n", message);
    exit(1);
}

static void require(int condition, const char *message) {
    if (!condition) fail(message);
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

static int64_t scalar_i64(sqlite3 *db, const char *sql) {
    sqlite3_stmt *statement = NULL;
    require(sqlite3_prepare_v2(db, sql, -1, &statement, NULL) == SQLITE_OK, "failed to prepare scalar query");
    require(sqlite3_step(statement) == SQLITE_ROW, "scalar query returned no row");
    int64_t value = sqlite3_column_int64(statement, 0);
    sqlite3_finalize(statement);
    return value;
}

static int accept_events(
    void *user_data,
    lithograph_event_kind_v1 kind,
    const unsigned char *json,
    size_t json_len
) {
    (void)user_data;
    (void)kind;
    (void)json;
    (void)json_len;
    return SQLITE_OK;
}

int main(int argc, char **argv) {
    if (argc != 3) {
        fprintf(stderr, "usage: native-scale-smoke <extension-path> <database-path>\n");
        return 2;
    }
    const char *extension = argv[1];
    const char *database = argv[2];

    library_handle library = open_library(extension);
    require(library != NULL, "failed to load Lithograph library");

#ifdef _WIN32
    FARPROC begin_symbol = load_symbol(library, "lithograph_v1_tx_begin");
    FARPROC execute_symbol = load_symbol(library, "lithograph_v1_tx_execute");
    FARPROC commit_symbol = load_symbol(library, "lithograph_v1_tx_commit");
    FARPROC free_symbol = load_symbol(library, "lithograph_v1_free");
    tx_begin_fn tx_begin = NULL;
    tx_execute_fn tx_execute = NULL;
    tx_commit_fn tx_commit = NULL;
    free_fn lithograph_free = NULL;
    memcpy(&tx_begin, &begin_symbol, sizeof(tx_begin));
    memcpy(&tx_execute, &execute_symbol, sizeof(tx_execute));
    memcpy(&tx_commit, &commit_symbol, sizeof(tx_commit));
    memcpy(&lithograph_free, &free_symbol, sizeof(lithograph_free));
#else
    tx_begin_fn tx_begin = (tx_begin_fn)load_symbol(library, "lithograph_v1_tx_begin");
    tx_execute_fn tx_execute = (tx_execute_fn)load_symbol(library, "lithograph_v1_tx_execute");
    tx_commit_fn tx_commit = (tx_commit_fn)load_symbol(library, "lithograph_v1_tx_commit");
    free_fn lithograph_free = (free_fn)load_symbol(library, "lithograph_v1_free");
#endif
    require(tx_begin != NULL, "missing lithograph_v1_tx_begin export");
    require(tx_execute != NULL, "missing lithograph_v1_tx_execute export");
    require(tx_commit != NULL, "missing lithograph_v1_tx_commit export");
    require(lithograph_free != NULL, "missing lithograph_v1_free export");

    sqlite3 *db = NULL;
    require(sqlite3_open(database, &db) == SQLITE_OK, "failed to open scale database");
    load_extension(db, extension);
    const int64_t before = scalar_i64(db, "SELECT count(*) FROM main._lithograph_commits");
    const int64_t before_nodes = scalar_i64(
        db,
        "SELECT json_extract(lithograph('MATCH (n:NativeScale) RETURN count(n)'), '$.rows[0][0]')"
    );
    fprintf(stderr, "native-scale: begin\n");
    fflush(stderr);

    char *result_json = NULL;
    char *error_json = NULL;
    const char *options = "{\"author\":\"phase10-native-scale\",\"message\":\"multi-execution scale transaction\"}";
    require(
        tx_begin(db, options, strlen(options), &result_json, &error_json) == SQLITE_OK,
        "native scale tx_begin failed"
    );
    require(error_json == NULL, "native scale tx_begin returned error_json");
    lithograph_free(result_json);

    const char *first = "CREATE (:NativeScale {step:1}) FINISH";
    fprintf(stderr, "native-scale: execute-1\n");
    fflush(stderr);
    require(
        tx_execute(
            db, first, strlen(first), "{}", 2, "{}", 2,
            accept_events, NULL, &error_json
        ) == SQLITE_OK,
        "first native scale tx_execute failed"
    );
    require(error_json == NULL, "first native scale tx_execute returned error_json");

    const char *second = "CREATE (:NativeScale {step:2}) FINISH";
    fprintf(stderr, "native-scale: execute-2\n");
    fflush(stderr);
    require(
        tx_execute(
            db, second, strlen(second), "{}", 2, "{}", 2,
            accept_events, NULL, &error_json
        ) == SQLITE_OK,
        "second native scale tx_execute failed"
    );
    require(error_json == NULL, "second native scale tx_execute returned error_json");

    const char *read_staged = "MATCH (n:NativeScale) RETURN n.step ORDER BY n.step";
    fprintf(stderr, "native-scale: staged-read\n");
    fflush(stderr);
    require(
        tx_execute(
            db, read_staged, strlen(read_staged), "{}", 2, "{}", 2,
            accept_events, NULL, &error_json
        ) == SQLITE_OK,
        "native scale staged read failed"
    );
    require(error_json == NULL, "native scale staged read returned error_json");

    result_json = NULL;
    fprintf(stderr, "native-scale: commit\n");
    fflush(stderr);
    require(tx_commit(db, &result_json, &error_json) == SQLITE_OK, "native scale tx_commit failed");
    require(error_json == NULL, "native scale tx_commit returned error_json");
    require(result_json != NULL && strstr(result_json, "\"commit\":\"commit/") != NULL,
            "native scale tx_commit did not return a Commit");
    lithograph_free(result_json);
    fprintf(stderr, "native-scale: verify\n");
    fflush(stderr);

    const int64_t after = scalar_i64(db, "SELECT count(*) FROM main._lithograph_commits");
    require(after == before + 1, "multi-execution native transaction must publish exactly one Commit");
    const int64_t nodes = scalar_i64(
        db,
        "SELECT json_extract(lithograph('MATCH (n:NativeScale) RETURN count(n)'), '$.rows[0][0]')"
    );
    require(nodes == before_nodes + 2, "native scale transaction final graph state differs");

    printf("{\"commitsBefore\":%lld,\"commitsAfter\":%lld,\"nativeScaleNodesBefore\":%lld,\"nativeScaleNodesAfter\":%lld}\n",
           (long long)before, (long long)after, (long long)before_nodes, (long long)nodes);
    sqlite3_close(db);
    close_library(library);
    return 0;
}
