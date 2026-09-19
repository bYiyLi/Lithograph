#include <math.h>
#include <stdint.h>
#include <stddef.h>
#include <stdio.h>
#include <string.h>

#include <sqlite3ext.h>
SQLITE_EXTENSION_INIT1

#include "lithograph_embedding_provider.h"

#define PROVIDER_PREFIX "lithograph.embedding.v1/"
#define PROVIDER_A "synthetic-a"
#define PROVIDER_B "synthetic-b"

typedef struct synthetic_state {
    sqlite3 *db;
    const char *name;
    const unsigned char *identity;
    size_t identity_len;
    sqlite3_int64 validate_calls;
    sqlite3_int64 embed_calls;
    sqlite3_int64 embed_inputs;
    char *last_inputs;
} synthetic_state;

typedef struct synthetic_registration {
    lithograph_embedding_provider_v1 provider;
    synthetic_state state;
} synthetic_registration;

static sqlite3_int64 destructor_calls = 0;
static sqlite3_int64 active_embed_calls = 0;
static sqlite3_int64 non_autocommit_embed_calls = 0;

static sqlite3_mutex *counter_mutex(void) {
    return sqlite3_mutex_alloc(SQLITE_MUTEX_STATIC_APP1);
}

static void adjust_counter(sqlite3_int64 *counter, sqlite3_int64 delta) {
    sqlite3_mutex *mutex = counter_mutex();
    sqlite3_mutex_enter(mutex);
    *counter += delta;
    sqlite3_mutex_leave(mutex);
}

static sqlite3_int64 read_counter(sqlite3_int64 *counter) {
    sqlite3_mutex *mutex = counter_mutex();
    sqlite3_mutex_enter(mutex);
    sqlite3_int64 value = *counter;
    sqlite3_mutex_leave(mutex);
    return value;
}

static int contains_bytes(
    const unsigned char *data,
    size_t len,
    const char *needle
) {
    const size_t needle_len = strlen(needle);
    size_t i;
    if (needle_len == 0 || needle_len > len) {
        return 0;
    }
    for (i = 0; i + needle_len <= len; ++i) {
        if (memcmp(data + i, needle, needle_len) == 0) {
            return 1;
        }
    }
    return 0;
}

static void clear_error(lithograph_embedding_error_v1 *error) {
    if (error == NULL) {
        return;
    }
    error->status = LITHOGRAPH_EMBEDDING_OK_V1;
    error->message = NULL;
    error->message_len = 0;
}

static int set_error(
    lithograph_embedding_error_v1 *error,
    int status,
    const char *message
) {
    const size_t len = strlen(message);
    unsigned char *copy = sqlite3_malloc64((sqlite3_uint64)len);
    if (error == NULL) {
        return status;
    }
    error->status = status;
    error->message = NULL;
    error->message_len = 0;
    if (copy == NULL && len != 0) {
        error->status = LITHOGRAPH_EMBEDDING_RESOURCE_ERROR_V1;
        return LITHOGRAPH_EMBEDDING_RESOURCE_ERROR_V1;
    }
    if (len != 0) {
        memcpy(copy, message, len);
    }
    error->message = copy;
    error->message_len = len;
    return status;
}

static int synthetic_validate(
    void *context,
    const unsigned char *config_json,
    size_t config_len,
    size_t dimensions,
    uint32_t coordinate_type,
    lithograph_embedding_error_v1 *error
) {
    synthetic_state *state = context;
    clear_error(error);
    if (state == NULL) {
        return set_error(error, LITHOGRAPH_EMBEDDING_INTERNAL_ERROR_V1, "missing synthetic context");
    }
    state->validate_calls += 1;
    if (coordinate_type != LITHOGRAPH_EMBEDDING_COORDINATE_FLOAT32_V1) {
        return set_error(error, LITHOGRAPH_EMBEDDING_INVALID_CONFIG_V1, "synthetic requires FLOAT32");
    }
    if (dimensions == 0 || dimensions > 4096) {
        return set_error(error, LITHOGRAPH_EMBEDDING_INVALID_CONFIG_V1, "synthetic dimensions out of range");
    }
    if (config_json == NULL && config_len != 0) {
        return set_error(error, LITHOGRAPH_EMBEDDING_INVALID_CONFIG_V1, "synthetic config pointer is NULL");
    }
    if (contains_bytes(config_json, config_len, "\"validate\":\"invalid\"")) {
        return set_error(error, LITHOGRAPH_EMBEDDING_INVALID_CONFIG_V1, "synthetic validation failure");
    }
    return LITHOGRAPH_EMBEDDING_OK_V1;
}

