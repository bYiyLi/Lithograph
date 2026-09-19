#include <math.h>
#include <sqlite3.h>
#include <stddef.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "lithograph_embedding_provider.h"

static const char *provider_a_key = "lithograph.embedding.v1/synthetic-a";
static const char *provider_b_key = "lithograph.embedding.v1/synthetic-b";

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
    return sqlite3_load_extension(db, path, "sqlite3_syntheticembedding_init", error);
}

static sqlite3_int64 scalar_int64(sqlite3 *db, const char *sql) {
    sqlite3_stmt *statement = NULL;
    require(sqlite3_prepare_v2(db, sql, -1, &statement, NULL) == SQLITE_OK, "prepare failed");
    require(sqlite3_step(statement) == SQLITE_ROW, "scalar returned no row");
    sqlite3_int64 value = sqlite3_column_int64(statement, 0);
    sqlite3_finalize(statement);
    return value;
}

static int always_cancel(void *user_data) {
    (void)user_data;
    return 1;
}

static void check_provider_contract(
    sqlite3 *db,
    lithograph_embedding_provider_v1 *provider
) {
    require(provider != NULL, "synthetic provider registration is missing");
    require(
        provider->abi_version == LITHOGRAPH_EMBEDDING_PROVIDER_ABI_VERSION_V1,
        "synthetic provider ABI version mismatch"
    );
    require(
        provider->struct_size >= sizeof(lithograph_embedding_provider_v1),
        "synthetic provider ABI struct is too small"
    );
    require(provider->context != NULL, "synthetic provider context is missing");
    require(
        provider->semantic_identity != NULL && provider->semantic_identity_len > 0,
        "synthetic provider semantic identity is missing"
    );
    require(provider->validate != NULL, "synthetic provider validate callback is missing");
    require(provider->embed_batch != NULL, "synthetic provider embed callback is missing");
    require(provider->free_batch != NULL, "synthetic provider free_batch callback is missing");
    require(provider->free_error != NULL, "synthetic provider free_error callback is missing");

    const char valid_config[] = "{}";
    lithograph_embedding_error_v1 error = {0};
    int rc = provider->validate(
        provider->context,
        (const unsigned char *)valid_config,
        strlen(valid_config),
        3,
        LITHOGRAPH_EMBEDDING_COORDINATE_FLOAT32_V1,
        &error
    );
    require(rc == LITHOGRAPH_EMBEDDING_OK_V1, "synthetic valid config was rejected");
    require(error.message == NULL, "synthetic successful validation returned an error");

    const char invalid_config[] = "{\"validate\":\"invalid\"}";
    rc = provider->validate(
        provider->context,
        (const unsigned char *)invalid_config,
        strlen(invalid_config),
        3,
        LITHOGRAPH_EMBEDDING_COORDINATE_FLOAT32_V1,
        &error
    );
    require(
        rc == LITHOGRAPH_EMBEDDING_INVALID_CONFIG_V1,
        "synthetic invalid config returned the wrong status"
    );
    require(error.message != NULL && error.message_len != 0, "synthetic validation error missing");
    provider->free_error(provider->context, &error);
    require(error.message == NULL, "synthetic free_error did not clear the error");

    static const unsigned char alpha[] = "alpha";
    static const unsigned char beta[] = "beta";
    const lithograph_embedding_text_v1 texts[] = {
        {alpha, sizeof(alpha) - 1},
        {beta, sizeof(beta) - 1},
    };
    lithograph_embedding_batch_v1 batch = {0};
    rc = provider->embed_batch(
        provider->context,
        (const unsigned char *)valid_config,
        strlen(valid_config),
        texts,
        2,
        3,
        LITHOGRAPH_EMBEDDING_COORDINATE_FLOAT32_V1,
        NULL,
        NULL,
        &batch,
        &error
    );
    require(rc == LITHOGRAPH_EMBEDDING_OK_V1, "synthetic embedBatch failed");
    require(batch.embedding_count == 2, "synthetic embedding count differs");
    require(batch.dimensions == 3, "synthetic embedding dimensions differ");
    require(batch.value_count == 6, "synthetic embedding value count differs");
    require(batch.values != NULL, "synthetic embedding payload is missing");
    for (size_t index = 0; index < batch.value_count; ++index) {
        require(isfinite(batch.values[index]), "synthetic embedding is not finite");
    }
    provider->free_batch(provider->context, &batch);
    require(
        batch.values == NULL && batch.value_count == 0,
        "synthetic free_batch did not clear output"
    );

    rc = provider->embed_batch(
        provider->context,
        (const unsigned char *)valid_config,
        strlen(valid_config),
        texts,
        2,
        3,
        LITHOGRAPH_EMBEDDING_COORDINATE_FLOAT32_V1,
        always_cancel,
        NULL,
        &batch,
        &error
    );
    require(
        rc == LITHOGRAPH_EMBEDDING_CANCELLED_V1,
        "synthetic cancellation returned the wrong status"
    );
    provider->free_error(provider->context, &error);
    require(
        scalar_int64(db, "SELECT synthetic_embedding_validate_calls('synthetic-a')") >= 2,
        "synthetic validate counter did not advance"
    );
    require(
        scalar_int64(db, "SELECT synthetic_embedding_embed_calls('synthetic-a')") >= 2,
        "synthetic embed counter did not advance"
    );
}

