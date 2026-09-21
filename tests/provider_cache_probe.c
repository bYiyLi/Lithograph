
#include "sqlite3ext.h"
SQLITE_EXTENSION_INIT1

#include <stddef.h>
#include <string.h>

#include "lithograph_embedding_provider.h"

static const char *PROVIDER_KEY = "lithograph.embedding.v1/openai-compatible";

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
    return sqlite3_create_function_v2(
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
}
