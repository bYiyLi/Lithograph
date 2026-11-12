// S22.1: reset/rebind the exact same stock SQLite statement, including errors,
// visibility, permissions, staged data and historical schema/index changes.
#include <sqlite3.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static unsigned checks;

static void require(sqlite3 *db, int ok, const char *message) {
    if (!ok) {
        fprintf(stderr, "S22.1 statement reuse: %s (%s)\n", message, sqlite3_errmsg(db));
        exit(1);
    }
    checks++;
}

static void cypher(sqlite3 *db, const char *text) {
    sqlite3_stmt *stmt = NULL;
    require(db, sqlite3_prepare_v2(db, "SELECT lithograph(?1)", -1, &stmt, NULL) == SQLITE_OK, "prepare setup");
    require(db, sqlite3_bind_text(stmt, 1, text, -1, SQLITE_STATIC) == SQLITE_OK, "bind setup");
    require(db, sqlite3_step(stmt) == SQLITE_ROW, "execute setup");
    require(db, sqlite3_step(stmt) == SQLITE_DONE, "drain setup");
    require(db, sqlite3_finalize(stmt) == SQLITE_OK, "finalize setup");
}

static void bind(sqlite3 *db, sqlite3_stmt *stmt, const char *params, const char *options) {
    // reset may return the previous invocation's expected error.
    (void)sqlite3_reset(stmt);
    require(db, sqlite3_clear_bindings(stmt) == SQLITE_OK, "clear old bindings");
    require(db, sqlite3_bind_text(stmt, 1,
        "MATCH (n:Common) WHERE n.v=$x RETURN n.v ORDER BY n.v", -1, SQLITE_STATIC) == SQLITE_OK, "bind query");
    require(db, sqlite3_bind_text(stmt, 2, params, -1, SQLITE_TRANSIENT) == SQLITE_OK, "bind parameters");
    require(db, sqlite3_bind_text(stmt, 3, options, -1, SQLITE_TRANSIENT) == SQLITE_OK, "bind options");
}

static void scalar_query(sqlite3 *db, sqlite3_stmt *stmt, const char *query,
                         const char *params, const char *options, const char *expected) {
    bind(db, stmt, params, options);
    if (query != NULL) {
        require(db, sqlite3_bind_text(stmt, 1, query, -1, SQLITE_STATIC) == SQLITE_OK, "rebind program query");
    }
    require(db, sqlite3_step(stmt) == SQLITE_ROW, "reused scalar result");
    const char *actual = (const char *)sqlite3_column_text(stmt, 0);
    require(db, actual != NULL && strcmp(actual, expected) == 0, "rebound scalar values");
    require(db, sqlite3_step(stmt) == SQLITE_DONE, "drain reused scalar");
}

static void scalar(sqlite3 *db, sqlite3_stmt *stmt, const char *params,
                   const char *options, const char *expected) {
    scalar_query(db, stmt, NULL, params, options, expected);
}

static void rows(sqlite3 *db, sqlite3_stmt *stmt, const char *params,
                 const char *options, unsigned expected, const char *value) {
    bind(db, stmt, params, options);
    unsigned count = 0;
    int rc;
    while ((rc = sqlite3_step(stmt)) == SQLITE_ROW) {
        const char *actual = (const char *)sqlite3_column_text(stmt, 0);
        require(db, actual != NULL && strcmp(actual, value) == 0, "rebound row value");
        count++;
    }
    require(db, rc == SQLITE_DONE && count == expected, "rebound row multiplicity");
}

static void type_error(sqlite3 *db, sqlite3_stmt *stmt, const char *params) {
    bind(db, stmt, params, "{}");
    require(db, sqlite3_step(stmt) == SQLITE_ERROR, "expected Cypher type error");
    require(db, strstr(sqlite3_errmsg(db), "LITHOGRAPH_TYPE_ERROR") != NULL, "stable type error");
}

static char *commit(sqlite3 *db) {
    sqlite3_stmt *stmt = NULL;
    require(db, sqlite3_prepare_v2(db,
        "SELECT json_extract(lithograph('MATCH (n:Common) RETURN 1 LIMIT 1'),'$.summary.commit')",
        -1, &stmt, NULL) == SQLITE_OK, "prepare immutable commit");
    require(db, sqlite3_step(stmt) == SQLITE_ROW, "read immutable commit");
    char *result = sqlite3_mprintf("{\"at\":\"%s\"}", sqlite3_column_text(stmt, 0));
    require(db, result != NULL, "copy immutable commit");
    require(db, sqlite3_finalize(stmt) == SQLITE_OK, "finish commit read");
    return result;
}

