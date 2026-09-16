/* v0.1.0 Native ABI example. POSIX (macOS/Linux), isolated in-memory database. */
#include <dlfcn.h>
#include <sqlite3.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "lithograph.h"

typedef int (*execute_fn)(sqlite3 *, const char *, size_t, const char *, size_t,
    const char *, size_t, lithograph_event_callback_v1, void *, char **);
typedef int (*begin_fn)(sqlite3 *, const char *, size_t, char **, char **);
typedef int (*commit_fn)(sqlite3 *, char **, char **);
typedef int (*abort_fn)(sqlite3 *, char **);
typedef void (*free_fn)(void *);

static free_fn release_json;

static void require(int condition, const char *message) {
    if (!condition) {
        fprintf(stderr, "%s\n", message);
        exit(EXIT_FAILURE);
    }
}

static void *symbol(void *library, const char *name) {
    void *result = dlsym(library, name);
    require(result != NULL, name);
    return result;
}

static void sql(sqlite3 *db, const char *text) {
    char *error = NULL;
    int rc = sqlite3_exec(db, text, NULL, NULL, &error);
    if (rc != SQLITE_OK) {
        fprintf(stderr, "SQLite error: %s\n", error == NULL ? "unknown" : error);
    }
    sqlite3_free(error); /* SQLite allocation, not Lithograph allocation. */
    require(rc == SQLITE_OK, "SQL call failed");
}

static int integer(sqlite3 *db, const char *text) {
    sqlite3_stmt *statement = NULL;
    require(sqlite3_prepare_v2(db, text, -1, &statement, NULL) == SQLITE_OK,
            sqlite3_errmsg(db));
    require(sqlite3_step(statement) == SQLITE_ROW, sqlite3_errmsg(db));
    int result = sqlite3_column_int(statement, 0);
    require(sqlite3_finalize(statement) == SQLITE_OK, "Could not finalize SQL cursor");
    return result;
}

static int events(void *data, lithograph_event_kind_v1 kind,
                  const unsigned char *json, size_t length) {
    int *summaries = data;
    if (kind == LITHOGRAPH_EVENT_SUMMARY_V1) {
        *summaries += 1;
    }
    printf("event %d: ", (int)kind);
    fwrite(json, 1, length, stdout); /* Borrowed bytes; no strlen(), no free(). */
    putchar('\n');
    return 0;
}

static int cancel(void *data, lithograph_event_kind_v1 kind,
                  const unsigned char *json, size_t length) {
    (void)data;
    (void)kind;
    (void)json;
    (void)length;
    return 1;
}

static int execute(execute_fn call, sqlite3 *db, const char *query,
                   lithograph_event_callback_v1 callback, void *data) {
    char *error = NULL;
    /* v0.1.0 requires explicit JSON objects; NULL,0 is not defaulted to {}. */
    int rc = call(db, query, strlen(query), "{}", 2, "{}", 2, callback, data, &error);
    if (error != NULL) {
        fprintf(stderr, "Native rc=%d: %s\n", rc, error);
    }
    release_json(error);
    return rc;
}

static void begin(begin_fn call, sqlite3 *db) {
    const char *options = "{\"branch\":\"main\",\"message\":\"Two writes, one Commit\"}";
    char *result = NULL;
    char *error = NULL;
    int rc = call(db, options, strlen(options), &result, &error);
    if (error != NULL) fprintf(stderr, "%s\n", error);
    if (result != NULL) printf("begin: %s\n", result);
    release_json(result);
    release_json(error);
    require(rc == SQLITE_OK, "Native begin failed");
}

