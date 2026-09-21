/*
 * Current Phase 15 C-host example.
 *
 * Lithograph no longer exposes an application query C ABI. C/C++ programs use
 * the standard SQLite API, load the extension, and execute the same SQL surface
 * as every other driver.
 */
#include <sqlite3.h>
#include <stdio.h>
#include <stdlib.h>

static void die(sqlite3 *db, const char *message) {
    fprintf(stderr, "%s: %s\n", message, sqlite3_errmsg(db));
    exit(1);
}

static void exec_ok(sqlite3 *db, const char *sql) {
    char *error = NULL;
    int rc = sqlite3_exec(db, sql, NULL, NULL, &error);
    if (rc != SQLITE_OK) {
        fprintf(stderr, "%s\n", error == NULL ? sqlite3_errmsg(db) : error);
        sqlite3_free(error);
        exit(1);
    }
    sqlite3_free(error);
}

int main(int argc, char **argv) {
    if (argc != 2) {
        fprintf(stderr, "usage: native_transaction /absolute/path/lithograph.extension\n");
        return 2;
    }
    sqlite3 *db = NULL;
    if (sqlite3_open(":memory:", &db) != SQLITE_OK) {
        die(db, "open failed");
    }
    if (sqlite3_enable_load_extension(db, 1) != SQLITE_OK) {
        die(db, "enable load extension failed");
    }
    char *error = NULL;
    if (sqlite3_load_extension(db, argv[1], "sqlite3_lithograph_init", &error) != SQLITE_OK) {
        fprintf(stderr, "%s\n", error == NULL ? "load failed" : error);
        sqlite3_free(error);
        return 1;
    }
    sqlite3_free(error);

    exec_ok(db, "SELECT lithograph_init()");
    exec_ok(db, "SELECT lithograph_tx_begin('{\"author\":\"c-host\"}')");
    exec_ok(db, "SELECT lithograph('CREATE (:Person {name:''A''}) FINISH')");
    exec_ok(db, "SELECT lithograph('CREATE (:Person {name:''B''}) FINISH')");
    exec_ok(db, "SELECT lithograph_tx_commit()");

    if (sqlite3_close(db) != SQLITE_OK) {
        die(db, "close failed");
    }
    puts("PASS: C host used SQL-only Lithograph execution");
    return 0;
}
