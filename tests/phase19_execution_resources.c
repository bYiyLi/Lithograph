#define _POSIX_C_SOURCE 200809L
#define _DARWIN_C_SOURCE
#include <sqlite3.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#ifdef _WIN32
#include <windows.h>
#else
#include <pthread.h>
#include <sys/resource.h>
#endif

static const char *small_budget = "{\"memoryLimitBytes\":16777216}";
static const char *stream_budget = "{\"memoryLimitBytes\":4194304}";
static const char *large_budget = "{\"memoryLimitBytes\":268435456}";
static const char *large_expression = "RETURN size(range(1,500000)) AS value";
static const char *million_rows =
    "UNWIND range(1,1000) AS a UNWIND range(1,1000) AS b RETURN a*1000+b AS value";
static const char *long_expression =
    "RETURN reduce(total=0,i IN range(1,1000)|"
    "total+reduce(inner=0,j IN range(1,20000)|inner+j)) AS value";

static void require(sqlite3 *db, int condition, const char *message) {
    if (!condition) {
        fprintf(stderr, "Phase 19: %s: %s\n", message,
                db == NULL ? "" : sqlite3_errmsg(db));
        exit(1);
    }
}

static uint64_t now_ns(void) {
#ifdef _WIN32
    LARGE_INTEGER frequency, counter;
    QueryPerformanceFrequency(&frequency);
    QueryPerformanceCounter(&counter);
    return (uint64_t)((double)counter.QuadPart * 1000000000.0 / frequency.QuadPart);
#else
    struct timespec value;
    if (clock_gettime(CLOCK_MONOTONIC, &value) != 0) {
        return 0;
    }
    return (uint64_t)value.tv_sec * 1000000000ULL + (uint64_t)value.tv_nsec;
#endif
}

static void execute(sqlite3 *db, const char *sql) {
    require(db, sqlite3_exec(db, sql, NULL, NULL, NULL) == SQLITE_OK, sql);
}

static sqlite3_int64 integer(sqlite3 *db, const char *sql) {
    sqlite3_stmt *statement = NULL;
    require(db, sqlite3_prepare_v2(db, sql, -1, &statement, NULL) == SQLITE_OK, sql);
    require(db, sqlite3_step(statement) == SQLITE_ROW, "read integer");
    sqlite3_int64 value = sqlite3_column_int64(statement, 0);
    require(db, sqlite3_finalize(statement) == SQLITE_OK, "finalize integer");
    return value;
}

static sqlite3_stmt *query(sqlite3 *db, const char *cypher, const char *options,
                           int rows) {
    sqlite3_stmt *statement = NULL;
    const char *sql = rows ? "SELECT event,data FROM lithograph_rows(?1,'{}',?2)"
                           : "SELECT lithograph(?1,'{}',?2)";
    require(db, sqlite3_prepare_v2(db, sql, -1, &statement, NULL) == SQLITE_OK,
            "prepare execution");
    require(db, sqlite3_bind_text(statement, 1, cypher, -1, SQLITE_TRANSIENT) == SQLITE_OK,
            "bind Cypher");
    require(db, sqlite3_bind_text(statement, 2, options, -1, SQLITE_TRANSIENT) == SQLITE_OK,
            "bind options");
    return statement;
}

static sqlite3_int64 result_integer(sqlite3 *db, const char *cypher,
                                     const char *options, int rows) {
    sqlite3_stmt *statement = query(db, cypher, options, rows);
    const char *path = rows ? "$[0]" : "$.rows[0][0]";
    if (rows) {
        require(db, sqlite3_step(statement) == SQLITE_ROW, "read columns event");
    }
    require(db, sqlite3_step(statement) == SQLITE_ROW, "read execution result");
    const char *json = (const char *)sqlite3_column_text(statement, rows ? 1 : 0);
    char *sql = sqlite3_mprintf("SELECT json_extract('%q','%q')", json, path);
    require(db, sql != NULL, "allocate result extraction");
    sqlite3_int64 value = integer(db, sql);
    sqlite3_free(sql);
    while (sqlite3_step(statement) == SQLITE_ROW) {
    }
    require(db, sqlite3_finalize(statement) == SQLITE_OK, "finalize successful execution");
    return value;
}

