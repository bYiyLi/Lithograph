#![forbid(unsafe_code)]

use lithograph_test_support::sqlite::{FileDatabaseFixture, extension_load_command};
use serde::Serialize;
use serde_json::json;
use std::env;
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[path = "lithograph-phase13/support.rs"]
mod support;
#[path = "lithograph-phase13/versioning.rs"]
mod versioning;
use support::*;

#[derive(Serialize)]
struct ProbeResult {
    phase: &'static str,
    extension: String,
    synthetic_provider: String,
    openai_provider: Option<String>,
    checks: Vec<&'static str>,
}

fn main() -> ExitCode {
    match run() {
        Ok(result) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&result).expect("probe result must serialize")
            );
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<ProbeResult, Box<dyn Error>> {
    let extension = required_path_arg(1, "Lithograph extension")?;
    let synthetic = required_path_arg(2, "synthetic Embedding Provider")?;
    let openai = env::args()
        .nth(3)
        .map(PathBuf::from)
        .map(fs::canonicalize)
        .transpose()?;
    let lithograph_load = format!(
        "{} sqlite3_lithograph_init",
        extension_load_command(&extension)?
    );
    let synthetic_load = provider_load_command(&synthetic, "sqlite3_syntheticembedding_init")?;
    let mut checks = Vec::new();

    run_primary_checks(&lithograph_load, &synthetic_load, &mut checks)?;
    run_boundary_checks(&lithograph_load, &synthetic_load, &mut checks)?;
    if let Some(openai) = openai.as_ref() {
        let openai_load = provider_load_command(openai, "sqlite3_lithographopenaicompatible_init")?;
        check_openai_schema_roundtrip(&lithograph_load, &openai_load)?;
        checks.push("openai-config-schema-roundtrip");
    }

    Ok(ProbeResult {
        phase: "13-managed-semantic-vector",
        extension: extension.display().to_string(),
        synthetic_provider: synthetic.display().to_string(),
        openai_provider: openai.map(|path| path.display().to_string()),
        checks,
    })
}

fn run_primary_checks(
    lithograph: &str,
    synthetic: &str,
    checks: &mut Vec<&'static str>,
) -> Result<(), Box<dyn Error>> {
    check_load_orders(lithograph, synthetic)?;
    checks.push("dual-extension-load-order");
    check_multi_target_membership(lithograph, synthetic)?;
    checks.push("multi-label-type-membership");
    check_schema_validation_negatives(lithograph, synthetic)?;
    checks.push("semantic-schema-negative-validation");
    check_source_edge_semantics(lithograph, synthetic)?;
    checks.push("semantic-source-exact-type-and-limit");
    check_query_rebuild_without_core_cache(lithograph, synthetic)?;
    checks.push("query-rebuild-without-core-cache");
    check_execution_local_embedding_dedup(lithograph, synthetic)?;
    checks.push("execution-local-exact-text-dedup");
    check_graph_view_nodes(lithograph, synthetic)?;
    checks.push("node-graph-view-before-provider");
    Ok(())
}

fn run_boundary_checks(
    lithograph: &str,
    synthetic: &str,
    checks: &mut Vec<&'static str>,
) -> Result<(), Box<dyn Error>> {
    check_graph_view_relationships(lithograph, synthetic)?;
    checks.push("relationship-endpoint-graph-view");
    check_history_provider_selection(lithograph, synthetic)?;
    checks.push("historical-provider-selection");
    versioning::check_semantic_diff_patch(lithograph, synthetic)?;
    checks.push("semantic-diff-patch-publication");
    versioning::check_semantic_merge_rebase(lithograph, synthetic)?;
    checks.push("semantic-merge-rebase-publication");
    check_version_publication_validation(lithograph, synthetic)?;
    checks.push("version-publication-provider-validation");
    check_provider_missing_inspection_and_drop(lithograph, synthetic)?;
    checks.push("provider-missing-inspection-drop");
    check_failure_atomicity(lithograph, synthetic)?;
    checks.push("provider-failure-atomicity");
    check_graph_mutation_provider_boundary(lithograph, synthetic)?;
    checks.push("graph-mutation-provider-boundary");
    check_rows_adapter_authority(lithograph, synthetic)?;
    checks.push("rows-adapter-authority");
    check_read_only_rebuild(lithograph, synthetic)?;
    checks.push("read-only-query-and-rebuild");
    check_explicit_transaction_staged_semantic(lithograph, synthetic)?;
    checks.push("explicit-transaction-staged-semantic");
    Ok(())
}

fn required_path_arg(index: usize, label: &str) -> Result<PathBuf, Box<dyn Error>> {
    env::args()
        .nth(index)
        .map(PathBuf::from)
        .ok_or_else(|| {
            format!(
                "usage: lithograph-phase13 <extension-path> <synthetic-provider-path> [openai-provider-path]; missing {label}"
            )
            .into()
        })
        .and_then(|path| fs::canonicalize(path).map_err(Into::into))
}

fn provider_load_command(path: &Path, entrypoint: &str) -> Result<String, Box<dyn Error>> {
    Ok(format!("{} {entrypoint}", extension_load_command(path)?))
}

fn check_load_orders(lithograph: &str, provider: &str) -> Result<(), Box<dyn Error>> {
    for (seed, loads) in [
        (0x1301, format!("{provider}\n{lithograph}")),
        (0x1302, format!("{lithograph}\n{provider}")),
    ] {
        let fixture = FileDatabaseFixture::new(seed)?;
        let create = semantic_node_create(
            "order_sem",
            "Order",
            "text",
            "synthetic-a",
            "{}",
            4,
            "cosine",
        );
        let output = fixture.execute_script(&format!(
            "{loads}\n             SELECT lithograph_init();\n             SELECT lithograph({});\n             SELECT 'validate=' || synthetic_embedding_validate_calls('synthetic-a');\n             SELECT 'embed=' || synthetic_embedding_embed_calls('synthetic-a');\n             SELECT 'statement=' || json_extract(lithograph('SHOW ALL INDEXES YIELD name, createStatement WHERE name = ''order_sem'' RETURN createStatement'), '$.rows[0][0]');",
            sql_literal(&create)
        ))?;
        require_line(&output, "validate=1")?;
        require_line(&output, "embed=0")?;
        let statement = prefixed_value(&output, "statement=")?;
        require(
            statement.starts_with("CALL db.index.semantic.createNodeIndex("),
            "Semantic createStatement did not use the procedure surface",
        )?;
        require(
            !statement.contains("CREATE SEMANTIC INDEX"),
            "Semantic createStatement exposed nonexistent Cypher grammar",
        )?;
        let replay = fixture.execute_script(&format!(
            "{loads}\n             SELECT lithograph('DROP INDEX order_sem');\n             SELECT 'replay=' || json_extract(lithograph({statement}), '$.summary.counters.indexesAdded');",
            statement = sql_literal(statement),
        ))?;
        require_line(&replay, "replay=1")?;
    }
    Ok(())
}

