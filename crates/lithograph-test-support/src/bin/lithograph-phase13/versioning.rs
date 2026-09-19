use super::support::{
    branch_head_text, execute_script_allowing_failure, require, semantic_node_create, sql_literal,
};
use lithograph_test_support::sqlite::FileDatabaseFixture;
use serde_json::{Value as JsonValue, json};
use std::error::Error;

pub(super) fn check_semantic_diff_patch(
    lithograph: &str,
    provider: &str,
) -> Result<(), Box<dyn Error>> {
    let (fixture, loads, patch) = semantic_diff_fixture(lithograph, provider)?;
    assert_semantic_diff_shape(&patch)?;
    check_semantic_patch_publication(&fixture, lithograph, &loads, patch)
}

fn semantic_diff_fixture(
    lithograph: &str,
    provider: &str,
) -> Result<(FileDatabaseFixture, String, JsonValue), Box<dyn Error>> {
    let fixture = FileDatabaseFixture::new(0x1315)?;
    let loads = format!("{lithograph}\n{provider}");
    fixture.execute_script(&format!("{loads}\nSELECT lithograph_init();"))?;

    let base_create = semantic_node_create(
        "version_sem",
        "Doc",
        "text",
        "synthetic-a",
        r#"{"variant":"base"}"#,
        4,
        "cosine",
    );
    scalar_query(&fixture, &loads, &base_create, "{}", "{}")?;
    scalar_query(
        &fixture,
        lithograph,
        "CALL lithograph.tag.create('semantic-base','branch/main') YIELD name RETURN name",
        "{}",
        "{}",
    )?;
    scalar_query(&fixture, lithograph, "DROP INDEX version_sem", "{}", "{}")?;

    let next_create = semantic_node_create(
        "version_sem",
        "Doc",
        "body",
        "synthetic-b",
        r#"{"variant":"next"}"#,
        4,
        "cosine",
    );
    scalar_query(&fixture, &loads, &next_create, "{}", "{}")?;
    scalar_query(
        &fixture,
        lithograph,
        "CALL lithograph.tag.create('semantic-next','branch/main') YIELD name RETURN name",
        "{}",
        "{}",
    )?;
    let diff = scalar_query(
        &fixture,
        lithograph,
        "CALL lithograph.diff('tag/semantic-base','tag/semantic-next') YIELD patch RETURN patch",
        "{}",
        "{}",
    )?;
    Ok((fixture, loads, diff["rows"][0][0].clone()))
}

fn assert_semantic_diff_shape(patch: &JsonValue) -> Result<(), Box<dyn Error>> {
    let operations = patch["operations"]
        .as_array()
        .ok_or("Semantic diff patch has no operations array")?;
    require(
        operations.len() == 1
            && operations[0]["op"] == "SetIndex"
            && operations[0]["slot"] == "index/version_sem",
        "provider/config/source change was not one Semantic Index slot update",
    )?;
    require(
        operations[0]["before"]["configuration"]["provider"] == "synthetic-a"
            && operations[0]["after"]["configuration"]["provider"] == "synthetic-b"
            && operations[0]["after"]["configuration"]["provider_config"]["variant"] == "next"
            && operations[0]["after"]["target"]["properties"] == json!(["body"]),
        "Semantic diff lost provider/config/source definition changes",
    )
}

fn check_semantic_patch_publication(
    fixture: &FileDatabaseFixture,
    lithograph: &str,
    loads: &str,
    patch: JsonValue,
) -> Result<(), Box<dyn Error>> {
    scalar_query(
        fixture,
        lithograph,
        "CALL lithograph.reset('tag/semantic-base') YIELD to RETURN to",
        "{}",
        "{}",
    )?;
    let before = branch_head_text(fixture, lithograph)?;
    let params = json!({"patch": patch}).to_string();
    let apply = "CALL lithograph.patch.apply($patch) YIELD commit RETURN commit";
    let (success, _, stderr) = execute_script_allowing_failure(
        fixture.path(),
        &scalar_script(lithograph, apply, &params, "{}"),
    )?;
    require(
        !success,
        "Semantic Patch unexpectedly published without Provider",
    )?;
    require(
        stderr.contains("INVALID_ARGUMENT") && stderr.contains("synthetic-b"),
        "Semantic Patch missing-Provider failure lost stable category/name",
    )?;
    require(
        branch_head_text(fixture, lithograph)? == before,
        "failed Semantic Patch moved Branch head",
    )?;

    scalar_query(fixture, loads, apply, &params, "{}")?;
    let show = scalar_query(
        fixture,
        lithograph,
        "SHOW ALL INDEXES YIELD name, indexProvider, options WHERE name = 'version_sem' RETURN indexProvider, options",
        "{}",
        "{}",
    )?;
    require(
        show["rows"][0][0] == "synthetic-b"
            && show["rows"][0][1]["indexConfig"]["providerConfig"]["variant"] == "next",
        "Semantic Patch replay did not publish the target definition",
    )
}

pub(super) fn check_semantic_merge_rebase(
    lithograph: &str,
    provider: &str,
) -> Result<(), Box<dyn Error>> {
    check_semantic_merge_publication(lithograph, provider)?;
    check_semantic_rebase_publication(lithograph, provider)
}