static uint64_t fnv1a(const unsigned char *data, size_t len, uint64_t seed) {
    uint64_t hash = 1469598103934665603ULL ^ seed;
    size_t i;
    for (i = 0; i < len; ++i) {
        hash ^= (uint64_t)data[i];
        hash *= 1099511628211ULL;
    }
    return hash;
}

static float coordinate(
    const unsigned char *data,
    size_t len,
    size_t ordinal
) {
    uint64_t hash = fnv1a(data, len, (uint64_t)(ordinal + 1) * 0x9e3779b97f4a7c15ULL);
    int value = (int)(hash % 2001ULL) - 1000;
    if (value == 0) {
        value = (int)(ordinal % 17U) + 1;
    }
    return (float)value / 1000.0f;
}

static int record_inputs(
    synthetic_state *state,
    const lithograph_embedding_text_v1 *texts,
    size_t text_count
) {
    size_t total = 1;
    size_t i;
    char *output;
    char *cursor;

    for (i = 0; i < text_count; ++i) {
        total += 32 + texts[i].len;
    }
    output = sqlite3_malloc64((sqlite3_uint64)total);
    if (output == NULL) {
        return SQLITE_NOMEM;
    }
    cursor = output;
    for (i = 0; i < text_count; ++i) {
        if (i != 0) {
            *cursor++ = '|';
        }
        sqlite3_snprintf(
            31,
            cursor,
            "%llu:",
            (unsigned long long)texts[i].len
        );
        cursor += strlen(cursor);
        if (texts[i].len != 0) {
            memcpy(cursor, texts[i].data, texts[i].len);
            cursor += texts[i].len;
        }
    }
    *cursor = '\0';
    sqlite3_free(state->last_inputs);
    state->last_inputs = output;
    return SQLITE_OK;
}

static int validate_embed_arguments(
    synthetic_state *state,
    const lithograph_embedding_text_v1 *texts,
    size_t text_count,
    size_t dimensions,
    uint32_t coordinate_type,
    lithograph_embedding_batch_v1 *result,
    lithograph_embedding_error_v1 *error
) {
    if (state == NULL || result == NULL) {
        return set_error(
            error,
            LITHOGRAPH_EMBEDDING_INTERNAL_ERROR_V1,
            "invalid synthetic callback state"
        );
    }
    state->embed_calls += 1;
    if (coordinate_type != LITHOGRAPH_EMBEDDING_COORDINATE_FLOAT32_V1
        || dimensions == 0
        || dimensions > 4096
        || text_count == 0
        || texts == NULL) {
        return set_error(
            error,
            LITHOGRAPH_EMBEDDING_INVALID_CONFIG_V1,
            "invalid synthetic embed arguments"
        );
    }
    state->embed_inputs += (sqlite3_int64)text_count;
    return LITHOGRAPH_EMBEDDING_OK_V1;
}

static int configured_embed_failure(
    const unsigned char *config_json,
    size_t config_len,
    lithograph_embedding_error_v1 *error
) {
    if (contains_bytes(config_json, config_len, "\"fail\":\"io\"")) {
        return set_error(
            error,
            LITHOGRAPH_EMBEDDING_IO_ERROR_V1,
            "synthetic I/O failure"
        );
    }
    if (contains_bytes(config_json, config_len, "\"fail\":\"resource\"")) {
        return set_error(
            error,
            LITHOGRAPH_EMBEDDING_RESOURCE_ERROR_V1,
            "synthetic resource failure"
        );
    }
    if (contains_bytes(config_json, config_len, "\"fail\":\"cancel\"")) {
        return set_error(
            error,
            LITHOGRAPH_EMBEDDING_CANCELLED_V1,
            "synthetic configured cancellation"
        );
    }
    return LITHOGRAPH_EMBEDDING_OK_V1;
}

