#include "sqlite3ext.h"
SQLITE_EXTENSION_INIT1

#include <stddef.h>
#include <string.h>

typedef struct Phase12State Phase12State;
typedef struct Phase12Registration Phase12Registration;
typedef struct Phase12Tokenizer Phase12Tokenizer;

struct Phase12State {
  int create_count;
  int delete_count;
  int document_calls;
  int query_calls;
  int prefix_calls;
  int aux_calls;
  int colocated_tokens;
  char last_args[512];
  char last_name[64];
};

struct Phase12Registration {
  Phase12State *state;
  const char *name;
  int synonym_mode;
};

struct Phase12Tokenizer {
  Phase12Registration *registration;
  int failure_mode;
};

static Phase12State g_state;
static Phase12Registration g_echo = {&g_state, "phase12_echo", 0};
static Phase12Registration g_synonym = {&g_state, "phase12_synonym", 1};
static Phase12Registration g_legacy_english = {&g_state, "english", 0};
static Phase12Registration g_legacy_standard = {
    &g_state, "standard-no-stop-words", 0};

static int phase12_fts5_api(sqlite3 *db, fts5_api **api) {
  sqlite3_stmt *statement = 0;
  int rc;
  int step_rc;
  int finalize_rc;

  *api = 0;
  rc = sqlite3_prepare_v2(db, "SELECT fts5(?1)", -1, &statement, 0);
  if (rc == SQLITE_OK) {
    rc = sqlite3_bind_pointer(statement, 1, (void *)api, "fts5_api_ptr", 0);
  }
  if (rc == SQLITE_OK) {
    step_rc = sqlite3_step(statement);
    if (step_rc != SQLITE_ROW && step_rc != SQLITE_DONE) {
      rc = step_rc;
    }
  }
  finalize_rc = sqlite3_finalize(statement);
  if (rc == SQLITE_OK) {
    rc = finalize_rc;
  }
  if (rc == SQLITE_OK && *api == 0) {
    return SQLITE_ERROR;
  }
  return rc;
}

static void phase12_record_args(
    Phase12Registration *registration,
    const char **args,
    int arg_count) {
  int offset = 0;
  int i;

  sqlite3_snprintf(
      (int)sizeof(g_state.last_name),
      g_state.last_name,
      "%s",
      registration->name);
  g_state.last_args[0] = '\0';
  for (i = 0; i < arg_count && offset < (int)sizeof(g_state.last_args) - 1; i++) {
    int remaining = (int)sizeof(g_state.last_args) - offset;
    sqlite3_snprintf(
        remaining,
        &g_state.last_args[offset],
        "%s%s",
        i == 0 ? "" : "|",
        args[i]);
    offset = (int)strlen(g_state.last_args);
  }
}

static int phase12_create(
    void *context,
    const char **args,
    int arg_count,
    Fts5Tokenizer **output) {
  Phase12Registration *registration = (Phase12Registration *)context;
  Phase12Tokenizer *tokenizer;

  registration->state->create_count++;
  phase12_record_args(registration, args, arg_count);
  if (arg_count > 0 && strcmp(args[0], "fail") == 0) {
    return SQLITE_ERROR;
  }
  if (arg_count > 0 && strcmp(args[0], "nomem") == 0) {
    return SQLITE_NOMEM;
  }
  tokenizer = sqlite3_malloc64(sizeof(*tokenizer));
  if (tokenizer == 0) {
    return SQLITE_NOMEM;
  }
  tokenizer->registration = registration;
  tokenizer->failure_mode = 0;
  if (arg_count > 0 && strcmp(args[0], "tokenize_fail") == 0) {
    tokenizer->failure_mode = 1;
  } else if (arg_count > 0 && strcmp(args[0], "fail_on_bad") == 0) {
    tokenizer->failure_mode = 2;
  }
  *output = (Fts5Tokenizer *)tokenizer;
  return SQLITE_OK;
}