fn check_multi_target_membership(lithograph: &str, provider: &str) -> Result<(), Box<dyn Error>> {
    let fixture = FileDatabaseFixture::new(0x1312)?;
    let node_create = "CALL db.index.semantic.createNodeIndex('multi_node',['Alpha','Beta'],'text',{provider:'synthetic-a',providerConfig:{},dimensions:4,similarity:'cosine'})";
    let relationship_create = "CALL db.index.semantic.createRelationshipIndex('multi_rel',['LINK','ALT'],'text',{provider:'synthetic-a',providerConfig:{},dimensions:4,similarity:'cosine'})";
    let node_query = "CALL db.index.semantic.queryNodes('multi_node','probe',{limit:10}) YIELD node RETURN node.name ORDER BY node.name";
    let relationship_query = "CALL db.index.semantic.queryRelationships('multi_rel','probe',{limit:10}) YIELD relationship RETURN relationship.text ORDER BY relationship.text";
    let output = fixture.execute_script(&format!(
        "{lithograph}\n{provider}\n         SELECT lithograph_init();\n         SELECT lithograph('CREATE (a:Alpha {{name:''alpha'', text:''alpha''}}), (b:Beta {{name:''beta'', text:''beta''}}), (c:Alpha:Beta {{name:''both'', text:''both''}}), (x:Endpoint {{name:''x''}}), (y:Endpoint {{name:''y''}}) CREATE (x)-[:LINK {{text:''link-text''}}]->(y), (x)-[:ALT {{text:''alt-text''}}]->(y) FINISH');\n         SELECT lithograph({node_create});\n         SELECT lithograph({relationship_create});\n         SELECT 'node=' || lithograph({node_query});\n         SELECT 'relationship=' || lithograph({relationship_query});\n         SELECT 'show-node=' || lithograph('SHOW ALL INDEXES YIELD name, labelsOrTypes WHERE name = ''multi_node'' RETURN labelsOrTypes');\n         SELECT 'show-rel=' || lithograph('SHOW ALL INDEXES YIELD name, labelsOrTypes WHERE name = ''multi_rel'' RETURN labelsOrTypes');\n         SELECT 'vector-count=' || json_extract(lithograph('SHOW VECTOR INDEXES YIELD name RETURN count(*)'), '$.rows[0][0]');",
        node_create = sql_literal(node_create),
        relationship_create = sql_literal(relationship_create),
        node_query = sql_literal(node_query),
        relationship_query = sql_literal(relationship_query),
    ))?;
    let nodes = prefixed_json(&output, "node=")?;
    require(
        nodes["rows"] == json!([["alpha"], ["beta"], ["both"]]),
        "multi-Label Semantic Index did not union/deduplicate all targets",
    )?;
    let relationships = prefixed_json(&output, "relationship=")?;
    require(
        relationships["rows"] == json!([["alt-text"], ["link-text"]]),
        "multi-Type Semantic Index did not include all Relationship Types",
    )?;
    let show_node = prefixed_json(&output, "show-node=")?;
    require(
        show_node["rows"] == json!([[["Alpha", "Beta"]]]),
        "SHOW did not preserve all Semantic Node labels",
    )?;
    let show_rel = prefixed_json(&output, "show-rel=")?;
    require(
        show_rel["rows"] == json!([[["LINK", "ALT"]]]),
        "SHOW did not preserve all Semantic Relationship Types",
    )?;
    require_line(&output, "vector-count=0")
}

fn check_schema_validation_negatives(
    lithograph: &str,
    provider: &str,
) -> Result<(), Box<dyn Error>> {
    let fixture = FileDatabaseFixture::new(0x1314)?;
    fixture.execute_script(&format!(
        "{lithograph}\n{provider}\nSELECT lithograph_init();"
    ))?;
    let before = branch_head_text(&fixture, lithograph)?;
    let invalid = [
        "CALL db.index.semantic.createNodeIndex('dup',['Doc','Doc'],'text',{provider:'synthetic-a',providerConfig:{},dimensions:4,similarity:'cosine'})",
        "CALL db.index.semantic.createNodeIndex('unknown',['Doc'],'text',{provider:'synthetic-a',providerConfig:{},dimensions:4,similarity:'cosine',unknown:true})",
        "CALL db.index.semantic.createNodeIndex('non_json',['Doc'],'text',{provider:'synthetic-a',providerConfig:{bad:date('2026-01-01')},dimensions:4,similarity:'cosine'})",
        "CALL db.index.semantic.createNodeIndex('zero_dim',['Doc'],'text',{provider:'synthetic-a',providerConfig:{},dimensions:0,similarity:'cosine'})",
        "CALL db.index.semantic.createNodeIndex('bad_similarity',['Doc'],'text',{provider:'synthetic-a',providerConfig:{},dimensions:4,similarity:'dot'})",
    ];
    for query in invalid {
        let (success, _, stderr) = execute_script_allowing_failure(
            fixture.path(),
            &format!(
                "{lithograph}\n{provider}\nSELECT lithograph({});",
                sql_literal(query)
            ),
        )?;
        require(!success, "invalid Semantic schema unexpectedly succeeded")?;
        require(
            stderr.contains("SCHEMA_ERROR"),
            &format!("invalid Semantic schema lost SCHEMA_ERROR: {stderr}"),
        )?;
        require(
            branch_head_text(&fixture, lithograph)? == before,
            "invalid Semantic schema moved the Branch head",
        )?;
    }
    Ok(())
}

fn check_source_edge_semantics(lithograph: &str, provider: &str) -> Result<(), Box<dyn Error>> {
    let fixture = FileDatabaseFixture::new(0x1315)?;
    let create = semantic_node_create("edge_sem", "Doc", "text", "synthetic-a", "{}", 4, "cosine");
    let rebuild = "CALL db.index.semantic.rebuild('edge_sem','branch/main') YIELD indexedEntities, embeddedTexts RETURN indexedEntities, embeddedTexts";
    let zero_limit =
        "CALL db.index.semantic.queryNodes('edge_sem','probe',{limit:0}) YIELD node RETURN node";
    let output = fixture.execute_script(&format!(
        "{lithograph}\n{provider}\n         SELECT lithograph_init();\n         SELECT lithograph('CREATE (:Doc {{name:''empty'', text:''''}}), (:Doc {{name:''composed'', text:''é''}}), (:Doc {{name:''decomposed'', text:''é''}}), (:Doc {{name:''null'', text:null}}), (:Doc {{name:''integer'', text:42}}), (:Doc {{name:''missing''}}) FINISH');\n         SELECT lithograph({create});\n         SELECT synthetic_embedding_reset('synthetic-a');\n         SELECT 'rebuild=' || lithograph({rebuild});\n         SELECT 'inputs=' || synthetic_embedding_last_inputs('synthetic-a');\n         SELECT synthetic_embedding_reset('synthetic-a');\n         SELECT 'zero=' || lithograph({zero_limit});\n         SELECT 'zero-validate=' || synthetic_embedding_validate_calls('synthetic-a');\n         SELECT 'zero-embed=' || synthetic_embedding_embed_calls('synthetic-a');",
        create = sql_literal(&create),
        rebuild = sql_literal(rebuild),
        zero_limit = sql_literal(zero_limit),
    ))?;
    let rebuild = prefixed_json(&output, "rebuild=")?;
    require(
        rebuild["rows"] == json!([[3, 3]]),
        "Semantic rebuild did not include exactly the STRING source values",
    )?;
    let inputs = prefixed_value(&output, "inputs=")?;
    require(
        inputs.contains("0:") && inputs.contains("2:é") && inputs.contains("3:é"),
        "Semantic source text was trimmed/normalized or Unicode bytes were conflated",
    )?;
    require_line(&output, "zero-validate=1")?;
    require_line(&output, "zero-embed=0")?;
    let zero = prefixed_json(&output, "zero=")?;
    require(zero["rows"] == json!([]), "semantic limit=0 returned rows")?;

    for query in [
        "CALL db.index.semantic.queryNodes('edge_sem','probe',{skip:-1,limit:1})",
        "CALL db.index.semantic.queryNodes('edge_sem','probe',{limit:null})",
        "CALL db.index.semantic.queryNodes('edge_sem','probe',{limit:1,unknown:true})",
    ] {
        let (success, _, stderr) = execute_script_allowing_failure(
            fixture.path(),
            &format!(
                "{lithograph}\n{provider}\nSELECT lithograph({});",
                sql_literal(query)
            ),
        )?;
        require(
            !success,
            "invalid Semantic query options unexpectedly succeeded",
        )?;
        require(
            stderr.contains("INVALID_ARGUMENT"),
            &format!("invalid Semantic query options lost INVALID_ARGUMENT: {stderr}"),
        )?;
    }
    Ok(())
}

