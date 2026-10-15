#include "sqlite3.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static void fail(sqlite3 *db, const char *message) {
    fprintf(stderr, "%s: %s\n", message, db ? sqlite3_errmsg(db) : "no database");
    exit(1);
}

static void execute(sqlite3 *db, const char *sql) {
    char *error = NULL;
    int rc = sqlite3_exec(db, sql, NULL, NULL, &error);
    if (rc != SQLITE_OK) {
        fprintf(stderr, "failed SQL: %s\n%s\n", sql, error ? error : sqlite3_errmsg(db));
        sqlite3_free(error);
        exit(1);
    }
    sqlite3_free(error);
}

static void require_error(sqlite3 *db, const char *sql, const char *expected) {
    sqlite3_stmt *statement = NULL;
    int rc = sqlite3_prepare_v2(db, sql, -1, &statement, NULL);
    if (rc == SQLITE_OK) {
        while ((rc = sqlite3_step(statement)) == SQLITE_ROW) {
        }
    }
    if (rc == SQLITE_OK || rc == SQLITE_DONE) {
        if (statement != NULL) {
            sqlite3_finalize(statement);
        }
        fprintf(stderr, "expected SQL to fail: %s\n", sql);
        exit(1);
    }
    const char *message = sqlite3_errmsg(db);
    if (message == NULL || strstr(message, expected) == NULL) {
        fprintf(stderr, "unexpected error for %s: %s\n", sql, message ? message : "NULL");
        if (statement != NULL) {
            sqlite3_finalize(statement);
        }
        exit(1);
    }
    if (statement != NULL) {
        sqlite3_finalize(statement);
    }
}

static sqlite3_int64 scalar_int(sqlite3 *db, const char *sql) {
    sqlite3_stmt *statement = NULL;
    if (sqlite3_prepare_v2(db, sql, -1, &statement, NULL) != SQLITE_OK) {
        fail(db, "failed to prepare scalar integer query");
    }
    if (sqlite3_step(statement) != SQLITE_ROW) {
        fail(db, "failed to step scalar integer query");
    }
    sqlite3_int64 value = sqlite3_column_int64(statement, 0);
    if (sqlite3_finalize(statement) != SQLITE_OK) {
        fail(db, "failed to finalize scalar integer query");
    }
    return value;
}

static void require_text(sqlite3 *db, const char *sql, const char *expected) {
    sqlite3_stmt *statement = NULL;
    if (sqlite3_prepare_v2(db, sql, -1, &statement, NULL) != SQLITE_OK) {
        fail(db, "failed to prepare scalar text query");
    }
    if (sqlite3_step(statement) != SQLITE_ROW) {
        fail(db, "failed to step scalar text query");
    }
    const unsigned char *text = sqlite3_column_text(statement, 0);
    if (text == NULL || strcmp((const char *)text, expected) != 0) {
        fprintf(stderr, "unexpected result for %s: %s\n", sql, text ? (const char *)text : "NULL");
        exit(1);
    }
    if (sqlite3_finalize(statement) != SQLITE_OK) {
        fail(db, "failed to finalize scalar text query");
    }
}

static void require_tx_execute_absent(sqlite3 *db) {
    sqlite3_stmt *statement = NULL;
    int rc = sqlite3_prepare_v2(
        db,
        "SELECT lithograph_tx_execute('RETURN 1')",
        -1,
        &statement,
        NULL
    );
    if (rc == SQLITE_OK) {
        sqlite3_finalize(statement);
        fprintf(stderr, "lithograph_tx_execute unexpectedly exists\n");
        exit(1);
    }
    if (statement != NULL) {
        sqlite3_finalize(statement);
    }
}

