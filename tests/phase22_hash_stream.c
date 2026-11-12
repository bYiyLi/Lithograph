#define _POSIX_C_SOURCE 200809L
#include <sqlite3.h>
#include <ctype.h>
#include <errno.h>
#include <inttypes.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

/* A single public rows drain. The Python owner supplies immutable provenance,
 * independent process RSS, cgroup/FD evidence and chronological repetitions. */
#define FNV_OFFSET UINT64_C(14695981039346656037)
#define FNV_PRIME UINT64_C(1099511628211)
static const char *COLUMNS = "[\"0\",\"elementId(l)\",\"elementId(r)\",\"l.leftKey\",\"r.rightKey\"]";
typedef struct { uint64_t wall, cpu; } Stamp;
typedef struct { uint64_t statements, vm, fullscan, sorts; int invalid; } Work;
typedef struct {
    const char *extension, *path, *at, *kind, *sqlite_source_id;
    uint64_t nodes, first, hot, expected, memory;
    int before, profile, trace, require_spill, interrupt, early, bilateral;
} Config;
typedef struct {
    uint64_t rows, bytes, hash, commits;
    Stamp first, full, cleanup, recovery, interrupt_terminal;
    char *summary;
    int reset_rc, interrupt_autocommit;
} Drain;

static void need(sqlite3 *db, int condition, const char *message) {
    if (!condition) {
        fprintf(stderr, "%s%s%s [sqlite_rc=%d]\n", message, db ? ": " : "",
                db ? sqlite3_errmsg(db) : "", db ? sqlite3_errcode(db) : 0);
        exit(1);
    }
}
static Stamp stamp(void) {
    struct timespec wall, cpu;
    need(NULL, clock_gettime(CLOCK_MONOTONIC, &wall) == 0, "monotonic clock failed");
    need(NULL, clock_gettime(CLOCK_PROCESS_CPUTIME_ID, &cpu) == 0, "CPU clock failed");
    return (Stamp){(uint64_t)wall.tv_sec * UINT64_C(1000000000) + (uint64_t)wall.tv_nsec,
                   (uint64_t)cpu.tv_sec * UINT64_C(1000000000) + (uint64_t)cpu.tv_nsec};
}
static Stamp elapsed(Stamp start) { Stamp end = stamp(); return (Stamp){end.wall-start.wall, end.cpu-start.cpu}; }
static double us(uint64_t value) { return (double)value / 1000.0; }
static void quote(const char *value) {
    putchar('"');
    for (const unsigned char *p = (const unsigned char *)value; *p; ++p) {
        if (*p == '"' || *p == '\\') { putchar('\\'); putchar(*p); }
        else if (*p < 32) printf("\\u%04x", (unsigned)*p);
        else putchar(*p);
    }
    putchar('"');
}
static void exec_sql(sqlite3 *db, const char *sql) {
    char *error = NULL;
    int rc = sqlite3_exec(db, sql, NULL, NULL, &error);
    if (rc != SQLITE_OK) fprintf(stderr, "SQL failure: %s\n", error ? error : "unknown");
    sqlite3_free(error);
    need(db, rc == SQLITE_OK, "SQL failed");
}
static sqlite3_stmt *prepare(sqlite3 *db, const char *sql) {
    sqlite3_stmt *statement = NULL;
    need(db, sqlite3_prepare_v2(db, sql, -1, &statement, NULL) == SQLITE_OK, "prepare failed");
    return statement;
}
static const char *text(sqlite3 *db, sqlite3_stmt *statement, int column) {
    need(db, sqlite3_column_type(statement, column) == SQLITE_TEXT, "column is not TEXT");
    const char *value = (const char *)sqlite3_column_text(statement, column);
    need(db, value != NULL, "TEXT access failed");
    need(db, strlen(value) == (size_t)sqlite3_column_bytes(statement, column), "TEXT contains NUL");
    return value;
}
static uint64_t integer(sqlite3 *db, sqlite3_stmt *statement, int column) {
    need(db, sqlite3_column_type(statement, column) == SQLITE_INTEGER, "column is not INTEGER");
    sqlite3_int64 value = sqlite3_column_int64(statement, column);
    need(db, value >= 0, "negative metadata integer");
    return (uint64_t)value;
}
static void bind_text(sqlite3 *db, sqlite3_stmt *statement, int column, const char *value) {
    need(db, sqlite3_bind_text(statement, column, value, -1, SQLITE_TRANSIENT) == SQLITE_OK, "TEXT bind failed");
}
static void finish_metadata(sqlite3 *db, sqlite3_stmt *statement) {
    need(db, sqlite3_step(statement) == SQLITE_DONE, "metadata has extra rows");
    need(db, sqlite3_finalize(statement) == SQLITE_OK, "metadata finalize failed");
}
static char *copy_text(sqlite3 *db, sqlite3_stmt *statement, int column) {
    char *result = sqlite3_mprintf("%s", text(db, statement, column));
    need(db, result != NULL, "metadata allocation failed");
    return result;
}
static uint64_t number(const char *value) {
    need(NULL, value[0] != '\0', "empty integer argument");
    for (const unsigned char *p = (const unsigned char *)value; *p; ++p)
        need(NULL, isdigit(*p), "integer argument contains non-digit");
    errno = 0; char *end = NULL; unsigned long long parsed = strtoull(value, &end, 10);
    need(NULL, errno != ERANGE && end && *end == '\0', "integer argument overflow");
    return (uint64_t)parsed;
}
static Config positional_arguments(int argc, char **argv) {
    need(NULL, argc >= 9, "usage: phase22_hash_stream EXT DB AT N FIRST HOT full|small24 MEMORY --sqlite-source-id EXPECTED [--bilateral-hot] [--before] [--profile] [--trace] [--require-spill] [--interrupt|--early-close]");
    Config config = {0};
    config.extension = argv[1]; config.path = argv[2]; config.at = argv[3];
    config.nodes = number(argv[4]); config.first = number(argv[5]); config.hot = number(argv[6]);
    config.kind = argv[7]; config.memory = number(argv[8]);
    need(NULL, config.nodes >= 1000 && config.nodes <= 1000000, "fixture size out of range");
    need(NULL, config.first > 0 && config.first <= (uint64_t)INT64_MAX-config.nodes, "Node ID range overflow");
    need(NULL, config.hot <= config.nodes && config.memory <= INT64_MAX, "hot/quota out of range");
    need(NULL, strlen(config.at) == 71 && strncmp(config.at, "commit/", 7) == 0, "AT must be full commit descriptor");
    for (const char *p = config.at+7; *p; ++p)
        need(NULL, (*p >= '0' && *p <= '9') || (*p >= 'a' && *p <= 'f'), "AT is not lowercase hex");
    need(NULL, strcmp(config.kind, "full") == 0 || strcmp(config.kind, "small24") == 0, "unknown case");
    config.expected = strcmp(config.kind, "full") == 0 ? config.nodes : 24;
    return config;
}
static int optional_argument(Config *config, int i, int argc, char **argv) {
    if (strcmp(argv[i], "--sqlite-source-id") == 0) {
        need(NULL, config->sqlite_source_id == NULL && i+1 < argc, "--sqlite-source-id requires exactly one value");
        config->sqlite_source_id = argv[++i];
        need(NULL, config->sqlite_source_id[0] != '\0', "empty expected SQLite source ID");
    } else if (strcmp(argv[i], "--before") == 0) config->before = 1;
    else if (strcmp(argv[i], "--bilateral-hot") == 0) config->bilateral = 1;
    else if (strcmp(argv[i], "--profile") == 0) config->profile = 1;
    else if (strcmp(argv[i], "--trace") == 0) config->trace = 1;
    else if (strcmp(argv[i], "--require-spill") == 0) config->require_spill = 1;
    else if (strcmp(argv[i], "--interrupt") == 0) config->interrupt = 1;
    else if (strcmp(argv[i], "--early-close") == 0) config->early = 1;
    else need(NULL, 0, "unknown flag");
    return i;
}
static void validate_arguments(Config *config) {
    need(NULL, config->sqlite_source_id != NULL, "--sqlite-source-id EXPECTED is required");
    need(NULL, !config->bilateral || (config->nodes == 200002 && config->hot == 100001), "bilateral fixture must have N=200002 and 100001 matching hot keys on both sides");
    if (config->bilateral && strcmp(config->kind, "full") == 0) config->expected = 400002;
    need(NULL, !config->before || config->nodes == 1000, "Before is restricted to the safe 1k fixture");
    need(NULL, !config->require_spill || (config->profile && !config->before), "spill assertion requires After PROFILE");
    need(NULL, !(config->early && config->interrupt), "choose only one partial lifecycle");
    need(NULL, !(config->require_spill && (config->early || config->interrupt)), "partial execution cannot prove complete spill success");
}
static Config arguments(int argc, char **argv) {
    Config config = positional_arguments(argc, argv);
    for (int i = 9; i < argc; ++i) i = optional_argument(&config, i, argc, argv);
    validate_arguments(&config);
    return config;
}
static sqlite3 *open_ro(const Config *config) {
    sqlite3 *db = NULL; char *error = NULL;
    need(db, sqlite3_open_v2(config->path, &db, SQLITE_OPEN_READONLY, NULL) == SQLITE_OK, "readonly open failed");
    need(db, sqlite3_db_readonly(db, "main") == 1, "main is not actually readonly");
    sqlite3_stmt *runtime = prepare(db, "SELECT sqlite_source_id()");
    need(db, sqlite3_step(runtime) == SQLITE_ROW, "actual connection SQLite source ID missing");
    need(db, strcmp(text(db, runtime, 0), config->sqlite_source_id) == 0, "actual connection SQLite source ID differs before extension load");
    finish_metadata(db, runtime);
    need(db, sqlite3_enable_load_extension(db, 1) == SQLITE_OK, "enable extension load failed");
    int rc = sqlite3_load_extension(db, config->extension, "sqlite3_lithograph_init", &error);
    if (rc != SQLITE_OK) fprintf(stderr, "extension load: %s\n", error ? error : "unknown");
    sqlite3_free(error);
    need(db, rc == SQLITE_OK, "extension load failed");
    need(db, sqlite3_enable_load_extension(db, 0) == SQLITE_OK, "disable extension load failed");
    exec_sql(db, "PRAGMA cache_size=-2000");
    exec_sql(db, "BEGIN DEFERRED");
    need(db, sqlite3_get_autocommit(db) == 0, "BEGIN failed");
    return db;
}
static uint64_t head(sqlite3 *db, const Config *config) {
    sqlite3_stmt *statement = prepare(db, "SELECT lower(hex(commit_id)),(SELECT count(*) FROM _lithograph_commits) FROM _lithograph_branches WHERE name='main'");
    need(db, sqlite3_step(statement) == SQLITE_ROW, "main head missing");
    need(db, strcmp(text(db, statement, 0), config->at+7) == 0, "main head differs from immutable AT");
    uint64_t commits = integer(db, statement, 1);
    finish_metadata(db, statement);
    return commits;
}
static char *environment(sqlite3 *db) {
    sqlite3_stmt *statement = prepare(db, "SELECT json_object('version',sqlite_version(),'sourceId',sqlite_source_id(),'compileOptions',json((SELECT json_group_array(compile_options) FROM pragma_compile_options)),'cacheSize',(SELECT cache_size FROM pragma_cache_size),'tempStore',(SELECT temp_store FROM pragma_temp_store),'journalMode',(SELECT journal_mode FROM pragma_journal_mode))");
    need(db, sqlite3_step(statement) == SQLITE_ROW, "runtime metadata missing");
    char *result = copy_text(db, statement, 0); finish_metadata(db, statement); return result;
}
static void checkpoint(sqlite3 *db, const Config *config) {
    sqlite3_stmt *statement = prepare(db, "SELECT json_extract(CAST(metadata AS TEXT),'$.nodes'),json_extract(CAST(metadata AS TEXT),'$.relationships') FROM _lithograph_checkpoints WHERE lower(hex(commit_id))=?1");
    bind_text(db, statement, 1, config->at+7);
    need(db, sqlite3_step(statement) == SQLITE_ROW && integer(db, statement, 0) == config->nodes && integer(db, statement, 1) == 0, "fixture CP dimensions differ");
    finish_metadata(db, statement);
}
static char *plan(sqlite3 *db, const Config *config, const char *query, const char *options) {
    sqlite3_stmt *statement = prepare(db, "SELECT json_extract(lithograph(?1,'{}',?2),'$.rows[0][0]')");
    char *explain = sqlite3_mprintf("EXPLAIN %s", query);
    need(db, explain != NULL, "EXPLAIN allocation failed");
    bind_text(db, statement, 1, explain); bind_text(db, statement, 2, options); sqlite3_free(explain);
    need(db, sqlite3_step(statement) == SQLITE_ROW, "EXPLAIN failed");
    char *result = copy_text(db, statement, 0); finish_metadata(db, statement);
    need(db, config->before || (strcmp(config->kind, "small24") == 0 && !config->require_spill) || strstr(result, "HashJoin") != NULL, "After full/required-spill case did not select HashJoin before pressure");
    return result;
}
static int trace(unsigned event, void *context, void *statement, void *detail) {
    (void)detail;
    if (event == SQLITE_TRACE_PROFILE) {
        Work *work = context;
        int vm = sqlite3_stmt_status(statement, SQLITE_STMTSTATUS_VM_STEP, 1);
        int fullscan = sqlite3_stmt_status(statement, SQLITE_STMTSTATUS_FULLSCAN_STEP, 1);
        int sorts = sqlite3_stmt_status(statement, SQLITE_STMTSTATUS_SORT, 1);
        if (vm < 0 || fullscan < 0 || sorts < 0) work->invalid = 1;
        else { work->vm += (uint64_t)vm; work->fullscan += (uint64_t)fullscan; work->sorts += (uint64_t)sorts; }
        ++work->statements;
    }
    return 0;
}
static void start_trace(sqlite3 *db, Work *work) {
    for (sqlite3_stmt *s = sqlite3_next_stmt(db, NULL); s; s = sqlite3_next_stmt(db, s)) {
        sqlite3_stmt_status(s, SQLITE_STMTSTATUS_VM_STEP, 1);
        sqlite3_stmt_status(s, SQLITE_STMTSTATUS_FULLSCAN_STEP, 1);
        sqlite3_stmt_status(s, SQLITE_STMTSTATUS_SORT, 1);
    }
    need(db, sqlite3_trace_v2(db, SQLITE_TRACE_PROFILE, trace, work) == SQLITE_OK, "trace install failed");
}
static int json_space(char value) { return value == ' ' || value == '\n' || value == '\r' || value == '\t'; }
static void token(sqlite3 *db, const char **cursor, char expected) {
    while (json_space(**cursor)) ++*cursor;
    need(db, **cursor == expected, "typed row punctuation differs"); ++*cursor;
}
static int64_t row_integer(sqlite3 *db, const char **cursor) {
    while (json_space(**cursor)) ++*cursor;
    const char *digits = *cursor + (**cursor == '-');
    need(db, !(digits[0] == '0' && isdigit((unsigned char)digits[1])), "JSON integer has a leading zero");
    need(db, **cursor == '-' || isdigit((unsigned char)**cursor), "row value is not JSON integer");
    errno = 0; char *end = NULL; long long result = strtoll(*cursor, &end, 10);
    need(db, errno != ERANGE && end != *cursor, "row integer overflow");
    *cursor = end; return (int64_t)result;
}
static int64_t row_id(sqlite3 *db, const char **cursor) {
    while (json_space(**cursor)) ++*cursor;
    need(db, strncmp(*cursor, "\"n:", 3) == 0, "row ID is not a Node element ID"); *cursor += 3;
    need(db, **cursor >= '1' && **cursor <= '9', "Node ID is not canonical positive decimal");
    int64_t result = row_integer(db, cursor);
    need(db, **cursor == '"', "Node ID string contains non-digits"); ++*cursor; return result;
}
static uint64_t oracle_slot(sqlite3 *db, const Config *config, uint64_t left, uint64_t right,
                            int64_t left_key, int64_t right_key) {
    need(db, left < config->nodes && right < config->nodes, "row ID is outside fixture Node range");
    uint64_t key, slot;
    if (!config->bilateral) {
        need(db, left < config->expected, "left ID is outside the complete oracle");
        key = left < config->hot ? 1 : left+1;
        need(db, right == key-1, "right ID differs from the offset oracle");
        slot = left;
    } else if (strcmp(config->kind, "small24") == 0) {
        need(db, (left == 0 || (left >= 2 && left <= 24)) && right == 0,
             "bilateral small24 membership/pair differs");
        key = 1; slot = left == 0 ? 0 : left-1;
    } else if (left < 100001) {
        need(db, right < 2, "bilateral key1 hot pair differs");
        key = 1; slot = left*2+right;
    } else if (left == 100001) {
        need(db, right >= 2 && right <= 100002, "bilateral key2 hot pair differs");
        key = 2; slot = 200002+right-2;
    } else {
        need(db, left <= 200000 && right == left+1, "bilateral unique tail pair differs");
        key = left-99999; slot = 300003+left-100002;
    }
    need(db, left_key == (int64_t)key && right_key == (int64_t)key && slot < config->expected,
         "typed key/complete pair index differs");
    return slot;
}
static void row(sqlite3 *db, const char *data, const Config *config, unsigned char *seen, Drain *out) {
    const char *cursor = data;
    token(db, &cursor, '['); need(db, row_integer(db, &cursor) == 0, "first row cell differs"); token(db, &cursor, ',');
    int64_t left = row_id(db, &cursor); token(db, &cursor, ','); int64_t right = row_id(db, &cursor); token(db, &cursor, ',');
    int64_t left_key = row_integer(db, &cursor); token(db, &cursor, ','); int64_t right_key = row_integer(db, &cursor);
    token(db, &cursor, ']'); while (json_space(*cursor)) ++cursor;
    need(db, *cursor == '\0' && left >= (int64_t)config->first && right >= (int64_t)config->first, "row shape/ID differs");
    uint64_t slot = oracle_slot(db, config, (uint64_t)left-config->first, (uint64_t)right-config->first,
                                left_key, right_key);
    unsigned char mask = (unsigned char)(1U << (slot%8));
    need(db, !(seen[slot/8] & mask), "duplicate ID pair changes complete bag multiplicity"); seen[slot/8] |= mask;
    for (const unsigned char *p = (const unsigned char *)data; *p; ++p) out->hash = (out->hash ^ (uint64_t)*p)*FNV_PRIME;
    out->bytes += (uint64_t)strlen(data); ++out->rows;
}
static void bind_stream(sqlite3 *db, sqlite3_stmt *statement, const char *query, const char *options) {
    need(db, sqlite3_clear_bindings(statement) == SQLITE_OK, "clear bindings failed");
    bind_text(db, statement, 1, query); bind_text(db, statement, 2, options);
}
static void summary(sqlite3 *db, const Config *config, const Drain *out) {
    sqlite3_stmt *statement = prepare(db, "SELECT json_extract(?1,'$.metrics.rows'),json_extract(?1,'$.metrics.dbHits'),json_extract(?1,'$.commit'),json_type(?1,'$.profile'),(SELECT coalesce(sum(json_extract(value,'$.dbHits')),0) FROM json_each(?1,'$.profile.operators')),(SELECT count(*) FROM json_each(?1,'$.profile.operators') WHERE json_extract(value,'$.operator')='HashJoin' AND json_extract(value,'$.rows')>0)");
    bind_text(db, statement, 1, out->summary);
    need(db, sqlite3_step(statement) == SQLITE_ROW, "summary JSON missing");
    need(db, integer(db, statement, 0) == config->expected && strcmp(text(db, statement, 2), config->at) == 0, "summary rows/Commit differ");
    uint64_t hits = integer(db, statement, 1);
    if (config->profile) {
        need(db, strcmp(text(db, statement, 3), "object") == 0, "PROFILE missing");
        need(db, integer(db, statement, 4) == hits, "PROFILE operator dbHits sum differs from summary");
        need(db, config->before || (strcmp(config->kind, "small24") == 0 && !config->require_spill) || integer(db, statement, 5) > 0, "PROFILE full/required-spill case did not execute HashJoin");
    } else need(db, sqlite3_column_type(statement, 3) == SQLITE_NULL, "ordinary sample unexpectedly profiled");
    finish_metadata(db, statement);
    if (config->profile) {
        statement = prepare(db, "SELECT count(*) FROM json_each(?1,'$.profile.operators') o WHERE json_extract(o.value,'$.operator')='HashJoin' AND (coalesce(json_type(o.value,'$.hash'),'null')!='object' OR (SELECT count(*) FROM json_each(o.value,'$.hash') h WHERE h.key IN ('buildRows','probeRows','hashCollisions','spillPartitions','spillBatches','spillVmSteps','spillEncodedBytes','memoryPeakBytes','tempAllocatedBytes') AND h.type='integer' AND h.value>=0)!=9)");
        bind_text(db, statement, 1, out->summary);
        need(db, sqlite3_step(statement) == SQLITE_ROW && integer(db, statement, 0) == 0, "Hash PROFILE fields are missing or not nonnegative integers");
        finish_metadata(db, statement);
    }
    if (config->require_spill) {
        statement = prepare(db, "SELECT count(*) FROM json_each(?1,'$.profile.operators') WHERE json_extract(value,'$.operator')='HashJoin' AND json_extract(value,'$.hash.buildRows')>0 AND json_extract(value,'$.hash.probeRows')>0 AND json_extract(value,'$.hash.spillPartitions')>0 AND json_extract(value,'$.hash.spillBatches')>0 AND json_extract(value,'$.hash.spillEncodedBytes')>0 AND json_extract(value,'$.hash.spillVmSteps')>0 AND json_extract(value,'$.hash.tempAllocatedBytes')>0 AND json_extract(value,'$.hash.memoryPeakBytes')>0");
        bind_text(db, statement, 1, out->summary);
        need(db, sqlite3_step(statement) == SQLITE_ROW && integer(db, statement, 0) > 0, "complete Grace spill evidence missing");
        finish_metadata(db, statement);
    }
}
static void recover(sqlite3 *db, sqlite3_stmt *statement, const Config *config, const char *options) {
    bind_stream(db, statement, "RETURN 1", options);
    uint64_t ordinal = 0; int rc;
    while ((rc = sqlite3_step(statement)) == SQLITE_ROW) {
        need(db, sqlite3_column_count(statement) == 3 && integer(db, statement, 0) == ordinal, "recovery ordinal differs");
        const char *event = text(db, statement, 1), *data = text(db, statement, 2);
        if (ordinal == 0) need(db, strcmp(event, "columns") == 0 && strcmp(data, "[\"1\"]") == 0, "recovery columns differ");
        else if (ordinal == 1) need(db, strcmp(event, "row") == 0 && strcmp(data, "[1]") == 0, "recovery row differs");
        else if (ordinal == 2) {
            need(db, strcmp(event, "summary") == 0, "recovery summary missing");
            Drain recovered = {0}; recovered.summary = (char *)data;
            Config one = *config; one.expected = 1; one.profile = 0; one.require_spill = 0; summary(db, &one, &recovered);
        } else need(db, 0, "recovery has extra events");
        ++ordinal;
    }
    need(db, rc == SQLITE_DONE && ordinal == 3, "recovery did not fully finish");
    need(db, sqlite3_reset(statement) == SQLITE_OK, "recovery reset failed");
}
static void reset_and_check(sqlite3 *db, const Config *config, sqlite3_stmt *statement,
                            Drain *out, int rc, int terminal, int columns, Stamp interrupt_started) {
    if (config->interrupt) {
        out->interrupt_terminal = elapsed(interrupt_started);
        need(db, rc == SQLITE_INTERRUPT && !terminal && out->rows >= 1, "real interrupt did not return SQLITE_INTERRUPT");
        out->reset_rc = sqlite3_reset(statement); need(db, out->reset_rc == SQLITE_INTERRUPT, "interrupt reset code differs");
        out->interrupt_autocommit = sqlite3_get_autocommit(db);
    } else if (config->early) {
        need(db, rc == SQLITE_ROW && out->rows == 1 && !terminal, "early close boundary differs");
        out->reset_rc = sqlite3_reset(statement); need(db, out->reset_rc == SQLITE_OK, "early close reset failed");
    } else {
        need(db, rc == SQLITE_DONE && terminal && columns && out->rows == config->expected, "incomplete result oracle");
        out->reset_rc = sqlite3_reset(statement); need(db, out->reset_rc == SQLITE_OK, "complete reset failed");
    }
}
static Drain drain(sqlite3 *db, const Config *config, const char *query, const char *options, Work *work) {
    Drain out = {0}; out.hash = FNV_OFFSET;
    unsigned char *seen = calloc((size_t)((config->expected+7)/8), 1); need(db, seen != NULL, "oracle bitset allocation failed");
    if (config->trace) start_trace(db, work);
    Stamp start = stamp();
    sqlite3_stmt *statement = prepare(db, "SELECT ordinal,event,data FROM lithograph_rows(?1,'{}',?2)");
    bind_stream(db, statement, query, options);
    uint64_t ordinal = 0; int terminal = 0, columns = 0, rc;
    Stamp interrupt_started = {0};
    while ((rc = sqlite3_step(statement)) == SQLITE_ROW) {
        need(db, sqlite3_column_count(statement) == 3 && integer(db, statement, 0) == ordinal++, "stream ordinal/column count differs");
        const char *event = text(db, statement, 1), *data = text(db, statement, 2);
        need(db, !terminal, "event after summary");
        if (strcmp(event, "columns") == 0) {
            need(db, !columns && ordinal == 1 && strcmp(data, COLUMNS) == 0, "columns differ"); columns = 1;
        } else if (strcmp(event, "row") == 0) {
            need(db, columns && out.rows < config->expected, "unexpected/extra row");
            if (!out.rows) out.first = elapsed(start);
            row(db, data, config, seen, &out);
            if (config->early) break;
            if (config->interrupt && out.rows == 1) { interrupt_started = stamp(); sqlite3_interrupt(db); }
        } else if (strcmp(event, "summary") == 0) {
            need(db, columns && !config->interrupt, "unexpected success summary");
            out.summary = copy_text(db, statement, 2); terminal = 1;
        } else need(db, 0, "unknown stream event");
    }
    out.full = elapsed(start); Stamp cleaning = stamp();
    reset_and_check(db, config, statement, &out, rc, terminal, columns, interrupt_started);
    need(db, sqlite3_trace_v2(db, 0, NULL, NULL) == SQLITE_OK, "trace remove failed");
    need(db, !work->invalid, "SQLite statement work counter overflowed");
    if (config->trace) need(db, work->statements > 1 && work->vm > 0, "trace missed nested engine SQL");
    out.cleanup = elapsed(cleaning);
    if (config->early || config->interrupt) {
        if (sqlite3_get_autocommit(db)) exec_sql(db, "BEGIN DEFERRED");
        Stamp recovery_start = stamp(); recover(db, statement, config, options); out.recovery = elapsed(recovery_start);
    } else summary(db, config, &out);
    need(db, sqlite3_finalize(statement) == SQLITE_OK, "stream finalize failed");
    free(seen); return out;
}
static void close_read(sqlite3 *db, const Config *config, uint64_t commits) {
    need(db, head(db, config) == commits, "read changed head/history");
    exec_sql(db, "COMMIT"); need(db, sqlite3_get_autocommit(db) == 1, "read transaction not released");
    need(db, head(db, config) == commits, "live head/history changed");
    need(db, sqlite3_next_stmt(db, NULL) == NULL, "prepared statement handle leaked");
    need(NULL, sqlite3_close(db) == SQLITE_OK, "connection close failed");
}
static void report(const Config *config, const Drain *result, const Work *work, const char *query,
                   const char *options, const char *env, const char *physical, uint64_t commits, Stamp total) {
    printf("{\"status\":\"passed\",\"surface\":\"rows\",\"case\":"); quote(config->kind);
    printf(",\"database\":"); quote(config->path); printf(",\"at\":"); quote(config->at); printf(",\"query\":"); quote(query);
    printf(",\"options\":%s,\"sqlite\":%s,\"expected_sqlite_source_id\":", options, env); quote(config->sqlite_source_id);
    printf(",\"sqlite_source_id_checked_before_extension_load\":true,\"physical_plan\":"); quote(physical);
    printf(",\"before\":%s,\"profile\":%s,\"traced\":%s,\"require_spill\":%s,\"interrupted\":%s,\"early_closed\":%s,",
           config->before ? "true" : "false", config->profile ? "true" : "false", config->trace ? "true" : "false", config->require_spill ? "true" : "false", config->interrupt ? "true" : "false", config->early ? "true" : "false");
    printf("\"bilateral_matching_hot_fixture\":%s,", config->bilateral ? "true" : "false");
    printf("\"expected_rows\":%" PRIu64 ",\"rows\":%" PRIu64 ",\"actual_row_bytes\":%" PRIu64 ",\"fnv1a64_hex\":\"%016" PRIx64 "\",\"oracle_bitset_bytes\":%" PRIu64 ",",
           config->expected, result->rows, result->bytes, result->hash, (config->expected+7)/8);
    printf("\"complete_typed_bag_verified\":%s,\"db_readonly\":true,\"head_and_history_unchanged\":true,\"commits\":%" PRIu64 ",\"statements_finalized\":true,\"close_ok\":true,",
           config->early || config->interrupt ? "false" : "true", commits);
    printf("\"first_data_wall_us\":%.3f,\"first_data_cpu_us\":%.3f,\"full_wall_us\":%.3f,\"full_cpu_us\":%.3f,\"reset_cleanup_wall_us\":%.3f,\"reset_cleanup_cpu_us\":%.3f,\"reset_rc\":%d,\"interrupt_autocommit_after_reset\":%d,\"recovery_wall_us\":%.3f,\"recovery_cpu_us\":%.3f,\"end_to_end_wall_us\":%.3f,\"end_to_end_cpu_us\":%.3f,",
           us(result->first.wall), us(result->first.cpu), us(result->full.wall), us(result->full.cpu), us(result->cleanup.wall), us(result->cleanup.cpu), result->reset_rc, result->interrupt_autocommit,
           us(result->recovery.wall), us(result->recovery.cpu), us(total.wall), us(total.cpu));
    printf("\"interrupt_call_to_terminal_wall_us\":%.3f,\"interrupt_call_to_terminal_cpu_us\":%.3f,\"same_statement_recovered\":%s,\"reopened_readonly\":%s,",
           us(result->interrupt_terminal.wall), us(result->interrupt_terminal.cpu), config->interrupt || config->early ? "true" : "false", config->interrupt || config->early ? "true" : "false");
    printf("\"host_connection_trace\":{\"statements\":%" PRIu64 ",\"vm_steps\":%" PRIu64 ",\"fullscan_steps\":%" PRIu64 ",\"sorts\":%" PRIu64 "},\"summary\":%s,",
           work->statements, work->vm, work->fullscan, work->sorts, result->summary ? result->summary : "null");
    printf("\"checksum_scope\":\"actual UTF-8 row bytes in observed order, not an order-independent bag hash\",\"oracle_scope\":\"typed five cells, exact ID/key recipe, unique expected-pair bitset and complete cardinality; no retained row set\",\"timing_boundary\":\"prepare/bind/all steps/typed row checks/checksum/summary copy/DONE; excludes plan, oracle allocation, summary JSON validation, reset/finalize and read COMMIT\",\"trace_scope\":\"sample and reset on host connection only; private spill VM comes only from hash profile; trace and PROFILE are explicit instrumented samples\"}\n");
}
int main(int argc, char **argv) {
    Config config = arguments(argc, argv); Stamp overall = stamp();
    need(NULL, sqlite3_libversion_number() >= 3045000, "SQLite 3.45+ required");
    sqlite3 *db = open_ro(&config); uint64_t commits = head(db, &config); checkpoint(db, &config);
    char *env = environment(db);
    const char *pattern = strcmp(config.kind, "full") == 0 ? "MATCH (l:S5Left),(r:S5Right)" : "MATCH (l:S5Left:S5Small),(r:S5Right:S5Small)";
    char *query = sqlite3_mprintf("%s WHERE l.leftKey=r.rightKey RETURN 0,elementId(l),elementId(r),l.leftKey,r.rightKey", pattern);
    char *options = config.memory ? sqlite3_mprintf("{\"at\":\"%s\",\"memoryLimitBytes\":%" PRIu64 "}", config.at, config.memory) : sqlite3_mprintf("{\"at\":\"%s\"}", config.at);
    need(db, query && options, "query/options allocation failed");
    char *physical = plan(db, &config, query, options);
    char *execution = sqlite3_mprintf("%s%s", config.profile ? "PROFILE " : "", query);
    need(db, execution != NULL, "execution allocation failed");
    Work work = {0}; Drain result = drain(db, &config, execution, options, &work);
    close_read(db, &config, commits);
    if (config.early || config.interrupt) {
        db = open_ro(&config); need(db, head(db, &config) == commits, "reopen changed head/history");
        sqlite3_stmt *statement = prepare(db, "SELECT ordinal,event,data FROM lithograph_rows(?1,'{}',?2)");
        recover(db, statement, &config, options); need(db, sqlite3_finalize(statement) == SQLITE_OK, "reopen finalize failed");
        close_read(db, &config, commits);
    }
    Stamp total = elapsed(overall);
    report(&config, &result, &work, query, options, env, physical, commits, total);
    sqlite3_free(result.summary); sqlite3_free(execution); sqlite3_free(physical); sqlite3_free(options); sqlite3_free(query); sqlite3_free(env);
    need(NULL, fflush(stdout) == 0 && !ferror(stdout), "stdout delivery failed");
    return 0;
}