static void record_embed_execution_state(
    synthetic_state *state,
    const unsigned char *config_json,
    size_t config_len
) {
    if (state->db != NULL && sqlite3_get_autocommit(state->db) == 0) {
        adjust_counter(&non_autocommit_embed_calls, 1);
    }
    if (contains_bytes(config_json, config_len, "\"sleep_ms\":1000")) {
        adjust_counter(&active_embed_calls, 1);
        sqlite3_sleep(1000);
        adjust_counter(&active_embed_calls, -1);
    }
}

static int configure_publish_fault(
    synthetic_state *state,
    const unsigned char *config_json,
    size_t config_len,
    lithograph_embedding_error_v1 *error
) {
    if (!contains_bytes(
            config_json,
            config_len,
            "\"publish_fail\":\"query_only\""
        )) {
        return LITHOGRAPH_EMBEDDING_OK_V1;
    }
    if (state->db == NULL
        || sqlite3_exec(state->db, "PRAGMA query_only=ON", NULL, NULL, NULL)
            != SQLITE_OK) {
        return set_error(
            error,
            LITHOGRAPH_EMBEDDING_INTERNAL_ERROR_V1,
            "synthetic cache publish fault injection failed"
        );
    }
    return LITHOGRAPH_EMBEDDING_OK_V1;
}

static int allocate_embedding_values(
    const lithograph_embedding_text_v1 *texts,
    size_t text_count,
    size_t dimensions,
    float **output,
    size_t *output_count,
    lithograph_embedding_error_v1 *error
) {
    size_t i;
    size_t j;
    if (dimensions > SIZE_MAX / text_count) {
        return set_error(
            error,
            LITHOGRAPH_EMBEDDING_RESOURCE_ERROR_V1,
            "synthetic output size overflow"
        );
    }
    const size_t value_count = dimensions * text_count;
    if (value_count > SIZE_MAX / sizeof(float)) {
        return set_error(
            error,
            LITHOGRAPH_EMBEDDING_RESOURCE_ERROR_V1,
            "synthetic output size overflow"
        );
    }
    float *values = sqlite3_malloc64(
        (sqlite3_uint64)(value_count * sizeof(float))
    );
    if (values == NULL) {
        return set_error(
            error,
            LITHOGRAPH_EMBEDDING_RESOURCE_ERROR_V1,
            "synthetic output allocation failed"
        );
    }
    for (i = 0; i < text_count; ++i) {
        if (texts[i].data == NULL && texts[i].len != 0) {
            sqlite3_free(values);
            return set_error(
                error,
                LITHOGRAPH_EMBEDDING_INTERNAL_ERROR_V1,
                "synthetic text pointer is NULL"
            );
        }
        for (j = 0; j < dimensions; ++j) {
            values[i * dimensions + j] =
                coordinate(texts[i].data, texts[i].len, j);
        }
    }
    *output = values;
    *output_count = value_count;
    return LITHOGRAPH_EMBEDDING_OK_V1;
}