static void require_safe_execution_surfaces(sqlite3 *db) {
    sqlite3_int64 before = scalar_int(db, "SELECT count(*) FROM main._lithograph_commits");
    require_text(
        db,
        "SELECT json_extract(lithograph('RETURN 7 AS v','{}',"
        "'{\"readOnlyNoExternalIO\":true}'),'$.rows[0][0]')",
        "7"
    );
    require_text(
        db,
        "SELECT group_concat(event, ',') FROM lithograph_rows("
        "'RETURN 7 AS v','{}','{\"readOnlyNoExternalIO\":true}')",
        "columns,row,summary"
    );
    require_error(
        db,
        "SELECT lithograph('CREATE (:Blocked) FINISH','{}',"
        "'{\"readOnlyNoExternalIO\":true}')",
        "LITHOGRAPH_INVALID_ARGUMENT: options.readOnlyNoExternalIO forbids"
    );
    require_error(
        db,
        "SELECT lithograph('EXPLAIN UNWIND [] AS x CREATE (:Blocked) FINISH','{}',"
        "'{\"readOnlyNoExternalIO\":true}')",
        "LITHOGRAPH_INVALID_ARGUMENT: options.readOnlyNoExternalIO forbids"
    );
    require_error(
        db,
        "SELECT event FROM lithograph_rows('PROFILE CALL lithograph.branch.checkout(''main'')"
        " YIELD name RETURN name','{}','{\"readOnlyNoExternalIO\":true}')",
        "LITHOGRAPH_INVALID_ARGUMENT: options.readOnlyNoExternalIO forbids"
    );
    require_error(
        db,
        "SELECT event FROM lithograph_rows('CALL { CREATE (:Blocked) }',"
        "'{}','{\"readOnlyNoExternalIO\":true}')",
        "LITHOGRAPH_INVALID_ARGUMENT: options.readOnlyNoExternalIO forbids"
    );
    require_error(
        db,
        "SELECT lithograph('LOAD CSV FROM ''file:///__lithograph_safe_mode_forbidden__.csv''"
        " AS row RETURN row','{}','{\"readOnlyNoExternalIO\":true}')",
        "LITHOGRAPH_INVALID_ARGUMENT: options.readOnlyNoExternalIO forbids"
    );
    require_error(
        db,
        "SELECT event FROM lithograph_rows("
        "'CALL db.index.semantic.queryNodes(''missing'',''text'',{limit:1})"
        " YIELD node RETURN node','{}','{\"readOnlyNoExternalIO\":true}')",
        "LITHOGRAPH_INVALID_ARGUMENT: options.readOnlyNoExternalIO forbids"
    );
    require_error(
        db,
        "SELECT lithograph('RETURN 1','{}','{\"readOnlyNoExternalIO\":null}')",
        "LITHOGRAPH_INVALID_ARGUMENT"
    );
    require_error(
        db,
        "SELECT lithograph('EXPLAIN RETURN s\"{COUNT { LOAD CSV FROM "
        "''file:///__lithograph_safe_mode_forbidden__.csv'' AS r RETURN r }}\" AS x',"
        "'{}','{\"readOnlyNoExternalIO\":true}')",
        "LITHOGRAPH_INVALID_ARGUMENT: options.readOnlyNoExternalIO forbids"
    );
    require_error(
        db,
        "SELECT event FROM lithograph_rows('PROFILE RETURN s\"{COUNT { LOAD CSV FROM "
        "''file:///__lithograph_safe_mode_forbidden__.csv'' AS r RETURN r }}\" AS x',"
        "'{}','{\"readOnlyNoExternalIO\":true}')",
        "LITHOGRAPH_INVALID_ARGUMENT: options.readOnlyNoExternalIO forbids"
    );
    if (scalar_int(db, "SELECT count(*) FROM main._lithograph_commits") != before) {
        fail(db, "safe-mode rejected query changed history");
    }
}

static void require_safe_explicit_transaction(sqlite3 *db) {
    require_text(db, "SELECT json_type(lithograph_tx_begin('{}'),'$.baseCommit')", "text");
    require_text(
        db,
        "SELECT group_concat(event, ',') FROM lithograph_rows("
        "'RETURN 1 AS v','{}','{\"readOnlyNoExternalIO\":true}')",
        "columns,row,summary"
    );
    require_text(db, "SELECT json_type(lithograph_tx_commit(),'$.commit')", "text");
    require_text(db, "SELECT json_type(lithograph_tx_begin('{}'),'$.baseCommit')", "text");
    require_error(
        db,
        "SELECT lithograph('CREATE (:Blocked) FINISH','{}',"
        "'{\"readOnlyNoExternalIO\":true}')",
        "LITHOGRAPH_INVALID_ARGUMENT: options.readOnlyNoExternalIO forbids"
    );
    require_error(db, "SELECT lithograph_tx_commit()", "LITHOGRAPH_INVALID_ARGUMENT");
}