fn check_query_rebuild_without_core_cache(
    lithograph: &str,
    provider: &str,
) -> Result<(), Box<dyn Error>> {
    let fixture = FileDatabaseFixture::new(0x1303)?;
    let create = semantic_node_create("doc_sem", "Doc", "text", "synthetic-a", "{}", 4, "cosine");
    let query = "CALL db.index.semantic.queryNodes('doc_sem','hello',{skip:0,limit:3}) YIELD node, score RETURN node.name, score";
    let rebuild = "CALL db.index.semantic.rebuild('doc_sem','branch/main') YIELD name, commit, indexedEntities, embeddedTexts RETURN name, commit, indexedEntities, embeddedTexts";
    fixture.execute_script(&format!(
        "{lithograph}\n{provider}\n\
         SELECT lithograph_init();\n\
         SELECT lithograph('CREATE (:Doc {{name:''alpha'', text:''hello''}}), (:Doc {{name:''beta'', text:''world''}}), (:Doc {{name:''dup'', text:''hello''}}) FINISH');\n\
         SELECT lithograph({create});",
        create = sql_literal(&create),
    ))?;
    let head_before = branch_head_text(&fixture, lithograph)?;
    let commits_before = commit_count(&fixture)?;

    let output = fixture.execute_script(&format!(
        "{lithograph}\n{provider}\n\
         SELECT synthetic_embedding_reset('synthetic-a');\n\
         SELECT 'query=' || lithograph({query});\n\
         SELECT 'query-validate=' || synthetic_embedding_validate_calls('synthetic-a');\n\
         SELECT 'query-embed=' || synthetic_embedding_embed_calls('synthetic-a');\n\
         SELECT synthetic_embedding_reset('synthetic-a');\n\
         SELECT 'repeat=' || lithograph({query});\n\
         SELECT 'repeat-embed=' || synthetic_embedding_embed_calls('synthetic-a');\n\
         SELECT synthetic_embedding_reset('synthetic-a');\n\
         SELECT 'rebuild=' || lithograph({rebuild});\n\
         SELECT 'rebuild-validate=' || synthetic_embedding_validate_calls('synthetic-a');\n\
         SELECT 'rebuild-embed=' || synthetic_embedding_embed_calls('synthetic-a');",
        query = sql_literal(query),
        rebuild = sql_literal(rebuild),
    ))?;

    assert_semantic_query_output(&output)?;
    assert_semantic_rebuild_output(&output)?;
    assert_core_cache_surface_removed(&fixture, lithograph)?;

    require(
        branch_head_text(&fixture, lithograph)? == head_before,
        "Semantic query/rebuild moved the Branch head",
    )?;
    require(
        commit_count(&fixture)? == commits_before,
        "Semantic query/rebuild created a Commit",
    )
}

fn assert_semantic_query_output(output: &str) -> Result<(), Box<dyn Error>> {
    let query_json = prefixed_json(output, "query=")?;
    let rows = query_json["rows"]
        .as_array()
        .ok_or("Semantic query rows are not an array")?;
    require(rows.len() == 3, "Semantic query row count differs")?;
    require(
        rows[0][0] == "alpha" && rows[0][1] == 1.0,
        "Semantic query first result differs",
    )?;
    require(
        rows[1][0] == "dup" && rows[1][1] == 1.0,
        "Semantic query duplicate result differs",
    )?;
    require(rows[2][0] == "beta", "Semantic query third result differs")?;
    require(
        query_json["summary"]["queryType"] == "read" && query_json["summary"]["commit"].is_string(),
        "Semantic query summary differs",
    )?;
    require_line(output, "query-validate=1")?;
    require_positive_value(output, "query-embed=")?;
    require_positive_value(output, "repeat-embed=")?;
    require_line(output, "rebuild-validate=1")?;
    require_positive_value(output, "rebuild-embed=")
}

fn assert_semantic_rebuild_output(output: &str) -> Result<(), Box<dyn Error>> {
    let rebuild_json = prefixed_json(output, "rebuild=")?;
    require(
        rebuild_json["columns"] == json!(["name", "commit", "indexedEntities", "embeddedTexts"]),
        "Semantic rebuild exposed the removed cacheHits column",
    )?;
    require(
        rebuild_json["rows"][0][2] == 3,
        "Semantic rebuild indexedEntities differs",
    )?;
    require(
        rebuild_json["summary"]["queryType"] == "read"
            && rebuild_json["summary"]["commit"].is_string(),
        "Semantic rebuild summary no longer matches the pinned target Commit contract",
    )
}

fn assert_core_cache_surface_removed(
    fixture: &FileDatabaseFixture,
    lithograph: &str,
) -> Result<(), Box<dyn Error>> {
    require_core_cache_absent(fixture)?;
    for procedure in [
        "CALL db.index.semantic.cache.configure({enabled:false})",
        "CALL db.index.semantic.cache.stats()",
        "CALL db.index.semantic.cache.clear()",
    ] {
        let (success, _, stderr) = execute_script_allowing_failure(
            fixture.path(),
            &format!(
                "{lithograph}\nSELECT lithograph({});",
                sql_literal(procedure)
            ),
        )?;
        require(!success, "removed Core cache procedure unexpectedly exists")?;
        require(
            stderr.contains("SEMANTIC_ERROR"),
            &format!("removed Core cache procedure lost semantic error: {stderr}"),
        )?;
    }
    Ok(())
}

