#include "sqlite3.h"

#include <math.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static const char *no_external = "{\"readAccess\":{\"allowExternalIO\":false}}";
static const char *hidden_source =
    "{\"readAccess\":{\"query\":{\"denyProperties\":[\"text\"]}}}";
static const char *semantic_node_create =
    "CALL db.index.semantic.createNodeIndex('doc_sem',['Doc'],'text',"
    "{provider:'synthetic-a',providerConfig:{},dimensions:4,similarity:'cosine'})";
static const char *semantic_rel_create =
    "CALL db.index.semantic.createRelationshipIndex('rel_sem',['LINK'],'text',"
    "{provider:'synthetic-b',providerConfig:{},dimensions:4,similarity:'cosine'})";
static const char *fulltext_create =
    "CREATE FULLTEXT INDEX doc_fts FOR (n:Doc) ON EACH [n.text] "
    "OPTIONS {indexConfig:{`fulltext.analyzer`:'phase12_echo'}}";
static const char *fulltext_rel_create =
    "CREATE FULLTEXT INDEX rel_fts FOR ()-[r:LINK]-() ON EACH [r.text] "
    "OPTIONS {indexConfig:{`fulltext.analyzer`:'phase12_echo'}}";
static const char *semantic_node_query =
    "CALL db.index.semantic.queryNodes('doc_sem','graph',{limit:1}) "
    "YIELD node RETURN node.text";
static const char *semantic_rel_query =
    "CALL db.index.semantic.queryRelationships('rel_sem','graph',{limit:1}) "
    "YIELD relationship RETURN relationship.text";
static const char *fulltext_query =
    "CALL db.index.fulltext.queryNodes('doc_fts','graph') YIELD node RETURN node.text";
static const char *fulltext_rel_query =
    "CALL db.index.fulltext.queryRelationships('rel_fts','graph') "
    "YIELD relationship RETURN relationship.text";

static void fail(sqlite3 *db, const char *message) {
  fprintf(stderr, "Phase 18 external-access smoke: %s: %s\n", message,
          db == NULL ? "" : sqlite3_errmsg(db));
  exit(1);
}

static void require(sqlite3 *db, int condition, const char *message) {
  if (!condition) {
    fail(db, message);
  }
}

static void sql(sqlite3 *db, const char *text) {
  if (sqlite3_exec(db, text, NULL, NULL, NULL) != SQLITE_OK) {
    fail(db, text);
  }
}

static sqlite3_int64 integer(sqlite3 *db, const char *text) {
  sqlite3_stmt *statement = NULL;
  sqlite3_int64 result;
  require(db, sqlite3_prepare_v2(db, text, -1, &statement, NULL) == SQLITE_OK,
          "prepare counter read");
  require(db, sqlite3_step(statement) == SQLITE_ROW, "read counter");
  result = sqlite3_column_int64(statement, 0);
  require(db, sqlite3_finalize(statement) == SQLITE_OK, "finalize counter read");
  return result;
}

static char *execute(sqlite3 *db, const char *query, const char *options) {
  sqlite3_stmt *statement = NULL;
  char *result;
  require(db,
          sqlite3_prepare_v2(db, "SELECT lithograph(?1,'{}',?2)", -1,
                             &statement, NULL) == SQLITE_OK,
          "prepare scalar call");
  sqlite3_bind_text(statement, 1, query, -1, SQLITE_STATIC);
  sqlite3_bind_text(statement, 2, options, -1, SQLITE_STATIC);
  if (sqlite3_step(statement) != SQLITE_ROW) {
    fprintf(stderr, "query: %s\n", query);
    fail(db, "execute scalar call");
  }
  result = sqlite3_mprintf("%s", sqlite3_column_text(statement, 0));
  require(db, result != NULL, "copy query result");
  require(db, sqlite3_step(statement) == SQLITE_DONE, "consume scalar call");
  require(db, sqlite3_finalize(statement) == SQLITE_OK, "finalize scalar call");
  return result;
}

