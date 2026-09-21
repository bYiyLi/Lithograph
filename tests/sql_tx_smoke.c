#include <sqlite3.h>

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static void fail(sqlite3 *db, const char *message) {
    fprintf(stderr, "%s: %s\n", message, db == NULL ? "no database" : sqlite3_errmsg(db));
    exit(1);
}

static void require(int condition, const char *message) {
    if (!condition) {
        fprintf(stderr, "%s\n", message);
        exit(1);
    }
}

static void execute(sqlite3 *db, const char *sql) {
    char *error = NULL;
    int code = sqlite3_exec(db, sql, NULL, NULL, &error);
    if (code != SQLITE_OK) {
        fprintf(stderr, "SQL failed (%d): %s\n%s\n", code, error == NULL ? "unknown" : error, sql);
        sqlite3_free(error);
        exit(1);
    }
}

static int execute_error(sqlite3 *db, const char *sql, const char *expected) {
    char *error = NULL;
    int code = sqlite3_exec(db, sql, NULL, NULL, &error);
    require(code != SQLITE_OK, "SQL unexpectedly succeeded");
    if (error == NULL || strstr(error, expected) == NULL) {
        fprintf(stderr, "expected error containing %s, got %s\nSQL: %s\n", expected, error == NULL ? "NULL" : error, sql);
        sqlite3_free(error);
        exit(1);
    }
    sqlite3_free(error);
    return code;
}

static sqlite3_int64 scalar_int(sqlite3 *db, const char *sql) {
    sqlite3_stmt *statement = NULL;
    if (sqlite3_prepare_v2(db, sql, -1, &statement, NULL) != SQLITE_OK) {
        fail(db, "failed to prepare integer scalar");
    }
    if (sqlite3_step(statement) != SQLITE_ROW) {
        sqlite3_finalize(statement);
        fail(db, "integer scalar did not return a row");
    }
    sqlite3_int64 value = sqlite3_column_int64(statement, 0);
    if (sqlite3_step(statement) != SQLITE_DONE) {
        sqlite3_finalize(statement);
        fail(db, "integer scalar returned more than one row");
    }
    if (sqlite3_finalize(statement) != SQLITE_OK) {
        fail(db, "failed to finalize integer scalar");
    }
    return value;
}

static void scalar_text(sqlite3 *db, const char *sql, char *output, size_t capacity) {
    sqlite3_stmt *statement = NULL;
    if (sqlite3_prepare_v2(db, sql, -1, &statement, NULL) != SQLITE_OK) {
        fail(db, "failed to prepare text scalar");
    }
    if (sqlite3_step(statement) != SQLITE_ROW) {
        sqlite3_finalize(statement);
        fail(db, "text scalar did not return a row");
    }
    const unsigned char *text = sqlite3_column_text(statement, 0);
    int bytes = sqlite3_column_bytes(statement, 0);
    require(text != NULL && bytes >= 0, "text scalar returned NULL");
    require((size_t)bytes + 1 <= capacity, "text scalar output buffer is too small");
    memcpy(output, text, (size_t)bytes);
    output[bytes] = '\0';
    if (sqlite3_step(statement) != SQLITE_DONE) {
        sqlite3_finalize(statement);
        fail(db, "text scalar returned more than one row");
    }
    if (sqlite3_finalize(statement) != SQLITE_OK) {
        fail(db, "failed to finalize text scalar");
    }
}

static void load_extension(sqlite3 *db, const char *path) {
    char *error = NULL;
    require(sqlite3_enable_load_extension(db, 1) == SQLITE_OK, "failed to enable extension loading");
    int code = sqlite3_load_extension(db, path, "sqlite3_lithograph_init", &error);
    if (code != SQLITE_OK) {
        fprintf(stderr, "sqlite3_load_extension failed: %s\n", error == NULL ? "unknown" : error);
        sqlite3_free(error);
        exit(1);
    }
}

static sqlite3 *open_loaded(const char *database, const char *extension) {
    sqlite3 *db = NULL;
    if (sqlite3_open(database, &db) != SQLITE_OK) {
        fail(db, "failed to open database");
    }
    load_extension(db, extension);
    return db;
}

