#include <sqlite3.h>
#include <stddef.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "lithograph_embedding_provider.h"

static const char *provider_key = "lithograph.embedding.v1/openai-compatible";

static void fail(const char *message) {
    fprintf(stderr, "%s\n", message);
    exit(1);
}

static void require(int condition, const char *message) {
    if (!condition) {
        fail(message);
    }
}

static int load_provider(sqlite3 *db, const char *path, char **error) {
    require(
        sqlite3_enable_load_extension(db, 1) == SQLITE_OK,
        "failed to enable SQLite extension loading"
    );
    return sqlite3_load_extension(db, path, "sqlite3_extension_init", error);
}

int main(int argc, char **argv) {
    if (argc != 2) {
        fprintf(stderr, "usage: %s <openai-compatible-provider-path>\n", argv[0]);
        return 2;
    }

    sqlite3 *db = NULL;
    require(sqlite3_open(":memory:", &db) == SQLITE_OK, "failed to open SQLite database");

    char *error = NULL;
    int rc = load_provider(db, argv[1], &error);
    if (rc != SQLITE_OK) {
        fprintf(stderr, "provider load failed: %s\n", error == NULL ? "unknown" : error);
        sqlite3_free(error);
        sqlite3_close(db);
        return 1;
    }
    sqlite3_free(error);
    error = NULL;

    lithograph_embedding_provider_v1 *provider =
        sqlite3_get_clientdata(db, provider_key);
    require(provider != NULL, "provider client-data registration is missing");
    require(
        provider->abi_version == LITHOGRAPH_EMBEDDING_PROVIDER_ABI_VERSION_V1,
        "provider ABI version mismatch"
    );
    require(
        provider->struct_size >= sizeof(lithograph_embedding_provider_v1),
        "provider ABI struct is too small"
    );
    require(provider->context != NULL, "provider context is missing");
    require(
        provider->semantic_identity != NULL && provider->semantic_identity_len > 0,
        "provider semantic identity is missing"
    );
    require(provider->validate != NULL, "provider validate callback is missing");
    require(provider->embed_batch != NULL, "provider embed callback is missing");
    require(provider->free_batch != NULL, "provider free_batch callback is missing");
    require(provider->free_error != NULL, "provider free_error callback is missing");

    const char valid_config[] = "{\"model\":\"smoke-model\"}";
    lithograph_embedding_error_v1 provider_error = {0};
    rc = provider->validate(
        provider->context,
        (const unsigned char *)valid_config,
        strlen(valid_config),
        3,
        LITHOGRAPH_EMBEDDING_COORDINATE_FLOAT32_V1,
        &provider_error
    );
    require(rc == LITHOGRAPH_EMBEDDING_OK_V1, "valid provider config was rejected");
    require(provider_error.message == NULL, "successful validation returned an error message");

    rc = provider->validate(
        provider->context,
        (const unsigned char *)valid_config,
        strlen(valid_config),
        4097,
        LITHOGRAPH_EMBEDDING_COORDINATE_FLOAT32_V1,
        &provider_error
    );
    require(
        rc == LITHOGRAPH_EMBEDDING_INVALID_CONFIG_V1,
        "out-of-range dimensions returned the wrong status"
    );
    provider->free_error(provider->context, &provider_error);

    const char invalid_config[] = "{\"model\":\"\"}";
    rc = provider->validate(
        provider->context,
        (const unsigned char *)invalid_config,
        strlen(invalid_config),
        3,
        LITHOGRAPH_EMBEDDING_COORDINATE_FLOAT32_V1,
        &provider_error
    );
    require(
        rc == LITHOGRAPH_EMBEDDING_INVALID_CONFIG_V1,
        "invalid provider config returned the wrong status"
    );
    require(
        provider_error.message != NULL && provider_error.message_len > 0,
        "invalid provider config did not return an error message"
    );
    provider->free_error(provider->context, &provider_error);
    require(provider_error.message == NULL, "provider error was not cleared after free_error");

    rc = load_provider(db, argv[1], &error);
    require(rc != SQLITE_OK, "duplicate provider registration unexpectedly succeeded");
    sqlite3_free(error);
    require(
        sqlite3_get_clientdata(db, provider_key) == provider,
        "duplicate registration replaced the original provider"
    );

    require(sqlite3_close(db) == SQLITE_OK, "failed to close SQLite database");
    puts("openai-compatible provider smoke passed");
    return 0;
}