static void phase12_delete(Fts5Tokenizer *opaque) {
  Phase12Tokenizer *tokenizer = (Phase12Tokenizer *)opaque;
  if (tokenizer == 0) {
    return;
  }
  tokenizer->registration->state->delete_count++;
  sqlite3_free(tokenizer);
}

static int phase12_is_token_byte(unsigned char value) {
  if (value >= 0x80) {
    return 1;
  }
  return (value >= 'a' && value <= 'z') ||
         (value >= 'A' && value <= 'Z') ||
         (value >= '0' && value <= '9') || value == '_';
}

static int phase12_token_equals(
    const char *text,
    int length,
    const char *expected) {
  size_t expected_length = strlen(expected);
  return expected_length == (size_t)length &&
         memcmp(text, expected, expected_length) == 0;
}

static int phase12_contains(
    const char *text,
    int length,
    const char *needle) {
  size_t needle_length = strlen(needle);
  int offset;

  if (needle_length == 0 || needle_length > (size_t)length) {
    return 0;
  }
  for (offset = 0; offset <= length - (int)needle_length; offset++) {
    if (memcmp(&text[offset], needle, needle_length) == 0) {
      return 1;
    }
  }
  return 0;
}

static int phase12_emit(
    Phase12Tokenizer *tokenizer,
    void *context,
    int flags,
    const char *text,
    int length,
    int start,
    int end,
    int (*callback)(void *, int, const char *, int, int, int)) {
  const char *emitted = text;
  int emitted_length = length;
  int rc;

  if ((flags & FTS5_TOKENIZE_QUERY) != 0) {
    if ((flags & FTS5_TOKENIZE_PREFIX) != 0 &&
        phase12_token_equals(text, length, "pref")) {
      emitted = "doc";
      emitted_length = 3;
    } else if (phase12_token_equals(text, length, "needle")) {
      emitted = "document";
      emitted_length = 8;
    }
  }

  rc = callback(context, 0, emitted, emitted_length, start, end);
  if (rc != SQLITE_OK) {
    return rc;
  }
  if (tokenizer->registration->synonym_mode != 0 &&
      phase12_token_equals(text, length, "usa")) {
    tokenizer->registration->state->colocated_tokens++;
    rc = callback(
        context,
        FTS5_TOKEN_COLOCATED,
        "america",
        7,
        start,
        end);
  }
  return rc;
}

static int phase12_tokenize(
    Fts5Tokenizer *opaque,
    void *context,
    int flags,
    const char *text,
    int text_length,
    int (*callback)(void *, int, const char *, int, int, int)) {
  Phase12Tokenizer *tokenizer = (Phase12Tokenizer *)opaque;
  Phase12State *state = tokenizer->registration->state;
  int offset = 0;

  if ((flags & FTS5_TOKENIZE_DOCUMENT) != 0) {
    state->document_calls++;
  }
  if ((flags & FTS5_TOKENIZE_QUERY) != 0) {
    state->query_calls++;
  }
  if ((flags & FTS5_TOKENIZE_PREFIX) != 0) {
    state->prefix_calls++;
  }
  if ((flags & FTS5_TOKENIZE_AUX) != 0) {
    state->aux_calls++;
  }
  if (tokenizer->failure_mode == 1 ||
      (tokenizer->failure_mode == 2 &&
       phase12_contains(text, text_length, "explode"))) {
    return SQLITE_ERROR;
  }

  while (offset < text_length) {
    int start;
    int rc;
    while (offset < text_length &&
           !phase12_is_token_byte((unsigned char)text[offset])) {
      offset++;
    }
    start = offset;
    while (offset < text_length &&
           phase12_is_token_byte((unsigned char)text[offset])) {
      offset++;
    }
    if (start == offset) {
      break;
    }
    rc = phase12_emit(
        tokenizer,
        context,
        flags,
        &text[start],
        offset - start,
        start,
        offset,
        callback);
    if (rc != SQLITE_OK) {
      return rc;
    }
  }
  return SQLITE_OK;
}