static sqlite3_int64 commit_count(sqlite3 *db) {
    return scalar_int(db, "SELECT count(*) FROM main._lithograph_commits");
}

static sqlite3_int64 label_count(sqlite3 *db, const char *label) {
    char sql[512];
    int written = snprintf(
        sql,
        sizeof(sql),
        "SELECT json_extract(lithograph('MATCH (n:%s) RETURN count(n)'), '$.rows[0][0]')",
        label
    );
    require(written > 0 && (size_t)written < sizeof(sql), "label-count SQL is too large");
    return scalar_int(db, sql);
}

typedef struct {
    sqlite3_int64 commits;
    sqlite3_int64 layers;
    sqlite3_int64 schema_objects;
    char main_head[65];
} canonical_history_snapshot;

static canonical_history_snapshot snapshot_canonical_history(sqlite3 *db) {
    canonical_history_snapshot snapshot = {
        .commits = scalar_int(db, "SELECT count(*) FROM main._lithograph_commits"),
        .layers = scalar_int(db, "SELECT count(*) FROM main._lithograph_layers"),
        .schema_objects = scalar_int(db, "SELECT count(*) FROM main._lithograph_schema_objects"),
        .main_head = {0},
    };
    scalar_text(
        db,
        "SELECT hex(commit_id) FROM main._lithograph_branches WHERE name='main'",
        snapshot.main_head,
        sizeof(snapshot.main_head)
    );
    return snapshot;
}

static void require_canonical_history_unchanged(
    sqlite3 *db,
    const canonical_history_snapshot *before,
    const char *message
) {
    canonical_history_snapshot after = snapshot_canonical_history(db);
    if (
        after.commits != before->commits ||
        after.layers != before->layers ||
        after.schema_objects != before->schema_objects ||
        strcmp(after.main_head, before->main_head) != 0
    ) {
        fprintf(
            stderr,
            "%s (commits %lld/%lld, layers %lld/%lld, schemas %lld/%lld, head %s/%s)\n",
            message,
            (long long)before->commits,
            (long long)after.commits,
            (long long)before->layers,
            (long long)after.layers,
            (long long)before->schema_objects,
            (long long)after.schema_objects,
            before->main_head,
            after.main_head
        );
        exit(1);
    }
    require(
        scalar_int(db, "SELECT json_extract(lithograph_integrity_check(), '$.ok')") == 1,
        "canonical history failed integrity verification after rollback"
    );
}

static void check_registration(sqlite3 *db) {
    sqlite3_int64 count = scalar_int(
        db,
        "SELECT count(*) FROM pragma_function_list "
        "WHERE name IN ('lithograph_tx_begin','lithograph_tx_commit','lithograph_tx_abort') "
        "AND (flags & 2048)=0 AND (flags & 524288)!=0"
    );
    require(count == 3, "transaction SQL functions have incorrect registration flags");
    require(
        scalar_int(db, "SELECT count(*) FROM pragma_function_list WHERE name='lithograph_tx_execute'") == 0,
        "lithograph_tx_execute must not be registered"
    );
}

static void check_commit_and_staged_read(sqlite3 *writer, sqlite3 *reader) {
    sqlite3_int64 before = commit_count(writer);
    char result[4096];
    scalar_text(
        writer,
        "SELECT lithograph_tx_begin('{\"author\":\"sql\",\"message\":\"two writes\"}')",
        result,
        sizeof(result)
    );
    require(strstr(result, "baseCommit") != NULL, "tx_begin result shape differs");
    require(sqlite3_get_autocommit(writer) == 0, "tx_begin did not start a SQLite transaction");
    scalar_text(writer, "SELECT lithograph('CREATE (:SqlTx {name: ''one''}) FINISH')", result, sizeof(result));
    scalar_text(
        writer,
        "SELECT lithograph('CREATE (:SqlTx {name: $name}) FINISH','{\"name\":\"two\"}')",
        result,
        sizeof(result)
    );
    scalar_text(
        writer,
        "SELECT json_extract(lithograph('MATCH (n:SqlTx) RETURN count(n)'), '$.rows[0][0]')",
        result,
        sizeof(result)
    );
    require(strcmp(result, "2") == 0, "normal execution did not read prior staged writes");
    require(label_count(reader, "SqlTx") == 0, "another connection observed staged writes");
    scalar_text(writer, "SELECT lithograph_tx_commit()", result, sizeof(result));
    require(strstr(result, "\"commit\"") != NULL, "tx_commit result shape differs");
    require(sqlite3_get_autocommit(writer) != 0, "tx_commit did not commit the SQLite transaction");
    require(commit_count(writer) == before + 1, "multiple SQL tx writes did not produce exactly one Commit");
    require(label_count(reader, "SqlTx") == 2, "committed SQL tx writes are not visible");
    scalar_text(
        writer,
        "SELECT author || ':' || message FROM main._lithograph_commits ORDER BY committed_at DESC LIMIT 1",
        result,
        sizeof(result)
    );
    require(strcmp(result, "sql:two writes") == 0, "tx_begin metadata was not preserved");
}