static void require_early_close_rolls_back(sqlite3 *db) {
    sqlite3_stmt *statement = NULL;
    const char *sql =
        "SELECT event FROM lithograph_rows("
        "'CREATE (:EarlySmoke {v:1}) RETURN 1 AS x') LIMIT 2";
    if (sqlite3_prepare_v2(db, sql, -1, &statement, NULL) != SQLITE_OK) {
        fail(db, "failed to prepare early-close stream");
    }
    if (sqlite3_step(statement) != SQLITE_ROW || sqlite3_step(statement) != SQLITE_ROW) {
        fail(db, "failed to consume early-close stream");
    }
    if (sqlite3_finalize(statement) != SQLITE_OK) {
        fail(db, "failed to finalize early-close stream");
    }
    sqlite3_int64 count = scalar_int(
        db,
        "SELECT json_extract(lithograph("
        "'MATCH (n:EarlySmoke) RETURN count(n) AS c'), '$.rows[0][0]')"
    );
    if (count != 0) {
        fprintf(stderr, "early-close mutation was not rolled back: %lld\n", (long long)count);
        exit(1);
    }
}

static void require_outer_sql_lifecycle(sqlite3 *db) {
    sqlite3_stmt *statement = NULL;
    const char *limit_sql =
        "SELECT event FROM lithograph_rows("
        "'CREATE (:OuterLimit {v:1}) RETURN 1 AS x') LIMIT 1";
    if (sqlite3_prepare_v2(db, limit_sql, -1, &statement, NULL) != SQLITE_OK) {
        fail(db, "failed to prepare outer LIMIT stream");
    }
    if (sqlite3_step(statement) != SQLITE_ROW) {
        fail(db, "outer LIMIT did not return columns");
    }
    const unsigned char *event = sqlite3_column_text(statement, 0);
    if (event == NULL || strcmp((const char *)event, "columns") != 0) {
        fail(db, "outer LIMIT did not stop on columns");
    }
    if (sqlite3_finalize(statement) != SQLITE_OK) {
        fail(db, "failed to finalize outer LIMIT stream");
    }
    if (scalar_int(
            db,
            "SELECT json_extract(lithograph("
            "'MATCH (n:OuterLimit) RETURN count(n)'), '$.rows[0][0]')"
        ) != 0) {
        fail(db, "outer LIMIT 1 triggered side effects");
    }

    if (scalar_int(
            db,
            "SELECT count(*) FROM lithograph_rows("
            "'CREATE (:WhereRow {v:1}) RETURN 1 AS x') WHERE event='row'"
        ) != 1) {
        fail(db, "WHERE event=row returned the wrong row count");
    }
    if (scalar_int(
            db,
            "SELECT json_extract(lithograph("
            "'MATCH (n:WhereRow) RETURN count(n)'), '$.rows[0][0]')"
        ) != 1) {
        fail(db, "WHERE event=row did not advance execution to terminal");
    }

    if (scalar_int(
            db,
            "SELECT count(*) FROM lithograph_rows("
            "'CREATE (:WhereSummary {v:1}) RETURN 1 AS x') WHERE event='summary'"
        ) != 1) {
        fail(db, "WHERE event=summary returned the wrong row count");
    }
    if (scalar_int(
            db,
            "SELECT json_extract(lithograph("
            "'MATCH (n:WhereSummary) RETURN count(n)'), '$.rows[0][0]')"
        ) != 1) {
        fail(db, "WHERE event=summary did not execute row-producing work");
    }
}

static void require_sequential_scans(sqlite3 *db) {
    if (scalar_int(
            db,
            "SELECT count(*) FROM lithograph_rows("
            "'CREATE (:SequentialScan {v:1}) RETURN 1 AS x')"
        ) != 3) {
        fail(db, "first sequential rows scan did not reach summary");
    }
    if (scalar_int(
            db,
            "SELECT count(*) FROM lithograph_rows("
            "'CREATE (:SequentialScan {v:2}) RETURN 1 AS x')"
        ) != 3) {
        fail(db, "second sequential rows scan did not reach summary");
    }
    if (scalar_int(
            db,
            "SELECT json_extract(lithograph("
            "'MATCH (n:SequentialScan) RETURN count(n)'), '$.rows[0][0]')"
        ) != 2) {
        fail(db, "sequential scans were implicitly deduplicated");
    }
}

