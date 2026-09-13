#include <sqlite3.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#ifdef _WIN32
#include <windows.h>
#else
#include <dlfcn.h>
#include <pthread.h>
#include <unistd.h>
#endif

#include "lithograph.h"

typedef int (*execute_fn)(
    sqlite3 *, const char *, size_t, const char *, size_t, const char *, size_t,
    lithograph_event_callback_v1, void *, char **
);
typedef int (*validate_fn)(sqlite3 *, const char *, size_t, char **);
typedef int (*tx_begin_fn)(sqlite3 *, const char *, size_t, char **, char **);
typedef int (*tx_commit_fn)(sqlite3 *, char **, char **);
typedef int (*tx_abort_fn)(sqlite3 *, char **);
typedef void (*free_fn)(void *);

static int64_t commit_count(sqlite3 *db);

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

static int deny_format2_sidecar_create(
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
    if (
        action == SQLITE_CREATE_TABLE
        && arg1 != NULL
        && strcmp(arg1, "_lithograph_tags") == 0
    ) {
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

static int deny_meta_insert_rollback_to_and_full_rollback(
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
    if (action == SQLITE_TRANSACTION && arg1 != NULL && strcmp(arg1, "ROLLBACK") == 0) {
        return SQLITE_DENY;
    }
    return SQLITE_OK;
}

static int callback_count = 0;
static lithograph_event_kind_v1 callback_kinds[8];
static void *callback_user_data = NULL;
static char callback_row_json[4096];
static char callback_summary_json[4096];

static void reset_callback_capture(void) {
    callback_count = 0;
    callback_user_data = NULL;
    callback_row_json[0] = '\0';
    callback_summary_json[0] = '\0';
}

static int event_callback(
    void *user_data,
    lithograph_event_kind_v1 kind,
    const unsigned char *json,
    size_t json_len
) {
    callback_user_data = user_data;
    if (callback_count < (int)(sizeof(callback_kinds) / sizeof(callback_kinds[0]))) {
        callback_kinds[callback_count] = kind;
    }
    if (kind == LITHOGRAPH_EVENT_ROW_V1 && json_len < sizeof(callback_row_json)) {
        memcpy(callback_row_json, json, json_len);
        callback_row_json[json_len] = '\0';
    }
    if (kind == LITHOGRAPH_EVENT_SUMMARY_V1 && json_len < sizeof(callback_summary_json)) {
        memcpy(callback_summary_json, json, json_len);
        callback_summary_json[json_len] = '\0';
    }
    callback_count += 1;
    return 0;
}

static int cancel_on_row_callback(
    void *user_data,
    lithograph_event_kind_v1 kind,
    const unsigned char *json,
    size_t json_len
) {
    (void)user_data;
    (void)json;
    (void)json_len;
    if (callback_count < (int)(sizeof(callback_kinds) / sizeof(callback_kinds[0]))) {
        callback_kinds[callback_count] = kind;
    }
    callback_count += 1;
    return kind == LITHOGRAPH_EVENT_ROW_V1 ? 1 : 0;
}

static int cancel_on_summary_callback(
    void *user_data,
    lithograph_event_kind_v1 kind,
    const unsigned char *json,
    size_t json_len
) {
    (void)user_data;
    (void)json;
    (void)json_len;
    if (callback_count < (int)(sizeof(callback_kinds) / sizeof(callback_kinds[0]))) {
        callback_kinds[callback_count] = kind;
    }
    callback_count += 1;
    return kind == LITHOGRAPH_EVENT_SUMMARY_V1 ? 1 : 0;
}

#ifndef _WIN32
typedef struct native_serialization_context {
    sqlite3 *db;
    execute_fn tx_execute;
    tx_commit_fn tx_commit;
    pthread_mutex_t mutex;
    pthread_cond_t condition;
    int callback_entered;
    int release_callback;
    int commit_started;
    int commit_finished;
    int execute_rc;
    int commit_rc;
    char *execute_error;
    char *commit_result;
    char *commit_error;
} native_serialization_context;

static int blocking_explicit_tx_callback(
    void *user_data,
    lithograph_event_kind_v1 kind,
    const unsigned char *json,
    size_t json_len
) {
    (void)json;
    (void)json_len;
    native_serialization_context *context = (native_serialization_context *)user_data;
    if (kind != LITHOGRAPH_EVENT_COLUMNS_V1) {
        return 0;
    }
    require(pthread_mutex_lock(&context->mutex) == 0, "failed to lock Native serialization callback mutex");
    if (!context->callback_entered) {
        context->callback_entered = 1;
        require(pthread_cond_broadcast(&context->condition) == 0, "failed to signal Native serialization callback");
        while (!context->release_callback) {
            require(pthread_cond_wait(&context->condition, &context->mutex) == 0, "failed to wait in Native serialization callback");
        }
    }
    require(pthread_mutex_unlock(&context->mutex) == 0, "failed to unlock Native serialization callback mutex");
    return 0;
}

static void *run_blocked_tx_execute(void *user_data) {
    native_serialization_context *context = (native_serialization_context *)user_data;
    const char *query = "CREATE (:NativeSerialized {value:1}) FINISH";
    context->execute_rc = context->tx_execute(
        context->db,
        query,
        strlen(query),
        "{}",
        2,
        "{}",
        2,
        blocking_explicit_tx_callback,
        context,
        &context->execute_error
    );
    return NULL;
}

static void *run_concurrent_tx_commit(void *user_data) {
    native_serialization_context *context = (native_serialization_context *)user_data;
    require(pthread_mutex_lock(&context->mutex) == 0, "failed to lock Native serialization commit mutex");
    context->commit_started = 1;
    require(pthread_cond_broadcast(&context->condition) == 0, "failed to signal Native serialization commit start");
    require(pthread_mutex_unlock(&context->mutex) == 0, "failed to unlock Native serialization commit mutex");

    context->commit_rc = context->tx_commit(
        context->db,
        &context->commit_result,
        &context->commit_error
    );

    require(pthread_mutex_lock(&context->mutex) == 0, "failed to lock Native serialization completion mutex");
    context->commit_finished = 1;
    require(pthread_cond_broadcast(&context->condition) == 0, "failed to signal Native serialization commit completion");
    require(pthread_mutex_unlock(&context->mutex) == 0, "failed to unlock Native serialization completion mutex");
    return NULL;
}
#endif

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

static void check_write_release_fault_cleanup(const char *path) {
    sqlite3 *db = NULL;
    require(sqlite3_open(":memory:", &db) == SQLITE_OK, "failed to open write release-fault database");
    load_extension(db, path);
    char *error = NULL;
    require(
        sqlite3_exec(db, "SELECT lithograph_init();", NULL, NULL, &error) == SQLITE_OK,
        error == NULL ? "write release-fault init failed" : error
    );
    sqlite3_free(error);
    int64_t before = commit_count(db);
    require(
        sqlite3_set_authorizer(db, deny_internal_release, NULL) == SQLITE_OK,
        "failed to set write release-fault authorizer"
    );

    error = NULL;
    int rc = sqlite3_exec(
        db,
        "SELECT lithograph('CREATE (:ReleaseFaultWrite) FINISH');",
        NULL,
        NULL,
        &error
    );
    require(rc != SQLITE_OK, "write release-fault invocation must fail");
    require(
        error != NULL && strstr(error, "LITHOGRAPH_INTERNAL_ERROR") != NULL,
        "write release-fault fallback must surface INTERNAL_ERROR"
    );
    sqlite3_free(error);
    require(sqlite3_get_autocommit(db) != 0, "write release-fault cleanup must restore autocommit");
    require(
        sqlite3_set_authorizer(db, NULL, NULL) == SQLITE_OK,
        "failed to clear write release-fault authorizer"
    );
    require(commit_count(db) == before, "write release-fault cleanup left a Commit behind");

    sqlite3_stmt *statement = NULL;
    require(
        sqlite3_prepare_v2(
            db,
            "SELECT count(*) FROM main._lithograph_labels WHERE name = 'ReleaseFaultWrite'",
            -1,
            &statement,
            NULL
        ) == SQLITE_OK,
        "failed to prepare write release-fault dictionary check"
    );
    require(sqlite3_step(statement) == SQLITE_ROW, "write release-fault dictionary check returned no row");
    require(sqlite3_column_int64(statement, 0) == 0, "write release-fault cleanup left dictionary state");
    sqlite3_finalize(statement);
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

static void check_full_rollback_fault_discards_connection(const char *path) {
    sqlite3 *db = NULL;
    require(sqlite3_open(":memory:", &db) == SQLITE_OK, "failed to open full-rollback-fault database");
    load_extension(db, path);
    require(
        sqlite3_set_authorizer(db, deny_meta_insert_rollback_to_and_full_rollback, NULL) == SQLITE_OK,
        "failed to set full-rollback-fault authorizer"
    );

    char *error = NULL;
    int rc = sqlite3_exec(db, "SELECT lithograph_init();", NULL, NULL, &error);
    require(rc != SQLITE_OK, "full-rollback-fault init must fail");
    require(
        error != NULL && strstr(error, "LITHOGRAPH_INTERNAL_ERROR") != NULL,
        "full-rollback failure must surface INTERNAL_ERROR"
    );
    require(
        strstr(error, "full SQLite rollback also failed") != NULL,
        "full-rollback failure must report that cleanup could not be completed"
    );
    sqlite3_free(error);

    /* The contract requires callers to discard the connection after this path. */
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

static void check_busy_error_contract(const char *path) {
    char database_path[1024];
#ifdef _WIN32
    char temp_directory[MAX_PATH];
    DWORD temp_length = GetTempPathA(MAX_PATH, temp_directory);
    require(temp_length > 0 && temp_length < MAX_PATH, "failed to resolve Windows temporary directory");
    int written = snprintf(
        database_path,
        sizeof(database_path),
        "%slithograph-native-busy-%lu.db",
        temp_directory,
        (unsigned long)GetCurrentProcessId()
    );
#else
    const char *temp_directory = getenv("TMPDIR");
    if (temp_directory == NULL || temp_directory[0] == '\0') {
        temp_directory = "/tmp";
    }
    int written = snprintf(
        database_path,
        sizeof(database_path),
        "%s/lithograph-native-busy-%lu.db",
        temp_directory,
        (unsigned long)getpid()
    );
#endif
    require(written > 0 && (size_t)written < sizeof(database_path), "temporary database path is too long");
    (void)remove(database_path);

    sqlite3 *writer = NULL;
    sqlite3 *reader = NULL;
    require(sqlite3_open(database_path, &writer) == SQLITE_OK, "failed to open BUSY writer database");
    require(sqlite3_open(database_path, &reader) == SQLITE_OK, "failed to open BUSY reader database");
    load_extension(writer, path);
    load_extension(reader, path);
    require(sqlite3_busy_timeout(reader, 0) == SQLITE_OK, "failed to disable BUSY timeout");

    char *error = NULL;
    require(
        sqlite3_exec(writer, "SELECT lithograph_init();", NULL, NULL, &error) == SQLITE_OK,
        error == NULL ? "BUSY fixture init failed" : error
    );
    sqlite3_free(error);

    sqlite3_stmt *statement = NULL;
    require(
        sqlite3_prepare_v2(reader, "SELECT lithograph_version();", -1, &statement, NULL) == SQLITE_OK,
        "failed to prepare BUSY version probe"
    );
    require(
        sqlite3_exec(writer, "BEGIN EXCLUSIVE;", NULL, NULL, &error) == SQLITE_OK,
        error == NULL ? "failed to acquire BUSY fixture lock" : error
    );
    sqlite3_free(error);

    int rc = sqlite3_step(statement);
    require(rc == SQLITE_BUSY || rc == SQLITE_LOCKED, "locked metadata read must preserve SQLite BUSY/LOCKED code");
    require(
        strstr(sqlite3_errmsg(reader), "LITHOGRAPH_BUSY") != NULL,
        "locked metadata read must surface stable LITHOGRAPH_BUSY error text"
    );
    sqlite3_finalize(statement);

    error = NULL;
    require(
        sqlite3_exec(writer, "ROLLBACK;", NULL, NULL, &error) == SQLITE_OK,
        error == NULL ? "failed to release BUSY fixture lock" : error
    );
    sqlite3_free(error);
    sqlite3_close(reader);
    sqlite3_close(writer);
    (void)remove(database_path);
}

static void check_native_read_events(
    execute_fn execute,
    free_fn lithograph_free,
    sqlite3 *db,
    const char *query
) {
    char *error_json = NULL;
    callback_count = 0;
    callback_user_data = NULL;
    int user_data_marker = 42;
    int rc = execute(
        db,
        query,
        strlen(query),
        "{}",
        2,
        "{}",
        2,
        event_callback,
        &user_data_marker,
        &error_json
    );
    require(rc == SQLITE_OK, "read execution must succeed after init");
    require(error_json == NULL, "successful native execution must not allocate error_json");
    require(callback_count == 3, "RETURN 1 must emit COLUMNS, ROW, SUMMARY");
    require(callback_kinds[0] == LITHOGRAPH_EVENT_COLUMNS_V1, "first event must be COLUMNS");
    require(callback_kinds[1] == LITHOGRAPH_EVENT_ROW_V1, "second event must be ROW");
    require(callback_kinds[2] == LITHOGRAPH_EVENT_SUMMARY_V1, "third event must be SUMMARY");
    require(callback_user_data == &user_data_marker, "native execute must forward user_data");

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
        cancel_on_row_callback,
        NULL,
        &error_json
    );
    require(rc == SQLITE_INTERRUPT, "callback cancellation must return SQLITE_INTERRUPT");
    require_error(error_json, "RESOURCE_ERROR");
    require(callback_count == 2, "callback cancellation must stop before SUMMARY");
    require(callback_kinds[0] == LITHOGRAPH_EVENT_COLUMNS_V1, "cancel flow must begin with COLUMNS");
    require(callback_kinds[1] == LITHOGRAPH_EVENT_ROW_V1, "cancel flow must stop on ROW");
    lithograph_free(error_json);
    lithograph_free(NULL);
}

static int64_t commit_count(sqlite3 *db) {
    sqlite3_stmt *statement = NULL;
    require(
        sqlite3_prepare_v2(db, "SELECT count(*) FROM main._lithograph_commits", -1, &statement, NULL) == SQLITE_OK,
        "failed to prepare Commit count"
    );
    require(sqlite3_step(statement) == SQLITE_ROW, "Commit count returned no row");
    int64_t count = sqlite3_column_int64(statement, 0);
    sqlite3_finalize(statement);
    return count;
}

static void check_native_write_cancel(
    execute_fn execute,
    free_fn lithograph_free,
    sqlite3 *db
) {
    const char *query = "CREATE (n:CancelledNative) RETURN n";
    int64_t before = commit_count(db);
    char *error_json = NULL;
    callback_count = 0;
    int rc = execute(
        db,
        query,
        strlen(query),
        "{}",
        2,
        "{}",
        2,
        cancel_on_row_callback,
        NULL,
        &error_json
    );
    require(rc == SQLITE_INTERRUPT, "cancelled native write must return SQLITE_INTERRUPT");
    require_error(error_json, "RESOURCE_ERROR");
    require(callback_count == 2, "cancelled native write must stop after COLUMNS and ROW");
    require(callback_kinds[0] == LITHOGRAPH_EVENT_COLUMNS_V1, "write cancel must begin with COLUMNS");
    require(callback_kinds[1] == LITHOGRAPH_EVENT_ROW_V1, "write cancel must occur on ROW");
    lithograph_free(error_json);
    require(commit_count(db) == before, "cancelled native write must not persist a Commit");

    sqlite3_stmt *statement = NULL;
    require(
        sqlite3_prepare_v2(
            db,
            "SELECT json_extract(lithograph('MATCH (n:CancelledNative) RETURN count(n)'), '$.rows[0][0]')",
            -1,
            &statement,
            NULL
        ) == SQLITE_OK,
        "failed to prepare cancelled-write verification"
    );
    require(sqlite3_step(statement) == SQLITE_ROW, "cancelled-write verification returned no row");
    require(sqlite3_column_int64(statement, 0) == 0, "cancelled native write left graph data behind");
    sqlite3_finalize(statement);

    const char *summary_query = "CREATE (:CancelledAtSummary) FINISH";
    before = commit_count(db);
    error_json = NULL;
    callback_count = 0;
    rc = execute(
        db,
        summary_query,
        strlen(summary_query),
        "{}",
        2,
        "{}",
        2,
        cancel_on_summary_callback,
        NULL,
        &error_json
    );
    require(rc == SQLITE_INTERRUPT, "summary-cancelled native write must return SQLITE_INTERRUPT");
    require_error(error_json, "RESOURCE_ERROR");
    require(callback_count == 2, "summary cancellation must follow COLUMNS with SUMMARY");
    require(callback_kinds[0] == LITHOGRAPH_EVENT_COLUMNS_V1, "summary cancel must begin with COLUMNS");
    require(callback_kinds[1] == LITHOGRAPH_EVENT_SUMMARY_V1, "summary cancel must stop on SUMMARY");
    lithograph_free(error_json);
    require(commit_count(db) == before, "summary-cancelled native write must not persist a Commit");

    require(
        sqlite3_prepare_v2(
            db,
            "SELECT json_extract(lithograph('MATCH (n:CancelledAtSummary) RETURN count(n)'), '$.rows[0][0]')",
            -1,
            &statement,
            NULL
        ) == SQLITE_OK,
        "failed to prepare summary-cancelled write verification"
    );
    require(sqlite3_step(statement) == SQLITE_ROW, "summary-cancelled write verification returned no row");
    require(sqlite3_column_int64(statement, 0) == 0, "summary-cancelled native write left graph data behind");
    sqlite3_finalize(statement);
}

static void check_native_transaction_batches(
    execute_fn execute,
    free_fn lithograph_free,
    sqlite3 *db
) {
    const char *query =
        "UNWIND [1,2,3] AS value "
        "CALL (value) { CREATE (:NativeBatch {value:value}) } "
        "IN TRANSACTIONS OF 2 ROWS FINISH";
    int64_t before = commit_count(db);
    char *error_json = NULL;
    callback_count = 0;
    int rc = execute(
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
    require(rc == SQLITE_OK, "native transaction-owning query must succeed in autocommit mode");
    require(error_json == NULL, "successful transaction batches must not allocate error_json");
    require(commit_count(db) == before + 2, "three rows batched by two must create exactly two Commits");

    sqlite3_stmt *statement = NULL;
    require(
        sqlite3_prepare_v2(
            db,
            "SELECT json_extract(lithograph('MATCH (n:NativeBatch) RETURN count(n)'), '$.rows[0][0]')",
            -1,
            &statement,
            NULL
        ) == SQLITE_OK,
        "failed to prepare native batch verification"
    );
    require(sqlite3_step(statement) == SQLITE_ROW, "native batch verification returned no row");
    require(sqlite3_column_int64(statement, 0) == 3, "native batch query did not persist all rows");
    sqlite3_finalize(statement);

    char *sqlite_error = NULL;
    require(
        sqlite3_exec(db, "BEGIN;", NULL, NULL, &sqlite_error) == SQLITE_OK,
        sqlite_error == NULL ? "failed to start caller transaction" : sqlite_error
    );
    sqlite3_free(sqlite_error);
    error_json = NULL;
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
    require(rc == SQLITE_ERROR, "transaction-owning query inside caller transaction must fail");
    require_error(error_json, "TRANSACTION_BOUNDARY_REQUIRED");
    lithograph_free(error_json);
    sqlite_error = NULL;
    require(
        sqlite3_exec(db, "ROLLBACK;", NULL, NULL, &sqlite_error) == SQLITE_OK,
        sqlite_error == NULL ? "failed to rollback caller transaction" : sqlite_error
    );
    sqlite3_free(sqlite_error);
}

static void check_native_explicit_transaction_basic(
    tx_begin_fn tx_begin,
    execute_fn tx_execute,
    tx_commit_fn tx_commit,
    free_fn lithograph_free,
    sqlite3 *db
) {
    int64_t before = commit_count(db);
    char *result_json = NULL;
    char *error_json = NULL;
    const char *begin_options = "{\"author\":\"tx-author\",\"message\":\"tx-message\"}";
    int rc = tx_begin(db, begin_options, strlen(begin_options), &result_json, &error_json);
    require(rc == SQLITE_OK, "explicit tx_begin must succeed in autocommit mode");
    require(result_json != NULL && strstr(result_json, "baseCommit") != NULL, "tx_begin must return baseCommit");
    require(error_json == NULL, "successful tx_begin must not allocate error_json");
    lithograph_free(result_json);

    const char *create_query = "CREATE (:NativeExplicit {value:1})";
    reset_callback_capture();
    rc = tx_execute(
        db,
        create_query,
        strlen(create_query),
        "{}",
        2,
        "{}",
        2,
        event_callback,
        NULL,
        &error_json
    );
    require(rc == SQLITE_OK, "first explicit tx_execute must succeed");
    require(error_json == NULL, "successful tx_execute must not allocate error_json");
    require(strstr(callback_summary_json, "\"commit\":null") != NULL, "staged statement summary must hide Commit identity");

    const char *update_query = "MATCH (n:NativeExplicit) SET n.value = 2 RETURN n.value";
    reset_callback_capture();
    rc = tx_execute(
        db,
        update_query,
        strlen(update_query),
        "{}",
        2,
        "{}",
        2,
        event_callback,
        NULL,
        &error_json
    );
    require(rc == SQLITE_OK, "second explicit tx_execute must see staged state");
    require(strcmp(callback_row_json, "[2]") == 0, "second tx_execute did not observe first staged write");
    require(strstr(callback_summary_json, "\"commit\":null") != NULL, "second staged summary leaked Commit identity");

    result_json = NULL;
    rc = tx_commit(db, &result_json, &error_json);
    require(rc == SQLITE_OK, "explicit tx_commit must succeed");
    require(error_json == NULL, "successful tx_commit must not allocate error_json");
    require(result_json != NULL && strstr(result_json, "\"commit\":\"commit/") != NULL, "tx_commit must return final Commit");
    lithograph_free(result_json);
    require(commit_count(db) == before + 1, "two tx_execute writes must publish exactly one Commit");

    sqlite3_stmt *statement = NULL;
    require(
        sqlite3_prepare_v2(
            db,
            "SELECT json_extract(lithograph('MATCH (n:NativeExplicit) RETURN n.value'), '$.rows[0][0]')",
            -1,
            &statement,
            NULL
        ) == SQLITE_OK,
        "failed to prepare explicit transaction verification"
    );
    require(sqlite3_step(statement) == SQLITE_ROW, "explicit transaction verification returned no row");
    require(sqlite3_column_int64(statement, 0) == 2, "explicit transaction final state is wrong");
    sqlite3_finalize(statement);

    require(
        sqlite3_prepare_v2(
            db,
            "SELECT c.author, c.message FROM main._lithograph_branches b JOIN main._lithograph_commits c ON c.id=b.commit_id WHERE b.name='main'",
            -1,
            &statement,
            NULL
        ) == SQLITE_OK,
        "failed to prepare explicit transaction metadata verification"
    );
    require(sqlite3_step(statement) == SQLITE_ROW, "explicit transaction metadata returned no row");
    require(strcmp((const char *)sqlite3_column_text(statement, 0), "tx-author") == 0, "tx_commit author did not come from tx_begin");
    require(strcmp((const char *)sqlite3_column_text(statement, 1), "tx-message") == 0, "tx_commit message did not come from tx_begin");
    sqlite3_finalize(statement);

    before = commit_count(db);
    result_json = NULL;
    error_json = NULL;
    require(tx_begin(db, "{}", 2, &result_json, &error_json) == SQLITE_OK, "read-only tx_begin failed");
    lithograph_free(result_json);
    reset_callback_capture();
    require(
        tx_execute(db, "RETURN 7", 8, "{}", 2, "{}", 2, event_callback, NULL, &error_json) == SQLITE_OK,
        "read-only tx_execute failed"
    );
    require(strcmp(callback_row_json, "[7]") == 0, "read-only tx_execute returned wrong row");
    result_json = NULL;
    require(tx_commit(db, &result_json, &error_json) == SQLITE_OK, "read-only tx_commit failed");
    require(result_json != NULL && strstr(result_json, "\"commit\":\"commit/") != NULL, "read-only tx_commit must return base Commit");
    lithograph_free(result_json);
    require(commit_count(db) == before, "read-only explicit transaction must not create a Commit");

    before = commit_count(db);
    result_json = NULL;
    require(tx_begin(db, "{}", 2, &result_json, &error_json) == SQLITE_OK, "net-zero tx_begin failed");
    lithograph_free(result_json);
    const char *net_create = "CREATE (:NativeNetZero) FINISH";
    require(
        tx_execute(db, net_create, strlen(net_create), "{}", 2, "{}", 2, event_callback, NULL, &error_json) == SQLITE_OK,
        "net-zero create failed"
    );
    const char *net_delete = "MATCH (n:NativeNetZero) DETACH DELETE n FINISH";
    require(
        tx_execute(db, net_delete, strlen(net_delete), "{}", 2, "{}", 2, event_callback, NULL, &error_json) == SQLITE_OK,
        "net-zero delete failed"
    );
    result_json = NULL;
    require(tx_commit(db, &result_json, &error_json) == SQLITE_OK, "net-zero tx_commit failed");
    require(result_json != NULL && strstr(result_json, "\"nodesCreated\":0") != NULL, "net-zero counters must describe final canonical delta");
    lithograph_free(result_json);
    require(commit_count(db) == before + 1, "net-zero write intent must create exactly one empty-delta Commit");
}

#ifndef _WIN32
static void check_native_same_connection_serialization(
    tx_begin_fn tx_begin,
    execute_fn tx_execute,
    tx_commit_fn tx_commit,
    free_fn lithograph_free,
    sqlite3 *db
) {
    int64_t before = commit_count(db);
    char *result_json = NULL;
    char *error_json = NULL;
    require(tx_begin(db, "{}", 2, &result_json, &error_json) == SQLITE_OK, "serialized fixture tx_begin failed");
    lithograph_free(result_json);

    native_serialization_context context;
    memset(&context, 0, sizeof(context));
    context.db = db;
    context.tx_execute = tx_execute;
    context.tx_commit = tx_commit;
    require(pthread_mutex_init(&context.mutex, NULL) == 0, "failed to initialize Native serialization mutex");
    require(pthread_cond_init(&context.condition, NULL) == 0, "failed to initialize Native serialization condition");

    pthread_t execute_thread;
    pthread_t commit_thread;
    require(pthread_create(&execute_thread, NULL, run_blocked_tx_execute, &context) == 0, "failed to create Native tx_execute thread");

    require(pthread_mutex_lock(&context.mutex) == 0, "failed to wait for Native tx_execute callback");
    while (!context.callback_entered) {
        require(pthread_cond_wait(&context.condition, &context.mutex) == 0, "failed waiting for Native tx_execute callback");
    }
    require(pthread_mutex_unlock(&context.mutex) == 0, "failed to release Native serialization wait mutex");

    require(pthread_create(&commit_thread, NULL, run_concurrent_tx_commit, &context) == 0, "failed to create Native tx_commit thread");
    require(pthread_mutex_lock(&context.mutex) == 0, "failed to wait for Native tx_commit start");
    while (!context.commit_started) {
        require(pthread_cond_wait(&context.condition, &context.mutex) == 0, "failed waiting for Native tx_commit start");
    }
    require(pthread_mutex_unlock(&context.mutex) == 0, "failed to release Native commit-start mutex");

    usleep(100000);
    require(pthread_mutex_lock(&context.mutex) == 0, "failed to inspect concurrent Native commit state");
    require(!context.commit_finished, "concurrent tx_commit crossed an in-flight same-connection tx_execute boundary");
    context.release_callback = 1;
    require(pthread_cond_broadcast(&context.condition) == 0, "failed to release Native tx_execute callback");
    require(pthread_mutex_unlock(&context.mutex) == 0, "failed to unlock Native callback release mutex");

    require(pthread_join(execute_thread, NULL) == 0, "failed to join Native tx_execute thread");
    require(pthread_join(commit_thread, NULL) == 0, "failed to join Native tx_commit thread");
    require(context.execute_rc == SQLITE_OK, "serialized tx_execute failed");
    require(context.execute_error == NULL, "serialized tx_execute allocated error_json");
    require(context.commit_rc == SQLITE_OK, "serialized tx_commit failed");
    require(context.commit_error == NULL, "serialized tx_commit allocated error_json");
    require(context.commit_result != NULL && strstr(context.commit_result, "\"commit\":\"commit/") != NULL, "serialized tx_commit did not return final Commit");
    lithograph_free(context.commit_result);
    require(commit_count(db) == before + 1, "serialized tx_execute/tx_commit did not publish exactly one Commit");

    sqlite3_stmt *statement = NULL;
    require(
        sqlite3_prepare_v2(
            db,
            "SELECT json_array_length(json_extract(lithograph('MATCH (n:NativeSerialized) RETURN n.value'), '$.rows'))",
            -1,
            &statement,
            NULL
        ) == SQLITE_OK,
        "failed to prepare Native serialization verification"
    );
    require(sqlite3_step(statement) == SQLITE_ROW, "Native serialization verification returned no row");
    require(sqlite3_column_int64(statement, 0) == 1, "serialized commit lost the in-flight tx_execute mutation");
    sqlite3_finalize(statement);

    require(pthread_cond_destroy(&context.condition) == 0, "failed to destroy Native serialization condition");
    require(pthread_mutex_destroy(&context.mutex) == 0, "failed to destroy Native serialization mutex");
}
#endif

static void check_native_explicit_transaction_fail_closed(
    tx_begin_fn tx_begin,
    execute_fn tx_execute,
    tx_commit_fn tx_commit,
    tx_abort_fn tx_abort,
    free_fn lithograph_free,
    sqlite3 *db
) {
    int64_t before = commit_count(db);
    char *result_json = NULL;
    char *error_json = NULL;
    require(tx_begin(db, "{}", 2, &result_json, &error_json) == SQLITE_OK, "abort fixture tx_begin failed");
    lithograph_free(result_json);
    const char *query = "CREATE (:NativeAbort)";
    require(
        tx_execute(db, query, strlen(query), "{}", 2, "{}", 2, event_callback, NULL, &error_json) == SQLITE_OK,
        "abort fixture tx_execute failed"
    );
    require(tx_abort(db, &error_json) == SQLITE_OK, "explicit tx_abort failed");
    require(commit_count(db) == before, "tx_abort left staged Commit history");

    result_json = NULL;
    require(tx_begin(db, "{}", 2, &result_json, &error_json) == SQLITE_OK, "failure fixture tx_begin failed");
    lithograph_free(result_json);
    require(
        tx_execute(db, query, strlen(query), "{}", 2, "{}", 2, event_callback, NULL, &error_json) == SQLITE_OK,
        "failure fixture first tx_execute failed"
    );
    const char *invalid = "MATCH (n) RETURN n,";
    error_json = NULL;
    int rc = tx_execute(
        db,
        invalid,
        strlen(invalid),
        "{}",
        2,
        "{}",
        2,
        event_callback,
        NULL,
        &error_json
    );
    require(rc == SQLITE_ERROR, "invalid tx_execute must fail");
    require_error(error_json, "PARSE_ERROR");
    lithograph_free(error_json);
    require(commit_count(db) == before, "failed tx_execute did not rollback the full explicit transaction");
    result_json = NULL;
    error_json = NULL;
    rc = tx_commit(db, &result_json, &error_json);
    require(rc == SQLITE_MISUSE, "commit after fail-closed abort must be misuse");
    require_error(error_json, "INVALID_ARGUMENT");
    lithograph_free(error_json);

    result_json = NULL;
    error_json = NULL;
    require(tx_begin(db, "{}", 2, &result_json, &error_json) == SQLITE_OK, "callback fixture tx_begin failed");
    lithograph_free(result_json);
    const char *cancelled = "CREATE (n:NativeExplicitCancel) RETURN n";
    reset_callback_capture();
    rc = tx_execute(
        db,
        cancelled,
        strlen(cancelled),
        "{}",
        2,
        "{}",
        2,
        cancel_on_row_callback,
        NULL,
        &error_json
    );
    require(rc == SQLITE_INTERRUPT, "callback-cancelled tx_execute must return SQLITE_INTERRUPT");
    require_error(error_json, "RESOURCE_ERROR");
    lithograph_free(error_json);
    require(commit_count(db) == before, "callback cancellation did not rollback explicit transaction");

    result_json = NULL;
    error_json = NULL;
    require(tx_begin(db, "{}", 2, &result_json, &error_json) == SQLITE_OK, "version-boundary tx_begin failed");
    lithograph_free(result_json);
    require(
        tx_execute(db, query, strlen(query), "{}", 2, "{}", 2, event_callback, NULL, &error_json) == SQLITE_OK,
        "version-boundary staged write failed"
    );
    const char *version_query = "CALL lithograph.branch.list() YIELD name RETURN name";
    error_json = NULL;
    rc = tx_execute(
        db,
        version_query,
        strlen(version_query),
        "{}",
        2,
        "{}",
        2,
        event_callback,
        NULL,
        &error_json
    );
    require(rc == SQLITE_ERROR, "Version Procedure inside explicit transaction must fail");
    require_error(error_json, "TRANSACTION_BOUNDARY_REQUIRED");
    lithograph_free(error_json);
    require(commit_count(db) == before, "forbidden Version Procedure did not fail-close staged writes");
}

static void expect_explicit_tx_execute_failure(
    tx_begin_fn tx_begin,
    execute_fn tx_execute,
    free_fn lithograph_free,
    sqlite3 *db,
    const char *query,
    const char *options,
    const char *category
) {
    char *result_json = NULL;
    char *error_json = NULL;
    require(tx_begin(db, "{}", 2, &result_json, &error_json) == SQLITE_OK, "boundary fixture tx_begin failed");
    lithograph_free(result_json);
    int rc = tx_execute(
        db,
        query,
        strlen(query),
        "{}",
        2,
        options,
        strlen(options),
        event_callback,
        NULL,
        &error_json
    );
    require(rc != SQLITE_OK, "explicit tx boundary query unexpectedly succeeded");
    require_error(error_json, category);
    lithograph_free(error_json);
}

static void check_native_explicit_transaction_owned_work_boundaries(
    tx_begin_fn tx_begin,
    execute_fn tx_execute,
    free_fn lithograph_free,
    sqlite3 *db
) {
    int64_t before = commit_count(db);
    expect_explicit_tx_execute_failure(
        tx_begin,
        tx_execute,
        lithograph_free,
        db,
        "WITH 'file:///definitely-not-read.csv' AS source LOAD CSV FROM source AS row RETURN row",
        "{}",
        "TRANSACTION_BOUNDARY_REQUIRED"
    );
    require(commit_count(db) == before, "LOAD CSV boundary failure left staged history");
    expect_explicit_tx_execute_failure(
        tx_begin,
        tx_execute,
        lithograph_free,
        db,
        "UNWIND [1] AS value CALL (value) { CREATE (:NeverExplicitBatch {value:value}) } IN TRANSACTIONS OF 1 ROWS FINISH",
        "{}",
        "TRANSACTION_BOUNDARY_REQUIRED"
    );
    require(commit_count(db) == before, "IN TRANSACTIONS boundary failure left staged history");
}

static void check_native_explicit_transaction_option_boundaries(
    tx_begin_fn tx_begin,
    execute_fn tx_execute,
    free_fn lithograph_free,
    sqlite3 *db
) {
    const char *options[] = {
        "{\"branch\":\"main\"}",
        "{\"at\":\"branch/main\"}",
        "{\"author\":\"nope\"}",
        "{\"message\":\"nope\"}",
        "{\"mergeSession\":{\"id\":\"merge-session/00000000-0000-4000-8000-000000000000\",\"revision\":1}}"
    };
    for (size_t index = 0; index < sizeof(options) / sizeof(options[0]); index++) {
        expect_explicit_tx_execute_failure(
            tx_begin,
            tx_execute,
            lithograph_free,
            db,
            "RETURN 1",
            options[index],
            "INVALID_ARGUMENT"
        );
    }
}

static void check_native_explicit_transaction_constraint_visibility(
    tx_begin_fn tx_begin,
    execute_fn tx_execute,
    free_fn lithograph_free,
    sqlite3 *db
) {
    int64_t before = commit_count(db);
    char *result_json = NULL;
    char *error_json = NULL;
    require(tx_begin(db, "{}", 2, &result_json, &error_json) == SQLITE_OK, "constraint fixture tx_begin failed");
    lithograph_free(result_json);
    const char *constraint =
        "CREATE CONSTRAINT native_explicit_unique FOR (n:NativeConstraint) REQUIRE n.email IS UNIQUE";
    require(
        tx_execute(db, constraint, strlen(constraint), "{}", 2, "{}", 2, event_callback, NULL, &error_json) == SQLITE_OK,
        "staged Constraint creation failed"
    );
    const char *first = "CREATE (:NativeConstraint {email:'same@example.test'}) FINISH";
    require(
        tx_execute(db, first, strlen(first), "{}", 2, "{}", 2, event_callback, NULL, &error_json) == SQLITE_OK,
        "first constrained staged write failed"
    );
    const char *duplicate = "CREATE (:NativeConstraint {email:'same@example.test'}) FINISH";
    int rc = tx_execute(
        db,
        duplicate,
        strlen(duplicate),
        "{}",
        2,
        "{}",
        2,
        event_callback,
        NULL,
        &error_json
    );
    require(rc != SQLITE_OK, "staged Constraint did not reject a later invalid statement");
    require_error(error_json, "CONSTRAINT_ERROR");
    lithograph_free(error_json);
    require(commit_count(db) == before, "Constraint failure did not rollback the full explicit transaction");
}

static void check_native_explicit_transaction_view_and_clocks(
    tx_begin_fn tx_begin,
    execute_fn tx_execute,
    tx_abort_fn tx_abort,
    free_fn lithograph_free,
    sqlite3 *db
) {
    char *result_json = NULL;
    char *error_json = NULL;
    require(tx_begin(db, "{}", 2, &result_json, &error_json) == SQLITE_OK, "view/clock tx_begin failed");
    lithograph_free(result_json);

    const char *create_query = "CREATE (:NativeView {value:1}) FINISH";
    require(
        tx_execute(db, create_query, strlen(create_query), "{}", 2, "{}", 2, event_callback, NULL, &error_json) == SQLITE_OK,
        "view/clock staged create failed"
    );
    const char *view_query = "MATCH (n) RETURN count(n)";
    const char *view_options = "{\"graphView\":{\"requireAllLabels\":[\"NativeView\"]}}";
    reset_callback_capture();
    require(
        tx_execute(
            db,
            view_query,
            strlen(view_query),
            "{}",
            2,
            view_options,
            strlen(view_options),
            event_callback,
            NULL,
            &error_json
        ) == SQLITE_OK,
        "graphView tx_execute failed"
    );
    require(strcmp(callback_row_json, "[1]") == 0, "graphView did not observe staged Label state");

    const char *index_query = "CREATE RANGE INDEX native_view_value FOR (n:NativeView) ON (n.value)";
    require(
        tx_execute(db, index_query, strlen(index_query), "{}", 2, "{}", 2, event_callback, NULL, &error_json) == SQLITE_OK,
        "staged Index creation failed"
    );
    const char *show_query = "SHOW INDEXES YIELD name WHERE name='native_view_value' RETURN name";
    reset_callback_capture();
    require(
        tx_execute(db, show_query, strlen(show_query), "{}", 2, "{}", 2, event_callback, NULL, &error_json) == SQLITE_OK,
        "staged Schema/Index read failed"
    );
    require(strcmp(callback_row_json, "[\"native_view_value\"]") == 0, "later tx_execute did not observe staged Schema/Index state");

    const char *transaction_clock = "RETURN datetime.transaction()";
    reset_callback_capture();
    require(
        tx_execute(db, transaction_clock, strlen(transaction_clock), "{}", 2, "{}", 2, event_callback, NULL, &error_json) == SQLITE_OK,
        "first transaction clock query failed"
    );
    char transaction_first[4096];
    require(strlen(callback_row_json) < sizeof(transaction_first), "transaction clock row is too large");
    strcpy(transaction_first, callback_row_json);

    const char *statement_clock = "RETURN datetime.statement()";
    reset_callback_capture();
    require(
        tx_execute(db, statement_clock, strlen(statement_clock), "{}", 2, "{}", 2, event_callback, NULL, &error_json) == SQLITE_OK,
        "first statement clock query failed"
    );
    char statement_first[4096];
    require(strlen(callback_row_json) < sizeof(statement_first), "statement clock row is too large");
    strcpy(statement_first, callback_row_json);
    sqlite3_sleep(20);

    reset_callback_capture();
    require(
        tx_execute(db, transaction_clock, strlen(transaction_clock), "{}", 2, "{}", 2, event_callback, NULL, &error_json) == SQLITE_OK,
        "second transaction clock query failed"
    );
    require(strcmp(transaction_first, callback_row_json) == 0, "datetime.transaction() changed across tx_execute calls");
    reset_callback_capture();
    require(
        tx_execute(db, statement_clock, strlen(statement_clock), "{}", 2, "{}", 2, event_callback, NULL, &error_json) == SQLITE_OK,
        "second statement clock query failed"
    );
    require(strcmp(statement_first, callback_row_json) != 0, "datetime.statement() did not advance across tx_execute calls");
    require(tx_abort(db, &error_json) == SQLITE_OK, "view/clock tx_abort failed");
}

static void check_native_explicit_transaction_isolation(
    const char *path,
    tx_begin_fn tx_begin,
    execute_fn tx_execute,
    tx_commit_fn tx_commit,
    free_fn lithograph_free
) {
    char database_path[1024];
#ifdef _WIN32
    char temp_directory[MAX_PATH];
    DWORD temp_length = GetTempPathA(MAX_PATH, temp_directory);
    require(temp_length > 0 && temp_length < MAX_PATH, "failed to resolve Windows temporary directory");
    int written = snprintf(
        database_path,
        sizeof(database_path),
        "%slithograph-native-explicit-%lu.db",
        temp_directory,
        (unsigned long)GetCurrentProcessId()
    );
#else
    const char *temp_directory = getenv("TMPDIR");
    if (temp_directory == NULL || temp_directory[0] == '\0') {
        temp_directory = "/tmp";
    }
    int written = snprintf(
        database_path,
        sizeof(database_path),
        "%s/lithograph-native-explicit-%lu.db",
        temp_directory,
        (unsigned long)getpid()
    );
#endif
    require(written > 0 && (size_t)written < sizeof(database_path), "explicit transaction database path is too long");
    (void)remove(database_path);

    sqlite3 *writer = NULL;
    sqlite3 *reader = NULL;
    require(sqlite3_open(database_path, &writer) == SQLITE_OK, "failed to open explicit writer database");
    require(sqlite3_open(database_path, &reader) == SQLITE_OK, "failed to open explicit reader database");
    load_extension(writer, path);
    load_extension(reader, path);
    char *sqlite_error = NULL;
    require(
        sqlite3_exec(writer, "PRAGMA journal_mode=WAL; SELECT lithograph_init();", NULL, NULL, &sqlite_error) == SQLITE_OK,
        sqlite_error == NULL ? "failed to initialize explicit isolation database" : sqlite_error
    );
    sqlite3_free(sqlite_error);
    int64_t before = commit_count(reader);

    char *result_json = NULL;
    char *error_json = NULL;
    require(tx_begin(writer, "{}", 2, &result_json, &error_json) == SQLITE_OK, "isolation tx_begin failed");
    lithograph_free(result_json);
    const char *create_query = "CREATE (:NativeIsolation) FINISH";
    require(
        tx_execute(
            writer,
            create_query,
            strlen(create_query),
            "{}",
            2,
            "{}",
            2,
            event_callback,
            NULL,
            &error_json
        ) == SQLITE_OK,
        "isolation staged write failed"
    );
    require(commit_count(reader) == before, "other connection observed staged Commit history before tx_commit");
    sqlite3_stmt *statement = NULL;
    require(
        sqlite3_prepare_v2(
            reader,
            "SELECT json_extract(lithograph('MATCH (n:NativeIsolation) RETURN count(n)'), '$.rows[0][0]')",
            -1,
            &statement,
            NULL
        ) == SQLITE_OK,
        "failed to prepare staged visibility read"
    );
    require(sqlite3_step(statement) == SQLITE_ROW, "staged visibility read returned no row");
    require(sqlite3_column_int64(statement, 0) == 0, "other connection observed staged graph state before tx_commit");
    sqlite3_finalize(statement);

    result_json = NULL;
    require(tx_commit(writer, &result_json, &error_json) == SQLITE_OK, "isolation tx_commit failed");
    lithograph_free(result_json);
    require(commit_count(reader) == before + 1, "other connection did not observe exactly one final Commit after tx_commit");
    require(
        sqlite3_prepare_v2(
            reader,
            "SELECT json_extract(lithograph('MATCH (n:NativeIsolation) RETURN count(n)'), '$.rows[0][0]')",
            -1,
            &statement,
            NULL
        ) == SQLITE_OK,
        "failed to prepare committed visibility read"
    );
    require(sqlite3_step(statement) == SQLITE_ROW, "committed visibility read returned no row");
    require(sqlite3_column_int64(statement, 0) == 1, "other connection did not observe committed graph state");
    sqlite3_finalize(statement);

    result_json = NULL;
    require(tx_begin(writer, "{}", 2, &result_json, &error_json) == SQLITE_OK, "teardown tx_begin failed");
    lithograph_free(result_json);
    const char *teardown_query = "CREATE (:NativeTeardown) FINISH";
    require(
        tx_execute(
            writer,
            teardown_query,
            strlen(teardown_query),
            "{}",
            2,
            "{}",
            2,
            event_callback,
            NULL,
            &error_json
        ) == SQLITE_OK,
        "teardown staged write failed"
    );
    require(sqlite3_close(writer) == SQLITE_OK, "closing active explicit transaction connection failed");
    writer = NULL;
    require(commit_count(reader) == before + 1, "connection teardown published staged Commit history");
    require(
        sqlite3_prepare_v2(
            reader,
            "SELECT json_extract(lithograph('MATCH (n:NativeTeardown) RETURN count(n)'), '$.rows[0][0]')",
            -1,
            &statement,
            NULL
        ) == SQLITE_OK,
        "failed to prepare teardown rollback read"
    );
    require(sqlite3_step(statement) == SQLITE_ROW, "teardown rollback read returned no row");
    require(sqlite3_column_int64(statement, 0) == 0, "connection teardown left staged graph state");
    sqlite3_finalize(statement);
    sqlite3_close(reader);
    (void)remove(database_path);
}

static void check_native_explicit_transaction_boundaries(
    execute_fn execute,
    validate_fn validate,
    tx_begin_fn tx_begin,
    execute_fn tx_execute,
    tx_commit_fn tx_commit,
    tx_abort_fn tx_abort,
    free_fn lithograph_free,
    sqlite3 *db
) {
    char *result_json = NULL;
    char *error_json = NULL;
    require(tx_begin(db, "{}", 2, &result_json, &error_json) == SQLITE_OK, "boundary tx_begin failed");
    lithograph_free(result_json);
    result_json = NULL;
    error_json = NULL;
    int rc = tx_begin(db, "{}", 2, &result_json, &error_json);
    require(rc == SQLITE_ERROR, "nested tx_begin must be rejected");
    require_error(error_json, "TRANSACTION_BOUNDARY_REQUIRED");
    lithograph_free(error_json);
    error_json = NULL;
    const char *read_query = "RETURN 1";
    rc = validate(db, read_query, strlen(read_query), &error_json);
    require(rc == SQLITE_ERROR, "ordinary validate must be blocked by explicit transaction");
    require_error(error_json, "TRANSACTION_BOUNDARY_REQUIRED");
    lithograph_free(error_json);
    error_json = NULL;
    rc = execute(
        db,
        read_query,
        strlen(read_query),
        "{}",
        2,
        "{}",
        2,
        event_callback,
        NULL,
        &error_json
    );
    require(rc == SQLITE_ERROR, "ordinary execute must be blocked by explicit transaction");
    require_error(error_json, "TRANSACTION_BOUNDARY_REQUIRED");
    lithograph_free(error_json);
    char *sqlite_error = NULL;
    for (size_t index = 0; index < 4; index++) {
        const char *blocked_sql[] = {
            "SELECT lithograph('RETURN 1');",
            "SELECT lithograph_validate('RETURN 1');",
            "SELECT lithograph_init();",
            "SELECT lithograph_integrity_check();"
        };
        sqlite_error = NULL;
        rc = sqlite3_exec(db, blocked_sql[index], NULL, NULL, &sqlite_error);
        require(rc != SQLITE_OK, "SQL Bridge graph/version surface must be blocked by explicit transaction");
        require(
            sqlite_error != NULL && strstr(sqlite_error, "TRANSACTION_BOUNDARY_REQUIRED") != NULL,
            "blocked SQL Bridge surface must return TRANSACTION_BOUNDARY_REQUIRED"
        );
        sqlite3_free(sqlite_error);
    }
    sqlite_error = NULL;
    rc = sqlite3_exec(
        db,
        "SELECT row FROM lithograph_rows('RETURN 1');",
        NULL,
        NULL,
        &sqlite_error
    );
    require(rc != SQLITE_OK, "lithograph_rows must be blocked by explicit transaction");
    require(
        sqlite_error != NULL && strstr(sqlite_error, "TRANSACTION_BOUNDARY_REQUIRED") != NULL,
        "blocked lithograph_rows must return TRANSACTION_BOUNDARY_REQUIRED"
    );
    sqlite3_free(sqlite_error);
    sqlite_error = NULL;
    require(
        sqlite3_exec(db, "SELECT lithograph_version();", NULL, NULL, &sqlite_error) == SQLITE_OK,
        sqlite_error == NULL ? "lithograph_version must remain available" : sqlite_error
    );
    sqlite3_free(sqlite_error);
    require(tx_abort(db, &error_json) == SQLITE_OK, "boundary tx_abort failed");

    error_json = NULL;
    require(
        tx_begin(db, "{}", 2, NULL, &error_json) == SQLITE_OK,
        "tx_begin must allow NULL result_json out-pointer"
    );
    require(error_json == NULL, "successful tx_begin with NULL result out-pointer allocated error_json");
    require(tx_abort(db, NULL) == SQLITE_OK, "tx_abort must allow NULL error_json out-pointer");

    sqlite_error = NULL;
    require(sqlite3_exec(db, "BEGIN", NULL, NULL, &sqlite_error) == SQLITE_OK, "caller BEGIN failed");
    sqlite3_free(sqlite_error);
    error_json = NULL;
    rc = tx_begin(db, "{}", 2, &result_json, &error_json);
    require(rc == SQLITE_ERROR, "tx_begin inside caller transaction must fail");
    require_error(error_json, "TRANSACTION_BOUNDARY_REQUIRED");
    lithograph_free(error_json);
    sqlite_error = NULL;
    require(sqlite3_exec(db, "ROLLBACK", NULL, NULL, &sqlite_error) == SQLITE_OK, "caller ROLLBACK failed");
    sqlite3_free(sqlite_error);

    int64_t before = commit_count(db);
    const char *stale_options =
        "{\"expectedHead\":\"commit/0000000000000000000000000000000000000000000000000000000000000000\"}";
    result_json = NULL;
    error_json = NULL;
    rc = tx_begin(db, stale_options, strlen(stale_options), &result_json, &error_json);
    require(rc == SQLITE_ERROR, "stale expectedHead must reject tx_begin");
    require_error(error_json, "BRANCH_HEAD_MOVED");
    lithograph_free(error_json);
    require(sqlite3_get_autocommit(db) != 0, "rejected expectedHead must leave no active SQLite transaction");
    require(commit_count(db) == before, "rejected expectedHead must not create staged history");

    error_json = NULL;
    rc = tx_execute(db, read_query, strlen(read_query), "{}", 2, "{}", 2, event_callback, NULL, &error_json);
    require(rc == SQLITE_MISUSE, "tx_execute without active transaction must be misuse");
    require_error(error_json, "INVALID_ARGUMENT");
    lithograph_free(error_json);
    error_json = NULL;
    rc = tx_commit(db, &result_json, &error_json);
    require(rc == SQLITE_MISUSE, "tx_commit without active transaction must be misuse");
    require_error(error_json, "INVALID_ARGUMENT");
    lithograph_free(error_json);
}

static void make_format1_root_fixture(sqlite3 *db) {
    char *error = NULL;
    require(
        sqlite3_exec(db, "SELECT lithograph_init();", NULL, NULL, &error) == SQLITE_OK,
        error == NULL ? "format1 fixture init failed" : error
    );
    sqlite3_free(error);
    const char *legacy_root =
        "23e60794878d0ce1fa5bb1d102507a6589ea7a8cd84d530a8d77302d771b119a";
    char sql[2048];
    int written = snprintf(
        sql,
        sizeof(sql),
        "DROP TABLE main._lithograph_merge_resolutions;"
        "DROP TABLE main._lithograph_merge_sessions;"
        "DROP TABLE main._lithograph_tags;"
        "DROP TABLE main._lithograph_commit_data;"
        "UPDATE main._lithograph_meta SET storage_format=1 WHERE id=1;"
        "UPDATE main._lithograph_commits SET id=X'%s', format_version=1 WHERE parent1 IS NULL;"
        "UPDATE main._lithograph_branches SET commit_id=X'%s' WHERE name='main';",
        legacy_root,
        legacy_root
    );
    require(written > 0 && (size_t)written < sizeof(sql), "format1 fixture SQL overflow");
    error = NULL;
    require(
        sqlite3_exec(db, sql, NULL, NULL, &error) == SQLITE_OK,
        error == NULL ? "failed to transform database into format1 fixture" : error
    );
    sqlite3_free(error);
}

static void check_format1_migration_preserves_history(const char *path) {
    sqlite3 *db = NULL;
    require(sqlite3_open(":memory:", &db) == SQLITE_OK, "failed to open migration database");
    load_extension(db, path);
    make_format1_root_fixture(db);
    require(
        sqlite3_set_authorizer(db, deny_format2_sidecar_create, NULL) == SQLITE_OK,
        "failed to set migration fault authorizer"
    );
    char *error = NULL;
    int rc = sqlite3_exec(db, "SELECT lithograph_init();", NULL, NULL, &error);
    require(rc != SQLITE_OK, "fault-injected format1 migration must fail");
    sqlite3_free(error);
    require(
        sqlite3_set_authorizer(db, NULL, NULL) == SQLITE_OK,
        "failed to clear migration authorizer"
    );

    sqlite3_stmt *statement = NULL;
    require(
        sqlite3_prepare_v2(
            db,
            "SELECT storage_format, (SELECT count(*) FROM sqlite_schema WHERE name='_lithograph_commit_data') FROM _lithograph_meta WHERE id=1",
            -1,
            &statement,
            NULL
        ) == SQLITE_OK,
        "failed to inspect failed migration"
    );
    require(sqlite3_step(statement) == SQLITE_ROW, "failed migration inspection returned no row");
    require(sqlite3_column_int(statement, 0) == 1, "failed migration advanced storage format");
    require(sqlite3_column_int(statement, 1) == 0, "failed migration left a partial sidecar table");
    sqlite3_finalize(statement);

    error = NULL;
    require(
        sqlite3_exec(db, "SELECT lithograph_init();", NULL, NULL, &error) == SQLITE_OK,
        error == NULL ? "format1 migration failed" : error
    );
    sqlite3_free(error);
    statement = NULL;
    require(
        sqlite3_prepare_v2(
            db,
            "SELECT storage_format, lower(hex((SELECT id FROM _lithograph_commits WHERE parent1 IS NULL))), (SELECT count(*) FROM _lithograph_tags), (SELECT count(*) FROM _lithograph_commit_data), json_extract(lithograph_integrity_check(),'$.ok') FROM _lithograph_meta WHERE id=1",
            -1,
            &statement,
            NULL
        ) == SQLITE_OK,
        "failed to prepare migration verification"
    );
    require(sqlite3_step(statement) == SQLITE_ROW, "migration verification returned no row");
    require(sqlite3_column_int(statement, 0) == 2, "format1 migration did not advance to format2");
    require(
        strcmp(
            (const char *)sqlite3_column_text(statement, 1),
            "23e60794878d0ce1fa5bb1d102507a6589ea7a8cd84d530a8d77302d771b119a"
        ) == 0,
        "format1 migration rewrote the legacy Root Commit identity"
    );
    require(sqlite3_column_int(statement, 2) == 0, "migration must start with no Tags");
    require(sqlite3_column_int(statement, 3) == 0, "migration must start with no Commit Data");
    require(sqlite3_column_int(statement, 4) == 1, "migrated database failed integrity check");
    sqlite3_finalize(statement);
    sqlite3_close(db);
}

static void check_registered_native_surfaces(
    const char *path,
    execute_fn execute,
    validate_fn validate,
    tx_begin_fn tx_begin,
    execute_fn tx_execute,
    tx_commit_fn tx_commit,
    tx_abort_fn tx_abort,
    free_fn lithograph_free
) {
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
    require(rc == SQLITE_OK, "frontend validate must succeed after init");
    require(error_json == NULL, "successful native validation must not allocate error_json");

    const char *invalid_query = "MATCH (n) RETURN n,";
    error_json = NULL;
    rc = validate(db, invalid_query, strlen(invalid_query), &error_json);
    require(rc == SQLITE_ERROR, "invalid syntax must return SQLITE_ERROR");
    require_error(error_json, "PARSE_ERROR");
    require(strstr(error_json, "\"line\":1") != NULL, "parse error must expose line 1");
    require(strstr(error_json, "\"column\":20") != NULL, "parse error must expose stable column");
    lithograph_free(error_json);

    check_native_read_events(execute, lithograph_free, db, query);
    check_native_write_cancel(execute, lithograph_free, db);
    check_native_transaction_batches(execute, lithograph_free, db);
    check_native_explicit_transaction_basic(tx_begin, tx_execute, tx_commit, lithograph_free, db);
#ifndef _WIN32
    check_native_same_connection_serialization(
        tx_begin,
        tx_execute,
        tx_commit,
        lithograph_free,
        db
    );
#endif
    check_native_explicit_transaction_fail_closed(
        tx_begin,
        tx_execute,
        tx_commit,
        tx_abort,
        lithograph_free,
        db
    );
    check_native_explicit_transaction_owned_work_boundaries(
        tx_begin,
        tx_execute,
        lithograph_free,
        db
    );
    check_native_explicit_transaction_option_boundaries(
        tx_begin,
        tx_execute,
        lithograph_free,
        db
    );
    check_native_explicit_transaction_constraint_visibility(
        tx_begin,
        tx_execute,
        lithograph_free,
        db
    );
    check_native_explicit_transaction_view_and_clocks(
        tx_begin,
        tx_execute,
        tx_abort,
        lithograph_free,
        db
    );
    check_native_explicit_transaction_boundaries(
        execute,
        validate,
        tx_begin,
        tx_execute,
        tx_commit,
        tx_abort,
        lithograph_free,
        db
    );
    check_native_explicit_transaction_isolation(
        path,
        tx_begin,
        tx_execute,
        tx_commit,
        lithograph_free
    );

    sqlite3_close(db);

    sqlite3 *after_close = NULL;
    require(sqlite3_open(":memory:", &after_close) == SQLITE_OK, "failed to open post-close database");
    error_json = NULL;
    rc = validate(after_close, query, strlen(query), &error_json);
    require(rc == SQLITE_MISUSE, "connection-close cleanup must remove native registration");
    require_error(error_json, "INVALID_ARGUMENT");
    lithograph_free(error_json);
    sqlite3_close(after_close);

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

#ifdef _WIN32
    FARPROC execute_symbol = load_symbol(library, "lithograph_v1_execute");
    FARPROC validate_symbol = load_symbol(library, "lithograph_v1_validate");
    FARPROC tx_begin_symbol = load_symbol(library, "lithograph_v1_tx_begin");
    FARPROC tx_execute_symbol = load_symbol(library, "lithograph_v1_tx_execute");
    FARPROC tx_commit_symbol = load_symbol(library, "lithograph_v1_tx_commit");
    FARPROC tx_abort_symbol = load_symbol(library, "lithograph_v1_tx_abort");
    FARPROC free_symbol = load_symbol(library, "lithograph_v1_free");
    require(execute_symbol != NULL, "missing lithograph_v1_execute export");
    require(validate_symbol != NULL, "missing lithograph_v1_validate export");
    require(tx_begin_symbol != NULL, "missing lithograph_v1_tx_begin export");
    require(tx_execute_symbol != NULL, "missing lithograph_v1_tx_execute export");
    require(tx_commit_symbol != NULL, "missing lithograph_v1_tx_commit export");
    require(tx_abort_symbol != NULL, "missing lithograph_v1_tx_abort export");
    require(free_symbol != NULL, "missing lithograph_v1_free export");
    require(sizeof(execute_fn) == sizeof(execute_symbol), "unexpected Windows function-pointer size");
    require(sizeof(validate_fn) == sizeof(validate_symbol), "unexpected Windows function-pointer size");
    require(sizeof(tx_begin_fn) == sizeof(tx_begin_symbol), "unexpected Windows tx_begin pointer size");
    require(sizeof(tx_commit_fn) == sizeof(tx_commit_symbol), "unexpected Windows tx_commit pointer size");
    require(sizeof(tx_abort_fn) == sizeof(tx_abort_symbol), "unexpected Windows tx_abort pointer size");
    require(sizeof(free_fn) == sizeof(free_symbol), "unexpected Windows function-pointer size");
    execute_fn execute = NULL;
    validate_fn validate = NULL;
    tx_begin_fn tx_begin = NULL;
    execute_fn tx_execute = NULL;
    tx_commit_fn tx_commit = NULL;
    tx_abort_fn tx_abort = NULL;
    free_fn lithograph_free = NULL;
    memcpy(&execute, &execute_symbol, sizeof(execute));
    memcpy(&validate, &validate_symbol, sizeof(validate));
    memcpy(&tx_begin, &tx_begin_symbol, sizeof(tx_begin));
    memcpy(&tx_execute, &tx_execute_symbol, sizeof(tx_execute));
    memcpy(&tx_commit, &tx_commit_symbol, sizeof(tx_commit));
    memcpy(&tx_abort, &tx_abort_symbol, sizeof(tx_abort));
    memcpy(&lithograph_free, &free_symbol, sizeof(lithograph_free));
#else
    execute_fn execute = (execute_fn)load_symbol(library, "lithograph_v1_execute");
    validate_fn validate = (validate_fn)load_symbol(library, "lithograph_v1_validate");
    tx_begin_fn tx_begin = (tx_begin_fn)load_symbol(library, "lithograph_v1_tx_begin");
    execute_fn tx_execute = (execute_fn)load_symbol(library, "lithograph_v1_tx_execute");
    tx_commit_fn tx_commit = (tx_commit_fn)load_symbol(library, "lithograph_v1_tx_commit");
    tx_abort_fn tx_abort = (tx_abort_fn)load_symbol(library, "lithograph_v1_tx_abort");
    free_fn lithograph_free = (free_fn)load_symbol(library, "lithograph_v1_free");
#endif
    require(execute != NULL, "missing lithograph_v1_execute export");
    require(validate != NULL, "missing lithograph_v1_validate export");
    require(tx_begin != NULL, "missing lithograph_v1_tx_begin export");
    require(tx_execute != NULL, "missing lithograph_v1_tx_execute export");
    require(tx_commit != NULL, "missing lithograph_v1_tx_commit export");
    require(tx_abort != NULL, "missing lithograph_v1_tx_abort export");
    require(lithograph_free != NULL, "missing lithograph_v1_free export");

    check_registered_native_surfaces(path, execute, validate, tx_begin, tx_execute, tx_commit, tx_abort, lithograph_free);

    check_init_fault_rollback(path);
    check_release_fault_cleanup(path);
    check_write_release_fault_cleanup(path);
    check_rollback_to_fault_cleanup(path);
    check_full_rollback_fault_discards_connection(path);
    check_release_fault_aborts_outer_transaction(path);
    check_busy_error_contract(path);
    check_format1_migration_preserves_history(path);
    close_library(library);
    return 0;
}
