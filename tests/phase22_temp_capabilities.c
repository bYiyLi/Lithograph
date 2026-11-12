// S22.3: real host TEMP authorizer DENY/IGNORE must preserve complete results.
// Every fixture is private :memory:, and every graph/schema write uses Cypher.
#include <sqlite3.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

typedef struct { int response; int main_read; unsigned blocked; } Policy;
typedef struct { unsigned columns; unsigned rows; unsigned summaries; } Events;
static unsigned checks;
static const char *patterns[] = {"(n:Item)", "()-[r:LINK]->()"};
static const char *variables[] = {"n", "r"};

static void require(sqlite3 *db, int condition, const char *message) {
    if (!condition) {
        fprintf(stderr, "S22.3 TEMP capability: %s (%s)\n", message,
                db == NULL ? "no connection" : sqlite3_errmsg(db));
        exit(1);
    }
    checks++;
}

static sqlite3_stmt *statement(sqlite3 *db, const char *sql, const char *query) {
    sqlite3_stmt *stmt = NULL;
    require(db, sqlite3_prepare_v2(db, sql, -1, &stmt, NULL) == SQLITE_OK, "prepare public SQL");
    if (query != NULL)
        require(db, sqlite3_bind_text(stmt, 1, query, -1, SQLITE_STATIC) == SQLITE_OK, "bind Cypher");
    return stmt;
}

static char *text(sqlite3 *db, const char *sql, const char *query) {
    sqlite3_stmt *stmt = statement(db, sql, query);
    require(db, sqlite3_step(stmt) == SQLITE_ROW, "execute complete scalar");
    const char *value = (const char *)sqlite3_column_text(stmt, 0);
    require(db, value != NULL, "scalar text value");
    char *copy = sqlite3_mprintf("%s", value);
    require(db, copy != NULL, "copy bounded scalar");
    require(db, sqlite3_step(stmt) == SQLITE_DONE, "consume scalar");
    require(db, sqlite3_finalize(stmt) == SQLITE_OK, "finalize scalar");
    return copy;
}

static void cypher(sqlite3 *db, const char *query) {
    sqlite3_free(text(db, "SELECT lithograph(?1)", query));
}

static char *expected(sqlite3 *db, int value, unsigned count) {
    sqlite3_str *builder = sqlite3_str_new(db);
    sqlite3_str_appendall(builder, "[");
    for (unsigned i = 0; i < count; i++)
        sqlite3_str_appendf(builder, "%s[%d]", i == 0 ? "" : ",", value);
    sqlite3_str_appendall(builder, "]");
    char *result = sqlite3_str_finish(builder);
    require(db, result != NULL, "expected complete row bag");
    return result;
}

static char *query(unsigned kind, int value, int oracle) {
    const char *v = variables[kind];
    return sqlite3_mprintf("MATCH %s WHERE (%s.v=%d)%s RETURN %s.v",
                          patterns[kind], v, value, oracle ? " OR false" : "", v);
}

static void scalar(sqlite3 *db, const char *q, int value, unsigned count) {
    char *actual = text(db, "SELECT json_extract(lithograph(?1),'$.rows')", q);
    char *want = expected(db, value, count);
    if (strcmp(actual, want) != 0)
        fprintf(stderr, "query=%s expected=%s actual=%s\n", q, want, actual);
    require(db, strcmp(actual, want) == 0, "complete scalar row/multiplicity oracle");
    sqlite3_free(actual);
    sqlite3_free(want);
}

static void event(sqlite3 *db, sqlite3_stmt *stmt, Events *seen, int value) {
    const char *kind = (const char *)sqlite3_column_text(stmt, 0);
    const char *data = (const char *)sqlite3_column_text(stmt, 1);
    require(db, kind != NULL && data != NULL, "non-null stream event");
    require(db, seen->summaries == 0, "summary is terminal");
    if (strcmp(kind, "columns") == 0) {
        require(db, seen->columns == 0 && seen->rows == 0, "columns exactly first");
        seen->columns++;
    } else if (strcmp(kind, "row") == 0) {
        char want[32];
        (void)snprintf(want, sizeof(want), "[%d]", value);
        require(db, seen->columns == 1, "columns before row");
        require(db, strcmp(data, want) == 0, "complete row payload oracle");
        seen->rows++;
    } else if (strcmp(kind, "summary") == 0) {
        require(db, seen->columns == 1, "columns before summary");
        seen->summaries++;
    } else {
        require(db, 0, "known stream event");
    }
}