fn check_execution_local_embedding_dedup(
    lithograph: &str,
    provider: &str,
) -> Result<(), Box<dyn Error>> {
    let fixture = FileDatabaseFixture::new(0x13f2)?;
    let create = semantic_node_create("dedup_sem", "Doc", "text", "synthetic-a", "{}", 4, "cosine");
    let query =
        "CALL db.index.semantic.queryNodes('dedup_sem','probe',{limit:1}) YIELD node RETURN node";
    let output = fixture.execute_script(&format!(
        "{lithograph}\n{provider}\n         SELECT lithograph_init();\n         SELECT lithograph('UNWIND range(1,600) AS i CREATE (:Doc {{text:''dup-source''}}) FINISH');\n         SELECT lithograph({create});\n         SELECT synthetic_embedding_reset('synthetic-a');\n         SELECT lithograph({query});\n         SELECT 'dedup-inputs=' || synthetic_embedding_embed_inputs('synthetic-a');",
        create = sql_literal(&create),
        query = sql_literal(query),
    ))?;
    require_line(&output, "dedup-inputs=2")
}

fn require_positive_value(output: &str, prefix: &str) -> Result<(), Box<dyn Error>> {
    let value = prefixed_value(output, prefix)?.parse::<i64>()?;
    require(
        value > 0,
        &format!("expected a positive Provider counter for {prefix}, got {value}"),
    )
}

fn check_graph_view_nodes(lithograph: &str, provider: &str) -> Result<(), Box<dyn Error>> {
    let fixture = FileDatabaseFixture::new(0x1305)?;
    let create = semantic_node_create("view_sem", "Doc", "text", "synthetic-a", "{}", 4, "cosine");
    let query = "CALL db.index.semantic.queryNodes('view_sem','probe',{limit:10}) YIELD node, score RETURN node.name";
    let options = r#"{"graphView":{"requireAllLabels":["Visible"]}}"#;
    let output = fixture.execute_script(&format!(
        "{lithograph}\n{provider}\n         SELECT lithograph_init();\n         SELECT lithograph('CREATE (:Doc:Visible {{name:''visible'', text:''visible-text''}}), (:Doc:Hidden {{name:''hidden'', text:''hidden-text''}}) FINISH');\n         SELECT lithograph({create});\n         SELECT synthetic_embedding_reset('synthetic-a');\n         SELECT 'query=' || lithograph({query}, '{{}}', {options});\n         SELECT 'hnsw-view=' || count(*) FROM temp._lithograph_vector_cache_meta;\n         SELECT 'calls=' || synthetic_embedding_embed_calls('synthetic-a');\n         SELECT 'inputs=' || synthetic_embedding_last_inputs('synthetic-a');\n         SELECT synthetic_embedding_reset('synthetic-a');\n         SELECT 'full=' || lithograph({query});\n         SELECT 'hnsw-full=' || count(*) || ':' || COALESCE(sum(entry_count),0) FROM temp._lithograph_vector_cache_meta;\n         SELECT 'full-inputs=' || synthetic_embedding_last_inputs('synthetic-a');\n         SELECT synthetic_embedding_reset('synthetic-a');\n         SELECT 'view-reuse=' || lithograph({query}, '{{}}', {options});\n         SELECT 'hnsw-view-reuse=' || count(*) || ':' || COALESCE(sum(entry_count),0) FROM temp._lithograph_vector_cache_meta;\n         SELECT 'view-reuse-calls=' || synthetic_embedding_embed_calls('synthetic-a');",
        create = sql_literal(&create),
        query = sql_literal(query),
        options = sql_literal(options),
    ))?;
    let result = prefixed_json(&output, "query=")?;
    require(
        result["rows"] == json!([["visible"]]),
        "Node Graph View result differs",
    )?;
    require_line(&output, "hnsw-view=0")?;
    require_line(&output, "calls=2")?;
    let inputs = prefixed_value(&output, "inputs=")?;
    require(
        inputs.contains("visible-text"),
        "visible Node source was not embedded",
    )?;
    require(
        !inputs.contains("hidden-text"),
        "hidden Node source leaked to Provider",
    )?;
    let full = prefixed_json(&output, "full=")?;
    require(
        full["rows"].as_array().is_some_and(|rows| rows.len() == 2),
        "full-graph Semantic query did not return both Nodes",
    )?;
    require_line(&output, "hnsw-full=1:2")?;
    let full_inputs = prefixed_value(&output, "full-inputs=")?;
    require(
        full_inputs.contains("hidden-text"),
        "full-graph HNSW materialization did not embed hidden-by-view Node source",
    )?;
    let reused = prefixed_json(&output, "view-reuse=")?;
    require(
        reused["rows"] == json!([["visible"]]),
        "Node Graph View result differs when reusing full-graph HNSW",
    )?;
    require_line(&output, "hnsw-view-reuse=1:2")?;
    require_line(&output, "view-reuse-calls=1")?;
    Ok(())
}

fn check_graph_view_relationships(lithograph: &str, provider: &str) -> Result<(), Box<dyn Error>> {
    let fixture = FileDatabaseFixture::new(0x1306)?;
    let create =
        semantic_relationship_create("rel_sem", "LINK", "text", "synthetic-a", "{}", 4, "cosine");
    let query = "CALL db.index.semantic.queryRelationships('rel_sem','probe',{limit:10}) YIELD relationship, score RETURN relationship.text";
    let options = r#"{"graphView":{"requireAllLabels":["Visible"]}}"#;
    let output = fixture.execute_script(&format!(
        "{lithograph}\n{provider}\n         SELECT lithograph_init();\n         SELECT lithograph('CREATE (a:Visible {{name:''a''}}), (b:Visible {{name:''b''}}), (h:Hidden {{name:''h''}}) CREATE (a)-[:LINK {{text:''visible-rel''}}]->(b), (a)-[:LINK {{text:''hidden-rel''}}]->(h) FINISH');\n         SELECT lithograph({create});\n         SELECT synthetic_embedding_reset('synthetic-a');\n         SELECT 'query=' || lithograph({query}, '{{}}', {options});\n         SELECT 'hnsw-rel-view=' || count(*) FROM temp._lithograph_vector_cache_meta;\n         SELECT 'inputs=' || synthetic_embedding_last_inputs('synthetic-a');\n         SELECT synthetic_embedding_reset('synthetic-a');\n         SELECT 'full=' || lithograph({query});\n         SELECT 'hnsw-rel-full=' || count(*) || ':' || COALESCE(sum(entry_count),0) FROM temp._lithograph_vector_cache_meta;\n         SELECT synthetic_embedding_reset('synthetic-a');\n         SELECT 'view-reuse=' || lithograph({query}, '{{}}', {options});\n         SELECT 'hnsw-rel-reuse=' || count(*) || ':' || COALESCE(sum(entry_count),0) FROM temp._lithograph_vector_cache_meta;\n         SELECT 'view-reuse-calls=' || synthetic_embedding_embed_calls('synthetic-a');",
        create = sql_literal(&create),
        query = sql_literal(query),
        options = sql_literal(options),
    ))?;
    let result = prefixed_json(&output, "query=")?;
    require(
        result["rows"] == json!([["visible-rel"]]),
        "Relationship endpoint Graph View result differs",
    )?;
    require_line(&output, "hnsw-rel-view=0")?;
    let inputs = prefixed_value(&output, "inputs=")?;
    require(
        inputs.contains("visible-rel"),
        "visible Relationship source was not embedded",
    )?;
    require(
        !inputs.contains("hidden-rel"),
        "Relationship with hidden endpoint leaked source to Provider",
    )?;
    let full = prefixed_json(&output, "full=")?;
    require(
        full["rows"].as_array().is_some_and(|rows| rows.len() == 2),
        "full-graph Semantic query did not return both Relationships",
    )?;
    require_line(&output, "hnsw-rel-full=1:2")?;
    let reused = prefixed_json(&output, "view-reuse=")?;
    require(
        reused["rows"] == json!([["visible-rel"]]),
        "Relationship Graph View result differs when reusing full-graph HNSW",
    )?;
    require_line(&output, "hnsw-rel-reuse=1:2")?;
    require_line(&output, "view-reuse-calls=1")
}

