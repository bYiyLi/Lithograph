/* Included by phase18_external_access_smoke.c to share its real SQL fixtures. */

enum version_operation {
  VERSION_SQUASH,
  VERSION_REVERT,
  VERSION_REBASE,
  VERSION_MERGE_READY,
  VERSION_MERGE_FAST_FORWARD
};

static void check_host_validation(sqlite3 *db, int semantic) {
  if (semantic) {
    require(db, integer(db, "SELECT synthetic_embedding_validate_calls('synthetic-a')") > 0,
            "allowed version operation must validate the real Provider");
    require(db, integer(db, "SELECT phase12_tokenizer_stat('create')") == 0,
            "Semantic definition does not need a tokenizer");
  } else {
    require(db, integer(db, "SELECT phase12_tokenizer_stat('create')") > 0,
            "allowed version operation must construct the real tokenizer");
    require(db, integer(db, "SELECT synthetic_embedding_validate_calls('synthetic-a')") == 0,
            "FULLTEXT definition does not need an Embedding Provider");
  }
}

static void check_graph_integrity(sqlite3 *db) {
  require(db,
          integer(db, "SELECT json_extract(lithograph_integrity_check(),'$.ok')") == 1,
          "version access checks must preserve graph integrity");
}

static char *start_merge_query(sqlite3 *db, char **session, sqlite3_int64 *revision,
                               int fast_forward) {
  char *response = execute(db,
      "CALL lithograph.merge.start('branch/topic') YIELD session,revision,status "
      "RETURN session,revision,status", "{}");
  char *status = json_text(db, response, "$.rows[0][2]");
  char *query;
  require(db, strcmp(status, fast_forward ? "fast_forward" : "ready") == 0,
          "fixture must reach the intended Merge finalize path");
  *session = json_text(db, response, "$.rows[0][0]");
  *revision = json_integer(db, response, "$.rows[0][1]");
  query = sqlite3_mprintf("CALL lithograph.merge.finalize('%q',%lld)",
                          *session, *revision);
  sqlite3_free(status);
  sqlite3_free(response);
  return query;
}

static char *prepare_history_query(sqlite3 *db, const char *create_index,
                                   int semantic, enum version_operation operation,
                                   char **session, sqlite3_int64 *revision) {
  char *base = head(db);
  char *query;
  if (operation >= VERSION_REBASE) {
    call(db, "CALL lithograph.branch.create('topic')", "{}");
    if (operation == VERSION_REBASE) {
      call(db, "MATCH (n:Doc {id:'a'}) SET n.title='topic' FINISH",
           "{\"branch\":\"topic\"}");
      call(db, create_index, "{}");
      query = sqlite3_mprintf("CALL lithograph.rebase('branch/topic')");
    } else {
      if (operation == VERSION_MERGE_READY) {
        call(db, "MATCH (n:Doc {id:'a'}) SET n.title='main' FINISH", "{}");
      }
      call(db, create_index, "{\"branch\":\"topic\"}");
      query = start_merge_query(db, session, revision,
                               operation == VERSION_MERGE_FAST_FORWARD);
    }
  } else {
    call(db, create_index, "{}");
    if (operation == VERSION_REVERT) {
      char *removed;
      call(db, semantic ? "DROP INDEX doc_sem" : "DROP INDEX doc_fts", "{}");
      removed = head(db);
      query = sqlite3_mprintf("CALL lithograph.revert('%q')", removed);
      sqlite3_free(removed);
    } else {
      query = sqlite3_mprintf("CALL lithograph.squash('%q')", base);
    }
  }
  sqlite3_free(base);
  require(db, query != NULL, "allocate version operation query");
  return query;
}

static void check_merge_session_revision(sqlite3 *db, const char *session,
                                          sqlite3_int64 expected) {
  char *query = sqlite3_mprintf(
      "SELECT json_extract(lithograph('CALL lithograph.merge.get(''%q'') "
      "YIELD revision RETURN revision'),'$.rows[0][0]')", session);
  require(db, query != NULL, "allocate Session inspection query");
  require(db, integer(db, query) == expected,
          "denied finalize must preserve its Session revision");
  sqlite3_free(query);
}