static void execution_error(sqlite3 *db, const char *cypher, const char *options,
                            int rows, int expected, const char *message) {
    sqlite3_stmt *statement = query(db, cypher, options, rows);
    int code;
    while ((code = sqlite3_step(statement)) == SQLITE_ROW) {
        if (rows) {
            const char *event = (const char *)sqlite3_column_text(statement, 0);
            require(db, event != NULL && strcmp(event, "summary") != 0,
                    "failed execution emitted successful summary");
        }
    }
    if (code != expected) {
        fprintf(stderr, "expected code %d, got %d; query: %s; options: %s\n",
                expected, code, cypher, options);
    }
    require(db, code == expected, message);
    if (expected == SQLITE_ERROR) {
        require(db, strstr(sqlite3_errmsg(db), "INVALID_ARGUMENT") != NULL,
                "invalid option lost stable error category");
    } else if (expected == SQLITE_TOOBIG) {
        require(db, strstr(sqlite3_errmsg(db), "RESOURCE_ERROR") != NULL,
                "budget refusal lost stable error category");
    }
    require(db, sqlite3_finalize(statement) == expected, "finalize failed execution");
}

static sqlite3 *open_database(const char *extension) {
    sqlite3 *db = NULL;
    char *error = NULL;
    require(db, sqlite3_open(":memory:", &db) == SQLITE_OK, "open synthetic database");
    require(db, sqlite3_enable_load_extension(db, 1) == SQLITE_OK, "enable extension");
    int code = sqlite3_load_extension(db, extension, "sqlite3_lithograph_init", &error);
    if (code != SQLITE_OK) {
        fprintf(stderr, "extension load: %s\n", error == NULL ? "unknown" : error);
    }
    sqlite3_free(error);
    require(db, code == SQLITE_OK, "load extension");
    execute(db, "SELECT lithograph_init()");
    execute(db, "SELECT lithograph('RETURN 1')");
    return db;
}

static void names(sqlite3 *db) {
    execute(db, "SELECT lithograph('CREATE (a:人物:Person {id:1}),"
                "(b:人物:Person {id:2}), (a)-[:认识]->(b) FINISH')");
    const char *queries[] = {
        "MATCH (n:`人物`) RETURN count(n)",
        "MATCH (n:`Person`) RETURN count(n)",
        "MATCH (n) WHERE n:`人物` RETURN count(n)",
        "MATCH ()-[r:`认识`]->() RETURN count(r)"
    };
    for (size_t i = 0; i < sizeof(queries) / sizeof(queries[0]); ++i) {
        sqlite3_int64 expected = i == 3 ? 1 : 2;
        require(db, result_integer(db, queries[i], "{}", 0) == expected,
                "quoted name scalar result differs");
        require(db, result_integer(db, queries[i], "{}", 1) == expected,
                "quoted name stream result differs");
    }
    execute(db, "SELECT lithograph('CREATE (:`a``b`) FINISH')");
    require(db, result_integer(db, "MATCH (n:`a``b`) RETURN count(n)", "{}", 1) == 1,
            "escaped backtick name differs");
}