static void require_overlapping_read_cursors(sqlite3 *db) {
    sqlite3_stmt *left = NULL;
    sqlite3_stmt *right = NULL;
    if (sqlite3_prepare_v2(
            db,
            "SELECT event FROM lithograph_rows('RETURN 1 AS x')",
            -1,
            &left,
            NULL
        ) != SQLITE_OK ||
        sqlite3_prepare_v2(
            db,
            "SELECT event FROM lithograph_rows('RETURN 2 AS x')",
            -1,
            &right,
            NULL
        ) != SQLITE_OK) {
        fail(db, "failed to prepare overlapping read cursors");
    }
    for (int index = 0; index < 3; index++) {
        if (sqlite3_step(left) != SQLITE_ROW || sqlite3_step(right) != SQLITE_ROW) {
            fail(db, "overlapping read cursors did not advance independently");
        }
    }
    if (sqlite3_step(left) != SQLITE_DONE || sqlite3_step(right) != SQLITE_DONE) {
        fail(db, "overlapping read cursors did not reach EOF");
    }
    if (sqlite3_finalize(left) != SQLITE_OK || sqlite3_finalize(right) != SQLITE_OK) {
        fail(db, "failed to finalize overlapping read cursors");
    }
}

static void require_side_effect_overlap_rejected(sqlite3 *db) {
    sqlite3_stmt *outer = NULL;
    if (sqlite3_prepare_v2(
            db,
            "SELECT event FROM lithograph_rows("
            "'CREATE (:OverlapOuter {v:1}) RETURN 1 AS x')",
            -1,
            &outer,
            NULL
        ) != SQLITE_OK) {
        fail(db, "failed to prepare side-effecting overlap cursor");
    }
    if (sqlite3_step(outer) != SQLITE_ROW || sqlite3_step(outer) != SQLITE_ROW) {
        fail(db, "failed to advance side-effecting overlap cursor to provisional row");
    }

    sqlite3_stmt *inner = NULL;
    if (sqlite3_prepare_v2(
            db,
            "SELECT lithograph('CREATE (:OverlapInner {v:1}) FINISH')",
            -1,
            &inner,
            NULL
        ) != SQLITE_OK) {
        fail(db, "failed to prepare nested side-effecting execution");
    }
    int rc = sqlite3_step(inner);
    const char *message = sqlite3_errmsg(db);
    if (rc == SQLITE_ROW || rc == SQLITE_DONE ||
        message == NULL || strstr(message, "LITHOGRAPH_TRANSACTION_BOUNDARY_REQUIRED") == NULL) {
        fprintf(stderr, "nested side-effecting execution was not rejected: %s\n",
                message ? message : "NULL");
        sqlite3_finalize(inner);
        sqlite3_finalize(outer);
        exit(1);
    }
    sqlite3_finalize(inner);
    if (sqlite3_finalize(outer) != SQLITE_OK) {
        fail(db, "failed to finalize side-effecting overlap cursor");
    }
    if (scalar_int(
            db,
            "SELECT json_extract(lithograph("
            "'MATCH (n:OverlapOuter) RETURN count(n)'), '$.rows[0][0]')"
        ) != 0 ||
        scalar_int(
            db,
            "SELECT json_extract(lithograph("
            "'MATCH (n:OverlapInner) RETURN count(n)'), '$.rows[0][0]')"
        ) != 0) {
        fail(db, "overlap rejection left partial side effects");
    }
}

static void require_multiple_scans_and_overlap(sqlite3 *db) {
    require_sequential_scans(db);
    require_overlapping_read_cursors(db);
    require_side_effect_overlap_rejected(db);
}

static void schema_no_rows(sqlite3 *db, const char *query, const char *params, int rows) {
    char *sql = sqlite3_mprintf(
        rows ? "SELECT group_concat(event, ',') FROM lithograph_rows(%Q,%Q)" :
               "SELECT json_array_length(json_extract(lithograph(%Q,%Q),'$.rows'))",
        query, params
    );
    if (sql == NULL) {
        fail(db, "failed to allocate schema smoke SQL");
    }
    require_text(db, sql, rows ? "columns,summary" : "0");
    sqlite3_free(sql);
}