int main(int argc, char **argv) {
    if (argc != 2) {
        fprintf(stderr, "usage: %s <synthetic-provider-path>\n", argv[0]);
        return 2;
    }

    sqlite3 *first = NULL;
    require(sqlite3_open(":memory:", &first) == SQLITE_OK, "failed to open first SQLite connection");
    require(
        sqlite3_get_clientdata(first, provider_a_key) == NULL,
        "synthetic provider leaked into a connection before load"
    );
    char *error = NULL;
    require(
        load_provider(first, argv[1], &error) == SQLITE_OK,
        error == NULL ? "provider load failed" : error
    );
    sqlite3_free(error);
    error = NULL;

    lithograph_embedding_provider_v1 *provider_a =
        sqlite3_get_clientdata(first, provider_a_key);
    lithograph_embedding_provider_v1 *provider_b =
        sqlite3_get_clientdata(first, provider_b_key);
    require(provider_b != NULL, "second synthetic provider registration is missing");
    check_provider_contract(first, provider_a);

    require(
        load_provider(first, argv[1], &error) != SQLITE_OK,
        "duplicate synthetic load succeeded"
    );
    sqlite3_free(error);
    error = NULL;
    require(
        sqlite3_get_clientdata(first, provider_a_key) == provider_a,
        "duplicate synthetic load replaced provider A"
    );
    require(
        sqlite3_get_clientdata(first, provider_b_key) == provider_b,
        "duplicate synthetic load replaced provider B"
    );

    sqlite3 *second = NULL;
    require(sqlite3_open(":memory:", &second) == SQLITE_OK, "failed to open second SQLite connection");
    require(
        sqlite3_get_clientdata(second, provider_a_key) == NULL,
        "synthetic provider registration leaked across SQLite connections"
    );
    require(
        load_provider(second, argv[1], &error) == SQLITE_OK,
        error == NULL ? "second load failed" : error
    );
    sqlite3_free(error);
    error = NULL;
    require(
        scalar_int64(second, "SELECT synthetic_embedding_destructor_calls()") == 0,
        "provider destructor ran before its owning SQLite connection closed"
    );
    require(sqlite3_close(first) == SQLITE_OK, "failed to close first SQLite connection");
    require(
        scalar_int64(second, "SELECT synthetic_embedding_destructor_calls()") == 2,
        "first connection did not destroy both provider registrations exactly once"
    );
    require(sqlite3_close(second) == SQLITE_OK, "failed to close second SQLite connection");

    puts("synthetic embedding provider smoke passed");
    return 0;
}