static void memory(sqlite3 *db) {
    const char *invalid[] = {"null", "0", "-1", "1.5", "true", "\"16\"",
                             "9223372036854775808", "18446744073709551616"};
    for (size_t i = 0; i < sizeof(invalid) / sizeof(invalid[0]); ++i) {
        char option[128];
        snprintf(option, sizeof(option), "{\"memoryLimitBytes\":%s}", invalid[i]);
        execution_error(db, "RETURN 1", option, 0, SQLITE_ERROR, "invalid budget accepted");
        execution_error(db, "RETURN 1", option, 1, SQLITE_ERROR, "invalid stream budget accepted");
    }
    for (int rows = 0; rows <= 1; ++rows) {
        execution_error(db, large_expression, small_budget, rows, SQLITE_TOOBIG,
                        "large expression ignored execution budget");
        require(db, result_integer(db, large_expression, large_budget, rows) == 500000,
                "large expression failed with sufficient budget");
    }
    const char *intermediates[] = {
        "RETURN [i IN range(1,1000)|range(1,1000)] AS value",
        "RETURN size(reduce(s='x',i IN range(1,24)|s+s)) AS value",
        "WITH range(1,100000) AS value RETURN value AS a,value AS b,value AS c,value AS d"
    };
    for (size_t i = 0; i < sizeof(intermediates) / sizeof(intermediates[0]); ++i) {
        execution_error(db, intermediates[i], small_budget, 0, SQLITE_TOOBIG,
                        "intermediate value or encoding escaped budget");
    }
    require(db, result_integer(db, "RETURN size(range(1,10))", "{}", 0) == 10,
            "omitted budget changed existing behavior");
    require(db, result_integer(db, "RETURN 1", "{\"memoryLimitBytes\":9223372036854775807}", 0) == 1,
            "maximum positive budget rejected");
}

static void rollback(sqlite3 *db) {
    sqlite3_int64 commits = integer(db, "SELECT count(*) FROM _lithograph_commits");
    const char *write = "CREATE (:BudgetRollback) RETURN size(range(1,500000))";
    execution_error(db, write, small_budget, 0, SQLITE_TOOBIG, "write exceeded budget");
    require(db, integer(db, "SELECT count(*) FROM _lithograph_commits") == commits,
            "failed execution published commit");
    execute(db, "SELECT lithograph_tx_begin('{}')");
    execute(db, "SELECT lithograph('CREATE (:BudgetRollback) FINISH')");
    execution_error(db, large_expression, small_budget, 1, SQLITE_TOOBIG,
                    "explicit transaction execution exceeded budget");
    require(db, sqlite3_get_autocommit(db) != 0, "resource error left explicit transaction active");
    require(db, integer(db, "SELECT count(*) FROM _lithograph_commits") == commits,
            "failed explicit transaction published commit");
    require(db, result_integer(db, "MATCH (n:BudgetRollback) RETURN count(n)", "{}", 0) == 0,
            "failed execution retained graph writes");
    require(db, integer(db, "SELECT json_extract(lithograph_integrity_check(),'$.ok')") == 1,
            "rollback damaged canonical history");
}

static void batches(sqlite3 *db) {
    const char *cypher =
        "UNWIND [1,2] AS i CALL (i) { CREATE (:DurableBudget {i:i}) "
        "RETURN CASE WHEN i=1 THEN 1 ELSE size(range(1,500000)) END AS value "
        "} IN TRANSACTIONS OF 1 ROWS RETURN value";
    sqlite3_int64 commits = integer(db, "SELECT count(*) FROM _lithograph_commits");
    execution_error(db, cypher, small_budget, 1, SQLITE_TOOBIG,
                    "transaction batch ignored budget");
    require(db, integer(db, "SELECT count(*) FROM _lithograph_commits") == commits + 1,
            "failed batch discarded durable history or published failed batch");
    require(db, result_integer(db, "MATCH (n:DurableBudget) RETURN count(n)", "{}", 0) == 1,
            "budget failure did not preserve exactly the durable batch");
}