static void check_read_only_and_empty_delta(sqlite3 *db) {
    char result[4096];
    sqlite3_int64 before = commit_count(db);
    scalar_text(db, "SELECT lithograph_tx_begin('{}')", result, sizeof(result));
    scalar_text(db, "SELECT lithograph('RETURN 1 AS value')", result, sizeof(result));
    scalar_text(db, "SELECT lithograph_tx_commit()", result, sizeof(result));
    require(commit_count(db) == before, "read-only SQL transaction created a Commit");

    scalar_text(db, "SELECT lithograph_tx_begin('{}')", result, sizeof(result));
    scalar_text(db, "SELECT lithograph('CREATE (:SqlEmpty {id: 1}) FINISH')", result, sizeof(result));
    scalar_text(db, "SELECT lithograph('MATCH (n:SqlEmpty {id: 1}) DELETE n FINISH')", result, sizeof(result));
    scalar_text(db, "SELECT lithograph_tx_commit()", result, sizeof(result));
    require(commit_count(db) == before + 1, "empty-delta SQL transaction did not create one Commit");
    require(label_count(db, "SqlEmpty") == 0, "empty-delta SQL transaction changed graph state");
}

static void check_abort_and_fail_closed(sqlite3 *db) {
    char result[4096];
    canonical_history_snapshot before = snapshot_canonical_history(db);
    scalar_text(db, "SELECT lithograph_tx_begin('{}')", result, sizeof(result));
    scalar_text(db, "SELECT lithograph('CREATE (:SqlAbort) FINISH')", result, sizeof(result));
    scalar_text(
        db,
        "SELECT lithograph('CREATE RANGE INDEX sql_abort_idx FOR (n:SqlAbort) ON (n.value)')",
        result,
        sizeof(result)
    );
    scalar_text(db, "SELECT lithograph_tx_abort()", result, sizeof(result));
    require(strcmp(result, "{\"aborted\":true}") == 0, "tx_abort result shape differs");
    require(sqlite3_get_autocommit(db) != 0, "tx_abort did not rollback the SQLite transaction");
    require(label_count(db, "SqlAbort") == 0, "tx_abort left durable graph state");
    require_canonical_history_unchanged(db, &before, "tx_abort left canonical history state");

    scalar_text(db, "SELECT lithograph_tx_begin('{}')", result, sizeof(result));
    scalar_text(db, "SELECT lithograph('CREATE (:SqlFailure) FINISH')", result, sizeof(result));
    execute_error(db, "SELECT lithograph('not valid Cypher')", "LITHOGRAPH_PARSE_ERROR");
    require(sqlite3_get_autocommit(db) != 0, "failed transaction execution did not abort the SQLite transaction");
    require(label_count(db, "SqlFailure") == 0, "failed transaction execution left durable graph state");
    require_canonical_history_unchanged(db, &before, "failed transaction execution left canonical history state");

    scalar_text(db, "SELECT lithograph_tx_begin('{}')", result, sizeof(result));
    scalar_text(db, "SELECT lithograph('CREATE (:SqlBoundary) FINISH')", result, sizeof(result));
    execute_error(
        db,
        "SELECT lithograph('CALL lithograph.branch.list() YIELD name RETURN name')",
        "LITHOGRAPH_TRANSACTION_BOUNDARY_REQUIRED"
    );
    require(sqlite3_get_autocommit(db) != 0, "restricted transaction execution did not abort the SQLite transaction");
    require(
        label_count(db, "SqlBoundary") == 0,
        "restricted transaction execution left durable graph state"
    );
    require_canonical_history_unchanged(db, &before, "restricted transaction execution left canonical history state");
}