static void check_version_effect_case(const char *extension, const char *provider,
                                       const char *tokenizer, int rows, int semantic,
                                       enum version_operation operation) {
  sqlite3 *db = fixture(extension, provider, tokenizer);
  char *session = NULL;
  sqlite3_int64 revision = 0;
  char *query = prepare_history_query(db,
      semantic ? semantic_node_create : fulltext_create, semantic, operation,
      &session, &revision);
  reset_counters(db);
  if (operation == VERSION_MERGE_FAST_FORWARD) {
    call_surface(db, query, no_external, rows);
    no_callbacks(db);
  } else {
    denied_surface(db, query, no_external, rows);
    if (session != NULL) {
      check_merge_session_revision(db, session, revision);
    }
    check_graph_integrity(db);
    reset_counters(db);
    call_surface(db, query, "{}", rows);
    check_host_validation(db, semantic);
  }
  check_graph_integrity(db);
  sqlite3_free(session);
  sqlite3_free(query);
  require(db, sqlite3_close(db) == SQLITE_OK, "close version host-effect fixture");
}

static void check_signed_zero_config(sqlite3 *db, int negative) {
  sqlite3_stmt *statement = NULL;
  char *response = execute(db,
      "SHOW INDEXES YIELD name,options WHERE name='doc_sem' "
      "RETURN options.indexConfig.providerConfig.t", "{}");
  double value;
  require(db,
          sqlite3_prepare_v2(db, "SELECT json_extract(?1,'$.rows[0][0]')", -1,
                             &statement, NULL) == SQLITE_OK,
          "prepare opaque Provider config sign check");
  sqlite3_bind_text(statement, 1, response, -1, SQLITE_STATIC);
  require(db, sqlite3_step(statement) == SQLITE_ROW &&
              sqlite3_column_type(statement, 0) == SQLITE_FLOAT,
          "Provider config signed zero must remain a Float");
  value = sqlite3_column_double(statement, 0);
  require(db, value == 0.0 && !!signbit(value) == negative,
          "Provider config must retain its canonical signed zero");
  require(db, sqlite3_finalize(statement) == SQLITE_OK, "finalize config sign check");
  sqlite3_free(response);
}

static char *signed_config_patch(const char *from, const char *to) {
  return sqlite3_mprintf(
      "CALL lithograph.diff('%q','%q') YIELD patch "
      "CALL lithograph.patch.apply(patch) YIELD commit RETURN commit", from, to);
}

static void check_config_rebase(sqlite3 *db, const char *positive,
                                 const char *negative, int rows) {
  char *query = sqlite3_mprintf("CALL lithograph.branch.create('onto','%q')", positive);
  char *patch = signed_config_patch(positive, negative);
  require(db, query != NULL && patch != NULL, "allocate canonical config replay fixture");
  call(db, query, "{}");
  sqlite3_free(query);
  call(db, "MATCH (n:Doc {id:'a'}) SET n.title='onto' FINISH",
       "{\"branch\":\"onto\"}");
  query = sqlite3_mprintf("CALL lithograph.reset('%q')", positive);
  require(db, query != NULL, "allocate canonical replay reset");
  call(db, query, "{}");
  sqlite3_free(query);
  call(db, patch, "{}");
  sqlite3_free(patch);
  denied_surface(db, "CALL lithograph.rebase('branch/onto')", no_external, rows);
  check_signed_zero_config(db, 1);
  reset_counters(db);
  call_surface(db, "CALL lithograph.rebase('branch/onto')",
               "{\"writeAccess\":{\"allowSchemaChanges\":false}}", rows);
  check_host_validation(db, 1);
  check_signed_zero_config(db, 1);
}