static void phase12_stat(
    sqlite3_context *context,
    int argc,
    sqlite3_value **argv) {
  const unsigned char *name;
  int value = 0;

  (void)argc;
  name = sqlite3_value_text(argv[0]);
  if (name == 0) {
    sqlite3_result_null(context);
    return;
  }
  if (strcmp((const char *)name, "create") == 0) {
    value = g_state.create_count;
  } else if (strcmp((const char *)name, "delete") == 0) {
    value = g_state.delete_count;
  } else if (strcmp((const char *)name, "document") == 0) {
    value = g_state.document_calls;
  } else if (strcmp((const char *)name, "query") == 0) {
    value = g_state.query_calls;
  } else if (strcmp((const char *)name, "prefix") == 0) {
    value = g_state.prefix_calls;
  } else if (strcmp((const char *)name, "aux") == 0) {
    value = g_state.aux_calls;
  } else if (strcmp((const char *)name, "colocated") == 0) {
    value = g_state.colocated_tokens;
  } else {
    sqlite3_result_error(context, "unknown phase12 tokenizer stat", -1);
    return;
  }
  sqlite3_result_int(context, value);
}

static void phase12_args(
    sqlite3_context *context,
    int argc,
    sqlite3_value **argv) {
  (void)argc;
  (void)argv;
  sqlite3_result_text(context, g_state.last_args, -1, SQLITE_TRANSIENT);
}

static void phase12_name(
    sqlite3_context *context,
    int argc,
    sqlite3_value **argv) {
  (void)argc;
  (void)argv;
  sqlite3_result_text(context, g_state.last_name, -1, SQLITE_TRANSIENT);
}

static void phase12_reset(
    sqlite3_context *context,
    int argc,
    sqlite3_value **argv) {
  (void)argc;
  (void)argv;
  memset(&g_state, 0, sizeof(g_state));
  sqlite3_result_int(context, 1);
}

typedef struct Phase12AdapterProbe Phase12AdapterProbe;
struct Phase12AdapterProbe {
  int count;
  int valid;
};

static int phase12_adapter_token_callback(
    void *opaque,
    int token_flags,
    const char *token,
    int token_length,
    int start,
    int end) {
  Phase12AdapterProbe *probe = (Phase12AdapterProbe *)opaque;
  static const char *expected_tokens[] = {"aa", "bb"};
  static const int expected_start[] = {0, 3};
  static const int expected_end[] = {2, 5};
  int ordinal = probe->count;

  if (ordinal >= 2 || token_flags != 0 ||
      token_length != 2 ||
      memcmp(token, expected_tokens[ordinal], 2) != 0 ||
      start != expected_start[ordinal] || end != expected_end[ordinal]) {
    probe->valid = 0;
  }
  probe->count++;
  return SQLITE_OK;
}

static int phase12_adapter_abort_callback(
    void *opaque,
    int token_flags,
    const char *token,
    int token_length,
    int start,
    int end) {
  (void)opaque;
  (void)token_flags;
  (void)token;
  (void)token_length;
  (void)start;
  (void)end;
  return SQLITE_ABORT;
}