static void check_boundaries_and_arguments(sqlite3 *db) {
    char result[4096];
    execute(db, "BEGIN");
    execute_error(db, "SELECT lithograph_tx_begin('{}')", "LITHOGRAPH_TRANSACTION_BOUNDARY_REQUIRED");
    require(sqlite3_get_autocommit(db) == 0, "rejected outer transaction unexpectedly ended caller transaction");
    execute(db, "ROLLBACK");

    scalar_text(db, "SELECT lithograph_tx_begin('{}')", result, sizeof(result));
    execute_error(db, "SELECT lithograph_tx_begin('{}')", "LITHOGRAPH_TRANSACTION_BOUNDARY_REQUIRED");
    require(sqlite3_get_autocommit(db) == 0, "duplicate begin unexpectedly aborted the active transaction");
    scalar_text(db, "SELECT lithograph_tx_abort()", result, sizeof(result));

    execute_error(db, "SELECT lithograph_tx_execute('RETURN 1')", "no such function");
    execute_error(db, "SELECT lithograph_tx_commit()", "LITHOGRAPH_INVALID_ARGUMENT");
    execute_error(db, "SELECT lithograph_tx_abort()", "LITHOGRAPH_INVALID_ARGUMENT");

    execute_error(db, "SELECT lithograph_tx_begin(NULL)", "LITHOGRAPH_INVALID_ARGUMENT");
    execute_error(db, "SELECT lithograph_tx_begin(1)", "LITHOGRAPH_INVALID_ARGUMENT");
    execute_error(db, "SELECT lithograph_tx_begin('[]')", "LITHOGRAPH_INVALID_ARGUMENT");
    execute_error(db, "SELECT lithograph_tx_begin('{\"unknown\":1}')", "LITHOGRAPH_INVALID_ARGUMENT");
    execute_error(
        db,
        "SELECT lithograph_tx_begin('{\"expectedHead\":\"commit/0000000000000000000000000000000000000000000000000000000000000000\"}')",
        "LITHOGRAPH_BRANCH_HEAD_MOVED"
    );
    require(sqlite3_get_autocommit(db) != 0, "expectedHead mismatch left an active transaction");

    scalar_text(db, "SELECT lithograph_tx_begin('{}')", result, sizeof(result));
    execute_error(db, "SELECT lithograph('RETURN 1',NULL)", "LITHOGRAPH_INVALID_ARGUMENT");
    require(sqlite3_get_autocommit(db) != 0, "NULL execution argument did not fail-closed abort");

    scalar_text(db, "SELECT lithograph_tx_begin('{}')", result, sizeof(result));
    execute_error(
        db,
        "SELECT event FROM lithograph_rows('RETURN 1',NULL)",
        "LITHOGRAPH_INVALID_ARGUMENT"
    );
    require(
        sqlite3_get_autocommit(db) != 0,
        "NULL rows execution argument did not fail-closed abort"
    );

    scalar_text(db, "SELECT lithograph_tx_begin('{}')", result, sizeof(result));
    execute_error(
        db,
        "SELECT lithograph('RETURN 1','{}','{\"branch\":\"main\"}')",
        "LITHOGRAPH_INVALID_ARGUMENT"
    );
    require(sqlite3_get_autocommit(db) != 0, "invalid transaction execution options did not fail-closed abort");

    execute_error(db, "SELECT lithograph_tx_begin()", "wrong number of arguments");
    execute_error(db, "SELECT lithograph_tx_commit(1)", "wrong number of arguments");
}

