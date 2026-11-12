// Real SQLite-hosted READ work, including prepares performed inside lithograph().
// The fixture is synthetic, initialized separately, and opened read-only.
#include <sqlite3.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

typedef struct {
    uint64_t statements;
    uint64_t vm_steps;
    uint64_t key_prepares;
    uint64_t property_prepares;
    int invalid;
} Work;

static void require(sqlite3 *db, int condition, const char *why) {
    if (!condition) {
        fprintf(stderr, "Phase 22 READ: %s (%s)\n", why,
                db == NULL ? "no connection" : sqlite3_errmsg(db));
        exit(1);
    }
}

static int authorize(void *context, int action, const char *table,
                     const char *column, const char *database, const char *origin) {
    (void)database;
    (void)origin;
    Work *work = (Work *)context;
    if (action == SQLITE_READ && table != NULL && column != NULL) {
        if (strcmp(table, "_lithograph_prop_keys") == 0 && strcmp(column, "id") == 0)
            work->key_prepares++;
        if (strcmp(table, "_lithograph_cp_properties") == 0 && strcmp(column, "type_tag") == 0)
            work->property_prepares++;
    }
    return SQLITE_OK;
}

static int profile(unsigned event, void *context, void *statement, void *details) {
    (void)details;
    if (event == SQLITE_TRACE_PROFILE) {
        Work *work = (Work *)context;
        int steps = sqlite3_stmt_status((sqlite3_stmt *)statement, SQLITE_STMTSTATUS_VM_STEP, 1);
        if (steps < 0 || work->vm_steps > UINT64_MAX - (uint64_t)steps) work->invalid = 1;
        else work->vm_steps += (uint64_t)steps;
        work->statements++;
    }
    return 0;
}

int main(int argc, char **argv) {
    if (argc != 3) {
        fprintf(stderr, "usage: phase22-noindex-read-work <extension> <synthetic-db>\n");
        return 2;
    }
    sqlite3 *db = NULL;
    require(db, sqlite3_open_v2(argv[2], &db, SQLITE_OPEN_READONLY, NULL) == SQLITE_OK, "open fixture");
    require(db, sqlite3_enable_load_extension(db, 1) == SQLITE_OK, "enable .load");
    char *error = NULL;
    int status = sqlite3_load_extension(db, argv[1], "sqlite3_lithograph_init", &error);
    if (status != SQLITE_OK) {
        fprintf(stderr, "Phase 22 READ .load: %s\n", error == NULL ? "unknown" : error);
        sqlite3_free(error);
        sqlite3_close(db);
        return 1;
    }
    sqlite3_free(error);
    require(db, sqlite3_enable_load_extension(db, 0) == SQLITE_OK, "disable .load");
    Work work = {0};
    require(db, sqlite3_set_authorizer(db, authorize, &work) == SQLITE_OK, "observe prepares");
    require(db, sqlite3_trace_v2(db, SQLITE_TRACE_PROFILE, profile, &work) == SQLITE_OK, "observe VM");
    clock_t start = clock();
    sqlite3_stmt *statement = NULL;
    require(db, sqlite3_prepare_v2(db,
            "SELECT json_extract(lithograph('MATCH (n:Plain) WHERE n.v=1 RETURN n.k'),'$.rows')",
            -1, &statement, NULL) == SQLITE_OK, "prepare public SQL");
    require(db, sqlite3_step(statement) == SQLITE_ROW && sqlite3_column_type(statement, 0) == SQLITE_TEXT
            && strcmp((const char *)sqlite3_column_text(statement, 0), "[[0]]") == 0, "complete correct output");
    require(db, sqlite3_step(statement) == SQLITE_DONE, "drain public SQL");
    require(db, sqlite3_finalize(statement) == SQLITE_OK, "finalize public SQL");
    clock_t elapsed = clock() - start;
    require(db, elapsed >= 0 && !work.invalid, "valid work counters");
    require(db, sqlite3_trace_v2(db, 0, NULL, NULL) == SQLITE_OK, "clear VM observer");
    require(db, sqlite3_set_authorizer(db, NULL, NULL) == SQLITE_OK, "clear prepare observer");
    unsigned remaining = 0;
    for (statement = sqlite3_next_stmt(db, NULL); statement != NULL; statement = sqlite3_next_stmt(db, statement)) {
        require(db, !sqlite3_stmt_busy(statement), "READ leaves no busy statements");
        remaining++;
    }
    require(db, remaining <= 16, "statement cache is bounded");
    require(db, sqlite3_exec(db, "SELECT lithograph('RETURN 1')", NULL, NULL, NULL) == SQLITE_OK, "connection reusable");
    require(db, sqlite3_close(db) == SQLITE_OK, "connection closes cleanly");
    printf("{\"sqlite\":\"%s\",\"statements\":%llu,\"vm_steps\":%llu,"
           "\"key_prepares\":%llu,\"property_prepares\":%llu,\"cpu_micros\":%.0f,\"cached_idle\":%u}\n",
           sqlite3_libversion(), (unsigned long long)work.statements, (unsigned long long)work.vm_steps,
           (unsigned long long)work.key_prepares, (unsigned long long)work.property_prepares,
           (double)elapsed * 1000000.0 / CLOCKS_PER_SEC, remaining);
    return 0;
}