static void require_schema_scratch_clean(sqlite3 *db) {
    if (!sqlite3_get_autocommit(db)) {
        fail(db, "schema execution left an active transaction");
    }
    if (scalar_int(db, "SELECT count(*) FROM sqlite_temp_master "
                       "WHERE name GLOB '_lithograph_fts_probe_*'") != 0) {
        fail(db, "schema execution left an FTS analyzer probe");
    }
    if (scalar_int(db, "SELECT count(*) FROM sqlite_temp_master WHERE type='table' "
                       "AND name='_lithograph_constraint_validation_seen'") != 0 &&
        scalar_int(db, "SELECT count(*) FROM temp._lithograph_constraint_validation_seen") != 0) {
        fail(db, "schema execution left uniqueness scratch content");
    }
}

static void schema_failure(sqlite3 *db, const char *query, const char *params,
                           int rows, const char *expected) {
    sqlite3_int64 commits = scalar_int(db, "SELECT count(*) FROM main._lithograph_commits");
    char *sql = sqlite3_mprintf(
        rows ? "SELECT event FROM lithograph_rows(%Q,%Q)" : "SELECT lithograph(%Q,%Q)",
        query, params
    );
    if (sql == NULL) {
        fail(db, "failed to allocate failing schema smoke SQL");
    }
    require_error(db, sql, expected);
    sqlite3_free(sql);
    if (scalar_int(db, "SELECT count(*) FROM main._lithograph_commits") != commits) {
        fail(db, "failed schema execution published a Commit");
    }
    require_schema_scratch_clean(db);
}

static void schema_result(sqlite3 *db, const char *query, const char *expected, int rows) {
    char *sql = sqlite3_mprintf(
        rows ? "SELECT group_concat(data, '') FROM lithograph_rows(%Q) WHERE event='row'" :
               "SELECT json_extract(lithograph(%Q),'$.rows[0]')",
        query
    );
    if (sql == NULL) {
        fail(db, "failed to allocate schema result SQL");
    }
    require_text(db, sql, expected);
    sqlite3_free(sql);
}

static void test_rows_constraints(sqlite3 *db) {
    const char *definitions[] = {
        "CREATE CONSTRAINT rows_node_key FOR (n:RowsKeyNode) REQUIRE n.id IS NODE KEY",
        "CREATE CONSTRAINT rows_node_unique FOR (n:RowsUniqueNode) REQUIRE n.id IS UNIQUE",
        "CREATE CONSTRAINT rows_rel_key FOR ()-[r:ROWS_KEY_REL]-() "
        "REQUIRE r.id IS RELATIONSHIP KEY",
        "CREATE CONSTRAINT rows_rel_unique FOR ()-[r:ROWS_UNIQUE_REL]-() "
        "REQUIRE r.id IS UNIQUE",
        "ALTER CURRENT GRAPH TYPE ADD {"
        "(:RowsTypedNode => {id :: INTEGER IS KEY, code :: INTEGER IS UNIQUE}),"
        "()-[:ROWS_TYPED_REL => {id :: INTEGER IS KEY, code :: INTEGER IS UNIQUE}]->()}"
    };
    struct constraint_case {
        const char *write;
        const char *count;
    } cases[] = {
        {"CREATE (:RowsKeyNode {id:$id}) FINISH",
         "MATCH (n:RowsKeyNode) RETURN count(n)"},
        {"CREATE (:RowsUniqueNode {id:$id}) FINISH",
         "MATCH (n:RowsUniqueNode) RETURN count(n)"},
        {"CREATE ()-[:ROWS_KEY_REL {id:$id}]->() FINISH",
         "MATCH ()-[r:ROWS_KEY_REL]->() RETURN count(r)"},
        {"CREATE ()-[:ROWS_UNIQUE_REL {id:$id}]->() FINISH",
         "MATCH ()-[r:ROWS_UNIQUE_REL]->() RETURN count(r)"},
        {"CREATE (:RowsTypedNode {id:$id, code:$id}) FINISH",
         "MATCH (n:RowsTypedNode) RETURN count(n)"},
        {"CREATE ()-[:ROWS_TYPED_REL {id:$id, code:$id}]->() FINISH",
         "MATCH ()-[r:ROWS_TYPED_REL]->() RETURN count(r)"}
    };
    for (size_t index = 0; index < sizeof(definitions) / sizeof(definitions[0]); index++) {
        schema_no_rows(db, definitions[index], "{}", (int)(index % 2));
        require_schema_scratch_clean(db);
    }
    for (size_t index = 0; index < sizeof(cases) / sizeof(cases[0]); index++) {
        schema_no_rows(db, cases[index].write, "{\"id\":1}", 0);
        schema_no_rows(db, cases[index].write, "{\"id\":2}", 1);
        require_schema_scratch_clean(db);
        schema_failure(db, cases[index].write, "{\"id\":1}", 0,
                       "LITHOGRAPH_CONSTRAINT_ERROR");
        schema_failure(db, cases[index].write, "{\"id\":2}", 1,
                       "LITHOGRAPH_CONSTRAINT_ERROR");
        schema_result(db, cases[index].count, "[2]", 0);
        schema_result(db, cases[index].count, "[2]", 1);
        schema_no_rows(db, cases[index].write, "{\"id\":3}", 1);
        schema_result(db, cases[index].count, "[3]", 0);
        require_schema_scratch_clean(db);
    }
    schema_failure(db, "CREATE (:RowsTypedNode {id:9, code:1}) FINISH", "{}", 1,
                   "LITHOGRAPH_CONSTRAINT_ERROR");
    schema_failure(db, "CREATE ()-[:ROWS_TYPED_REL {id:9, code:1}]->() FINISH", "{}", 1,
                   "LITHOGRAPH_CONSTRAINT_ERROR");
    schema_result(db, "MATCH (n:RowsTypedNode) RETURN count(n)", "[3]", 0);
    schema_result(db, "MATCH ()-[r:ROWS_TYPED_REL]->() RETURN count(r)", "[3]", 0);
}