static void check_config_merge(sqlite3 *db, const char *positive,
                                const char *negative, int rows) {
  char *query = sqlite3_mprintf("CALL lithograph.reset('%q')", positive);
  char *session = NULL;
  sqlite3_int64 revision = 0;
  require(db, query != NULL, "allocate canonical Merge reset");
  call(db, query, "{}");
  sqlite3_free(query);
  call(db, "MATCH (n:Doc {id:'a'}) SET n.title='main' FINISH", "{}");
  query = sqlite3_mprintf("CALL lithograph.branch.create('topic','%q')", negative);
  require(db, query != NULL, "allocate canonical Merge source");
  call(db, query, "{}");
  sqlite3_free(query);
  query = start_merge_query(db, &session, &revision, 0);
  denied_surface(db, query, "{\"writeAccess\":{\"allowSchemaChanges\":false}}", rows);
  check_merge_session_revision(db, session, revision);
  check_signed_zero_config(db, 0);
  denied_surface(db, query, no_external, rows);
  check_merge_session_revision(db, session, revision);
  reset_counters(db);
  call_surface(db, query, "{}", rows);
  check_host_validation(db, 1);
  check_signed_zero_config(db, 1);
  require(db, integer(db, "SELECT count(*) FROM _lithograph_merge_sessions") == 0,
          "successful canonical config finalize must consume its Session");
  sqlite3_free(session);
  sqlite3_free(query);
}

static void check_canonical_config_case(const char *extension, const char *provider,
                                        const char *tokenizer, int rows) {
  sqlite3 *db = fixture(extension, provider, tokenizer);
  char *positive;
  char *negative;
  char *query;
  char *response;
  char *patch_commit;
  call(db,
       "CALL db.index.semantic.createNodeIndex('doc_sem',['Doc'],'text',"
       "{provider:'synthetic-a',providerConfig:{t:0.0},dimensions:4,similarity:'cosine'})",
       "{}");
  positive = head(db);
  call(db, "DROP INDEX doc_sem", "{}");
  call(db,
       "CALL db.index.semantic.createNodeIndex('doc_sem',['Doc'],'text',"
       "{provider:'synthetic-a',providerConfig:{t:-0.0},dimensions:4,similarity:'cosine'})",
       "{}");
  negative = head(db);
  check_signed_zero_config(db, 1);
  query = sqlite3_mprintf(
      "CALL lithograph.diff('%q','%q') YIELD patch RETURN size(patch.operations)",
      negative, positive);
  require(db, query != NULL, "allocate signed-zero Diff query");
  response = execute(db, query, "{}");
  require(db, json_integer(db, response, "$.rows[0][0]") == 1,
          "canonical signed-zero config change must produce one Index operation");
  sqlite3_free(response);
  sqlite3_free(query);
  query = sqlite3_mprintf("CALL lithograph.reset('%q')", positive);
  require(db, query != NULL, "allocate signed-zero Reset query");
  denied_surface(db, query, "{\"writeAccess\":{\"allowSchemaChanges\":false}}", rows);
  check_signed_zero_config(db, 1);
  sqlite3_free(query);
  query = signed_config_patch(negative, positive);
  require(db, query != NULL, "allocate signed-zero Patch query");
  denied_surface(db, query, no_external, rows);
  check_signed_zero_config(db, 1);
  reset_counters(db);
  call_surface(db, query, "{}", rows);
  check_host_validation(db, 1);
  check_signed_zero_config(db, 0);
  sqlite3_free(query);
  patch_commit = head(db);
  query = sqlite3_mprintf("CALL lithograph.revert('%q')", patch_commit);
  require(db, query != NULL, "allocate signed-zero Revert query");
  denied_surface(db, query, no_external, rows);
  reset_counters(db);
  call_surface(db, query, "{}", rows);
  check_host_validation(db, 1);
  check_signed_zero_config(db, 1);
  sqlite3_free(query);
  sqlite3_free(patch_commit);
  check_config_rebase(db, positive, negative, rows);
  check_config_merge(db, positive, negative, rows);
  check_graph_integrity(db);
  sqlite3_free(negative);
  sqlite3_free(positive);
  require(db, sqlite3_close(db) == SQLITE_OK, "close canonical config fixture");
}

static void check_version_effect_matrix(const char *extension, const char *provider,
                                         const char *tokenizer) {
  int rows;
  int semantic;
  int operation;
  for (rows = 0; rows < 2; rows++) {
    for (semantic = 0; semantic < 2; semantic++) {
      for (operation = VERSION_SQUASH; operation <= VERSION_MERGE_FAST_FORWARD;
           operation++) {
        check_version_effect_case(extension, provider, tokenizer, rows, semantic,
                                   (enum version_operation)operation);
      }
    }
    check_canonical_config_case(extension, provider, tokenizer, rows);
  }
}
