
#include "sqlite3ext.h"
SQLITE_EXTENSION_INIT1

#include <stddef.h>
#include <stdio.h>
#include <string.h>

#include "lithograph_embedding_provider.h"

static const char *PROVIDER_KEY = "lithograph.embedding.v1/openai-compatible";

typedef struct initialization_barrier {
    const char *ready_path;
    const char *release_path;
    int observed;
    int failed;
    int cancel_on_retry;
    int checks_after_profile;
} initialization_barrier;

static initialization_barrier *active_barrier;

static FILE *open_marker(const char *path, const char *mode) {
#ifdef _WIN32
    FILE *file = NULL;
    return fopen_s(&file, path, mode) == 0 ? file : NULL;
#else
    return fopen(path, mode);
#endif
}

static int path_exists(const char *path) {
    FILE *file = open_marker(path, "rb");
    if (file == NULL) {
        return 0;
    }
    fclose(file);
    return 1;
}

static int cache_profile(unsigned int event, void *unused, void *statement, void *elapsed) {
    (void)unused;
    (void)elapsed;
    initialization_barrier *barrier = active_barrier;
    const char *sql = sqlite3_sql((sqlite3_stmt *)statement);
    int schema_inventory = sql != NULL &&
        (strstr(sql, "SELECT name FROM sqlite_schema WHERE type='table'") == sql ||
         strstr(sql, "SELECT name,type FROM sqlite_schema WHERE name NOT GLOB 'sqlite_*'") == sql);
    if (event != SQLITE_TRACE_PROFILE || barrier == NULL || barrier->observed || !schema_inventory) {
        return 0;
    }
    /* PROFILE runs after the empty schema SELECT has finished, before the
       Provider starts its separate, bounded BEGIN IMMEDIATE retry budget. */
    barrier->observed = 1;
    FILE *ready = open_marker(barrier->ready_path, "wb");
    if (ready == NULL || fclose(ready) != 0) {
        barrier->failed = 1;
        return 0;
    }
    /* Parent allows four seconds to collect all ready markers and another
       four seconds for the winner to publish before it resumes stale readers. */
    for (int waited = 0; waited < 15000; ++waited) {
        if (path_exists(barrier->release_path)) {
            return 0;
        }
        sqlite3_sleep(1);
    }
    barrier->failed = 1;
    return 0;
}

static int trace_cache_connection(sqlite3 *db, char **error, const sqlite3_api_routines *api) {
    (void)error;
    (void)api;
    return sqlite3_trace_v2(db, SQLITE_TRACE_PROFILE, cache_profile, NULL);
}

static int barrier_cancel(void *user_data) {
    initialization_barrier *barrier = (initialization_barrier *)user_data;
    if (barrier->observed) {
        barrier->checks_after_profile += 1;
    }
    return barrier->failed || (barrier->cancel_on_retry && barrier->checks_after_profile > 1);
}

typedef struct cancel_state {
    int calls;
    int cancel_after;
} cancel_state;

static int cancel_after_n_calls(void *user_data) {
    cancel_state *state = (cancel_state *)user_data;
    state->calls += 1;
    return state->calls >= state->cancel_after;
}

static int valid_status_args(int argc, sqlite3_value **argv) {
    return (argc == 3 || argc == 4) &&
        sqlite3_value_type(argv[0]) == SQLITE_TEXT &&
        sqlite3_value_type(argv[1]) == SQLITE_TEXT &&
        sqlite3_value_type(argv[2]) == SQLITE_INTEGER &&
        (argc != 4 || sqlite3_value_type(argv[3]) == SQLITE_INTEGER);
}

static lithograph_embedding_provider_v1 *provider_or_error(sqlite3_context *context) {
    sqlite3 *db = sqlite3_context_db_handle(context);
    lithograph_embedding_provider_v1 *provider =
        (lithograph_embedding_provider_v1 *)sqlite3_get_clientdata(db, PROVIDER_KEY);
    if (provider == NULL ||
        provider->abi_version != LITHOGRAPH_EMBEDDING_PROVIDER_ABI_VERSION_V1 ||
        provider->embed_batch == NULL ||
        provider->free_batch == NULL ||
        provider->free_error == NULL) {
        sqlite3_result_error(context, "openai-compatible provider is not registered", -1);
        return NULL;
    }
    return provider;
}