static void stream(sqlite3 *db) {
    sqlite3_stmt *statement = query(db, million_rows, stream_budget, 1);
    sqlite3_int64 count = 0;
    int summaries = 0, code;
    while ((code = sqlite3_step(statement)) == SQLITE_ROW) {
        const char *event = (const char *)sqlite3_column_text(statement, 0);
        require(db, event != NULL, "NULL stream event");
        count += strcmp(event, "row") == 0;
        summaries += strcmp(event, "summary") == 0;
    }
    require(db, code == SQLITE_DONE, "fixed-budget million-row stream failed");
    require(db, count == 1000000 && summaries == 1, "million-row stream count or summary differs");
    require(db, sqlite3_finalize(statement) == SQLITE_OK, "finalize million-row stream");
    execution_error(db, million_rows, stream_budget, 0, SQLITE_TOOBIG,
                    "scalar collector ignored budget");
}

static sqlite3_stmt *sort_cursor(sqlite3 *db) {
    char payload[8193];
    memset(payload, 'x', sizeof(payload) - 1);
    payload[sizeof(payload) - 1] = '\0';
    char *cypher = sqlite3_mprintf(
        "UNWIND range(1,4000) AS i RETURN i,'%q' AS payload ORDER BY i DESC", payload);
    require(db, cypher != NULL, "allocate sort probe");
    sqlite3_stmt *statement = query(db, cypher, stream_budget, 1);
    sqlite3_free(cypher);
    return statement;
}

static void spill(sqlite3 *db) {
    sqlite3_stmt *statement = sort_cursor(db);
    sqlite3_stmt *extract = NULL;
    require(db, sqlite3_prepare_v2(db,
            "SELECT json_extract(?1,'$[0]'),length(json_extract(?1,'$[1]'))",
            -1, &extract, NULL) == SQLITE_OK, "prepare sorted row verification");
    int expected = 4000, summaries = 0, code;
    while ((code = sqlite3_step(statement)) == SQLITE_ROW) {
        const char *event = (const char *)sqlite3_column_text(statement, 0);
        require(db, event != NULL, "NULL sorted event");
        if (strcmp(event, "row") == 0) {
            sqlite3_bind_text(extract, 1, (const char *)sqlite3_column_text(statement, 1),
                              -1, SQLITE_TRANSIENT);
            require(db, sqlite3_step(extract) == SQLITE_ROW, "decode sorted row");
            require(db, sqlite3_column_int(extract, 0) == expected-- &&
                        sqlite3_column_int(extract, 1) == 8192,
                    "sort changed ordering or row payload");
            require(db, sqlite3_reset(extract) == SQLITE_OK, "reset sorted verifier");
        }
        summaries += strcmp(event, "summary") == 0;
    }
    require(db, code == SQLITE_DONE && expected == 0 && summaries == 1,
            "byte-budget sort did not drain all rows");
    require(db, sqlite3_finalize(extract) == SQLITE_OK, "finalize sorted verifier");
    require(db, sqlite3_finalize(statement) == SQLITE_OK, "finalize sorted stream");
    statement = sort_cursor(db);
    require(db, sqlite3_step(statement) == SQLITE_ROW, "read early-close sort columns");
    require(db, sqlite3_step(statement) == SQLITE_ROW, "read early-close sort row");
    require(db, sqlite3_finalize(statement) == SQLITE_OK, "close unfinished spill");
    require(db, result_integer(db, "RETURN 42", stream_budget, 0) == 42,
            "spill early-close retained resource state");
}

static void lifecycle(sqlite3 *db, const char *extension) {
    sqlite3_stmt *first = query(db, million_rows, stream_budget, 1);
    sqlite3_stmt *second = query(db, "RETURN 42", small_budget, 1);
    require(db, sqlite3_step(first) == SQLITE_ROW, "start first cursor");
    require(db, sqlite3_step(first) == SQLITE_ROW, "read first cursor");
    require(db, sqlite3_step(second) == SQLITE_ROW, "start independent cursor");
    require(db, sqlite3_step(second) == SQLITE_ROW, "read independent cursor");
    require(db, sqlite3_finalize(first) == SQLITE_OK, "close unfinished cursor");
    require(db, sqlite3_finalize(second) == SQLITE_OK, "close second cursor");
    sqlite3 *other = open_database(extension);
    require(other, result_integer(other, "RETURN 42", small_budget, 0) == 42,
            "another connection inherited budget");
    require(other, sqlite3_close(other) == SQLITE_OK, "close independent database");
    require(db, result_integer(db, "RETURN 42", small_budget, 0) == 42,
            "early close retained resource state");
}