static void test_rows_fulltext(sqlite3 *db) {
    schema_no_rows(db, "CREATE (:RowsText:RowsTextRecover {text:'running graphs'}) FINISH", "{}", 0);
    schema_no_rows(db, "CREATE FULLTEXT INDEX rows_text_default "
                       "FOR (n:RowsText) ON EACH [n.text]", "{}", 1);
    require_schema_scratch_clean(db);
    schema_result(db, "CALL db.index.fulltext.queryNodes('rows_text_default','graphs') "
                      "YIELD node RETURN node.text", "[\"running graphs\"]", 0);
    schema_result(db, "CALL db.index.fulltext.queryNodes('rows_text_default','graphs') "
                      "YIELD node RETURN node.text", "[\"running graphs\"]", 1);
    schema_no_rows(db, "CREATE FULLTEXT INDEX rows_text_porter "
                       "FOR (n:RowsText) ON EACH [n.text] "
                       "OPTIONS {indexConfig:{`fulltext.analyzer`:'porter unicode61'}}", "{}", 1);
    require_schema_scratch_clean(db);
    schema_result(db, "CALL db.index.fulltext.queryNodes('rows_text_porter','run') "
                      "YIELD node RETURN node.text", "[\"running graphs\"]", 0);
    schema_result(db, "CALL db.index.fulltext.queryNodes('rows_text_porter','run') "
                      "YIELD node RETURN node.text", "[\"running graphs\"]", 1);
    schema_failure(db, "CREATE FULLTEXT INDEX rows_text_recovered "
                       "FOR (n:RowsTextRecover) ON EACH [n.text] "
                       "OPTIONS {indexConfig:{`fulltext.analyzer`:'missing_rows_tokenizer'}}",
                   "{}", 1, "LITHOGRAPH_SCHEMA_ERROR");
    schema_result(db, "SHOW FULLTEXT INDEXES YIELD name "
                      "WHERE name='rows_text_recovered' RETURN count(*)", "[0]", 0);
    schema_no_rows(db, "CREATE FULLTEXT INDEX rows_text_recovered "
                       "FOR (n:RowsTextRecover) ON EACH [n.text]", "{}", 1);
    schema_result(db, "CALL db.index.fulltext.queryNodes('rows_text_recovered','graphs') "
                      "YIELD node RETURN node.text", "[\"running graphs\"]", 1);
    require_schema_scratch_clean(db);
}