static void call(sqlite3 *db, const char *query, const char *options) {
  sqlite3_free(execute(db, query, options));
}

static char *json_text(sqlite3 *db, const char *json, const char *path) {
  sqlite3_stmt *statement = NULL;
  char *result;
  require(db,
          sqlite3_prepare_v2(db, "SELECT json_extract(?1,?2)", -1,
                             &statement, NULL) == SQLITE_OK,
          "prepare result extraction");
  sqlite3_bind_text(statement, 1, json, -1, SQLITE_STATIC);
  sqlite3_bind_text(statement, 2, path, -1, SQLITE_STATIC);
  require(db, sqlite3_step(statement) == SQLITE_ROW, "extract result");
  require(db, sqlite3_column_type(statement, 0) == SQLITE_TEXT,
          "result must contain a string");
  result = sqlite3_mprintf("%s", sqlite3_column_text(statement, 0));
  require(db, result != NULL, "copy extracted string");
  require(db, sqlite3_finalize(statement) == SQLITE_OK, "finalize result extraction");
  return result;
}

static sqlite3_int64 json_integer(sqlite3 *db, const char *json, const char *path) {
  sqlite3_stmt *statement = NULL;
  sqlite3_int64 result;
  require(db,
          sqlite3_prepare_v2(db, "SELECT json_extract(?1,?2)", -1,
                             &statement, NULL) == SQLITE_OK,
          "prepare integer result extraction");
  sqlite3_bind_text(statement, 1, json, -1, SQLITE_STATIC);
  sqlite3_bind_text(statement, 2, path, -1, SQLITE_STATIC);
  require(db, sqlite3_step(statement) == SQLITE_ROW &&
              sqlite3_column_type(statement, 0) == SQLITE_INTEGER,
          "result must contain an integer");
  result = sqlite3_column_int64(statement, 0);
  require(db, sqlite3_finalize(statement) == SQLITE_OK, "finalize integer extraction");
  return result;
}

static char *head(sqlite3 *db) {
  char *response = execute(
      db, "CALL lithograph.commit.get('branch/main') YIELD commit RETURN commit", "{}");
  char *result = json_text(db, response, "$.rows[0][0]");
  sqlite3_free(response);
  return result;
}

static void reset_counters(sqlite3 *db) {
  sql(db, "SELECT synthetic_embedding_reset('synthetic-a'),"
          "synthetic_embedding_reset('synthetic-b'),phase12_tokenizer_reset()");
}

static void no_callbacks(sqlite3 *db) {
  require(db,
          integer(db,
                  "SELECT synthetic_embedding_validate_calls('synthetic-a') + "
                  "synthetic_embedding_validate_calls('synthetic-b')") == 0,
          "Provider validate must not be called");
  require(db,
          integer(db,
                  "SELECT synthetic_embedding_embed_calls('synthetic-a') + "
                  "synthetic_embedding_embed_calls('synthetic-b')") == 0,
          "Provider embed must not be called");
  require(db, integer(db, "SELECT phase12_tokenizer_stat('create')") == 0,
          "host tokenizer xCreate must not be called");
  require(db,
          integer(db,
                  "SELECT phase12_tokenizer_stat('document') + "
                  "phase12_tokenizer_stat('query') + "
                  "phase12_tokenizer_stat('prefix') + "
                  "phase12_tokenizer_stat('aux')") == 0,
          "host tokenizer xTokenize must not be called");
}