static int synthetic_embed_batch(
    void *context,
    const unsigned char *config_json,
    size_t config_len,
    const lithograph_embedding_text_v1 *texts,
    size_t text_count,
    size_t dimensions,
    uint32_t coordinate_type,
    lithograph_embedding_cancel_callback_v1 is_cancelled,
    void *cancel_user_data,
    lithograph_embedding_batch_v1 *result,
    lithograph_embedding_error_v1 *error
) {
    // #lizard forgives(parameter_count)
    synthetic_state *state = context;
    clear_error(error);
    if (result != NULL) {
        memset(result, 0, sizeof(*result));
    }
    int status = validate_embed_arguments(
        state,
        texts,
        text_count,
        dimensions,
        coordinate_type,
        result,
        error
    );
    if (status != LITHOGRAPH_EMBEDDING_OK_V1) {
        return status;
    }
    if (is_cancelled != NULL && is_cancelled(cancel_user_data) != 0) {
        return set_error(error, LITHOGRAPH_EMBEDDING_CANCELLED_V1, "synthetic cancelled");
    }
    status = configured_embed_failure(config_json, config_len, error);
    if (status != LITHOGRAPH_EMBEDDING_OK_V1) {
        return status;
    }
    record_embed_execution_state(state, config_json, config_len);
    if (record_inputs(state, texts, text_count) != SQLITE_OK) {
        return set_error(error, LITHOGRAPH_EMBEDDING_RESOURCE_ERROR_V1, "synthetic input log allocation failed");
    }
    float *values = NULL;
    size_t value_count = 0;
    status = allocate_embedding_values(
        texts,
        text_count,
        dimensions,
        &values,
        &value_count,
        error
    );
    if (status != LITHOGRAPH_EMBEDDING_OK_V1) {
        return status;
    }
    if (contains_bytes(config_json, config_len, "\"invalid\":\"nan\"")) {
        values[0] = NAN;
    }
    if (contains_bytes(config_json, config_len, "\"invalid\":\"zero\"")) {
        memset(values, 0, value_count * sizeof(float));
    }
    result->values = values;
    result->value_count = value_count;
    result->embedding_count = text_count;
    result->dimensions = contains_bytes(
        config_json,
        config_len,
        "\"invalid\":\"dimension\""
    ) ? dimensions + 1 : dimensions;
    status = configure_publish_fault(state, config_json, config_len, error);
    if (status != LITHOGRAPH_EMBEDDING_OK_V1) {
        sqlite3_free(result->values);
        memset(result, 0, sizeof(*result));
        return status;
    }
    return LITHOGRAPH_EMBEDDING_OK_V1;
}

static void synthetic_free_batch(
    void *context,
    lithograph_embedding_batch_v1 *result
) {
    (void)context;
    if (result == NULL) {
        return;
    }
    sqlite3_free(result->values);
    memset(result, 0, sizeof(*result));
}

static void synthetic_free_error(
    void *context,
    lithograph_embedding_error_v1 *error
) {
    (void)context;
    if (error == NULL) {
        return;
    }
    sqlite3_free(error->message);
    clear_error(error);
}

static void synthetic_destroy(void *value) {
    synthetic_registration *registration = value;
    if (registration == NULL) {
        return;
    }
    adjust_counter(&destructor_calls, 1);
    sqlite3_free(registration->state.last_inputs);
    sqlite3_free(registration);
}

static void unregister_provider(sqlite3 *db, const char *name) {
    char key[128];
    sqlite3_snprintf((int)sizeof(key), key, "%s%s", PROVIDER_PREFIX, name);
    (void)sqlite3_set_clientdata(db, key, NULL, NULL);
}

static int register_provider(
    sqlite3 *db,
    const char *name,
    const unsigned char *identity,
    char **error
) {
    char key[128];
    synthetic_registration *registration;
    int rc;

    sqlite3_snprintf((int)sizeof(key), key, "%s%s", PROVIDER_PREFIX, name);
    if (sqlite3_get_clientdata(db, key) != NULL) {
        if (error != NULL) {
            *error = sqlite3_mprintf("synthetic embedding provider %s is already registered", name);
        }
        return SQLITE_ERROR;
    }
    registration = sqlite3_malloc64(sizeof(*registration));
    if (registration == NULL) {
        return SQLITE_NOMEM;
    }
    memset(registration, 0, sizeof(*registration));
    registration->state.db = db;
    registration->state.name = name;
    registration->state.identity = identity;
    registration->state.identity_len = strlen((const char *)identity);
    registration->provider.abi_version = LITHOGRAPH_EMBEDDING_PROVIDER_ABI_VERSION_V1;
    registration->provider.struct_size = sizeof(registration->provider);
    registration->provider.context = &registration->state;
    registration->provider.semantic_identity = identity;
    registration->provider.semantic_identity_len = registration->state.identity_len;
    registration->provider.validate = synthetic_validate;
    registration->provider.embed_batch = synthetic_embed_batch;
    registration->provider.free_batch = synthetic_free_batch;
    registration->provider.free_error = synthetic_free_error;

    rc = sqlite3_set_clientdata(db, key, &registration->provider, synthetic_destroy);
    if (rc != SQLITE_OK && rc != SQLITE_NOMEM) {
        sqlite3_free(registration);
    }
    return rc;
}