static void test_rows_schema_early_close(sqlite3 *db) {
    const char *queries[] = {
        "CREATE CONSTRAINT rows_never_created FOR (n:RowsEarly) REQUIRE n.id IS NODE KEY",
        "CREATE FULLTEXT INDEX rows_never_fulltext FOR (n:RowsEarly) ON EACH [n.text]"
    };
    sqlite3_int64 commits = scalar_int(db, "SELECT count(*) FROM main._lithograph_commits");
    for (size_t index = 0; index < sizeof(queries) / sizeof(queries[0]); index++) {
        char *sql = sqlite3_mprintf("SELECT event FROM lithograph_rows(%Q)", queries[index]);
        sqlite3_stmt *statement = NULL;
        if (sql == NULL || sqlite3_prepare_v2(db, sql, -1, &statement, NULL) != SQLITE_OK) {
            fail(db, "failed to prepare metadata-only schema stream");
        }
        sqlite3_free(sql);
        if (sqlite3_step(statement) != SQLITE_ROW) {
            fail(db, "schema stream did not begin with metadata");
        }
        const unsigned char *event = sqlite3_column_text(statement, 0);
        if (event == NULL || strcmp((const char *)event, "columns") != 0) {
            fail(db, "schema stream did not return a columns event");
        }
        if (sqlite3_finalize(statement) != SQLITE_OK) {
            fail(db, "failed to close metadata-only schema stream");
        }
        require_schema_scratch_clean(db);
    }
    if (scalar_int(db, "SELECT count(*) FROM main._lithograph_commits") != commits) {
        fail(db, "metadata-only schema stream changed history");
    }
    schema_result(db, "SHOW CONSTRAINTS YIELD name WHERE name='rows_never_created' "
                      "RETURN count(*)", "[0]", 0);
    schema_result(db, "SHOW FULLTEXT INDEXES YIELD name WHERE name='rows_never_fulltext' "
                      "RETURN count(*)", "[0]", 0);
}

static void test_rows_schema_parity(sqlite3 *db) {
    test_rows_constraints(db);
    test_rows_fulltext(db);
    test_rows_schema_early_close(db);
}

static void require_checkout_surfaces(sqlite3 *db) {
    execute(db, "SELECT lithograph('CALL lithograph.branch.create(''checkout-smoke'')')");
    require_text(
        db,
        "SELECT json_extract(lithograph('CALL lithograph.branch.checkout(''checkout-smoke'') "
        "YIELD name RETURN name'), '$.rows[0][0]')",
        "checkout-smoke"
    );
    require_text(
        db,
        "SELECT json_extract(lithograph('CALL lithograph.branch.checkout(''main'') "
        "YIELD name RETURN name'), '$.rows[0][0]')",
        "main"
    );

    sqlite3_stmt *statement = NULL;
    const char *early_sql =
        "SELECT event FROM lithograph_rows('CALL lithograph.branch.checkout(''checkout-smoke'') "
        "YIELD name RETURN name') LIMIT 2";
    if (sqlite3_prepare_v2(db, early_sql, -1, &statement, NULL) != SQLITE_OK) {
        fail(db, "failed to prepare checkout early-close stream");
    }
    if (sqlite3_step(statement) != SQLITE_ROW || sqlite3_step(statement) != SQLITE_ROW) {
        fail(db, "failed to consume provisional checkout row");
    }
    if (sqlite3_finalize(statement) != SQLITE_OK) {
        fail(db, "failed to finalize checkout early-close stream");
    }
    require_text(
        db,
        "SELECT json_extract(lithograph('CALL lithograph.branch.list() YIELD name,active "
        "WHERE active RETURN name'), '$.rows[0][0]')",
        "main"
    );

    require_text(
        db,
        "SELECT group_concat(event, ',') FROM lithograph_rows('CALL lithograph.branch.checkout("
        "''checkout-smoke'') YIELD name RETURN name')",
        "columns,row,summary"
    );
    require_text(
        db,
        "SELECT json_extract(lithograph('CALL lithograph.branch.list() YIELD name,active "
        "WHERE active RETURN name'), '$.rows[0][0]')",
        "checkout-smoke"
    );
    execute(db, "SELECT lithograph('CALL lithograph.branch.checkout(''main'')')");
}