static void rows(sqlite3 *db, const char *q, int value, unsigned count) {
    sqlite3_stmt *stmt = statement(db, "SELECT event,data FROM lithograph_rows(?1)", q);
    Events seen = {0};
    int rc;
    while ((rc = sqlite3_step(stmt)) == SQLITE_ROW) event(db, stmt, &seen, value);
    require(db, rc == SQLITE_DONE, "complete rows adapter");
    require(db, seen.rows == count, "complete stream multiplicity");
    require(db, seen.summaries == 1, "one success summary");
    require(db, sqlite3_finalize(stmt) == SQLITE_OK, "finalize row stream");
}

static void both(sqlite3 *db, const char *q, int value, unsigned count) {
    scalar(db, q, value, count);
    rows(db, q, value, count);
}

static void plan(sqlite3 *db, const char *q, int indexed) {
    char *explain = sqlite3_mprintf("EXPLAIN %s", q);
    require(db, explain != NULL, "bounded EXPLAIN");
    char *actual = text(db, "SELECT json_extract(lithograph(?1),'$.rows[0][0]')", explain);
    require(db, (strstr(actual, "IndexSeek") != NULL) == indexed, "truthful physical plan");
    sqlite3_free(actual);
    sqlite3_free(explain);
}

static void profile(sqlite3 *db, const char *q, unsigned count, int indexed) {
    char *profiled = sqlite3_mprintf("PROFILE %s", q);
    require(db, profiled != NULL, "bounded PROFILE");
    sqlite3_stmt *stmt = statement(db,
        "WITH result(envelope) AS MATERIALIZED (SELECT lithograph(?1)) "
        "SELECT (SELECT count(*) FROM json_each(result.envelope,'$.rows')),"
        "(SELECT count(*) FROM json_each(result.envelope,'$.summary.profile.operators') AS op "
        "WHERE json_extract(op.value,'$.operator')='IndexSeek' "
        "AND json_extract(op.value,'$.dbHits')>0) FROM result", profiled);
    require(db, sqlite3_step(stmt) == SQLITE_ROW, "actual PROFILE result");
    require(db, sqlite3_column_int64(stmt, 0) == count, "PROFILE complete row count");
    require(db, (sqlite3_column_int64(stmt, 1) > 0) == indexed, "real Index/fallback producer");
    require(db, sqlite3_step(stmt) == SQLITE_DONE, "consume PROFILE");
    require(db, sqlite3_finalize(stmt) == SQLITE_OK, "finalize PROFILE");
    sqlite3_free(profiled);
}

static char *catalog(sqlite3 *db) {
    return text(db, "SELECT coalesce(group_concat(v,'|'),'') FROM "
        "(SELECT type||':'||name||':'||coalesce(sql,'') AS v "
        "FROM temp.sqlite_master ORDER BY type,name)", NULL);
}

static int temp_write(int action, const char *database) {
    if (action == SQLITE_CREATE_TEMP_TABLE || action == SQLITE_CREATE_TEMP_INDEX) return 1;
    if (database == NULL || strcmp(database, "temp") != 0) return 0;
    return action == SQLITE_INSERT || action == SQLITE_UPDATE || action == SQLITE_DELETE;
}

static int authorize(void *context, int action, const char *name,
                     const char *argument, const char *database, const char *trigger) {
    (void)argument;
    (void)trigger;
    Policy *policy = (Policy *)context;
    if (policy->main_read) {
        if (action != SQLITE_READ) return SQLITE_OK;
        if (database == NULL || strcmp(database, "main") != 0) return SQLITE_OK;
        if (name == NULL || strcmp(name, "_lithograph_commits") != 0) return SQLITE_OK;
    } else if (!temp_write(action, database)) {
        return SQLITE_OK;
    }
    policy->blocked++;
    return policy->response;
}

static sqlite3 *fixture(const char *extension) {
    sqlite3 *db = NULL;
    require(db, sqlite3_open(":memory:", &db) == SQLITE_OK, "open private fixture");
    require(db, sqlite3_enable_load_extension(db, 1) == SQLITE_OK, "enable load");
    require(db, sqlite3_load_extension(db, extension, "sqlite3_lithograph_init", NULL) == SQLITE_OK,
            "load extension");
    require(db, sqlite3_enable_load_extension(db, 0) == SQLITE_OK, "disable load");
    require(db, sqlite3_exec(db, "SELECT lithograph_init()", NULL, NULL, NULL) == SQLITE_OK, "init");
    cypher(db, "UNWIND range(0,15) AS i CREATE (:Item {v:1})-[:LINK {v:1}]->(:Endpoint)");
    cypher(db, "CREATE CONSTRAINT nv FOR(n:Item) REQUIRE n.v IS :: INTEGER");
    cypher(db, "CREATE CONSTRAINT rv FOR()-[r:LINK]-() REQUIRE r.v IS :: INTEGER");
    cypher(db, "CREATE RANGE INDEX nv_idx FOR(n:Item) ON(n.v)");
    cypher(db, "CREATE RANGE INDEX rv_idx FOR()-[r:LINK]-() ON(r.v)");
    return db;
}