static synthetic_state *lookup_state(sqlite3_context *context, sqlite3_value *argument) {
    sqlite3 *db = sqlite3_context_db_handle(context);
    const unsigned char *name = sqlite3_value_text(argument);
    char key[128];
    lithograph_embedding_provider_v1 *provider;
    if (name == NULL) {
        return NULL;
    }
    sqlite3_snprintf(
        (int)sizeof(key),
        key,
        "%s%s",
        PROVIDER_PREFIX,
        (const char *)name
    );
    provider = sqlite3_get_clientdata(db, key);
    return provider == NULL ? NULL : provider->context;
}

static void counter_function(
    sqlite3_context *context,
    int argc,
    sqlite3_value **argv
) {
    synthetic_state *state;
    int kind = (int)(intptr_t)sqlite3_user_data(context);
    if (argc != 1) {
        sqlite3_result_error(context, "provider name required", -1);
        return;
    }
    state = lookup_state(context, argv[0]);
    if (state == NULL) {
        sqlite3_result_null(context);
        return;
    }
    sqlite3_result_int64(context,
        kind == 1 ? state->validate_calls
        : kind == 2 ? state->embed_calls
        : state->embed_inputs
    );
}

static void last_inputs_function(
    sqlite3_context *context,
    int argc,
    sqlite3_value **argv
) {
    synthetic_state *state;
    if (argc != 1) {
        sqlite3_result_error(context, "provider name required", -1);
        return;
    }
    state = lookup_state(context, argv[0]);
    if (state == NULL || state->last_inputs == NULL) {
        sqlite3_result_null(context);
        return;
    }
    sqlite3_result_text(context, state->last_inputs, -1, SQLITE_TRANSIENT);
}

static void reset_function(
    sqlite3_context *context,
    int argc,
    sqlite3_value **argv
) {
    synthetic_state *state;
    if (argc != 1) {
        sqlite3_result_error(context, "provider name required", -1);
        return;
    }
    state = lookup_state(context, argv[0]);
    if (state == NULL) {
        sqlite3_result_int(context, 0);
        return;
    }
    state->validate_calls = 0;
    state->embed_calls = 0;
    state->embed_inputs = 0;
    sqlite3_free(state->last_inputs);
    state->last_inputs = NULL;
    sqlite3_result_int(context, 1);
}

static void destructor_calls_function(
    sqlite3_context *context,
    int argc,
    sqlite3_value **argv
) {
    (void)argv;
    if (argc != 0) {
        sqlite3_result_error(context, "no arguments expected", -1);
        return;
    }
    sqlite3_result_int64(context, read_counter(&destructor_calls));
}

static void global_counter_function(
    sqlite3_context *context,
    int argc,
    sqlite3_value **argv
) {
    (void)argv;
    if (argc != 0) {
        sqlite3_result_error(context, "no arguments expected", -1);
        return;
    }
    int kind = (int)(intptr_t)sqlite3_user_data(context);
    sqlite3_result_int64(
        context,
        kind == 1
            ? read_counter(&active_embed_calls)
            : read_counter(&non_autocommit_embed_calls)
    );
}