static void phase12_adapter_probe(
    sqlite3_context *context,
    int argc,
    sqlite3_value **argv) {
  sqlite3 *db = sqlite3_context_db_handle(context);
  fts5_api *api = 0;
  fts5_tokenizer tokenizer;
  Fts5Tokenizer *instance = 0;
  void *user_data = 0;
  const char *args[] = {
      "phase12_echo", "phase12_echo", "phase12_aux_probe"};
  Phase12AdapterProbe probe = {0, 1};
  int rc;
  int abort_rc;

  (void)argc;
  (void)argv;
  memset(&tokenizer, 0, sizeof(tokenizer));
  rc = phase12_fts5_api(db, &api);
  if (rc == SQLITE_OK) {
    rc = api->xFindTokenizer(
        api, "lithograph_dual_v1", &user_data, &tokenizer);
  }
  if (rc == SQLITE_OK && tokenizer.xCreate != 0) {
    rc = tokenizer.xCreate(user_data, args, 3, &instance);
  }
  if (rc == SQLITE_OK && tokenizer.xTokenize != 0) {
    rc = tokenizer.xTokenize(
        instance,
        &probe,
        FTS5_TOKENIZE_AUX,
        "aa bb",
        5,
        phase12_adapter_token_callback);
  }
  if (rc == SQLITE_OK && (probe.count != 2 || probe.valid == 0)) {
    rc = SQLITE_ERROR;
  }
  abort_rc = SQLITE_ERROR;
  if (rc == SQLITE_OK && tokenizer.xTokenize != 0) {
    abort_rc = tokenizer.xTokenize(
        instance,
        0,
        FTS5_TOKENIZE_AUX,
        "aa",
        2,
        phase12_adapter_abort_callback);
    if (abort_rc != SQLITE_ABORT) {
      rc = SQLITE_ERROR;
    }
  }
  if (instance != 0 && tokenizer.xDelete != 0) {
    tokenizer.xDelete(instance);
  }
  if (rc != SQLITE_OK) {
    sqlite3_result_error(context, "Phase 12 adapter AUX probe failed", -1);
    sqlite3_result_error_code(context, rc);
    return;
  }
  sqlite3_result_int(context, 1);
}

#if defined(_WIN32)
__declspec(dllexport)
#endif
int sqlite3_phase12_tokenizer_init(
    sqlite3 *db,
    char **error_message,
    const sqlite3_api_routines *api_routines) {
  fts5_api *api = 0;
  fts5_tokenizer tokenizer = {phase12_create, phase12_delete, phase12_tokenize};
  int rc;

  (void)error_message;
  SQLITE_EXTENSION_INIT2(api_routines);
  memset(&g_state, 0, sizeof(g_state));

  rc = phase12_fts5_api(db, &api);
  if (rc != SQLITE_OK) {
    return rc;
  }
  rc = api->xCreateTokenizer(api, g_echo.name, &g_echo, &tokenizer, 0);
  if (rc != SQLITE_OK) {
    return rc;
  }
  rc = api->xCreateTokenizer(api, g_synonym.name, &g_synonym, &tokenizer, 0);
  if (rc != SQLITE_OK) {
    return rc;
  }
  rc = api->xCreateTokenizer(
      api, g_legacy_english.name, &g_legacy_english, &tokenizer, 0);
  if (rc != SQLITE_OK) {
    return rc;
  }
  rc = api->xCreateTokenizer(
      api, g_legacy_standard.name, &g_legacy_standard, &tokenizer, 0);
  if (rc != SQLITE_OK) {
    return rc;
  }
  rc = sqlite3_create_function(
      db,
      "phase12_tokenizer_stat",
      1,
      SQLITE_UTF8,
      0,
      phase12_stat,
      0,
      0);
  if (rc != SQLITE_OK) {
    return rc;
  }
  rc = sqlite3_create_function(
      db,
      "phase12_tokenizer_args",
      0,
      SQLITE_UTF8,
      0,
      phase12_args,
      0,
      0);
  if (rc != SQLITE_OK) {
    return rc;
  }
  rc = sqlite3_create_function(
      db,
      "phase12_tokenizer_name",
      0,
      SQLITE_UTF8,
      0,
      phase12_name,
      0,
      0);
  if (rc != SQLITE_OK) {
    return rc;
  }
  rc = sqlite3_create_function(
      db,
      "phase12_adapter_probe",
      0,
      SQLITE_UTF8,
      0,
      phase12_adapter_probe,
      0,
      0);
  if (rc != SQLITE_OK) {
    return rc;
  }
  return sqlite3_create_function(
      db,
      "phase12_tokenizer_reset",
      0,
      SQLITE_UTF8,
      0,
      phase12_reset,
      0,
      0);
}