fn check_history_provider_selection(
    lithograph: &str,
    provider: &str,
) -> Result<(), Box<dyn Error>> {
    let fixture = FileDatabaseFixture::new(0x1307)?;
    let create_a =
        semantic_node_create("hist_sem", "Doc", "text", "synthetic-a", "{}", 4, "cosine");
    let create_b =
        semantic_node_create("hist_sem", "Doc", "text", "synthetic-b", "{}", 4, "cosine");
    let query = "CALL db.index.semantic.queryNodes('hist_sem','probe',{limit:2}) YIELD node, score RETURN node.name";
    let historical_options = sql_literal(r#"{"at":"tag/semantic-a"}"#);
    let setup = fixture.execute_script(&format!(
        "{lithograph}\n{provider}\n         SELECT lithograph_init();\n         SELECT lithograph('CREATE (:Doc {{name:''a'', text:''alpha''}}), (:Doc {{name:''b'', text:''beta''}}) FINISH');\n         SELECT lithograph({create_a});\n         SELECT lithograph('CALL lithograph.tag.create(''semantic-a'',''branch/main'') YIELD name RETURN name');\n         SELECT lithograph('DROP INDEX hist_sem');\n         SELECT lithograph({create_b});",
        create_a = sql_literal(&create_a),
        create_b = sql_literal(&create_b),
    ))?;
    require(!setup.is_empty(), "historical setup returned no output")?;
    let output = fixture.execute_script(&format!(
        "{lithograph}\n{provider}\n         SELECT synthetic_embedding_reset('synthetic-a');\n         SELECT synthetic_embedding_reset('synthetic-b');\n         SELECT 'historical=' || lithograph({query}, '{{}}', {historical_options});\n         SELECT 'a=' || synthetic_embedding_validate_calls('synthetic-a') || ':' || synthetic_embedding_embed_calls('synthetic-a');\n         SELECT 'b=' || synthetic_embedding_validate_calls('synthetic-b') || ':' || synthetic_embedding_embed_calls('synthetic-b');\n         SELECT synthetic_embedding_reset('synthetic-a');\n         SELECT synthetic_embedding_reset('synthetic-b');\n         SELECT 'current=' || lithograph({query});\n         SELECT 'current-a=' || synthetic_embedding_validate_calls('synthetic-a') || ':' || synthetic_embedding_embed_calls('synthetic-a');\n         SELECT 'current-b=' || synthetic_embedding_validate_calls('synthetic-b') || ':' || synthetic_embedding_embed_calls('synthetic-b');",
        query = sql_literal(query),
        historical_options = historical_options,
    ))?;
    require_positive_pair(&output, "a=")?;
    require_line(&output, "b=0:0")?;
    require_line(&output, "current-a=0:0")?;
    require_positive_pair(&output, "current-b=")?;
    Ok(())
}

fn check_version_publication_validation(
    lithograph: &str,
    provider: &str,
) -> Result<(), Box<dyn Error>> {
    let fixture = FileDatabaseFixture::new(0x1313)?;
    let create = semantic_node_create(
        "revert_sem",
        "Doc",
        "text",
        "synthetic-a",
        "{}",
        4,
        "cosine",
    );
    let setup = fixture.execute_script(&format!(
        "{lithograph}\n{provider}\n         SELECT lithograph_init();\n         SELECT lithograph('CREATE (:Doc {{text:''alpha''}}) FINISH');\n         SELECT lithograph({create});\n         SELECT 'drop=' || lithograph('DROP INDEX revert_sem');",
        create = sql_literal(&create),
    ))?;
    let drop_result = prefixed_json(&setup, "drop=")?;
    let drop_commit = drop_result["summary"]["commit"]
        .as_str()
        .ok_or("DROP Semantic Index did not return a Commit")?
        .to_owned();
    let before = branch_head_text(&fixture, lithograph)?;
    require(
        before == drop_commit,
        "DROP Commit is not the active Branch head",
    )?;

    let revert = format!(
        "CALL lithograph.revert('{}') YIELD commit RETURN commit",
        drop_commit
    );
    let (success, _, stderr) = execute_script_allowing_failure(
        fixture.path(),
        &format!("{lithograph}\nSELECT lithograph({});", sql_literal(&revert)),
    )?;
    require(
        !success,
        "revert that re-added a Semantic Index unexpectedly succeeded without Provider",
    )?;
    require(
        stderr.contains("INVALID_ARGUMENT") && stderr.contains("synthetic-a"),
        "revert missing-Provider validation lost the expected error",
    )?;
    require(
        branch_head_text(&fixture, lithograph)? == before,
        "failed Semantic revert moved the Branch head",
    )?;

    let success = fixture.execute_script(&format!(
        "{lithograph}\n{provider}\n         SELECT 'revert=' || lithograph({revert});\n         SELECT 'count=' || json_extract(lithograph('SHOW ALL INDEXES YIELD name WHERE name = ''revert_sem'' RETURN count(*)'), '$.rows[0][0]');",
        revert = sql_literal(&revert),
    ))?;
    require_line(&success, "count=1")?;
    let result = prefixed_json(&success, "revert=")?;
    require(
        result["summary"]["queryType"] == "version",
        "Semantic revert summary type differs",
    )
}

fn check_provider_missing_inspection_and_drop(
    lithograph: &str,
    provider: &str,
) -> Result<(), Box<dyn Error>> {
    let fixture = FileDatabaseFixture::new(0x1308)?;
    let create = semantic_node_create(
        "missing_sem",
        "Doc",
        "text",
        "synthetic-a",
        "{}",
        4,
        "cosine",
    );
    fixture.execute_script(&format!(
        "{lithograph}\n{provider}\n\
         SELECT lithograph_init();\n\
         SELECT lithograph('CREATE (:Doc {{name:''a'', text:''alpha''}}) FINISH');\n\
         SELECT lithograph({create});\n\
         SELECT lithograph('CALL lithograph.tag.create(''missing-provider'',''branch/main'') YIELD name RETURN name');\n\
         SELECT lithograph('CALL db.index.semantic.rebuild(''missing_sem'',''branch/main'') YIELD embeddedTexts RETURN embeddedTexts');",
        create = sql_literal(&create)
    ))?;
    require_core_cache_absent(&fixture)?;

    let historical_options = sql_literal(r#"{"at":"tag/missing-provider"}"#);
    let inspection = fixture.execute_script(&format!(
        "{lithograph}\n\
         SELECT 'show=' || lithograph('SHOW ALL INDEXES YIELD name, type WHERE name = ''missing_sem'' RETURN name, type', '{{}}', {historical_options});",
        historical_options = historical_options,
    ))?;
    let show = prefixed_json(&inspection, "show=")?;
    require(
        show["rows"] == json!([["missing_sem", "SEMANTIC"]]),
        "SHOW requires Provider",
    )?;

    let query = "CALL db.index.semantic.queryNodes('missing_sem','probe',{limit:1}) YIELD node RETURN node.name";
    let (success, _, stderr) = execute_script_allowing_failure(
        fixture.path(),
        &format!(
            "{lithograph}\nSELECT lithograph({}, '{{}}', {historical_options});",
            sql_literal(query)
        ),
    )?;
    require(
        !success,
        "Semantic query unexpectedly worked without Provider",
    )?;
    require(
        stderr.contains("INVALID_ARGUMENT") && stderr.contains("synthetic-a"),
        "missing Provider failure lost stable category/name",
    )?;

    let rebuild = "CALL db.index.semantic.rebuild('missing_sem','tag/missing-provider')";
    let (success, _, stderr) = execute_script_allowing_failure(
        fixture.path(),
        &format!("{lithograph}\nSELECT lithograph({});", sql_literal(rebuild)),
    )?;
    require(
        !success,
        "Semantic rebuild unexpectedly worked without Provider",
    )?;
    require(
        stderr.contains("INVALID_ARGUMENT") && stderr.contains("synthetic-a"),
        "rebuild missing-Provider failure lost stable category/name",
    )?;

    let dropped = fixture.execute_script(&format!(
        "{lithograph}\nSELECT 'drop=' || json_extract(lithograph('DROP INDEX missing_sem'), '$.summary.counters.indexesRemoved');"
    ))?;
    require_line(&dropped, "drop=1")
}

fn check_failure_atomicity(lithograph: &str, provider: &str) -> Result<(), Box<dyn Error>> {
    let fixture = FileDatabaseFixture::new(0x1309)?;
    fixture.execute_script(&format!(
        "{lithograph}\n{provider}\nSELECT lithograph_init();\n         SELECT lithograph('CREATE (:Doc {{text:''alpha''}}), (:Doc {{text:''beta''}}) FINISH');"
    ))?;
    let before = branch_head_text(&fixture, lithograph)?;
    let invalid_create = semantic_node_create(
        "bad_validate",
        "Doc",
        "text",
        "synthetic-a",
        "{validate:'invalid'}",
        4,
        "cosine",
    );
    let (success, _, stderr) = execute_script_allowing_failure(
        fixture.path(),
        &format!(
            "{lithograph}\n{provider}\nSELECT lithograph({});",
            sql_literal(&invalid_create)
        ),
    )?;
    require(!success, "invalid Provider config unexpectedly published")?;
    require(
        stderr.contains("INVALID_ARGUMENT"),
        "validation error category differs",
    )?;
    require(
        branch_head_text(&fixture, lithograph)? == before,
        "failed create moved Branch head",
    )?;
    let absent = fixture.execute_script(&format!(
        "{lithograph}\nSELECT 'count=' || json_extract(lithograph('SHOW ALL INDEXES YIELD name WHERE name = ''bad_validate'' RETURN count(*)'), '$.rows[0][0]');"
    ))?;
    require_line(&absent, "count=0")?;
    check_provider_payload_failures(&fixture, lithograph, provider)
}

fn check_provider_payload_failures(
    fixture: &FileDatabaseFixture,
    lithograph: &str,
    provider: &str,
) -> Result<(), Box<dyn Error>> {
    let nan = semantic_node_create(
        "bad_nan",
        "Doc",
        "text",
        "synthetic-a",
        "{invalid:'nan'}",
        4,
        "cosine",
    );
    let zero = semantic_node_create(
        "bad_zero",
        "Doc",
        "text",
        "synthetic-a",
        "{invalid:'zero'}",
        4,
        "cosine",
    );
    let io = semantic_node_create(
        "bad_io",
        "Doc",
        "text",
        "synthetic-a",
        "{fail:'io'}",
        4,
        "cosine",
    );
    let cancelled = semantic_node_create(
        "bad_cancel",
        "Doc",
        "text",
        "synthetic-a",
        "{fail:'cancel'}",
        4,
        "cosine",
    );
    fixture.execute_script(&format!(
        "{lithograph}\n{provider}\nSELECT lithograph({nan});\nSELECT lithograph({zero});\nSELECT lithograph({io});\nSELECT lithograph({cancelled});",
        nan = sql_literal(&nan),
        zero = sql_literal(&zero),
        io = sql_literal(&io),
        cancelled = sql_literal(&cancelled),
    ))?;
    let bad_query =
        "CALL db.index.semantic.queryNodes('bad_nan','probe',{limit:2}) YIELD node RETURN node";
    let (success, _, stderr) = execute_script_allowing_failure(
        fixture.path(),
        &format!(
            "{lithograph}\n{provider}\nSELECT lithograph({});",
            sql_literal(bad_query)
        ),
    )?;
    require(
        !success,
        "non-finite Provider payload unexpectedly succeeded",
    )?;
    require(
        stderr.contains("INTERNAL_ERROR"),
        "invalid Provider payload did not fail closed as internal ABI violation",
    )?;
    require_core_cache_absent(fixture)?;

    let zero_query =
        "CALL db.index.semantic.queryNodes('bad_zero','probe',{limit:2}) YIELD node RETURN node";
    let (success, _, stderr) = execute_script_allowing_failure(
        fixture.path(),
        &format!(
            "{lithograph}\n{provider}\nSELECT lithograph({});",
            sql_literal(zero_query)
        ),
    )?;
    require(
        !success,
        "zero cosine Provider payload unexpectedly succeeded",
    )?;
    require(
        stderr.contains("SEMANTIC_ERROR") && stderr.contains("non-zero embeddings"),
        "zero cosine Provider payload did not fail with stable similarity semantics",
    )?;
    require_core_cache_absent(fixture)?;

    check_provider_error_failures(fixture, lithograph, provider)
}

fn check_provider_error_failures(
    fixture: &FileDatabaseFixture,
    lithograph: &str,
    provider: &str,
) -> Result<(), Box<dyn Error>> {
    let bad_io_query =
        "CALL db.index.semantic.queryNodes('bad_io','probe',{limit:2}) YIELD node RETURN node";
    let (success, _, stderr) = execute_script_allowing_failure(
        fixture.path(),
        &format!(
            "{lithograph}\n{provider}\nSELECT lithograph({});",
            sql_literal(bad_io_query)
        ),
    )?;
    require(
        !success,
        "Provider I/O failure unexpectedly completed query",
    )?;
    require(stderr.contains("IO_ERROR"), "Provider I/O category differs")?;
    require_core_cache_absent(fixture)?;

    let cancelled_query =
        "CALL db.index.semantic.queryNodes('bad_cancel','probe',{limit:2}) YIELD node RETURN node";
    let (success, _, stderr) = execute_script_allowing_failure(
        fixture.path(),
        &format!(
            "{lithograph}\n{provider}\nSELECT lithograph({});",
            sql_literal(cancelled_query)
        ),
    )?;
    require(
        !success,
        "cancelled Provider call unexpectedly completed query",
    )?;
    require(
        stderr.contains("INTERRUPTED") && stderr.contains("synthetic configured cancellation"),
        &format!("Provider cancellation category differs: {stderr}"),
    )?;
    require_core_cache_absent(fixture)
}

fn check_rows_adapter_authority(lithograph: &str, provider: &str) -> Result<(), Box<dyn Error>> {
    let fixture = FileDatabaseFixture::new(0x1310)?;
    let create = semantic_node_create("rows_sem", "Doc", "text", "synthetic-a", "{}", 4, "cosine");
    fixture.execute_script(&format!(
        "{lithograph}\n{provider}\nSELECT lithograph_init();\n\
         SELECT lithograph('CREATE (:Doc {{name:''a'', text:''alpha''}}) FINISH');\n\
         SELECT lithograph({create});",
        create = sql_literal(&create)
    ))?;

    let semantic_query = "CALL db.index.semantic.queryNodes('rows_sem','probe',{limit:1}) YIELD node RETURN node.name";
    let output = fixture.execute_script(&format!(
        "{lithograph}\n{provider}\n\
         SELECT ordinal || ':' || event || ':' || data FROM lithograph_rows({query});",
        query = sql_literal(semantic_query),
    ))?;
    require(
        output.lines().any(|line| line.starts_with("0:columns:")),
        "lithograph_rows Semantic query lost columns event",
    )?;
    require(
        output
            .lines()
            .any(|line| line.contains(":row:") && line.contains("\"a\"")),
        "lithograph_rows Semantic query lost row event",
    )?;
    require(
        output.lines().any(|line| line.contains(":summary:")),
        "lithograph_rows Semantic query lost summary event",
    )?;

    let rebuild = "CALL db.index.semantic.rebuild('rows_sem','branch/main') YIELD name, indexedEntities RETURN name, indexedEntities";
    let rebuild_output = fixture.execute_script(&format!(
        "{lithograph}\n{provider}\n\
         SELECT ordinal || ':' || event || ':' || data FROM lithograph_rows({rebuild});",
        rebuild = sql_literal(rebuild),
    ))?;
    require(
        rebuild_output.lines().any(|line| line.contains(":row:")),
        "lithograph_rows Semantic rebuild did not execute through SQL surface",
    )?;
    require(
        rebuild_output
            .lines()
            .any(|line| line.contains(":summary:")),
        "lithograph_rows Semantic rebuild lost summary event",
    )?;

    for procedure in [
        "CALL db.index.semantic.cache.configure({enabled:false})",
        "CALL db.index.semantic.cache.stats()",
        "CALL db.index.semantic.cache.clear()",
    ] {
        let (success, _, stderr) = execute_script_allowing_failure(
            fixture.path(),
            &format!(
                "{lithograph}\nSELECT event, data FROM lithograph_rows({});",
                sql_literal(procedure)
            ),
        )?;
        require(!success, "removed Core cache procedure unexpectedly exists")?;
        require(
            stderr.contains("SEMANTIC_ERROR"),
            &format!("removed Core cache procedure lost semantic error: {stderr}"),
        )?;
    }
    require_core_cache_absent(&fixture)
}

fn check_graph_mutation_provider_boundary(
    lithograph: &str,
    provider: &str,
) -> Result<(), Box<dyn Error>> {
    let fixture = FileDatabaseFixture::new(0x1313)?;
    let create = semantic_node_create(
        "mutation_sem",
        "Doc",
        "text",
        "synthetic-a",
        "{}",
        4,
        "cosine",
    );
    fixture.execute_script(&format!(
        "{lithograph}\n{provider}\nSELECT lithograph_init();\n         SELECT lithograph('CREATE (:Doc {{name:''a'', text:''alpha''}}) FINISH');\n         SELECT lithograph({create});",
        create = sql_literal(&create),
    ))?;
    let before = branch_head_text(&fixture, lithograph)?;
    let commits_before = commit_count(&fixture)?;
    let query = "CALL db.index.semantic.queryNodes('mutation_sem','probe',{limit:1}) YIELD node CREATE (:Audit {name:node.name}) RETURN node.name";
    let output = fixture.execute_script(&format!(
        "{lithograph}\n{provider}\n             SELECT synthetic_embedding_reset('synthetic-a');\n             SELECT 'mutation=' || lithograph({});\n             SELECT 'embed=' || synthetic_embedding_embed_calls('synthetic-a');",
        sql_literal(query),
    ))?;
    require_positive_value(&output, "embed=")?;
    let mutation = prefixed_json(&output, "mutation=")?;
    require(
        mutation["rows"] == json!([["a"]]),
        "Semantic Provider graph mutation result differs",
    )?;
    require(
        mutation["summary"]["queryType"] == "write" && mutation["summary"]["commit"].is_string(),
        "Semantic Provider graph mutation lost write summary",
    )?;
    require(
        branch_head_text(&fixture, lithograph)? != before,
        "successful Semantic Provider graph mutation did not move Branch head",
    )?;
    require(
        commit_count(&fixture)? == commits_before + 1,
        "Semantic Provider graph mutation did not create exactly one Commit",
    )?;
    let audit = fixture.execute_script(&format!(
        "{lithograph}\nSELECT 'audit=' || json_extract(lithograph('MATCH (node:Audit) RETURN count(node)'), '$.rows[0][0]');"
    ))?;
    require_line(&audit, "audit=1")
}

fn check_read_only_rebuild(lithograph: &str, provider: &str) -> Result<(), Box<dyn Error>> {
    let fixture = FileDatabaseFixture::new(0x1312)?;
    let create = semantic_node_create(
        "readonly_sem",
        "Doc",
        "text",
        "synthetic-a",
        "{}",
        4,
        "cosine",
    );
    fixture.execute_script(&format!(
        "{lithograph}\n{provider}\nSELECT lithograph_init();\n         SELECT lithograph('CREATE (:Doc {{name:''a'', text:''alpha''}}) FINISH');\n         SELECT lithograph({create});",
        create = sql_literal(&create),
    ))?;
    let query = "CALL db.index.semantic.queryNodes('readonly_sem','probe',{limit:1}) YIELD node RETURN node.name";
    let (success, stdout, stderr) = execute_readonly_script_allowing_failure(
        fixture.path(),
        &format!(
            "{lithograph}\n{provider}\n             SELECT synthetic_embedding_reset('synthetic-a');\n             SELECT 'query=' || lithograph({});\n             SELECT 'query-embed=' || synthetic_embedding_embed_calls('synthetic-a');",
            sql_literal(query)
        ),
    )?;
    require(
        success && stderr.is_empty() && stdout.contains("\"a\""),
        &format!("read-only Semantic query must remain executable: {stdout:?} {stderr:?}"),
    )?;
    require_line(&stdout, "query-embed=2")?;
    require_core_cache_absent(&fixture)?;

    let rebuild = "CALL db.index.semantic.rebuild('readonly_sem','branch/main')";
    let (success, stdout, stderr) = execute_readonly_script_allowing_failure(
        fixture.path(),
        &format!(
            "{lithograph}\n{provider}\n             SELECT synthetic_embedding_reset('synthetic-a');\n             SELECT lithograph({});\n             SELECT 'embed=' || synthetic_embedding_embed_calls('synthetic-a');",
            sql_literal(rebuild)
        ),
    )?;
    require(
        success && stderr.is_empty(),
        &format!(
            "read-only Semantic rebuild must succeed through TEMP state: {stdout:?} {stderr:?}"
        ),
    )?;
    require_line(&stdout, "embed=1")?;
    require_core_cache_absent(&fixture)
}

fn check_explicit_transaction_staged_semantic(
    lithograph: &str,
    provider: &str,
) -> Result<(), Box<dyn Error>> {
    let fixture = FileDatabaseFixture::new(0x13f1)?;
    let create = semantic_node_create(
        "staged_sem",
        "Doc",
        "text",
        "synthetic-a",
        "{}",
        4,
        "cosine",
    );
    let query = "CALL db.index.semantic.queryNodes('staged_sem','probe',{limit:10}) YIELD node RETURN node.name ORDER BY node.name";

    let output = fixture.execute_script(&format!(
        "{lithograph}\n{provider}\n         SELECT lithograph_init();\n         SELECT lithograph('CREATE (:Doc {{name:''base'', text:''base''}}) FINISH');\n         SELECT lithograph_tx_begin('{{}}');\n         SELECT lithograph({create});\n         SELECT 'staged-create=' || lithograph({query});\n         SELECT lithograph_tx_abort();\n         SELECT 'post-create-abort=' || json_extract(lithograph(
           'SHOW ALL INDEXES YIELD name WHERE name = ''staged_sem'' RETURN count(*)'
         ), '$.rows[0][0]');",
        create = sql_literal(&create),
        query = sql_literal(query),
    ))?;
    let staged_create = prefixed_json(&output, "staged-create=")?;
    require(
        staged_create["rows"] == json!([["base"]]),
        "explicit transaction Semantic query did not observe staged definition",
    )?;
    require_line(&output, "post-create-abort=0")?;

    let output = fixture.execute_script(&format!(
        "{lithograph}\n{provider}\n         SELECT lithograph({create});\n         SELECT lithograph_tx_begin('{{}}');\n         SELECT lithograph('CREATE (:Doc {{name:''staged'', text:''staged''}}) FINISH');\n         SELECT 'staged-source=' || lithograph({query});\n         SELECT lithograph_tx_abort();\n         SELECT 'post-source-abort=' || lithograph({query});",
        create = sql_literal(&create),
        query = sql_literal(query),
    ))?;
    let staged_source = prefixed_json(&output, "staged-source=")?;
    require(
        staged_source["rows"] == json!([["base"], ["staged"]]),
        "explicit transaction Semantic query did not observe staged source membership",
    )?;
    let post_source_abort = prefixed_json(&output, "post-source-abort=")?;
    require(
        post_source_abort["rows"] == json!([["base"]]),
        "aborted staged Semantic source remained visible",
    )?;

    let (ok, _stdout, stderr) = execute_script_allowing_failure(
        fixture.path(),
        &format!(
            "{lithograph}\n{provider}\n             SELECT lithograph_tx_begin('{{}}');\n             SELECT lithograph('DROP INDEX staged_sem');\n             SELECT lithograph({query});",
            query = sql_literal(query),
        ),
    )?;
    require(
        !ok && stderr.contains("staged_sem"),
        &format!("staged Semantic drop was not observed by query: {stderr}"),
    )?;

    let output = fixture.execute_script(&format!(
        "{lithograph}\n{provider}\n         SELECT 'post-drop-failure=' || lithograph({query});",
        query = sql_literal(query),
    ))?;
    let post_drop_failure = prefixed_json(&output, "post-drop-failure=")?;
    require(
        post_drop_failure["rows"] == json!([["base"]]),
        "failed staged Semantic drop damaged committed definition",
    )
}

