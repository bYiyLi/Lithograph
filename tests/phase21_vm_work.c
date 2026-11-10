// Measure SQLite VM work from the real host connection, including statements
// executed by lithograph(). This test never logs raw Cypher or user parameters.
#include <sqlite3.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

typedef struct {
    uint64_t statements;
    uint64_t nested;
    uint64_t outer;
    uint64_t vm_steps;
    uint64_t fullscan_steps;
    uint64_t sorts;
    int invalid;
} Counters;

static void require(sqlite3 *db, int condition, const char *why) {
    if (!condition) {
        fprintf(stderr, "Phase 21 VM: %s (%s)\n", why,
                db == NULL ? "no connection" : sqlite3_errmsg(db));
        exit(1);
    }
}

static void add_count(uint64_t *count, int value, Counters *counters) {
    if (value < 0 || *count > UINT64_MAX - (uint64_t)value) {
        counters->invalid = 1;
        return;
    }
    *count += (uint64_t)value;
}

static int trace_profile(unsigned kind, void *context, void *statement, void *details) {
    (void)details;
    if (kind != SQLITE_TRACE_PROFILE) return 0;
    Counters *counters = (Counters *)context;
    sqlite3_stmt *stmt = (sqlite3_stmt *)statement;
    add_count(&counters->vm_steps, sqlite3_stmt_status(stmt, SQLITE_STMTSTATUS_VM_STEP, 1), counters);
    add_count(&counters->fullscan_steps, sqlite3_stmt_status(stmt, SQLITE_STMTSTATUS_FULLSCAN_STEP, 1), counters);
    add_count(&counters->sorts, sqlite3_stmt_status(stmt, SQLITE_STMTSTATUS_SORT, 1), counters);
    const char *sql = sqlite3_sql(stmt);
    if (sql != NULL && strstr(sql, "json_extract(lithograph(") != NULL)
        counters->outer++;
    else
        counters->nested++;
    counters->statements++;
    return 0;
}

int main(int argc, char **argv) {
    if (argc != 3) {
        fprintf(stderr, "usage: phase21-vm-work <extension> <synthetic-database>\n");
        return 2;
    }
    sqlite3 *db = NULL;
    require(db, sqlite3_open(argv[2], &db) == SQLITE_OK, "open synthetic fixture");
    require(db, sqlite3_enable_load_extension(db, 1) == SQLITE_OK, "enable .load");
    char *load_error = NULL;
    int rc = sqlite3_load_extension(db, argv[1], "sqlite3_lithograph_init", &load_error);
    if (rc != SQLITE_OK) {
        fprintf(stderr, "Phase 21 extension load failed: %s\n",
                load_error == NULL ? "unknown" : load_error);
        sqlite3_free(load_error);
        sqlite3_close(db);
        return 1;
    }
    sqlite3_free(load_error);
    require(db, sqlite3_enable_load_extension(db, 0) == SQLITE_OK, "disable .load");
    require(db, sqlite3_exec(db, "SELECT lithograph_init()", NULL, NULL, NULL) == SQLITE_OK,
            "initialize existing fixture");

    Counters counters = {0};
    require(db, sqlite3_trace_v2(db, SQLITE_TRACE_PROFILE, trace_profile, &counters) == SQLITE_OK,
            "install SQLite VM trace");
    sqlite3_stmt *stmt = NULL;
    rc = sqlite3_prepare_v2(
        db, "SELECT json_extract(lithograph(?1,?2),'$.rows[0][0]')", -1, &stmt, NULL
    );
    require(db, rc == SQLITE_OK, "prepare Node ID query");
    require(db, sqlite3_bind_text(stmt, 1,
            "MATCH (a:Bench), (b:Bench) WHERE elementId(a)=$a AND elementId(b)=$b RETURN a.k",
            -1, SQLITE_STATIC) == SQLITE_OK, "bind query");
    require(db, sqlite3_bind_text(stmt, 2, "{\"a\":\"n:1\",\"b\":\"n:2\"}",
            -1, SQLITE_STATIC) == SQLITE_OK, "bind parameters");
    require(db, sqlite3_step(stmt) == SQLITE_ROW
            && sqlite3_column_type(stmt, 0) == SQLITE_INTEGER
            && sqlite3_column_int64(stmt, 0) == 0, "single correct Node ID result");
    require(db, sqlite3_step(stmt) == SQLITE_DONE, "consume result");
    require(db, sqlite3_finalize(stmt) == SQLITE_OK, "finalize result");
    require(db, sqlite3_trace_v2(db, 0, NULL, NULL) == SQLITE_OK, "clear trace");

    require(db, !counters.invalid && counters.outer == 1 && counters.nested > 0
            && counters.vm_steps > 0, "valid nested SQLite statement work sample");
    printf("{\"sqlite\":\"%s\",\"statements\":%llu,\"nested\":%llu,"
           "\"vm_steps\":%llu,\"fullscan_steps\":%llu,\"sorts\":%llu}\n",
           sqlite3_libversion(), (unsigned long long)counters.statements,
           (unsigned long long)counters.nested, (unsigned long long)counters.vm_steps,
           (unsigned long long)counters.fullscan_steps, (unsigned long long)counters.sorts);
    require(db, sqlite3_close(db) == SQLITE_OK, "close fixture");
    return 0;
}
