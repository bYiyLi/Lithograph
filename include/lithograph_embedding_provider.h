#ifndef LITHOGRAPH_EMBEDDING_PROVIDER_H
#define LITHOGRAPH_EMBEDDING_PROVIDER_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define LITHOGRAPH_EMBEDDING_PROVIDER_ABI_VERSION_V1 1u
#define LITHOGRAPH_EMBEDDING_COORDINATE_FLOAT32_V1 1u

typedef enum lithograph_embedding_status_v1 {
    LITHOGRAPH_EMBEDDING_OK_V1 = 0,
    LITHOGRAPH_EMBEDDING_INVALID_CONFIG_V1 = 1,
    LITHOGRAPH_EMBEDDING_IO_ERROR_V1 = 2,
    LITHOGRAPH_EMBEDDING_RESOURCE_ERROR_V1 = 3,
    LITHOGRAPH_EMBEDDING_CANCELLED_V1 = 4,
    LITHOGRAPH_EMBEDDING_INTERNAL_ERROR_V1 = 5
} lithograph_embedding_status_v1;

typedef struct lithograph_embedding_text_v1 {
    const unsigned char *data;
    size_t len;
} lithograph_embedding_text_v1;

typedef struct lithograph_embedding_error_v1 {
    int status;
    unsigned char *message;
    size_t message_len;
} lithograph_embedding_error_v1;

typedef struct lithograph_embedding_batch_v1 {
    float *values;
    size_t value_count;
    size_t embedding_count;
    size_t dimensions;
} lithograph_embedding_batch_v1;

typedef int (*lithograph_embedding_cancel_callback_v1)(void *user_data);

typedef int (*lithograph_embedding_validate_v1)(
    void *context,
    const unsigned char *config_json,
    size_t config_len,
    size_t dimensions,
    uint32_t coordinate_type,
    lithograph_embedding_error_v1 *error
);

typedef int (*lithograph_embedding_embed_batch_v1)(
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
);

typedef void (*lithograph_embedding_free_batch_v1)(
    void *context,
    lithograph_embedding_batch_v1 *result
);

typedef void (*lithograph_embedding_free_error_v1)(
    void *context,
    lithograph_embedding_error_v1 *error
);

typedef struct lithograph_embedding_provider_v1 {
    uint32_t abi_version;
    size_t struct_size;
    void *context;
    const unsigned char *semantic_identity;
    size_t semantic_identity_len;
    lithograph_embedding_validate_v1 validate;
    lithograph_embedding_embed_batch_v1 embed_batch;
    lithograph_embedding_free_batch_v1 free_batch;
    lithograph_embedding_free_error_v1 free_error;
} lithograph_embedding_provider_v1;

#ifdef __cplusplus
}
#endif

#endif
