#include <sqlite3.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

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
typedef int (*validate_fn)(sqlite3 *, const char *, size_t, char **);
typedef void (*free_fn)(void *);

#ifdef _WIN32
typedef HMODULE library_handle;

static library_handle open_library(const char *path) {
    return LoadLibraryA(path);
}

static FARPROC load_symbol(library_handle library, const char *name) {
    return GetProcAddress(library, name);
}

static void close_library(library_handle library) {
    FreeLibrary(library);
}
#else
typedef void *library_handle;

static library_handle open_library(const char *path) {
    return dlopen(path, RTLD_NOW | RTLD_LOCAL);
}

static void *load_symbol(library_handle library, const char *name) {
    return dlsym(library, name);
}

static void close_library(library_handle library) {
    dlclose(library);
}
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

static void require_error(char *error_json, const char *category) {
    require(error_json != NULL, "native failure must allocate error_json");
    require(strstr(error_json, category) != NULL, "native error category mismatch");
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

static int deny_meta_insert(
    void *data,
    int action,
    const char *arg1,
    const char *arg2,
    const char *database,
    const char *trigger
) {
    (void)data;
    (void)arg2;
    (void)database;
    (void)trigger;
    if (action == SQLITE_INSERT && arg1 != NULL && strcmp(arg1, "_lithograph_meta") == 0) {
        return SQLITE_DENY;
    }
    return SQLITE_OK;
}

static int deny_internal_release(
    void *data,
    int action,
    const char *arg1,
    const char *arg2,
    const char *database,
    const char *trigger
) {
    (void)data;
    (void)database;
    (void)trigger;
    if (
        action == SQLITE_SAVEPOINT
        && arg1 != NULL
        && arg2 != NULL
        && strcmp(arg1, "RELEASE") == 0
        && strncmp(arg2, "lithograph_invocation_", 22) == 0
    ) {
        return SQLITE_DENY;
    }
    return SQLITE_OK;
}

static int deny_meta_insert_and_rollback_to(
    void *data,
    int action,
    const char *arg1,
    const char *arg2,
    const char *database,
    const char *trigger
) {
    (void)data;
    (void)database;
    (void)trigger;
    if (action == SQLITE_INSERT && arg1 != NULL && strcmp(arg1, "_lithograph_meta") == 0) {
        return SQLITE_DENY;
    }
    if (
        action == SQLITE_SAVEPOINT
        && arg1 != NULL
        && arg2 != NULL
        && strcmp(arg1, "ROLLBACK") == 0
        && strncmp(arg2, "lithograph_invocation_", 22) == 0
    ) {
        return SQLITE_DENY;
    }
    return SQLITE_OK;
}

static int callback_count = 0;

static int event_callback(
    void *user_data,
    lithograph_event_kind_v1 kind,
    const unsigned char *json,
    size_t json_len
) {
    (void)user_data;
    (void)kind;
    (void)json;
    (void)json_len;
    callback_count += 1;
    return 0;
}

static void check_init_fault_rollback(const char *path) {
    sqlite3 *db = NULL;
    require(sqlite3_open(":memory:", &db) == SQLITE_OK, "failed to open fault-injection database");
    load_extension(db, path);
    require(sqlite3_set_authorizer(db, deny_meta_insert, NULL) == SQLITE_OK, "failed to set authorizer");

    char *error = NULL;
    int rc = sqlite3_exec(db, "SELECT lithograph_init();", NULL, NULL, &error);
    require(rc != SQLITE_OK, "fault-injected init must fail");
    sqlite3_free(error);
    require(sqlite3_set_authorizer(db, NULL, NULL) == SQLITE_OK, "failed to clear authorizer");

    sqlite3_stmt *statement = NULL;
    require(
        sqlite3_prepare_v2(
            db,
            "SELECT count(*) FROM sqlite_schema WHERE name = '_lithograph_meta'",
            -1,
            &statement,
            NULL
        ) == SQLITE_OK,
        "failed to inspect rollback state"
    );
    require(sqlite3_step(statement) == SQLITE_ROW, "rollback inspection returned no row");
    require(sqlite3_column_int(statement, 0) == 0, "failed init left a half-created metadata table");
    sqlite3_finalize(statement);
    sqlite3_close(db);
}

static void check_release_fault_cleanup(const char *path) {
    sqlite3 *db = NULL;
    require(sqlite3_open(":memory:", &db) == SQLITE_OK, "failed to open release-fault database");
    load_extension(db, path);
    require(
        sqlite3_set_authorizer(db, deny_internal_release, NULL) == SQLITE_OK,
        "failed to set release-fault authorizer"
    );

    char *error = NULL;
    int rc = sqlite3_exec(db, "SELECT lithograph_init();", NULL, NULL, &error);
    require(rc != SQLITE_OK, "release-fault init must fail");
    require(
        error != NULL && strstr(error, "LITHOGRAPH_INTERNAL_ERROR") != NULL,
        "release-fault fallback must surface INTERNAL_ERROR"
    );
    sqlite3_free(error);
    require(
        sqlite3_get_autocommit(db) != 0,
        "failed internal RELEASE must restore autocommit state"
    );
    require(sqlite3_set_authorizer(db, NULL, NULL) == SQLITE_OK, "failed to clear release-fault authorizer");

    sqlite3_stmt *statement = NULL;
    require(
        sqlite3_prepare_v2(
            db,
            "SELECT count(*) FROM main.sqlite_schema WHERE name = '_lithograph_meta'",
            -1,
            &statement,
            NULL
        ) == SQLITE_OK,
        "failed to inspect release-fault rollback state"
    );
    require(sqlite3_step(statement) == SQLITE_ROW, "release-fault inspection returned no row");
    require(sqlite3_column_int(statement, 0) == 0, "release-fault init left metadata behind");
    sqlite3_finalize(statement);

    error = NULL;
    require(
        sqlite3_exec(db, "SELECT lithograph_init();", NULL, NULL, &error) == SQLITE_OK,
        error == NULL ? "init after release-fault cleanup failed" : error
    );
    sqlite3_free(error);
    sqlite3_close(db);
}

static void check_rollback_to_fault_cleanup(const char *path) {
    sqlite3 *db = NULL;
    require(sqlite3_open(":memory:", &db) == SQLITE_OK, "failed to open rollback-fault database");
    load_extension(db, path);
    require(
        sqlite3_set_authorizer(db, deny_meta_insert_and_rollback_to, NULL) == SQLITE_OK,
        "failed to set rollback-fault authorizer"
    );

    char *error = NULL;
    int rc = sqlite3_exec(db, "SELECT lithograph_init();", NULL, NULL, &error);
    require(rc != SQLITE_OK, "rollback-fault init must fail");
    require(
        error != NULL && strstr(error, "LITHOGRAPH_INTERNAL_ERROR") != NULL,
        "rollback-fault fallback must surface INTERNAL_ERROR"
    );
    sqlite3_free(error);
    require(
        sqlite3_get_autocommit(db) != 0,
        "failed ROLLBACK TO must fall back to a full rollback"
    );
    require(
        sqlite3_set_authorizer(db, NULL, NULL) == SQLITE_OK,
        "failed to clear rollback-fault authorizer"
    );

    sqlite3_stmt *statement = NULL;
    require(
        sqlite3_prepare_v2(
            db,
            "SELECT count(*) FROM main.sqlite_schema WHERE name = '_lithograph_meta'",
            -1,
            &statement,
            NULL
        ) == SQLITE_OK,
        "failed to inspect rollback-fault state"
    );
    require(sqlite3_step(statement) == SQLITE_ROW, "rollback-fault inspection returned no row");
    require(sqlite3_column_int(statement, 0) == 0, "rollback-fault init left metadata behind");
    sqlite3_finalize(statement);
    sqlite3_close(db);
}

static void check_release_fault_aborts_outer_transaction(const char *path) {
    sqlite3 *db = NULL;
    require(sqlite3_open(":memory:", &db) == SQLITE_OK, "failed to open outer-release-fault database");
    load_extension(db, path);

    char *error = NULL;
    require(
        sqlite3_exec(db, "BEGIN; CREATE TABLE caller_state(v INTEGER);", NULL, NULL, &error) == SQLITE_OK,
        error == NULL ? "failed to establish caller-owned outer transaction" : error
    );
    sqlite3_free(error);
    require(sqlite3_get_autocommit(db) == 0, "caller-owned outer transaction must be active");

    require(
        sqlite3_set_authorizer(db, deny_internal_release, NULL) == SQLITE_OK,
        "failed to set outer release-fault authorizer"
    );
    error = NULL;
    int rc = sqlite3_exec(db, "SELECT lithograph_init();", NULL, NULL, &error);
    require(rc != SQLITE_OK, "outer release-fault init must fail");
    require(
        error != NULL && strstr(error, "LITHOGRAPH_INTERNAL_ERROR") != NULL,
        "outer release-fault fallback must surface INTERNAL_ERROR"
    );
    sqlite3_free(error);
    require(
        sqlite3_get_autocommit(db) != 0,
        "unrecoverable invocation-local cleanup must fail closed with a full rollback"
    );
    require(
        sqlite3_set_authorizer(db, NULL, NULL) == SQLITE_OK,
        "failed to clear outer release-fault authorizer"
    );

    sqlite3_stmt *statement = NULL;
    require(
        sqlite3_prepare_v2(
            db,
            "SELECT count(*) FROM main.sqlite_schema WHERE name IN ('caller_state', '_lithograph_meta')",
            -1,
            &statement,
            NULL
        ) == SQLITE_OK,
        "failed to inspect outer release-fault recovery state"
    );
    require(sqlite3_step(statement) == SQLITE_ROW, "outer release-fault inspection returned no row");
    require(
        sqlite3_column_int(statement, 0) == 0,
        "full rollback fallback must remove both caller and Lithograph transactional changes"
    );
    sqlite3_finalize(statement);
    sqlite3_close(db);
}

int main(int argc, char **argv) {
    if (argc != 2) {
        fprintf(stderr, "usage: native-abi-smoke <extension-path>\n");
        return 2;
    }
    const char *path = argv[1];

    library_handle library = open_library(path);
    if (library == NULL) {
#ifdef _WIN32
        fprintf(stderr, "LoadLibrary failed with error %lu\n", (unsigned long)GetLastError());
#else
        fprintf(stderr, "dlopen failed: %s\n", dlerror());
#endif
        return 1;
    }

    execute_fn execute = (execute_fn)load_symbol(library, "lithograph_v1_execute");
    validate_fn validate = (validate_fn)load_symbol(library, "lithograph_v1_validate");
    free_fn lithograph_free = (free_fn)load_symbol(library, "lithograph_v1_free");
    require(execute != NULL, "missing lithograph_v1_execute export");
    require(validate != NULL, "missing lithograph_v1_validate export");
    require(lithograph_free != NULL, "missing lithograph_v1_free export");

    const char *query = "RETURN 1";
    char *error_json = NULL;
    int rc = validate(NULL, query, strlen(query), &error_json);
    require(rc == SQLITE_MISUSE, "NULL db must return SQLITE_MISUSE");
    require_error(error_json, "INVALID_ARGUMENT");
    lithograph_free(error_json);

    sqlite3 *unloaded = NULL;
    require(sqlite3_open(":memory:", &unloaded) == SQLITE_OK, "failed to open unloaded native database");
    error_json = NULL;
    rc = validate(unloaded, query, strlen(query), &error_json);
    require(rc == SQLITE_MISUSE, "native call on an unloaded connection must return SQLITE_MISUSE");
    require_error(error_json, "INVALID_ARGUMENT");
    lithograph_free(error_json);
    sqlite3_close(unloaded);

    sqlite3 *db = NULL;
    require(sqlite3_open(":memory:", &db) == SQLITE_OK, "failed to open native smoke database");
    load_extension(db, path);
    load_extension(db, path);

    sqlite3 *other = NULL;
    require(sqlite3_open(":memory:", &other) == SQLITE_OK, "failed to open second native database");
    error_json = NULL;
    rc = validate(other, query, strlen(query), &error_json);
    require(rc == SQLITE_MISUSE, "a different connection must load Lithograph independently");
    require_error(error_json, "INVALID_ARGUMENT");
    lithograph_free(error_json);
    sqlite3_close(other);

    error_json = NULL;
    rc = validate(db, query, strlen(query), &error_json);
    require(rc == SQLITE_ERROR, "validate before init must return SQLITE_ERROR");
    require_error(error_json, "NOT_INITIALIZED");
    lithograph_free(error_json);

    char *sqlite_error = NULL;
    require(
        sqlite3_exec(db, "SELECT lithograph_init();", NULL, NULL, &sqlite_error) == SQLITE_OK,
        sqlite_error == NULL ? "lithograph_init failed" : sqlite_error
    );
    sqlite3_free(sqlite_error);

    error_json = NULL;
    rc = validate(db, query, strlen(query), &error_json);
    require(rc == SQLITE_ERROR, "Phase 01 validate must return SQLITE_ERROR until frontend exists");
    require_error(error_json, "SEMANTIC_ERROR");
    lithograph_free(error_json);

    error_json = NULL;
    callback_count = 0;
    rc = execute(
        db,
        query,
        strlen(query),
        "{}",
        2,
        "{}",
        2,
        event_callback,
        NULL,
        &error_json
    );
    require(rc == SQLITE_ERROR, "Phase 01 execute must return SQLITE_ERROR until engine exists");
    require_error(error_json, "SEMANTIC_ERROR");
    require(callback_count == 0, "unavailable execution must not emit partial events");
    lithograph_free(error_json);
    lithograph_free(NULL);

    sqlite3_close(db);

    sqlite3 *after_close = NULL;
    require(sqlite3_open(":memory:", &after_close) == SQLITE_OK, "failed to open post-close database");
    error_json = NULL;
    rc = validate(after_close, query, strlen(query), &error_json);
    require(rc == SQLITE_MISUSE, "connection-close cleanup must remove native registration");
    require_error(error_json, "INVALID_ARGUMENT");
    lithograph_free(error_json);
    sqlite3_close(after_close);

    check_init_fault_rollback(path);
    check_release_fault_cleanup(path);
    check_rollback_to_fault_cleanup(path);
    check_release_fault_aborts_outer_transaction(path);
    close_library(library);
    return 0;
}
