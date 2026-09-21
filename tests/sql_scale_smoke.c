#include <sqlite3.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static void fail(const char *message) {
    fprintf(stderr, "%s\n", message);
    exit(1);
}

static void require(int condition, const char *message) {
    if (!condition) {
        fail(message);
    }
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
    require(sqlite3_prepare_v2(db, sql, -1, &statement, NULL) == SQLITE_OK,
            "failed to prepare scalar query");
    require(sqlite3_step(statement) == SQLITE_ROW, "scalar query returned no row");
    int64_t value = sqlite3_column_int64(statement, 0);
    require(sqlite3_finalize(statement) == SQLITE_OK, "scalar query finalize failed");
    return value;
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
    return rc == SQLITE_DONE ? finalize_rc : rc;
}

static int64_t lithograph_row_count(sqlite3 *db, const char *query) {
    sqlite3_stmt *statement = NULL;
    const char *sql =
        "SELECT json_array_length(json_extract(lithograph(?1),'$.rows'))";
    require(sqlite3_prepare_v2(db, sql, -1, &statement, NULL) == SQLITE_OK,
            "failed to prepare Lithograph row-count query");
    require(sqlite3_bind_text(statement, 1, query, -1, SQLITE_TRANSIENT) == SQLITE_OK,
            "failed to bind Lithograph row-count query");
    require(sqlite3_step(statement) == SQLITE_ROW, "Lithograph row-count query returned no row");
    int64_t value = sqlite3_column_int64(statement, 0);
    require(sqlite3_finalize(statement) == SQLITE_OK, "Lithograph row-count finalize failed");
    return value;
}

static void begin_transaction(sqlite3 *db) {
    sqlite3_stmt *statement = NULL;
    const char *sql = "SELECT lithograph_tx_begin(?1)";
    const char *options =
        "{\"author\":\"phase10-sql-scale\","
        "\"message\":\"multi-execution scale transaction\"}";
    require(sqlite3_prepare_v2(db, sql, -1, &statement, NULL) == SQLITE_OK,
            "failed to prepare SQL tx_begin");
    require(sqlite3_bind_text(statement, 1, options, -1, SQLITE_TRANSIENT) == SQLITE_OK,
            "failed to bind SQL tx_begin options");
    require(sqlite3_step(statement) == SQLITE_ROW, "SQL tx_begin returned no row");
    require(sqlite3_finalize(statement) == SQLITE_OK, "SQL tx_begin finalize failed");
}

static void commit_transaction(sqlite3 *db) {
    sqlite3_stmt *statement = NULL;
    require(sqlite3_prepare_v2(db, "SELECT lithograph_tx_commit()", -1, &statement, NULL) == SQLITE_OK,
            "failed to prepare SQL tx_commit");
    require(sqlite3_step(statement) == SQLITE_ROW, "SQL tx_commit returned no row");
    const unsigned char *result = sqlite3_column_text(statement, 0);
    require(
        result != NULL && strstr((const char *)result, "\"commit\":\"commit/") != NULL,
        "SQL tx_commit did not return a Commit"
    );
    require(sqlite3_finalize(statement) == SQLITE_OK, "SQL tx_commit finalize failed");
}

int main(int argc, char **argv) {
    if (argc != 3) {
        fprintf(stderr, "usage: sql-scale-smoke <extension-path> <database-path>\n");
        return 2;
    }
    const char *extension = argv[1];
    const char *database = argv[2];

    sqlite3 *db = NULL;
    require(sqlite3_open(database, &db) == SQLITE_OK, "failed to open scale database");
    load_extension(db, extension);

    const int64_t before = scalar_i64(db, "SELECT count(*) FROM main._lithograph_commits");
    const int64_t before_nodes = scalar_i64(
        db,
        "SELECT json_extract(lithograph("
        "'MATCH (n:SqlScale) RETURN count(n)'"
        "), '$.rows[0][0]')"
    );

    begin_transaction(db);
    require(
        execute_lithograph(db, "CREATE (:SqlScale {step:1}) FINISH") == SQLITE_OK,
        "first SQL scale execution failed"
    );
    require(
        execute_lithograph(db, "CREATE (:SqlScale {step:2}) FINISH") == SQLITE_OK,
        "second SQL scale execution failed"
    );
    require(
        lithograph_row_count(
            db,
            "MATCH (n:SqlScale) RETURN n.step ORDER BY n.step"
        ) == before_nodes + 2,
        "SQL scale staged read did not see both staged writes"
    );
    commit_transaction(db);

    const int64_t after = scalar_i64(db, "SELECT count(*) FROM main._lithograph_commits");
    require(after == before + 1, "multi-execution SQL transaction must publish exactly one Commit");
    const int64_t nodes = scalar_i64(
        db,
        "SELECT json_extract(lithograph("
        "'MATCH (n:SqlScale) RETURN count(n)'"
        "), '$.rows[0][0]')"
    );
    require(nodes == before_nodes + 2, "SQL scale transaction final graph state differs");

    printf(
        "{\"commitsBefore\":%lld,\"commitsAfter\":%lld,"
        "\"sqlScaleNodesBefore\":%lld,\"sqlScaleNodesAfter\":%lld}\n",
        (long long)before,
        (long long)after,
        (long long)before_nodes,
        (long long)nodes
    );
    sqlite3_close(db);
    return 0;
}