int main(int argc, char **argv) {
    if (argc != 2) {
        fprintf(stderr, "usage: sql_surface_smoke <lithograph-extension>\n");
        return 2;
    }

    sqlite3 *db = NULL;
    if (sqlite3_open(":memory:", &db) != SQLITE_OK) {
        fail(db, "failed to open SQLite");
    }
    sqlite3_enable_load_extension(db, 1);
    char *error = NULL;
    if (sqlite3_load_extension(db, argv[1], "sqlite3_lithograph_init", &error) != SQLITE_OK) {
        fprintf(stderr, "failed to load Lithograph: %s\n", error ? error : "");
        sqlite3_free(error);
        sqlite3_close(db);
        return 1;
    }
    sqlite3_free(error);

    if (scalar_int(db, "SELECT json_extract(lithograph_init(),'$.storageFormat')") != 3) {
        fail(db, "unexpected Lithograph storage format");
    }
    require_text(
        db,
        "SELECT group_concat(event, ',') FROM lithograph_rows('RETURN 1 AS x')",
        "columns,row,summary"
    );
    require_text(
        db,
        "SELECT group_concat(event, ',') FROM lithograph_rows("
        "'MATCH (n:NoSuchRows) RETURN n')",
        "columns,summary"
    );
    require_text(
        db,
        "SELECT group_concat(event, ',') FROM lithograph_rows("
        "'CREATE (:NoReturnRows {v:1}) FINISH')",
        "columns,summary"
    );
    if (scalar_int(
            db,
            "SELECT json_extract(lithograph("
            "'MATCH (n:NoReturnRows) RETURN count(n)'), '$.rows[0][0]')"
        ) != 1) {
        fail(db, "no-RETURN rows execution did not commit");
    }
    require_safe_execution_surfaces(db);
    require_safe_explicit_transaction(db);
    test_rows_schema_parity(db);

    require_error(db, "SELECT * FROM lithograph_rows(NULL)", "LITHOGRAPH_INVALID_ARGUMENT");
    require_error(db, "SELECT * FROM lithograph_rows(1)", "LITHOGRAPH_INVALID_ARGUMENT");
    require_error(
        db,
        "SELECT * FROM lithograph_rows('RETURN $x', '[]')",
        "LITHOGRAPH_INVALID_ARGUMENT"
    );
    require_error(
        db,
        "SELECT * FROM lithograph_rows('RETURN 1', '{}', '{\"unknown\":1}')",
        "LITHOGRAPH_INVALID_ARGUMENT"
    );
    require_error(
        db,
        "SELECT lithograph('RETURN 1', NULL)",
        "LITHOGRAPH_INVALID_ARGUMENT"
    );

    execute(
        db,
        "CREATE VIEW rows_directonly_probe AS "
        "SELECT event,data FROM lithograph_rows('RETURN 1')"
    );
    require_error(db, "SELECT * FROM rows_directonly_probe", "unsafe use");
    execute(db, "DROP VIEW rows_directonly_probe");

    require_text(
        db,
        "SELECT group_concat(event, ',') FROM lithograph_rows("
        "'CREATE (:SqlSmoke {v:1}) RETURN 1 AS x')",
        "columns,row,summary"
    );
    if (scalar_int(
            db,
            "SELECT json_extract(lithograph("
            "'MATCH (n:SqlSmoke) RETURN count(n) AS c'), '$.rows[0][0]')"
        ) != 1) {
        fail(db, "rows mutation was not committed");
    }
    require_early_close_rolls_back(db);
    require_outer_sql_lifecycle(db);
    require_multiple_scans_and_overlap(db);
    require_checkout_surfaces(db);

    require_text(db, "SELECT json_type(lithograph_tx_begin('{}'),'$.baseCommit')", "text");
    require_text(
        db,
        "SELECT json_type(lithograph('CREATE (:TxSmoke {v:1}) FINISH'),'$.summary.commit')",
        "null"
    );
    if (scalar_int(
            db,
            "SELECT json_extract(lithograph("
            "'MATCH (n:TxSmoke) RETURN count(n) AS c'), '$.rows[0][0]')"
        ) != 1) {
        fail(db, "explicit transaction staged state is not visible");
    }
    require_text(db, "SELECT json_type(lithograph_tx_commit(),'$.commit')", "text");
    require_tx_execute_absent(db);

    sqlite3_close(db);
    puts("SQL surface smoke passed");
    return 0;
}