int main(int argc, char **argv) {
    if (argc != 2) return 2;
    sqlite3 *db = NULL;
    require(db, sqlite3_open(":memory:", &db) == SQLITE_OK, "open");
    require(db, sqlite3_enable_load_extension(db, 1) == SQLITE_OK, "enable extension");
    require(db, sqlite3_load_extension(db, argv[1], "sqlite3_lithograph_init", NULL) == SQLITE_OK, "load current extension");
    require(db, sqlite3_enable_load_extension(db, 0) == SQLITE_OK, "disable extension loading");
    require(db, sqlite3_exec(db, "SELECT lithograph_init()", NULL, NULL, NULL) == SQLITE_OK, "initialize");
    cypher(db, "CREATE (:Common:Rare {v:1}),(:Common {v:2}),(:Common {v:2}) FINISH");
    cypher(db, "CREATE CONSTRAINT value_type FOR (n:Common) REQUIRE n.v IS :: INTEGER");
    cypher(db, "CREATE RANGE INDEX values_range FOR (n:Common) ON (n.v)");
    sqlite3_stmt *stmt = NULL;
    require(db, sqlite3_prepare_v2(db,
        "SELECT json_extract(lithograph(?1,?2,?3),'$.rows')", -1, &stmt, NULL) == SQLITE_OK,
        "prepare exactly one reusable statement");
    const char *view = "{\"graphView\":{\"requireAllLabels\":[\"Rare\"]}}";
    sqlite3_stmt *row_stmt = NULL;
    require(db, sqlite3_prepare_v2(db,
        "SELECT data FROM lithograph_rows(?1,?2,?3) WHERE event='row'", -1, &row_stmt, NULL) == SQLITE_OK,
        "prepare exactly one reusable rows statement");
    for (unsigned iteration = 0; iteration < 8; iteration++) {
        scalar(db, stmt, "{\"x\":1}", "{}", "[[1]]");
        scalar(db, stmt, "{\"x\":2}", "{}", "[[2],[2]]");
        scalar(db, stmt, "{\"x\":2}", view, "[]");
        scalar(db, stmt, "{\"x\":null}", "{}", "[]");
        type_error(db, stmt, "{\"x\":\"wrong-family\"}");
        scalar(db, stmt, "{\"x\":\"wrong-family\"}",
            "{\"readAccess\":{\"query\":{\"denyProperties\":[\"v\"]}}}", "[]");
        scalar_query(db, stmt, "WITH 1 AS seed MATCH (n:Common) WHERE n.v=$x RETURN n.v ORDER BY n.v",
            "{\"x\":2}", "{}", "[[2],[2]]");
        scalar_query(db, stmt, "WITH 1 AS seed MATCH (n:Common) WHERE n.v=$x SET n.hit=seed RETURN n.v",
            "{\"x\":1}", view, "[[1]]");
        rows(db, row_stmt, "{\"x\":2}", "{}", 2, "[2]");
        rows(db, row_stmt, "{\"x\":2}", view, 0, "");
        type_error(db, row_stmt, "{\"x\":\"wrong-family\"}");
    }
    require(db, sqlite3_exec(db, "SELECT lithograph_tx_begin('{}')", NULL, NULL, NULL) == SQLITE_OK, "begin staged");
    cypher(db, "CREATE (:Common:Rare {v:900}) FINISH");
    scalar(db, stmt, "{\"x\":900}", view, "[[900]]");
    rows(db, row_stmt, "{\"x\":900}", view, 1, "[900]");
    cypher(db, "MATCH (n:Common) WHERE n.v=900 DELETE n");
    scalar(db, stmt, "{\"x\":900}", view, "[]");
    require(db, sqlite3_exec(db, "SELECT lithograph_tx_abort()", NULL, NULL, NULL) == SQLITE_OK, "abort staged");
    scalar(db, stmt, "{\"x\":900}", "{}", "[]");
    char *historical = commit(db);
    cypher(db, "DROP INDEX values_range");
    scalar(db, stmt, "{\"x\":1}", "{}", "[[1]]");
    scalar(db, stmt, "{\"x\":1}", historical, "[[1]]");
    cypher(db, "DROP CONSTRAINT value_type");
    cypher(db, "CREATE (:Common {v:'mixed'}) FINISH");
    cypher(db, "CREATE TEXT INDEX values_range FOR (n:Common) ON (n.v)");
    type_error(db, stmt, "{\"x\":1}");
    scalar(db, stmt, "{\"x\":1}", historical, "[[1]]");
    rows(db, row_stmt, "{\"x\":1}", historical, 1, "[1]");
    sqlite3_free(historical);
    require(db, sqlite3_finalize(row_stmt) == SQLITE_OK, "finalize exact reused rows statement");
    require(db, sqlite3_finalize(stmt) == SQLITE_OK, "finalize exact reused statement");
    require(db, sqlite3_exec(db, "PRAGMA quick_check", NULL, NULL, NULL) == SQLITE_OK, "integrity");
    printf("{\"stage\":\"S22.1\",\"sqlite\":\"%s\",\"statement_checks\":%u}\n", sqlite3_libversion(), checks);
    require(db, sqlite3_close(db) == SQLITE_OK, "close");
    return 0;
}