typedef struct { sqlite3 *db; uint64_t issued_ns; } cancellation;

#ifdef _WIN32
static DWORD WINAPI interrupt_later(LPVOID data) {
    cancellation *state = (cancellation *)data;
    Sleep(50);
#else
static void *interrupt_later(void *data) {
    cancellation *state = (cancellation *)data;
    struct timespec delay = {.tv_sec = 0, .tv_nsec = 50000000};
    while (nanosleep(&delay, &delay) != 0) {
    }
#endif
    state->issued_ns = now_ns();
    sqlite3_interrupt(state->db);
    return 0;
}

static double cancel_execution(sqlite3 *db, const char *cypher, int rows) {
    sqlite3_stmt *statement = query(db, cypher, "{}", rows);
    if (rows) {
        require(db, sqlite3_step(statement) == SQLITE_ROW, "read cancellation columns");
    }
    cancellation state = {.db = db, .issued_ns = 0};
#ifdef _WIN32
    HANDLE worker = CreateThread(NULL, 0, interrupt_later, &state, 0, NULL);
    require(db, worker != NULL, "start interrupt thread");
#else
    pthread_t worker;
    require(db, pthread_create(&worker, NULL, interrupt_later, &state) == 0,
            "start interrupt thread");
#endif
    int code = sqlite3_step(statement);
    uint64_t finished_ns = now_ns();
#ifdef _WIN32
    require(db, WaitForSingleObject(worker, INFINITE) == WAIT_OBJECT_0, "join interrupt thread");
    CloseHandle(worker);
#else
    require(db, pthread_join(worker, NULL) == 0, "join interrupt thread");
#endif
    require(db, code == SQLITE_INTERRUPT, "long expression did not return SQLITE_INTERRUPT");
    require(db, sqlite3_finalize(statement) == SQLITE_INTERRUPT, "finalize interrupted cursor");
    double delay_ms = (double)(finished_ns - state.issued_ns) / 1000000.0;
    printf("{\"case\":\"cancel\",\"interruptDelayMs\":%.3f,\"code\":%d}\n", delay_ms, code);
    const char *limit = getenv("LITHOGRAPH_PHASE19_MAX_CANCEL_MS");
    double maximum_ms = limit == NULL ? 500.0 : strtod(limit, NULL);
    require(db, maximum_ms > 0.0 && delay_ms <= maximum_ms,
            "interrupt latency exceeded local acceptance target");
    require(db, result_integer(db, "RETURN 42", "{}", 0) == 42,
            "interrupted connection did not recover");
    return delay_ms;
}

static void blob_interrupt_callback(sqlite3_context *context, int argc, sqlite3_value **argv) {
    (void)argc;
    (void)argv;
    sqlite3 *db = sqlite3_user_data(context);
    execute(db, "SAVEPOINT phase19_blob; INSERT INTO phase19_interrupt_host VALUES(1);"
                "CREATE TEMP TABLE phase19_expire_statements(value)");
    sqlite3_interrupt(db);
    sqlite3_blob *blob = NULL;
    int code = sqlite3_blob_open(db, "temp", "phase19_empty_blob_guard", "payload", 0, 1, &blob);
    require(db, code == SQLITE_INTERRUPT && blob == NULL && sqlite3_get_autocommit(db),
            "public writable blob interrupt did not abort the main transaction");
    sqlite3_result_error_code(context, SQLITE_INTERRUPT);
}

static void interrupted_blob_boundary(sqlite3 *db) {
    execute(db, "CREATE TABLE phase19_interrupt_host(value);"
                "CREATE TEMP TABLE phase19_empty_blob_guard(payload BLOB)");
    require(db, sqlite3_create_function_v2(db, "phase19_blob_interrupt", 0, SQLITE_UTF8,
            db, blob_interrupt_callback, NULL, NULL, NULL) == SQLITE_OK,
            "register public-API interrupt boundary probe");
    sqlite3_stmt *statement = NULL;
    require(db, sqlite3_prepare_v2(db, "SELECT phase19_blob_interrupt()", -1,
            &statement, NULL) == SQLITE_OK, "prepare interrupt boundary probe");
    require(db, sqlite3_step(statement) == SQLITE_INTERRUPT, "boundary lost interrupt code");
    require(db, sqlite3_finalize(statement) == SQLITE_INTERRUPT, "finalize boundary probe");
    require(db, sqlite3_get_autocommit(db) &&
                integer(db, "SELECT count(*) FROM phase19_interrupt_host") == 0,
            "interrupted blob VM retained main writes");
}

static void cancel_explicit_parameter_decode(sqlite3 *db) {
    const size_t size = 128U * 1024U * 1024U;
    const char *prefix = "{\"value\":\"";
    char *parameters = malloc(size + 32);
    require(db, parameters != NULL, "allocate synthetic parameter input");
    memcpy(parameters, prefix, strlen(prefix));
    memset(parameters + strlen(prefix), 'x', size);
    memcpy(parameters + strlen(prefix) + size, "\"}", 3);
    execute(db, "SELECT lithograph_tx_begin('{}')");
    execute(db, "SELECT lithograph('CREATE (:CancelParameterRollback) FINISH')");
    sqlite3_stmt *statement = NULL;
    require(db, sqlite3_prepare_v2(db, "SELECT lithograph('RETURN size($value)',?1,'{}')",
            -1, &statement, NULL) == SQLITE_OK, "prepare parameter cancellation");
    require(db, sqlite3_bind_text64(statement, 1, parameters, strlen(prefix) + size + 2,
            SQLITE_STATIC, SQLITE_UTF8) == SQLITE_OK, "bind synthetic parameter input");
    cancellation state = {.db = db, .issued_ns = 0};
#ifdef _WIN32
    HANDLE worker = CreateThread(NULL, 0, interrupt_later, &state, 0, NULL);
    require(db, worker != NULL, "start parameter interrupt thread");
#else
    pthread_t worker;
    require(db, pthread_create(&worker, NULL, interrupt_later, &state) == 0,
            "start parameter interrupt thread");
#endif
    int code = sqlite3_step(statement);
#ifdef _WIN32
    require(db, WaitForSingleObject(worker, INFINITE) == WAIT_OBJECT_0, "join parameter interrupt");
    CloseHandle(worker);
#else
    require(db, pthread_join(worker, NULL) == 0, "join parameter interrupt");
#endif
    require(db, code == SQLITE_INTERRUPT && sqlite3_get_autocommit(db),
            "prepare/parameter interrupt did not abort explicit transaction");
    require(db, sqlite3_finalize(statement) == SQLITE_INTERRUPT, "finalize parameter cancellation");
    free(parameters);
    require(db, result_integer(db, "MATCH (n:CancelParameterRollback) RETURN count(n)", "{}", 0) == 0,
            "parameter cancellation retained staged writes");
}

static void guard_storage_error(sqlite3 *db, const char *sql) {
    sqlite3_stmt *statement = NULL;
    require(db, sqlite3_prepare_v2(db, sql, -1, &statement, NULL) == SQLITE_OK,
            "prepare guard collision refusal");
    require(db, sqlite3_step(statement) == SQLITE_ERROR &&
                strstr(sqlite3_errmsg(db), "LITHOGRAPH_STORAGE_ERROR") != NULL,
            "incompatible interrupt guard was accepted or lost its error category");
    require(db, sqlite3_finalize(statement) == SQLITE_ERROR,
            "finalize guard collision refusal");
}

static void guard_collisions(sqlite3 *db) {
    const char *definitions[] = {
        "CREATE TEMP VIEW _lithograph_interrupt_guard AS SELECT x'00' AS payload",
        "CREATE TEMP TABLE _lithograph_interrupt_guard(host_data TEXT)",
        "CREATE TEMP TABLE _lithograph_interrupt_guard(id INTEGER PRIMARY KEY,payload BLOB) WITHOUT ROWID",
        "CREATE TEMP TABLE _lithograph_interrupt_guard(payload BLOB UNIQUE)"
    };
    execute(db, "CREATE TABLE phase19_guard_host(value)");
    for (size_t index = 0; index < sizeof(definitions) / sizeof(definitions[0]); ++index) {
        execute(db, "DROP TABLE temp._lithograph_interrupt_guard");
        execute(db, definitions[index]);
        sqlite3_int64 commits = integer(db, "SELECT count(*) FROM main._lithograph_commits");
        execute(db, "BEGIN; INSERT INTO phase19_guard_host VALUES(7)");
        guard_storage_error(db, "SELECT lithograph('CREATE (:GuardCollision) FINISH')");
        guard_storage_error(db,
            "SELECT event,data FROM lithograph_rows('CREATE (:GuardCollision) FINISH')");
        require(db, !sqlite3_get_autocommit(db) &&
                    integer(db, "SELECT count(*) FROM phase19_guard_host") == 1 &&
                    integer(db, "SELECT count(*) FROM main._lithograph_commits") == commits,
                "guard refusal changed caller transaction or created graph history");
        require(db, result_integer(db, "RETURN 42", small_budget, 0) == 42 &&
                    result_integer(db, "RETURN 43", small_budget, 1) == 43,
                "guard collision blocked or quarantined healthy read execution");
        execution_error(db, "RETURN 1", "{", 0, SQLITE_ERROR,
                        "guard collision changed scalar option validation priority");
        execution_error(db, "RETURN 1", "{", 1, SQLITE_ERROR,
                        "guard collision changed stream option validation priority");
        char *error = NULL;
        int code = sqlite3_exec(db, "SELECT lithograph('RETURN 1','{')", NULL, NULL, &error);
        require(db, code == SQLITE_ERROR && error != NULL &&
                    strstr(error, "INVALID_ARGUMENT") != NULL,
                "guard collision changed parameter validation priority");
        sqlite3_free(error);
        execute(db, "ROLLBACK");
        guard_storage_error(db, "SELECT lithograph_tx_begin('{}')");
        require(db, sqlite3_get_autocommit(db) &&
                    result_integer(db, "MATCH (n:GuardCollision) RETURN count(n)", "{}", 0) == 0,
                "guard refusal began an explicit transaction or retained graph writes");
        require(db, integer(db,
                    "SELECT count(*) FROM temp.sqlite_schema WHERE name='_lithograph_interrupt_guard'") == 1,
                "guard refusal removed the host TEMP object");
        execute(db, index == 0 ? "DROP VIEW temp._lithograph_interrupt_guard"
                               : "DROP TABLE temp._lithograph_interrupt_guard");
        execute(db, "CREATE TEMP TABLE _lithograph_interrupt_guard(payload BLOB)");
    }
}

static void cancel(sqlite3 *db) {
    guard_collisions(db);
    interrupted_blob_boundary(db);
    cancel_execution(db, long_expression, 1);
    const char *write =
        "CREATE (:CancelRollback) RETURN reduce(total=0,i IN range(1,1000)|"
        "total+reduce(inner=0,j IN range(1,20000)|inner+j)) AS value";
    sqlite3_int64 commits = integer(db, "SELECT count(*) FROM _lithograph_commits");
    cancel_execution(db, write, 0);
    cancel_execution(db, write, 1);
    require(db, integer(db, "SELECT count(*) FROM _lithograph_commits") == commits,
            "interrupted write published commit");
    execute(db, "SELECT lithograph_tx_begin('{}')");
    execute(db, "SELECT lithograph('CREATE (:CancelRollback) FINISH')");
    cancel_execution(db, long_expression, 1);
    require(db, sqlite3_get_autocommit(db) != 0, "interrupt left explicit transaction active");
    char *commit_error = NULL;
    int commit_code = sqlite3_exec(db, "SELECT lithograph_tx_commit()", NULL, NULL, &commit_error);
    require(db, commit_code == SQLITE_MISUSE && commit_error != NULL &&
            strstr(commit_error, "INVALID_ARGUMENT") != NULL,
            "interrupted explicit transaction remained committable");
    sqlite3_free(commit_error);
    require(db, integer(db, "SELECT count(*) FROM _lithograph_commits") == commits,
            "interrupted explicit transaction published commit");
    require(db, result_integer(db, "MATCH (n:CancelRollback) RETURN count(n)", "{}", 0) == 0,
            "interrupted execution retained graph writes");
    execute(db, "BEGIN; INSERT INTO phase19_interrupt_host VALUES(2)");
    cancel_execution(db, write, 1);
    require(db, sqlite3_get_autocommit(db) &&
                integer(db, "SELECT count(*) FROM phase19_interrupt_host") == 0,
            "write interrupt did not preserve SQLite whole-transaction abort semantics");
    cancel_explicit_parameter_decode(db);
}

static sqlite3_int64 process_peak(void) {
    sqlite3_int64 rss = 0;
#ifndef _WIN32
    struct rusage usage;
    if (getrusage(RUSAGE_SELF, &usage) == 0) {
        rss = usage.ru_maxrss;
#ifndef __APPLE__
        rss *= 1024;
#endif
    }
#endif
    return rss;
}

static void metrics(sqlite3 *db, const char *selected, sqlite3_int64 baseline_rss) {
    sqlite3_int64 rss = process_peak();
    sqlite3_int64 temp_bytes = integer(db, "PRAGMA temp.page_count") *
                               integer(db, "PRAGMA temp.page_size");
    printf("{\"case\":\"%s\",\"sqlite\":\"%s\",\"processPeakBytes\":%lld,"
           "\"processBaselinePeakBytes\":%lld,\"incrementalPeakBytes\":%lld,"
           "\"sqlitePeakBytes\":%lld,\"hostTempBytes\":%lld,\"status\":\"passed\"}\n",
           selected, sqlite3_libversion(), (long long)rss, (long long)baseline_rss,
           (long long)(rss > baseline_rss ? rss - baseline_rss : 0),
           (long long)sqlite3_memory_highwater(0), (long long)temp_bytes);
}

int main(int argc, char **argv) {
    require(NULL, argc == 3, "usage: phase19-execution-resources <extension> <case>");
    sqlite3 *db = open_database(argv[1]);
    const char *selected = argv[2];
    sqlite3_int64 baseline_rss = process_peak();
    if (strcmp(selected, "names") == 0) names(db);
    else if (strcmp(selected, "memory") == 0) memory(db);
    else if (strcmp(selected, "rollback") == 0) rollback(db);
    else if (strcmp(selected, "batches") == 0) batches(db);
    else if (strcmp(selected, "stream") == 0) stream(db);
    else if (strcmp(selected, "spill") == 0) spill(db);
    else if (strcmp(selected, "lifecycle") == 0) lifecycle(db, argv[1]);
    else if (strcmp(selected, "cancel") == 0) cancel(db);
    else require(db, 0, "unknown acceptance case");
    metrics(db, selected, baseline_rss);
    require(db, sqlite3_close(db) == SQLITE_OK, "close acceptance database");
    return 0;
}