int main(int argc, char **argv) {
    require(argc == 2 || argc == 3,
            "usage: native_transaction /absolute/path/lithograph.dylib "
            "[--probe-checkout|--probe-null-json]");
    require(sqlite3_libversion_number() >= 3045000, "SQLite >= 3.45.0 required");
    sqlite3 *db = NULL;
    require(sqlite3_open(":memory:", &db) == SQLITE_OK, "Could not open SQLite");
    char *load_error = NULL;
    require(sqlite3_db_config(db, SQLITE_DBCONFIG_ENABLE_LOAD_EXTENSION, 1, NULL) == SQLITE_OK,
            "Could not enable trusted extension loading");
    int rc = sqlite3_load_extension(db, argv[1], "sqlite3_lithograph_init", &load_error);
    if (load_error != NULL) fprintf(stderr, "%s\n", load_error);
    sqlite3_free(load_error);
    require(rc == SQLITE_OK, "Could not register Lithograph on this connection");
    require(sqlite3_db_config(db, SQLITE_DBCONFIG_ENABLE_LOAD_EXTENSION, 0, NULL) == SQLITE_OK,
            "Could not disable extension loading");
    void *library = dlopen(argv[1], RTLD_NOW | RTLD_LOCAL);
    require(library != NULL, "Could not open Native symbol handle");
    release_json = (free_fn)symbol(library, "lithograph_v1_free");
    execute_fn ordinary = (execute_fn)symbol(library, "lithograph_v1_execute");
    execute_fn tx_execute = (execute_fn)symbol(library, "lithograph_v1_tx_execute");
    begin_fn tx_begin = (begin_fn)symbol(library, "lithograph_v1_tx_begin");
    commit_fn tx_commit = (commit_fn)symbol(library, "lithograph_v1_tx_commit");
    abort_fn tx_abort = (abort_fn)symbol(library, "lithograph_v1_tx_abort");
    sql(db, "SELECT lithograph_init()");
    require(integer(db, "SELECT json_extract(lithograph_version(),'$.abi')") == 1,
            "Native ABI version mismatch");
    require(integer(db, "SELECT json_extract(lithograph_version(),'$.extension')='0.1.0'") == 1,
            "This example targets v0.1.0");
    int summaries = 0;
    if (argc == 3 && strcmp(argv[2], "--probe-null-json") == 0) {
        const char *text = "RETURN 1 AS ready";
        char *error = NULL;
        rc = ordinary(db, text, strlen(text), NULL, 0, "{}", 2, events, &summaries, &error);
        printf("NULL params: rc=%d, error=%s\n", rc, error == NULL ? "none" : error);
        require(rc == SQLITE_ERROR && error != NULL, "Expected v0.1.0 NULL params rejection");
        release_json(error);
        error = NULL;
        rc = ordinary(db, text, strlen(text), "{}", 2, NULL, 0, events, &summaries, &error);
        printf("NULL options: rc=%d, error=%s\n", rc, error == NULL ? "none" : error);
        require(rc == SQLITE_ERROR && error != NULL, "Expected v0.1.0 NULL options rejection");
        release_json(error);
    } else if (argc == 3) {
        require(strcmp(argv[2], "--probe-checkout") == 0, "Unknown argument");
        require(execute(ordinary, db, "CALL lithograph.branch.create('probe')", events,
                        &summaries) == SQLITE_OK, "Could not create probe Branch");
        rc = execute(ordinary, db, "CALL lithograph.branch.checkout('probe')", events, &summaries);
        printf("checkout probe returned SQLite code %d\n", rc);
        require(rc == SQLITE_ERROR, "Expected v0.1.0 checkout rejection");
    } else {
        begin(tx_begin, db);
        require(execute(tx_execute, db, "CREATE (:Person {name:'Alice'}) FINISH", events,
                        &summaries) == SQLITE_OK, "First staged write failed");
        require(execute(tx_execute, db, "CREATE (:Person {name:'Bob'}) FINISH", events,
                        &summaries) == SQLITE_OK, "Second staged write failed");
        char *result = NULL;
        char *error = NULL;
        rc = tx_commit(db, &result, &error);
        if (error != NULL) fprintf(stderr, "%s\n", error);
        if (result != NULL) printf("commit: %s\n", result);
        release_json(result);
        release_json(error);
        require(rc == SQLITE_OK && summaries == 2, "Commit or statement event count failed");
        require(integer(db, "SELECT json_extract(lithograph('CALL lithograph.log() "
            "YIELD commit RETURN count(commit)'), '$.rows[0][0]')") == 2,
            "Expected Root plus exactly one graph Commit");
        begin(tx_begin, db);
        require(execute(tx_execute, db, "CREATE (:Cancelled) FINISH", cancel, NULL)
                == SQLITE_INTERRUPT, "Expected callback cancellation");
        error = NULL;
        rc = tx_abort(db, &error);
        release_json(error);
        require(rc == SQLITE_MISUSE, "Failed execution must already abort its transaction");
        require(integer(db, "SELECT json_extract(lithograph('MATCH (n) RETURN count(n)'),"
                            "'$.rows[0][0]')") == 2, "Aborted transaction leaked a node");
        require(integer(db, "SELECT json_extract(lithograph_integrity_check(),'$.ok')") == 1,
                "Final integrity failed");
        puts("PASS: Native events, two statements/one Commit, cancellation, abort, integrity");
    }
    require(sqlite3_close(db) == SQLITE_OK, "Could not close database");
    require(dlclose(library) == 0, "Could not close Native symbol handle");
    return EXIT_SUCCESS;
}