static void warm(sqlite3 *db) {
    for (unsigned kind = 0; kind < 2; kind++) {
        char *q = query(kind, 1, 0);
        plan(db, q, 1);
        both(db, q, 1, 16);
        plan(db, q, 1);
        profile(db, q, 16, 1);
        sqlite3_free(q);
    }
}

static void main_read_error(sqlite3 *db, const char *q, int row_adapter) {
    const char *sql = row_adapter ? "SELECT event,data FROM lithograph_rows(?1)" : "SELECT lithograph(?1)";
    sqlite3_stmt *stmt = NULL;
    int rc = sqlite3_prepare_v2(db, sql, -1, &stmt, NULL);
    if (rc == SQLITE_OK) {
        require(db, sqlite3_bind_text(stmt, 1, q, -1, SQLITE_STATIC) == SQLITE_OK, "bind denied query");
        rc = sqlite3_step(stmt);
    }
    require(db, rc != SQLITE_ROW && rc != SQLITE_DONE, "real main READ denial remains error");
    require(db, (rc & 255) == SQLITE_AUTH || strstr(sqlite3_errmsg(db), "LITHOGRAPH_ACCESS_DENIED") != NULL,
            "main AUTH category preserved");
    (void)sqlite3_finalize(stmt);
}

static void denied_main(sqlite3 *db) {
    Policy policy = {SQLITE_DENY, 1, 0};
    require(db, sqlite3_set_authorizer(db, authorize, &policy) == SQLITE_OK, "deny main READ");
    for (unsigned kind = 0; kind < 2; kind++) {
        char *q = query(kind, 2, 0);
        main_read_error(db, q, 0);
        main_read_error(db, q, 1);
        sqlite3_free(q);
    }
    require(db, policy.blocked > 0, "main authorizer really ran");
    require(db, sqlite3_set_authorizer(db, NULL, NULL) == SQLITE_OK, "clear main authorizer");
}

static void run_case(const char *extension, const char *name, int response, int warmed) {
    sqlite3 *db = fixture(extension);
    if (warmed) warm(db);
    cypher(db, "CREATE (:Item {v:2})-[:LINK {v:2}]->(:Endpoint)");
    char *before = catalog(db);
    int changes = sqlite3_total_changes(db);
    Policy policy = {response, 0, 0};
    require(db, sqlite3_set_authorizer(db, authorize, &policy) == SQLITE_OK, "install host TEMP policy");
    printf("{\"case\":\"%s\",\"sqlite\":\"%s\",\"stage\":\"start\"}\n", name, sqlite3_libversion());
    fflush(stdout);
    for (unsigned kind = 0; kind < 2; kind++) {
        char *q = query(kind, 2, 0);
        char *oracle = query(kind, 2, 1);
        both(db, oracle, 2, 1);
        both(db, q, 2, 1);
        plan(db, q, 0);
        profile(db, q, 1, 0);
        sqlite3_free(q);
        sqlite3_free(oracle);
    }
    char *after = catalog(db);
    require(db, strcmp(before, after) == 0, "prepare-only probe preserves all init/warm TEMP catalog");
    require(db, sqlite3_total_changes(db) == changes, "TEMP-disabled reads perform no SQL writes");
    require(db, policy.blocked > 0, "host TEMP policy observed capability attempts");
    sqlite3_free(before);
    sqlite3_free(after);
    require(db, sqlite3_set_authorizer(db, NULL, NULL) == SQLITE_OK, "restore authorizer");
    denied_main(db);
    char *q = query(0, 2, 0);
    both(db, q, 2, 1);
    sqlite3_free(q);
    require(db, sqlite3_close(db) == SQLITE_OK, "close all cursors/leases");
    printf("{\"case\":\"%s\",\"stage\":\"passed\"}\n", name);
}

int main(int argc, char **argv) {
    if (argc != 2 && argc != 3) return 2;
    if (argc == 3) {
        if (strcmp(argv[2], "--warm-ignore") != 0) return 2;
        run_case(argv[1], "warm-new-commit-ignore", SQLITE_IGNORE, 1);
    } else {
        run_case(argv[1], "cold-deny", SQLITE_DENY, 0);
        run_case(argv[1], "cold-ignore", SQLITE_IGNORE, 0);
        run_case(argv[1], "warm-new-commit-ignore", SQLITE_IGNORE, 1);
    }
    printf("{\"stage\":\"S22.3\",\"sqlite\":\"%s\",\"temp_capability_checks\":%u}\n",
           sqlite3_libversion(), checks);
    return 0;
}