static void denied_surface(sqlite3 *db, const char *query, const char *options,
                           int rows) {
  char *before = head(db);
  sqlite3_int64 commits = integer(db, "SELECT count(*) FROM _lithograph_commits");
  sqlite3_int64 layers = integer(db, "SELECT count(*) FROM _lithograph_layers");
  sqlite3_int64 schemas = integer(db, "SELECT count(*) FROM _lithograph_schema_objects");
  {
    sqlite3_stmt *statement = NULL;
    int rc;
    const char *statement_sql = rows == 0
        ? "SELECT lithograph(?1,'{}',?2)"
        : "SELECT event,data FROM lithograph_rows(?1,'{}',?2)";
    reset_counters(db);
    require(db,
            sqlite3_prepare_v2(db, statement_sql, -1, &statement, NULL) == SQLITE_OK,
            "prepare denied call");
    sqlite3_bind_text(statement, 1, query, -1, SQLITE_STATIC);
    sqlite3_bind_text(statement, 2, options, -1, SQLITE_STATIC);
    do {
      rc = sqlite3_step(statement);
    } while (rc == SQLITE_ROW);
    if ((rc & 0xff) != SQLITE_AUTH ||
        strstr(sqlite3_errmsg(db), "LITHOGRAPH_ACCESS_DENIED:") == NULL) {
      fprintf(stderr, "query: %s\n", query);
      fail(db, rows == 0 ? "scalar must return ACCESS_DENIED / SQLITE_AUTH"
                        : "rows must return ACCESS_DENIED / SQLITE_AUTH");
    }
    sqlite3_finalize(statement);
    no_callbacks(db);
    {
      char *after = head(db);
      require(db, strcmp(before, after) == 0, "denial must preserve Branch head");
      sqlite3_free(after);
    }
  }
  require(db, integer(db, "SELECT count(*) FROM _lithograph_commits") == commits,
          "denial must not retain intermediate Commits");
  require(db, integer(db, "SELECT count(*) FROM _lithograph_layers") == layers,
          "denial must not retain intermediate Layers");
  require(db, integer(db, "SELECT count(*) FROM _lithograph_schema_objects") == schemas,
          "denial must not retain partial Schema objects");
  sqlite3_free(before);
}

static void denied(sqlite3 *db, const char *query, const char *options) {
  int rows;
  for (rows = 0; rows < 2; rows++) {
    denied_surface(db, query, options, rows);
  }
}

static void call_surface(sqlite3 *db, const char *query, const char *options,
                         int rows) {
  sqlite3_stmt *statement = NULL;
  int rc;
  int summaries = 0;
  if (!rows) {
    call(db, query, options);
    return;
  }
  require(db,
          sqlite3_prepare_v2(db,
                            "SELECT event,data FROM lithograph_rows(?1,'{}',?2)",
                            -1, &statement, NULL) == SQLITE_OK,
          "prepare allowed rows call");
  sqlite3_bind_text(statement, 1, query, -1, SQLITE_STATIC);
  sqlite3_bind_text(statement, 2, options, -1, SQLITE_STATIC);
  while ((rc = sqlite3_step(statement)) == SQLITE_ROW) {
    if (strcmp((const char *)sqlite3_column_text(statement, 0), "summary") == 0) {
      summaries++;
    }
  }
  require(db, rc == SQLITE_DONE && summaries == 1,
          "allowed rows call must reach its sole successful summary");
  require(db, sqlite3_finalize(statement) == SQLITE_OK, "finalize allowed rows call");
}

static void allowed_without_callbacks(sqlite3 *db, const char *query) {
  reset_counters(db);
  call(db, query, no_external);
  no_callbacks(db);
}

static void load(sqlite3 *db, const char *path, const char *entrypoint) {
  char *error = NULL;
  if (sqlite3_load_extension(db, path, entrypoint, &error) != SQLITE_OK) {
    fprintf(stderr, "load %s: %s\n", path, error == NULL ? "" : error);
    sqlite3_free(error);
    fail(db, "load extension fixture");
  }
}