static void check_result_limit_cleanup(sqlite3 *db) {
    char result[4096];
    canonical_history_snapshot before = snapshot_canonical_history(db);
    int old_limit = sqlite3_limit(db, SQLITE_LIMIT_LENGTH, 80);
    execute_error(db, "SELECT lithograph_tx_begin('{}')", "LITHOGRAPH_RESOURCE_ERROR");
    sqlite3_limit(db, SQLITE_LIMIT_LENGTH, old_limit);
    require(sqlite3_get_autocommit(db) != 0, "tx_begin result failure did not abort");

    scalar_text(db, "SELECT lithograph_tx_begin('{}')", result, sizeof(result));
    old_limit = sqlite3_limit(db, SQLITE_LIMIT_LENGTH, 100);
    execute_error(db, "SELECT lithograph('RETURN 1')", "LITHOGRAPH_RESOURCE_ERROR");
    sqlite3_limit(db, SQLITE_LIMIT_LENGTH, old_limit);
    require(sqlite3_get_autocommit(db) != 0, "transaction execution result failure did not abort");

    scalar_text(db, "SELECT lithograph_tx_begin('{}')", result, sizeof(result));
    scalar_text(db, "SELECT lithograph('CREATE (:SqlLimit) FINISH')", result, sizeof(result));
    old_limit = sqlite3_limit(db, SQLITE_LIMIT_LENGTH, 100);
    execute_error(db, "SELECT lithograph_tx_commit()", "LITHOGRAPH_RESOURCE_ERROR");
    sqlite3_limit(db, SQLITE_LIMIT_LENGTH, old_limit);
    require(sqlite3_get_autocommit(db) != 0, "tx_commit result failure did not abort");
    require(label_count(db, "SqlLimit") == 0, "result failure left durable graph state");
    require_canonical_history_unchanged(db, &before, "result failure left canonical history state");
}

static void check_ordinary_outer_transaction(sqlite3 *db) {
    canonical_history_snapshot before = snapshot_canonical_history(db);
    execute(db, "BEGIN");
    execute(db, "SELECT lithograph('CREATE (:SqlOuter {id:1}) FINISH')");
    execute(db, "SELECT lithograph('CREATE (:SqlOuter {id:2}) FINISH')");
    require(commit_count(db) == before.commits + 2, "ordinary outer transaction stopped producing per-query Commits");
    execute(db, "ROLLBACK");
    require(label_count(db, "SqlOuter") == 0, "ordinary outer rollback left durable graph state");
    require_canonical_history_unchanged(db, &before, "ordinary outer rollback left canonical history state");
}

static void check_close_cleanup(const char *database, const char *extension, sqlite3 *db) {
    char result[4096];
    canonical_history_snapshot before = snapshot_canonical_history(db);
    scalar_text(db, "SELECT lithograph_tx_begin('{}')", result, sizeof(result));
    scalar_text(db, "SELECT lithograph('CREATE (:SqlClose) FINISH')", result, sizeof(result));
    require(sqlite3_get_autocommit(db) == 0, "close-cleanup fixture has no active transaction");
    require(sqlite3_close(db) == SQLITE_OK, "failed to close connection with active SQL transaction");

    sqlite3 *reopened = open_loaded(database, extension);
    require(label_count(reopened, "SqlClose") == 0, "connection close left uncommitted graph data");
    require_canonical_history_unchanged(reopened, &before, "connection close left canonical history state");
    require(sqlite3_close(reopened) == SQLITE_OK, "failed to close reopened database");
}

int main(int argc, char **argv) {
    if (argc != 3) {
        fprintf(stderr, "usage: %s <extension-path> <database-path>\n", argv[0]);
        return 2;
    }
    const char *extension = argv[1];
    const char *database = argv[2];
    (void)remove(database);

    sqlite3 *writer = open_loaded(database, extension);
    execute(writer, "PRAGMA journal_mode=WAL");
    execute(writer, "SELECT lithograph_init()");
    sqlite3 *reader = open_loaded(database, extension);

    check_registration(writer);
    check_commit_and_staged_read(writer, reader);
    require(sqlite3_close(reader) == SQLITE_OK, "failed to close reader connection");
    check_read_only_and_empty_delta(writer);
    check_abort_and_fail_closed(writer);
    check_boundaries_and_arguments(writer);
    check_result_limit_cleanup(writer);
    check_ordinary_outer_transaction(writer);
    check_close_cleanup(database, extension, writer);

    (void)remove(database);
    puts("sql explicit transaction smoke: ok");
    return 0;
}