static int configure_cancel(
    sqlite3_context *context,
    int argc,
    sqlite3_value **argv,
    cancel_state *cancellation,
    lithograph_embedding_cancel_callback_v1 *callback,
    void **user_data
) {
    *callback = NULL;
    *user_data = NULL;
    if (argc != 4) {
        return 1;
    }
    sqlite3_int64 value = sqlite3_value_int64(argv[3]);
    if (value < 1 || value > 1000000) {
        sqlite3_result_error(context, "cancel_after must be between 1 and 1000000", -1);
        return 0;
    }
    cancellation->cancel_after = (int)value;
    *callback = cancel_after_n_calls;
    *user_data = cancellation;
    return 1;
}

static void free_probe_result(
    lithograph_embedding_provider_v1 *provider,
    lithograph_embedding_batch_v1 *result,
    lithograph_embedding_error_v1 *error
) {
    if (result->values != NULL || result->value_count != 0) {
        provider->free_batch(provider->context, result);
    }
    if (error->message != NULL || error->message_len != 0) {
        provider->free_error(provider->context, error);
    }
}

static void probe_status(sqlite3_context *context, int argc, sqlite3_value **argv) {
    if (!valid_status_args(argc, argv)) {
        sqlite3_result_error(context, "provider_cache_probe_status expects config TEXT, text TEXT, dimensions INTEGER [, cancel_after INTEGER]", -1);
        return;
    }
    lithograph_embedding_provider_v1 *provider = provider_or_error(context);
    if (provider == NULL) {
        return;
    }

    const unsigned char *config = sqlite3_value_text(argv[0]);
    const unsigned char *text = sqlite3_value_text(argv[1]);
    int config_len = sqlite3_value_bytes(argv[0]);
    int text_len = sqlite3_value_bytes(argv[1]);
    sqlite3_int64 dimensions_value = sqlite3_value_int64(argv[2]);
    if (config == NULL || text == NULL || config_len < 0 || text_len < 0 || dimensions_value < 1) {
        sqlite3_result_error(context, "invalid probe arguments", -1);
        return;
    }

    lithograph_embedding_text_v1 input = {
        .data = text,
        .len = (size_t)text_len,
    };
    lithograph_embedding_batch_v1 result = {0};
    lithograph_embedding_error_v1 error = {0};
    cancel_state cancellation = {0};
    lithograph_embedding_cancel_callback_v1 cancel_callback = NULL;
    void *cancel_user_data = NULL;
    if (!configure_cancel(
            context,
            argc,
            argv,
            &cancellation,
            &cancel_callback,
            &cancel_user_data
        )) {
        return;
    }
    int status = provider->embed_batch(
        provider->context,
        config,
        (size_t)config_len,
        &input,
        1,
        (size_t)dimensions_value,
        LITHOGRAPH_EMBEDDING_COORDINATE_FLOAT32_V1,
        cancel_callback,
        cancel_user_data,
        &result,
        &error
    );
    free_probe_result(provider, &result, &error);
    sqlite3_result_int(context, status);
}