fn check_semantic_merge_publication(
    lithograph: &str,
    provider: &str,
) -> Result<(), Box<dyn Error>> {
    let fixture = FileDatabaseFixture::new(0x1316)?;
    let loads = format!("{lithograph}\n{provider}");
    fixture.execute_script(&format!("{loads}\nSELECT lithograph_init();"))?;
    scalar_query(
        &fixture,
        lithograph,
        "CREATE (:Doc {text:'alpha'}) FINISH",
        "{}",
        "{}",
    )?;
    scalar_query(
        &fixture,
        lithograph,
        "CALL lithograph.branch.create('semantic-feature') YIELD name RETURN name",
        "{}",
        "{}",
    )?;
    let create = semantic_node_create(
        "merge_sem",
        "Doc",
        "text",
        "synthetic-a",
        r#"{"variant":"merge"}"#,
        4,
        "cosine",
    );
    scalar_query(
        &fixture,
        &loads,
        &create,
        "{}",
        r#"{"branch":"semantic-feature"}"#,
    )?;
    scalar_query(
        &fixture,
        lithograph,
        "CREATE (:MainOnly) FINISH",
        "{}",
        "{}",
    )?;

    let started = scalar_query(
        &fixture,
        lithograph,
        "CALL lithograph.merge.start('branch/semantic-feature') YIELD session, revision, status RETURN session, revision, status",
        "{}",
        "{}",
    )?;
    require(
        started["rows"][0][2] == "ready",
        "Semantic Merge did not prepare a ready candidate",
    )?;
    let session = started["rows"][0][0]
        .as_str()
        .ok_or("Semantic Merge did not return session")?;
    let revision = started["rows"][0][1]
        .as_i64()
        .ok_or("Semantic Merge did not return revision")?;
    let finalize = format!(
        "CALL lithograph.merge.finalize('{session}', {revision}) YIELD status, commit RETURN status, commit"
    );
    assert_publication_requires_provider(&fixture, lithograph, &finalize, "synthetic-a")?;
    let merged = scalar_query(&fixture, &loads, &finalize, "{}", "{}")?;
    require(
        merged["rows"][0][0] == "merged",
        "Semantic Merge did not finalize as a real merge",
    )
}

fn check_semantic_rebase_publication(
    lithograph: &str,
    provider: &str,
) -> Result<(), Box<dyn Error>> {
    let fixture = FileDatabaseFixture::new(0x1317)?;
    let loads = format!("{lithograph}\n{provider}");
    fixture.execute_script(&format!("{loads}\nSELECT lithograph_init();"))?;
    scalar_query(
        &fixture,
        lithograph,
        "CREATE (:Doc {text:'alpha'}) FINISH",
        "{}",
        "{}",
    )?;
    scalar_query(
        &fixture,
        lithograph,
        "CALL lithograph.branch.create('semantic-onto') YIELD name RETURN name",
        "{}",
        "{}",
    )?;
    let create = semantic_node_create(
        "rebase_sem",
        "Doc",
        "text",
        "synthetic-a",
        r#"{"variant":"rebase"}"#,
        4,
        "cosine",
    );
    scalar_query(&fixture, &loads, &create, "{}", "{}")?;
    scalar_query(
        &fixture,
        lithograph,
        "CREATE (:OntoOnly) FINISH",
        "{}",
        r#"{"branch":"semantic-onto"}"#,
    )?;

    let rebase =
        "CALL lithograph.rebase('branch/semantic-onto') YIELD status, commit RETURN status, commit";
    assert_publication_requires_provider(&fixture, lithograph, rebase, "synthetic-a")?;
    let rebased = scalar_query(&fixture, &loads, rebase, "{}", "{}")?;
    require(
        rebased["rows"][0][0] == "rebased",
        "Semantic Rebase did not publish after Provider became available",
    )
}

fn assert_publication_requires_provider(
    fixture: &FileDatabaseFixture,
    lithograph: &str,
    query: &str,
    provider_name: &str,
) -> Result<(), Box<dyn Error>> {
    let before = branch_head_text(fixture, lithograph)?;
    let (success, _, stderr) = execute_script_allowing_failure(
        fixture.path(),
        &scalar_script(lithograph, query, "{}", "{}"),
    )?;
    require(
        !success,
        "Semantic version publication unexpectedly skipped Provider",
    )?;
    require(
        stderr.contains("INVALID_ARGUMENT") && stderr.contains(provider_name),
        "Semantic version publication missing-Provider error lost category/name",
    )?;
    require(
        branch_head_text(fixture, lithograph)? == before,
        "failed Semantic version publication moved Branch head",
    )
}

fn scalar_query(
    fixture: &FileDatabaseFixture,
    loads: &str,
    query: &str,
    params: &str,
    options: &str,
) -> Result<JsonValue, Box<dyn Error>> {
    let output = fixture.execute_script(&scalar_script(loads, query, params, options))?;
    Ok(serde_json::from_str(output.trim())?)
}

fn scalar_script(loads: &str, query: &str, params: &str, options: &str) -> String {
    format!(
        "{loads}\nSELECT lithograph({}, {}, {});",
        sql_literal(query),
        sql_literal(params),
        sql_literal(options)
    )
}