fn check_openai_schema_roundtrip(
    lithograph: &str,
    openai_provider: &str,
) -> Result<(), Box<dyn Error>> {
    let fixture = FileDatabaseFixture::new(0x1311)?;
    let config_a = r#"{base_url:'https://one.example/v1',api_key_env:'PHASE13_KEY',model:'model-a',send_dimensions:false,encoding_format:'base64',user:'user-a',organization:'org-a',project:'project-a',headers:{XRoute:'one'},timeout_ms:1234,max_retries:3,batch_size:7,semantic_identity:'deploy-a'}"#;
    let config_b = r#"{base_url:'https://two.example/v1',api_key:'direct-value',model:'model-b',encoding_format:'float',timeout_ms:4321,max_retries:1,batch_size:9,semantic_identity:'deploy-b'}"#;
    let create_a = semantic_node_create(
        "openai_a",
        "Doc",
        "text",
        "openai-compatible",
        config_a,
        8,
        "cosine",
    );
    let create_b = semantic_node_create(
        "openai_b",
        "Doc",
        "text",
        "openai-compatible",
        config_b,
        8,
        "euclidean",
    );
    let output = fixture.execute_script(&format!(
        "{lithograph}\n{openai_provider}\nSELECT lithograph_init();\n         SELECT lithograph({a});\nSELECT lithograph({b});\n         SELECT 'show=' || lithograph('SHOW ALL INDEXES YIELD name, indexProvider, options WHERE name STARTS WITH ''openai_'' RETURN name, indexProvider, options ORDER BY name');",
        a = sql_literal(&create_a),
        b = sql_literal(&create_b),
    ))?;
    let show = prefixed_json(&output, "show=")?;
    require(
        show["rows"].as_array().is_some_and(|rows| rows.len() == 2),
        "OpenAI indices missing",
    )?;
    require(
        show["rows"][0][1] == "openai-compatible",
        "OpenAI provider name differs",
    )?;
    require(
        show["rows"][0][2]["indexConfig"]["providerConfig"]["api_key_env"] == "PHASE13_KEY",
        "OpenAI api_key_env did not round-trip through Schema",
    )?;
    require(
        show["rows"][1][2]["indexConfig"]["providerConfig"]["model"] == "model-b",
        "one provider registration did not preserve per-index config",
    )
}