static sqlite3 *fixture(const char *extension, const char *provider,
                        const char *tokenizer) {
  sqlite3 *db = NULL;
  require(NULL, sqlite3_open(":memory:", &db) == SQLITE_OK, "open temporary graph");
  require(db, sqlite3_enable_load_extension(db, 1) == SQLITE_OK,
          "enable trusted fixture loading");
  load(db, extension, "sqlite3_lithograph_init");
  load(db, provider, "sqlite3_syntheticembedding_init");
  load(db, tokenizer, "sqlite3_phase12_tokenizer_init");
  require(db, sqlite3_enable_load_extension(db, 0) == SQLITE_OK,
          "disable extension loading");
  sql(db, "SELECT lithograph_init()");
  call(db,
       "CREATE (a:Doc {id:'a',text:'graph document'}),"
       "(b:Doc {id:'b',text:'graph relationship'}),"
       "(a)-[:LINK {text:'graph link'}]->(b) FINISH", "{}");
  return db;
}

static void allowed_semantic_search(sqlite3 *db, const char *query,
                                    const char *provider) {
  char *response;
  char *value;
  char *validate_query = sqlite3_mprintf(
      "SELECT synthetic_embedding_validate_calls('%q')", provider);
  char *embed_query = sqlite3_mprintf(
      "SELECT synthetic_embedding_embed_calls('%q')", provider);
  require(db, validate_query != NULL && embed_query != NULL,
          "allocate Provider counter queries");
  reset_counters(db);
  response = execute(db, query, "{}");
  value = json_text(db, response, "$.rows[0][0]");
  require(db, strncmp(value, "graph", 5) == 0, "allowed Semantic query must find a graph hit");
  require(db, integer(db, validate_query) > 0,
          "each allowed Semantic query must validate its real Provider");
  require(db, integer(db, embed_query) > 0,
          "each allowed Semantic query must embed through its real Provider");
  sqlite3_free(value);
  sqlite3_free(response);
  sqlite3_free(embed_query);
  sqlite3_free(validate_query);
}

static void allowed_fulltext_search(sqlite3 *db, const char *query) {
  char *response;
  char *value;
  reset_counters(db);
  response = execute(db, query, "{}");
  value = json_text(db, response, "$.rows[0][0]");
  require(db, strncmp(value, "graph", 5) == 0, "allowed FULLTEXT query must find a graph hit");
  require(db, integer(db, "SELECT phase12_tokenizer_stat('create')") > 0,
          "each allowed FULLTEXT query must create the registered tokenizer");
  require(db, integer(db, "SELECT phase12_tokenizer_stat('query')") > 0,
          "each allowed FULLTEXT query must tokenize through the registered tokenizer");
  sqlite3_free(value);
  sqlite3_free(response);
}

static void check_schema_and_search(sqlite3 *db) {
  denied(db, semantic_node_create, no_external);
  denied(db, semantic_rel_create, no_external);
  denied(db, fulltext_create, no_external);
  denied(db, fulltext_rel_create, no_external);
  call(db, semantic_node_create, "{}");
  call(db, semantic_rel_create, "{}");
  call(db, fulltext_create, "{}");
  call(db, fulltext_rel_create, "{}");
  allowed_semantic_search(db, semantic_node_query, "synthetic-a");
  allowed_semantic_search(db, semantic_rel_query, "synthetic-b");
  allowed_fulltext_search(db, fulltext_query);
  allowed_fulltext_search(db, fulltext_rel_query);

  /* These calls run after both derived indexes and query results have been used. */
  denied(db, semantic_node_query, no_external);
  denied(db, semantic_rel_query, no_external);
  denied(db, fulltext_query, no_external);
  denied(db, fulltext_rel_query, no_external);
  denied(db,
         "CALL db.index.semantic.queryNodes('doc_sem','graph',{limit:0}) "
         "YIELD node RETURN node", no_external);
  denied(db, "CALL db.index.semantic.rebuild('doc_sem','branch/main')", no_external);
  denied(db, "CALL db.index.semantic.rebuild('rel_sem','branch/main')", no_external);
  denied(db, semantic_node_query, hidden_source);
  denied(db, semantic_rel_query, hidden_source);
  denied(db, fulltext_query, hidden_source);
  denied(db, fulltext_rel_query, hidden_source);
}