static int register_functions(sqlite3 *db) {
    const int flags = SQLITE_UTF8 | SQLITE_DIRECTONLY;
    int rc;

    rc = sqlite3_create_function_v2(
        db, "synthetic_embedding_validate_calls", 1, flags,
        (void *)(intptr_t)1, counter_function, NULL, NULL, NULL
    );
    if (rc != SQLITE_OK) {
        return rc;
    }
    rc = sqlite3_create_function_v2(
        db, "synthetic_embedding_embed_calls", 1, flags,
        (void *)(intptr_t)2, counter_function, NULL, NULL, NULL
    );
    if (rc != SQLITE_OK) {
        return rc;
    }
    rc = sqlite3_create_function_v2(
        db, "synthetic_embedding_embed_inputs", 1, flags,
        (void *)(intptr_t)3, counter_function, NULL, NULL, NULL
    );
    if (rc != SQLITE_OK) {
        return rc;
    }
    rc = sqlite3_create_function_v2(
        db, "synthetic_embedding_last_inputs", 1, flags,
        NULL, last_inputs_function, NULL, NULL, NULL
    );
    if (rc != SQLITE_OK) {
        return rc;
    }
    rc = sqlite3_create_function_v2(
        db, "synthetic_embedding_reset", 1, flags,
        NULL, reset_function, NULL, NULL, NULL
    );
    if (rc != SQLITE_OK) {
        return rc;
    }
    rc = sqlite3_create_function_v2(
        db, "synthetic_embedding_destructor_calls", 0, flags,
        NULL, destructor_calls_function, NULL, NULL, NULL
    );
    if (rc != SQLITE_OK) {
        return rc;
    }
    rc = sqlite3_create_function_v2(
        db, "synthetic_embedding_active_calls", 0, flags,
        (void *)(intptr_t)1, global_counter_function, NULL, NULL, NULL
    );
    if (rc != SQLITE_OK) {
        return rc;
    }
    return sqlite3_create_function_v2(
        db, "synthetic_embedding_non_autocommit_calls", 0, flags,
        (void *)(intptr_t)2, global_counter_function, NULL, NULL, NULL
    );
}

static void unregister_functions(sqlite3 *db) {
    static const struct {
        const char *name;
        int argc;
    } functions[] = {
        {"synthetic_embedding_validate_calls", 1},
        {"synthetic_embedding_embed_calls", 1},
        {"synthetic_embedding_embed_inputs", 1},
        {"synthetic_embedding_last_inputs", 1},
        {"synthetic_embedding_reset", 1},
        {"synthetic_embedding_destructor_calls", 0},
        {"synthetic_embedding_active_calls", 0},
        {"synthetic_embedding_non_autocommit_calls", 0},
    };
    size_t index;
    for (index = 0; index < sizeof(functions) / sizeof(functions[0]); ++index) {
        (void)sqlite3_create_function_v2(
            db,
            functions[index].name,
            functions[index].argc,
            SQLITE_UTF8,
            NULL,
            NULL,
            NULL,
            NULL,
            NULL
        );
    }
}

#ifdef _WIN32
__declspec(dllexport)
#endif
int sqlite3_syntheticembedding_init(
    sqlite3 *db,
    char **error,
    const sqlite3_api_routines *api
) {
    static const unsigned char identity_a[] = "synthetic-a/v1";
    static const unsigned char identity_b[] = "synthetic-b/v1";
    int rc;

    SQLITE_EXTENSION_INIT2(api);

    if (sqlite3_libversion_number() < 3044000) {
        if (error != NULL) {
            *error = sqlite3_mprintf("synthetic embedding provider requires SQLite 3.44+");
        }
        return SQLITE_ERROR;
    }
    if (sqlite3_get_clientdata(db, PROVIDER_PREFIX PROVIDER_A) != NULL
        || sqlite3_get_clientdata(db, PROVIDER_PREFIX PROVIDER_B) != NULL) {
        if (error != NULL) {
            *error = sqlite3_mprintf("synthetic embedding provider is already registered");
        }
        return SQLITE_ERROR;
    }
    rc = register_provider(db, PROVIDER_A, identity_a, error);
    if (rc != SQLITE_OK) {
        return rc;
    }
    rc = register_provider(db, PROVIDER_B, identity_b, error);
    if (rc != SQLITE_OK) {
        unregister_provider(db, PROVIDER_A);
        return rc;
    }
    rc = register_functions(db);
    if (rc != SQLITE_OK) {
        unregister_functions(db);
        unregister_provider(db, PROVIDER_B);
        unregister_provider(db, PROVIDER_A);
    }
    return rc;
}

#ifdef _WIN32
__declspec(dllexport)
#endif
int sqlite3_extension_init(
    sqlite3 *db,
    char **error,
    const sqlite3_api_routines *api
) {
    return sqlite3_syntheticembedding_init(db, error, api);
}