static void probe_error(sqlite3_context *context, int argc, sqlite3_value **argv) {
    if (argc != 3 ||
        sqlite3_value_type(argv[0]) != SQLITE_TEXT ||
        sqlite3_value_type(argv[1]) != SQLITE_TEXT ||
        sqlite3_value_type(argv[2]) != SQLITE_INTEGER) {
        sqlite3_result_error(context, "provider_cache_probe_error expects config TEXT, text TEXT, dimensions INTEGER", -1);
        return;
    }

    sqlite3 *db = sqlite3_context_db_handle(context);
    lithograph_embedding_provider_v1 *provider =
        (lithograph_embedding_provider_v1 *)sqlite3_get_clientdata(db, PROVIDER_KEY);
    if (provider == NULL || provider->embed_batch == NULL ||
        provider->free_batch == NULL || provider->free_error == NULL) {
        sqlite3_result_error(context, "openai-compatible provider is not registered", -1);
        return;
    }

    const unsigned char *config = sqlite3_value_text(argv[0]);
    const unsigned char *text = sqlite3_value_text(argv[1]);
    int config_len = sqlite3_value_bytes(argv[0]);
    int text_len = sqlite3_value_bytes(argv[1]);
    sqlite3_int64 dimensions_value = sqlite3_value_int64(argv[2]);
    lithograph_embedding_text_v1 input = {
        .data = text,
        .len = (size_t)text_len,
    };
    lithograph_embedding_batch_v1 result = {0};
    lithograph_embedding_error_v1 error = {0};
    int status = provider->embed_batch(
        provider->context,
        config,
        (size_t)config_len,
        &input,
        1,
        (size_t)dimensions_value,
        LITHOGRAPH_EMBEDDING_COORDINATE_FLOAT32_V1,
        NULL,
        NULL,
        &result,
        &error
    );
    if (result.values != NULL || result.value_count != 0) {
        provider->free_batch(provider->context, &result);
    }
    if (status == LITHOGRAPH_EMBEDDING_OK_V1) {
        sqlite3_result_text(context, "OK", -1, SQLITE_STATIC);
    } else if (error.message != NULL) {
        sqlite3_result_text(
            context,
            (const char *)error.message,
            (int)error.message_len,
            SQLITE_TRANSIENT
        );
    } else {
        sqlite3_result_text(context, "provider returned no error message", -1, SQLITE_STATIC);
    }
    if (error.message != NULL || error.message_len != 0) {
        provider->free_error(provider->context, &error);
    }
}

static int valid_vector_args(int argc, sqlite3_value **argv) {
    return (argc == 3 || argc == 6) &&
        sqlite3_value_type(argv[0]) == SQLITE_TEXT &&
        sqlite3_value_type(argv[1]) == SQLITE_TEXT &&
        sqlite3_value_type(argv[2]) == SQLITE_INTEGER &&
        (argc != 6 || (sqlite3_value_type(argv[3]) == SQLITE_TEXT &&
                       sqlite3_value_type(argv[4]) == SQLITE_TEXT &&
                       sqlite3_value_type(argv[5]) == SQLITE_INTEGER));
}

static void probe_vector(sqlite3_context *context, int argc, sqlite3_value **argv) {
    if (!valid_vector_args(argc, argv)) {
        sqlite3_result_error(context, "vector probe expects config, text, dimensions [, ready, release, cancel_on_retry]", -1);
        return;
    }
    lithograph_embedding_provider_v1 *provider = provider_or_error(context);
    if (provider == NULL) {
        return;
    }
    sqlite3_int64 dimensions = sqlite3_value_int64(argv[2]);
    if (dimensions < 1 || dimensions > 4096) {
        sqlite3_result_error(context, "invalid vector probe dimensions", -1);
        return;
    }
    initialization_barrier barrier = {0};
    if (argc == 6) {
        barrier.ready_path = (const char *)sqlite3_value_text(argv[3]);
        barrier.release_path = (const char *)sqlite3_value_text(argv[4]);
        barrier.cancel_on_retry = sqlite3_value_int(argv[5]) != 0;
        active_barrier = &barrier;
        int rc = sqlite3_auto_extension((void (*)(void))trace_cache_connection);
        if (rc != SQLITE_OK) {
            sqlite3_cancel_auto_extension((void (*)(void))trace_cache_connection);
            active_barrier = NULL;
            sqlite3_result_error_code(context, rc);
            return;
        }
    }
    lithograph_embedding_text_v1 input = {
        .data = sqlite3_value_text(argv[1]),
        .len = (size_t)sqlite3_value_bytes(argv[1]),
    };
    lithograph_embedding_batch_v1 result = {0};
    lithograph_embedding_error_v1 error = {0};
    int status = provider->embed_batch(
        provider->context,
        sqlite3_value_text(argv[0]),
        (size_t)sqlite3_value_bytes(argv[0]),
        &input, 1, (size_t)dimensions,
        LITHOGRAPH_EMBEDDING_COORDINATE_FLOAT32_V1,
        argc == 6 ? barrier_cancel : NULL,
        argc == 6 ? &barrier : NULL,
        &result, &error
    );
    if (argc == 6) {
        sqlite3_cancel_auto_extension((void (*)(void))trace_cache_connection);
        active_barrier = NULL;
    }
    if (argc == 6 && (!barrier.observed || barrier.failed)) {
        free_probe_result(provider, &result, &error);
        sqlite3_result_error(context, "cache schema PROFILE barrier was not reached or timed out", -1);
        return;
    }
    sqlite3_str *output = sqlite3_str_new(sqlite3_context_db_handle(context));
    sqlite3_str_appendf(output, "status=%d;dimensions=%llu;count=%llu;values=",
        status, (unsigned long long)result.dimensions, (unsigned long long)result.value_count);
    for (size_t i = 0; result.values != NULL && i < result.value_count; ++i) {
        sqlite3_str_appendf(output, "%s%.9g", i == 0 ? "" : ",", (double)result.values[i]);
    }
    sqlite3_str_appendall(output, ";error=");
    if (error.message != NULL) {
        sqlite3_str_append(output, (const char *)error.message, (int)error.message_len);
    }
    free_probe_result(provider, &result, &error);
    sqlite3_result_text(context, sqlite3_str_finish(output), -1, sqlite3_free);
}