static void check_graph_and_standard_index(sqlite3 *db) {
  allowed_without_callbacks(db, "CREATE (:Doc {id:'c',text:'graph created'}) FINISH");
  allowed_without_callbacks(db, "MATCH (n:Doc {id:'c'}) SET n.text='graph changed' FINISH");
  allowed_without_callbacks(db, "MATCH ()-[r:LINK]->() SET r.text='graph updated' FINISH");
  allowed_without_callbacks(db, "MATCH (n:Doc {id:'c'}) REMOVE n.text FINISH");
  allowed_without_callbacks(db, "MATCH (n:Doc {id:'c'}) DELETE n FINISH");
  allowed_without_callbacks(db, "CREATE RANGE INDEX doc_id FOR (n:Doc) ON (n.id)");
  allowed_without_callbacks(db, "CALL lithograph.index.rebuild('doc_id','branch/main')");
  allowed_without_callbacks(db, "MATCH (n:Doc) WHERE n.id='a' RETURN n.id,n.text");
}

static void check_version_schema_restoration(sqlite3 *db) {
  const char *restore_patch =
      "CALL lithograph.diff('tag/without-indexes','tag/with-indexes') YIELD patch "
      "CALL lithograph.patch.apply(patch) YIELD commit RETURN commit";
  call(db, "CALL lithograph.tag.create('with-indexes','branch/main')", "{}");
  call(db, "DROP INDEX doc_sem", "{}");
  call(db, "DROP INDEX rel_sem", "{}");
  call(db, "DROP INDEX doc_fts", "{}");
  call(db, "DROP INDEX rel_fts", "{}");
  call(db, "CALL lithograph.tag.create('without-indexes','branch/main')", "{}");
  denied(db, restore_patch, no_external);
  /* Reset only moves a ref to an already validated immutable Commit. */
  allowed_without_callbacks(db, "CALL lithograph.reset('tag/with-indexes')");
  allowed_without_callbacks(db, "CALL lithograph.reset('tag/without-indexes')");

  /* The same Patch remains executable when the external callbacks are allowed. */
  reset_counters(db);
  call(db, restore_patch, "{}");
  require(db,
          integer(db,
                  "SELECT synthetic_embedding_validate_calls('synthetic-a') + "
                  "synthetic_embedding_validate_calls('synthetic-b')") > 0,
          "allowed Schema restoration must reach Provider validation");
  require(db, integer(db, "SELECT phase12_tokenizer_stat('create')") > 0,
          "allowed Schema restoration must probe the registered tokenizer");
}

#include "phase18_external_access_version.c"

int main(int argc, char **argv) {
  sqlite3 *db;
  require(NULL, argc == 4,
          "usage: phase18-external-access-smoke <extension> <provider> <tokenizer>");
  require(NULL, sqlite3_libversion_number() >= 3045000, "SQLite 3.45+ is required");
  db = fixture(argv[1], argv[2], argv[3]);
  check_schema_and_search(db);
  check_graph_and_standard_index(db);
  check_version_schema_restoration(db);
  require(db,
          integer(db, "SELECT json_extract(lithograph_integrity_check(),'$.ok')") == 1,
          "graph integrity must remain valid");
  require(db, sqlite3_close(db) == SQLITE_OK, "close graph fixture");
  check_version_effect_matrix(argv[1], argv[2], argv[3]);
  puts("PASS: Phase 18 scalar/rows external-I/O denial, zero Provider/tokenizer "
       "callbacks, hidden Semantic sources, warm Search, ordinary writes, "
       "RANGE maintenance, version Schema restoration, integrity; "
       "20 version/host-index/surface cases (16 denied with rollback, "
       "4 callback-free fast-forwards), 2 canonical Provider config cases");
  return 0;
}
