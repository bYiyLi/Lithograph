#ifndef LITHOGRAPH_H
#define LITHOGRAPH_H

#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct sqlite3 sqlite3;

typedef enum lithograph_event_kind_v1 {
    LITHOGRAPH_EVENT_COLUMNS_V1 = 1,
    LITHOGRAPH_EVENT_ROW_V1 = 2,
    LITHOGRAPH_EVENT_SUMMARY_V1 = 3
} lithograph_event_kind_v1;

typedef int (*lithograph_event_callback_v1)(
    void *user_data,
    lithograph_event_kind_v1 kind,
    const unsigned char *json,
    size_t json_len
);

int lithograph_v1_execute(
    sqlite3 *db,
    const char *query,
    size_t query_len,
    const char *params_json,
    size_t params_len,
    const char *options_json,
    size_t options_len,
    lithograph_event_callback_v1 callback,
    void *user_data,
    char **error_json
);

int lithograph_v1_validate(
    sqlite3 *db,
    const char *query,
    size_t query_len,
    char **error_json
);

int lithograph_v1_tx_begin(
    sqlite3 *db,
    const char *options_json,
    size_t options_len,
    char **result_json,
    char **error_json
);

int lithograph_v1_tx_execute(
    sqlite3 *db,
    const char *query,
    size_t query_len,
    const char *params_json,
    size_t params_len,
    const char *options_json,
    size_t options_len,
    lithograph_event_callback_v1 callback,
    void *user_data,
    char **error_json
);

int lithograph_v1_tx_commit(
    sqlite3 *db,
    char **result_json,
    char **error_json
);

int lithograph_v1_tx_abort(
    sqlite3 *db,
    char **error_json
);

void lithograph_v1_free(void *ptr);

#ifdef __cplusplus
}
#endif

#endif