static void probe_sql_error(sqlite3_context *context, int argc, sqlite3_value **argv) {
    if (argc != 1 || sqlite3_value_type(argv[0]) != SQLITE_TEXT) {
        sqlite3_result_error(context, "SQL error probe expects one SQL TEXT argument", -1);
        return;
    }
    sqlite3 *db = sqlite3_context_db_handle(context);
    sqlite3_stmt *statement = NULL;
    int rc = sqlite3_prepare_v2(db, (const char *)sqlite3_value_text(argv[0]),
        sqlite3_value_bytes(argv[0]), &statement, NULL);
    if (rc == SQLITE_OK && statement != NULL) {
        do {
            rc = sqlite3_step(statement);
        } while (rc == SQLITE_ROW);
    }
    /* Finalize may alter the connection's error state. Capture the actual
       prepare/step result and message from this one execution first. */
    char *output = sqlite3_mprintf("code=%d;error=%s", rc & 0xff, sqlite3_errmsg(db));
    sqlite3_finalize(statement);
    if (output == NULL) {
        sqlite3_result_error_nomem(context);
        return;
    }
    sqlite3_result_text(context, output, -1, sqlite3_free);
}

#ifdef _WIN32
__declspec(dllexport)
#endif
int sqlite3_providercacheprobe_init(
    sqlite3 *db,
    char **error,
    const sqlite3_api_routines *api
) {
    (void)error;
    SQLITE_EXTENSION_INIT2(api);
    int rc = sqlite3_create_function_v2(
        db,
        "provider_cache_probe_status",
        3,
        SQLITE_UTF8 | SQLITE_DIRECTONLY,
        NULL,
        probe_status,
        NULL,
        NULL,
        NULL
    );
    if (rc != SQLITE_OK) {
        return rc;
    }
    rc = sqlite3_create_function_v2(
        db,
        "provider_cache_probe_status_cancel",
        4,
        SQLITE_UTF8 | SQLITE_DIRECTONLY,
        NULL,
        probe_status,
        NULL,
        NULL,
        NULL
    );
    if (rc != SQLITE_OK) {
        return rc;
    }
    rc = sqlite3_create_function_v2(
        db,
        "provider_cache_probe_error",
        3,
        SQLITE_UTF8 | SQLITE_DIRECTONLY,
        NULL,
        probe_error,
        NULL,
        NULL,
        NULL
    );
    if (rc != SQLITE_OK) {
        return rc;
    }
    rc = sqlite3_create_function_v2(db, "provider_cache_probe_vector", 3,
        SQLITE_UTF8 | SQLITE_DIRECTONLY, NULL, probe_vector, NULL, NULL, NULL);
    if (rc != SQLITE_OK) {
        return rc;
    }
    rc = sqlite3_create_function_v2(db, "provider_cache_probe_barrier", 6,
        SQLITE_UTF8 | SQLITE_DIRECTONLY, NULL, probe_vector, NULL, NULL, NULL);
    if (rc != SQLITE_OK) {
        return rc;
    }
    return sqlite3_create_function_v2(db, "provider_cache_probe_sql_error", 1,
        SQLITE_UTF8 | SQLITE_DIRECTONLY, NULL, probe_sql_error, NULL, NULL, NULL);
}
