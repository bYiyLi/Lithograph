#![forbid(unsafe_code)]

use lithograph_core::storage::{HashId, branch_head};
use lithograph_test_support::sqlite::{FileDatabaseFixture, FixtureError, extension_load_command};
use rusqlite::Connection;
use serde::Serialize;
use std::env;
use std::error::Error;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

#[derive(Serialize)]
struct ProbeResult {
    phase: &'static str,
    extension: String,
    tokenizer_extension: String,
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
    let tokenizer_extension = required_path_arg(2, "Phase 12 tokenizer extension")?;
    let lithograph_load = extension_load_command(&extension)?;
    let tokenizer_load = tokenizer_load_command(&tokenizer_extension)?;
    let mut checks = Vec::new();

    check_direct_fts5_oracle(&tokenizer_load)?;
    checks.push("direct-fts5-oracle");
    check_lithograph_load_order(&lithograph_load, &tokenizer_load, true)?;
    checks.push("tokenizer-before-lithograph");
    check_lithograph_load_order(&lithograph_load, &tokenizer_load, false)?;
    checks.push("lithograph-before-tokenizer");
    check_constructor_failure_is_atomic(&lithograph_load, &tokenizer_load)?;
    checks.push("constructor-failure-atomic");
    check_tokenization_failure_is_quarantined(&lithograph_load, &tokenizer_load)?;
    checks.push("tokenization-failure-quarantined");
    check_connection_local_registration(&lithograph_load, &tokenizer_load)?;
    checks.push("connection-local-registration");
    check_merge_publication_validation(&lithograph_load, &tokenizer_load)?;
    checks.push("merge-publication-validation");

    Ok(ProbeResult {
        phase: "12-fulltext-tokenizer",
        extension: extension.display().to_string(),
        tokenizer_extension: tokenizer_extension.display().to_string(),
        checks,
    })
}

fn required_path_arg(index: usize, label: &str) -> Result<PathBuf, Box<dyn Error>> {
    let path = env::args()
        .nth(index)
        .map(PathBuf::from)
        .ok_or_else(|| {
            format!(
                "usage: lithograph-phase12 <extension-path> <phase12-tokenizer-extension-path>; missing {label}"
            )
        })?;
    Ok(fs::canonicalize(path)?)
}

fn tokenizer_load_command(path: &Path) -> Result<String, Box<dyn Error>> {
    Ok(format!(
        "{} sqlite3_phase12_tokenizer_init",
        extension_load_command(path)?
    ))
}

fn check_direct_fts5_oracle(tokenizer_load: &str) -> Result<(), Box<dyn Error>> {
    let fixture = FileDatabaseFixture::new(0x1201)?;
    let output = fixture.execute_script(&format!(
        "{tokenizer_load}\n\
         SELECT phase12_tokenizer_reset();\n\
         CREATE VIRTUAL TABLE temp.phase12_direct USING fts5(value, tokenize='phase12_echo first ''two words'' '''' 中文');\n\
         SELECT 'args=' || phase12_tokenizer_name() || ':' || phase12_tokenizer_args();\n\
         INSERT INTO temp.phase12_direct(value) VALUES('document doc usa');\n\
         SELECT 'query=' || count(*) FROM temp.phase12_direct WHERE phase12_direct MATCH 'needle';\n\
         SELECT 'prefix=' || count(*) FROM temp.phase12_direct WHERE phase12_direct MATCH 'pref*';\n\
         DROP TABLE temp.phase12_direct;\n\
         CREATE VIRTUAL TABLE temp.phase12_syn USING fts5(value, tokenize='phase12_synonym');\n\
         INSERT INTO temp.phase12_syn(value) VALUES('usa');\n\
         SELECT 'synonym=' || count(*) FROM temp.phase12_syn WHERE phase12_syn MATCH 'america';\n\
         SELECT 'flags=' || phase12_tokenizer_stat('document') || ':' || phase12_tokenizer_stat('query') || ':' || phase12_tokenizer_stat('prefix') || ':' || phase12_tokenizer_stat('colocated');\n\
         DROP TABLE temp.phase12_syn;\n\
         SELECT 'lifetime=' || phase12_tokenizer_stat('create') || ':' || phase12_tokenizer_stat('delete');\n\
         CREATE VIRTUAL TABLE temp.phase12_legacy_english USING fts5(value, tokenize='english');\n\
         INSERT INTO temp.phase12_legacy_english(value) VALUES('document');\n\
         SELECT 'legacy-english=' || count(*) FROM temp.phase12_legacy_english WHERE phase12_legacy_english MATCH 'needle';\n\
         DROP TABLE temp.phase12_legacy_english;\n\
         CREATE VIRTUAL TABLE temp.phase12_legacy_standard USING fts5(value, tokenize='''standard-no-stop-words''');\n\
         INSERT INTO temp.phase12_legacy_standard(value) VALUES('document');\n\
         SELECT 'legacy-standard=' || count(*) FROM temp.phase12_legacy_standard WHERE phase12_legacy_standard MATCH 'needle';\n\
         DROP TABLE temp.phase12_legacy_standard;"
    ))?;
    require_contains(&output, "args=phase12_echo:first|two words||中文")?;
    require_contains(&output, "query=1")?;
    require_contains(&output, "prefix=1")?;
    require_contains(&output, "synonym=1")?;
    require_contains(&output, "flags=2:3:1:1")?;
    require_contains(&output, "lifetime=2:2")?;
    require_contains(&output, "legacy-english=1")?;
    require_contains(&output, "legacy-standard=1")
}

fn check_lithograph_load_order(
    lithograph_load: &str,
    tokenizer_load: &str,
    tokenizer_first: bool,
) -> Result<(), Box<dyn Error>> {
    let seed = if tokenizer_first { 0x1202 } else { 0x1203 };
    let fixture = FileDatabaseFixture::new(seed)?;
    let loads = if tokenizer_first {
        format!("{tokenizer_load}\n{lithograph_load}")
    } else {
        format!("{lithograph_load}\n{tokenizer_load}")
    };
    let custom_spec = "phase12_echo first 'two words' '' 中文";
    let create_custom = format!(
        "CREATE FULLTEXT INDEX custom_text FOR (n:Custom) ON EACH [n.text] OPTIONS {{indexConfig:{{`fulltext.analyzer`: \"{custom_spec}\"}}}}"
    );
    let script = format!(
        "{loads}\n\
         SELECT lithograph_init();\n\
         SELECT {reset};\n\
         SELECT {create_data};\n\
         SELECT {create_custom};\n\
         SELECT 'custom-args=' || phase12_tokenizer_name() || ':' || phase12_tokenizer_args();\n\
         SELECT 'custom-hit=' || json_extract({custom_query}, '$.rows[0][0]');\n\
         SELECT 'rows-hit=' || json_extract((SELECT row FROM lithograph_rows({rows_query}) WHERE ordinal=0), '$[0]');\n\
         SELECT {create_override};\n\
         SELECT {create_query_synonym};\n\
         SELECT phase12_tokenizer_reset();\n\
         SELECT 'override-hit=' || json_extract({override_query}, '$.rows[0][0]');\n\
         SELECT 'override-args=' || phase12_tokenizer_name() || ':' || phase12_tokenizer_args();\n\
         SELECT 'prefix-hit=' || json_extract({prefix_query}, '$.rows[0][0]');\n\
         SELECT 'override-flags=' || phase12_tokenizer_stat('query') || ':' || phase12_tokenizer_stat('prefix');\n\
         SELECT 'override-lifetime=' || phase12_tokenizer_stat('create') || ':' || phase12_tokenizer_stat('delete');\n\
         SELECT 'query-synonym-hit=' || json_extract({query_synonym_query}, '$.rows[0][0]');\n\
         SELECT 'override-temp=' || count(*) FROM temp.sqlite_schema WHERE type='table' AND name GLOB '_lithograph_fts_override_*' AND sql LIKE 'CREATE VIRTUAL TABLE%';\n\
         SELECT 'adapter-probe=' || phase12_adapter_probe();\n\
         SELECT {create_legacy_english};\n\
         SELECT 'legacy-english-hit=' || json_extract({legacy_english_query}, '$.rows[0][0]');\n\
         SELECT {create_legacy_standard};\n\
         SELECT 'legacy-standard-hit=' || json_extract({legacy_standard_query}, '$.rows[0][0]');\n\
         SELECT {create_synonym};\n\
         SELECT 'synonym-hit=' || json_extract({synonym_query}, '$.rows[0][0]');\n\
         SELECT 'colocated=' || phase12_tokenizer_stat('colocated');",
        reset = "phase12_tokenizer_reset()",
        create_data = lithograph_call(
            "CREATE (:Custom {name:'custom', text:'document doc usa'}), (:Override {name:'override', text:'document doc'}), (:QuerySynonym {name:'query-synonym', text:'america'}), (:LegacyEnglish {name:'legacy-english', text:'document'}), (:LegacyStandard {name:'legacy-standard', text:'document'}), (:Syn {name:'synonym', text:'usa'}) FINISH"
        ),
        create_custom = lithograph_call(&create_custom),
        custom_query = lithograph_call(
            "CALL db.index.fulltext.queryNodes('custom_text', 'needle') YIELD node RETURN node.name"
        ),
        rows_query = sql_literal(
            "CALL db.index.fulltext.queryNodes('custom_text', 'needle') YIELD node RETURN node.name"
        ),
        create_override = lithograph_call(
            "CREATE FULLTEXT INDEX override_text FOR (n:Override) ON EACH [n.text]"
        ),
        create_query_synonym = lithograph_call(
            "CREATE FULLTEXT INDEX query_synonym_text FOR (n:QuerySynonym) ON EACH [n.text]"
        ),
        override_query = lithograph_call(
            "CALL db.index.fulltext.queryNodes('override_text', 'needle', {analyzer:\"phase12_echo first 'two words' '' 中文\"}) YIELD node RETURN node.name"
        ),
        prefix_query = lithograph_call(
            "CALL db.index.fulltext.queryNodes('override_text', 'pref*', {analyzer:'phase12_echo'}) YIELD node RETURN node.name"
        ),
        query_synonym_query = lithograph_call(
            "CALL db.index.fulltext.queryNodes('query_synonym_text', 'usa', {analyzer:'phase12_synonym'}) YIELD node RETURN node.name"
        ),
        create_legacy_english = lithograph_call(
            "CREATE FULLTEXT INDEX legacy_english_text FOR (n:LegacyEnglish) ON EACH [n.text] OPTIONS {indexConfig:{`fulltext.analyzer`:'english'}}"
        ),
        legacy_english_query = lithograph_call(
            "CALL db.index.fulltext.queryNodes('legacy_english_text', 'needle') YIELD node RETURN node.name"
        ),
        create_legacy_standard = lithograph_call(
            "CREATE FULLTEXT INDEX legacy_standard_text FOR (n:LegacyStandard) ON EACH [n.text] OPTIONS {indexConfig:{`fulltext.analyzer`: \"'standard-no-stop-words'\"}}"
        ),
        legacy_standard_query = lithograph_call(
            "CALL db.index.fulltext.queryNodes('legacy_standard_text', 'needle') YIELD node RETURN node.name"
        ),
        create_synonym = lithograph_call(
            "CREATE FULLTEXT INDEX synonym_text FOR (n:Syn) ON EACH [n.text] OPTIONS {indexConfig:{`fulltext.analyzer`:'phase12_synonym'}}"
        ),
        synonym_query = lithograph_call(
            "CALL db.index.fulltext.queryNodes('synonym_text', 'america') YIELD node RETURN node.name"
        ),
    );
    let output = fixture.execute_script(&script)?;
    require_contains(&output, "custom-args=phase12_echo:first|two words||中文")?;
    require_contains(&output, "custom-hit=custom")?;
    require_contains(&output, "rows-hit=custom")?;
    require_contains(&output, "override-hit=override")?;
    require_contains(&output, "override-args=phase12_echo:first|two words||中文")?;
    require_contains(&output, "prefix-hit=override")?;
    require_nonzero_pair(&output, "override-flags=")?;
    require_contains(&output, "override-lifetime=2:2")?;
    require_contains(&output, "query-synonym-hit=query-synonym")?;
    require_contains(&output, "override-temp=2")?;
    require_contains(&output, "adapter-probe=1")?;
    require_contains(&output, "legacy-english-hit=legacy-english")?;
    require_contains(&output, "legacy-standard-hit=legacy-standard")?;
    require_contains(&output, "synonym-hit=synonym")?;
    require_positive_value(&output, "colocated=")
}

fn check_constructor_failure_is_atomic(
    lithograph_load: &str,
    tokenizer_load: &str,
) -> Result<(), Box<dyn Error>> {
    let fixture = FileDatabaseFixture::new(0x1204)?;
    fixture.execute_script(&format!(
        "{tokenizer_load}\n{lithograph_load}\nSELECT lithograph_init();\nSELECT {};",
        lithograph_call("CREATE (:ProbeFailure {text:'graph'}) FINISH")
    ))?;
    let create = "CREATE FULLTEXT INDEX must_fail FOR (n:ProbeFailure) ON EACH [n.text] OPTIONS {indexConfig:{`fulltext.analyzer`:'phase12_echo fail'}}";
    let script = format!(
        "{tokenizer_load}\n{lithograph_load}\nSELECT {};",
        lithograph_call(create)
    );
    match fixture.execute_script(&script) {
        Err(FixtureError::Sqlite { stderr, .. }) => require(
            stderr.contains("SCHEMA_ERROR"),
            "constructor failure must surface as SCHEMA_ERROR",
        )?,
        Err(error) => return Err(error.into()),
        Ok(_) => return Err("failing tokenizer constructor unexpectedly published Index".into()),
    }
    let resource_create = "CREATE FULLTEXT INDEX must_resource_fail FOR (n:ProbeFailure) ON EACH [n.text] OPTIONS {indexConfig:{`fulltext.analyzer`:'phase12_echo nomem'}}";
    let resource_script = format!(
        "{tokenizer_load}\n{lithograph_load}\nSELECT {};",
        lithograph_call(resource_create)
    );
    match fixture.execute_script(&resource_script) {
        Err(FixtureError::Sqlite { stderr, .. }) => require(
            stderr.contains("out of memory"),
            "SQL Bridge must expose SQLite's canonical SQLITE_NOMEM text",
        )?,
        Err(error) => return Err(error.into()),
        Ok(_) => {
            return Err(
                "resource-failing tokenizer constructor unexpectedly published Index".into(),
            );
        }
    }
    let show = fixture.execute_script(&format!(
        "{lithograph_load}\nSELECT json_array_length(json_extract({}, '$.rows'));",
        lithograph_call("SHOW FULLTEXT INDEXES YIELD name WHERE name IN ['must_fail', 'must_resource_fail'] RETURN name")
    ))?;
    require(
        show.trim() == "0",
        "failed schema publication left an Index definition",
    )
}

fn check_tokenization_failure_is_quarantined(
    lithograph_load: &str,
    tokenizer_load: &str,
) -> Result<(), Box<dyn Error>> {
    let fixture = FileDatabaseFixture::new(0x1206)?;
    let script = format!(
        "{tokenizer_load}\n{lithograph_load}\n\
         SELECT lithograph_init();\n\
         SELECT {create_data};\n\
         SELECT {create_index};\n\
         SELECT 'before=' || count(*) FROM temp.sqlite_schema WHERE type='table' AND name GLOB '_lithograph_fts_*' AND sql LIKE 'CREATE VIRTUAL TABLE%';\n\
         SELECT {query};\n\
         SELECT 'after1=' || count(*) FROM temp.sqlite_schema WHERE type='table' AND name GLOB '_lithograph_fts_*' AND sql LIKE 'CREATE VIRTUAL TABLE%';\n\
         SELECT 'ready1=' || count(*) FROM temp._lithograph_semantic_fts_ready;\n\
         SELECT 'life1=' || phase12_tokenizer_stat('create') || ':' || phase12_tokenizer_stat('delete');\n\
         SELECT {query};\n\
         SELECT 'after2=' || count(*) FROM temp.sqlite_schema WHERE type='table' AND name GLOB '_lithograph_fts_*' AND sql LIKE 'CREATE VIRTUAL TABLE%';\n\
         SELECT 'ready2=' || count(*) FROM temp._lithograph_semantic_fts_ready;\n\
         SELECT 'life2=' || phase12_tokenizer_stat('create') || ':' || phase12_tokenizer_stat('delete');",
        create_data = lithograph_call(
            "CREATE (:TokenizeFailure {name:'good', text:'document'}), (:TokenizeFailure {name:'bad', text:'explode'}) FINISH"
        ),
        create_index = lithograph_call(
            "CREATE FULLTEXT INDEX tokenize_failure_text FOR (n:TokenizeFailure) ON EACH [n.text] OPTIONS {indexConfig:{`fulltext.analyzer`:'phase12_echo fail_on_bad'}}"
        ),
        query = lithograph_call(
            "CALL db.index.fulltext.queryNodes('tokenize_failure_text', 'needle') YIELD node RETURN node.name"
        ),
    );
    let (success, stdout, stderr) = execute_script_allowing_failure(fixture.path(), &script)?;
    require(
        !success,
        "injected tokenizer failure unexpectedly succeeded",
    )?;
    require(
        stderr.matches("LITHOGRAPH_SEMANTIC_ERROR").count() == 2,
        "both injected tokenization failures must surface as SEMANTIC_ERROR",
    )?;
    require(
        !stderr.contains("LITHOGRAPH_BUSY"),
        "quarantined failed cache must not turn a retry into BUSY",
    )?;
    for expected in ["before=0", "after1=1", "ready1=0", "after2=1", "ready2=0"] {
        require_contains(&stdout, expected)?;
    }
    require(
        output_value(&stdout, "life1=")? == output_value(&stdout, "life2=")?,
        "retry must reset and reuse the quarantined TEMP table instead of constructing another cache",
    )
}

fn check_connection_local_registration(
    lithograph_load: &str,
    tokenizer_load: &str,
) -> Result<(), Box<dyn Error>> {
    let fixture = FileDatabaseFixture::new(0x1205)?;
    fixture.execute_script(&format!(
        "{tokenizer_load}\n{lithograph_load}\n\
         SELECT lithograph_init();\n\
         SELECT {};\n\
         SELECT {};\n\
         SELECT {};\n\
         SELECT {};",
        lithograph_call("CREATE (:LocalTokenizer {name:'before', text:'document'}) FINISH"),
        lithograph_call("CREATE FULLTEXT INDEX local_text FOR (n:LocalTokenizer) ON EACH [n.text] OPTIONS {indexConfig:{`fulltext.analyzer`:'phase12_echo'}}"),
        lithograph_call("CALL lithograph.tag.create('phase12-custom', 'branch/main') YIELD name RETURN name"),
        lithograph_call("CREATE (:LocalTokenizer {name:'future', text:'future'}) FINISH")
    ))?;

    let show = fixture.execute_script(&format!(
        "{lithograph_load}\nSELECT 'show=' || json_extract({}, '$.rows[0][0]');",
        lithograph_call("SHOW FULLTEXT INDEXES YIELD name WHERE name = 'local_text' RETURN name")
    ))?;
    require_contains(&show, "show=local_text")?;

    let query_without_plugin = format!(
        "{lithograph_load}\nSELECT {};",
        lithograph_call_with_options(
            "CALL db.index.fulltext.queryNodes('local_text', 'needle') YIELD node RETURN node.name",
            r#"{"at":"tag/phase12-custom"}"#,
        )
    );
    match fixture.execute_script(&query_without_plugin) {
        Err(FixtureError::Sqlite { stderr, .. }) => require(
            stderr.contains("SEMANTIC_ERROR"),
            "missing connection-local tokenizer must surface as SEMANTIC_ERROR",
        )?,
        Err(error) => return Err(error.into()),
        Ok(_) => {
            return Err("custom tokenizer unexpectedly leaked across SQLite connections".into());
        }
    }

    let historical_with_plugin = fixture.execute_script(&format!(
        "{tokenizer_load}\n{lithograph_load}\nSELECT 'historical=' || json_extract({}, '$.rows[0][0]');",
        lithograph_call_with_options(
            "CALL db.index.fulltext.queryNodes('local_text', 'needle') YIELD node RETURN node.name",
            r#"{"at":"tag/phase12-custom"}"#,
        )
    ))?;
    require_contains(&historical_with_plugin, "historical=before")?;

    fixture.execute_script(&format!(
        "{lithograph_load}\nSELECT {};",
        lithograph_call("CREATE (:LocalTokenizer {name:'after', text:'other'}) FINISH")
    ))?;
    fixture.execute_script(&format!(
        "{lithograph_load}\nSELECT {};",
        lithograph_call("DROP INDEX local_text")
    ))?;
    let show_after_drop = fixture.execute_script(&format!(
        "{lithograph_load}\nSELECT json_array_length(json_extract({}, '$.rows'));",
        lithograph_call("SHOW FULLTEXT INDEXES YIELD name WHERE name = 'local_text' RETURN name")
    ))?;
    require(
        show_after_drop.trim() == "0",
        "DROP of unavailable custom tokenizer Index should not require constructor",
    )
}

fn check_merge_publication_validation(
    lithograph_load: &str,
    tokenizer_load: &str,
) -> Result<(), Box<dyn Error>> {
    let (fixture, trusted_loads, finalize, before) =
        prepare_merge_publication_fixture(lithograph_load, tokenizer_load)?;
    assert_merge_finalize_rejected(&fixture, lithograph_load, &finalize, &before)?;

    let finalized = scalar_query(&fixture, &trusted_loads, &finalize, "{}")?;
    require(
        finalized["rows"][0][0] == "merged",
        "Merge must remain retryable after registering the required tokenizer",
    )?;
    require(
        branch_head(&Connection::open(fixture.path())?, "main")? != before,
        "successful Full-text Merge must move main Branch",
    )
}

fn prepare_merge_publication_fixture(
    lithograph_load: &str,
    tokenizer_load: &str,
) -> Result<(FileDatabaseFixture, String, String, HashId), Box<dyn Error>> {
    let fixture = FileDatabaseFixture::new(0x1207)?;
    let trusted_loads = format!("{tokenizer_load}\n{lithograph_load}");
    fixture.execute_script(&format!("{trusted_loads}\nSELECT lithograph_init();"))?;
    scalar_query(
        &fixture,
        &trusted_loads,
        "CREATE (:MergeDoc {name:'base', text:'document'}) FINISH",
        "{}",
    )?;
    scalar_query(
        &fixture,
        &trusted_loads,
        "CALL lithograph.branch.create('phase12-feature') YIELD name RETURN name",
        "{}",
    )?;
    scalar_query(
        &fixture,
        &trusted_loads,
        "CREATE FULLTEXT INDEX merge_text FOR (n:MergeDoc) ON EACH [n.text] OPTIONS {indexConfig:{`fulltext.analyzer`:'phase12_echo'}}",
        r#"{"branch":"phase12-feature"}"#,
    )?;
    scalar_query(
        &fixture,
        lithograph_load,
        "CREATE (:MainOnly {name:'ours'}) FINISH",
        "{}",
    )?;

    let started = scalar_query(
        &fixture,
        lithograph_load,
        "CALL lithograph.merge.start('branch/phase12-feature') YIELD session, revision, status RETURN session, revision, status",
        "{}",
    )?;
    require(
        started["rows"][0][2] == "ready",
        "diverged Full-text schema Merge must prepare as a real merge candidate",
    )?;
    let session = started["rows"][0][0]
        .as_str()
        .ok_or("merge.start must return session id")?;
    let revision = started["rows"][0][1]
        .as_i64()
        .ok_or("merge.start must return revision")?;
    let before = branch_head(&Connection::open(fixture.path())?, "main")?;
    let finalize = format!(
        "CALL lithograph.merge.finalize('{session}', {revision}) YIELD status, commit RETURN status, commit"
    );
    Ok((fixture, trusted_loads, finalize, before))
}

fn assert_merge_finalize_rejected(
    fixture: &FileDatabaseFixture,
    lithograph_load: &str,
    finalize: &str,
    before: &HashId,
) -> Result<(), Box<dyn Error>> {
    match scalar_query(fixture, lithograph_load, finalize, "{}") {
        Err(error) => require(
            error.to_string().contains("SCHEMA_ERROR"),
            "Merge publishing a custom Full-text definition without its tokenizer must fail as SCHEMA_ERROR",
        )?,
        Ok(_) => {
            return Err(
                "Merge unexpectedly published custom Full-text definition without tokenizer".into(),
            );
        }
    }
    require(
        branch_head(&Connection::open(fixture.path())?, "main")? == *before,
        "failed Full-text Merge validation must not move main Branch",
    )
}

fn lithograph_call(query: &str) -> String {
    format!("lithograph({}, '{{}}', '{{}}')", sql_literal(query))
}

fn lithograph_call_with_options(query: &str, options: &str) -> String {
    format!(
        "lithograph({}, '{{}}', {})",
        sql_literal(query),
        sql_literal(options)
    )
}

fn scalar_query(
    fixture: &FileDatabaseFixture,
    loads: &str,
    query: &str,
    options: &str,
) -> Result<serde_json::Value, Box<dyn Error>> {
    let text = fixture.execute_script(&format!(
        "{loads}\nSELECT lithograph({}, '{{}}', {});",
        sql_literal(query),
        sql_literal(options)
    ))?;
    serde_json::from_str(&text).map_err(Into::into)
}

fn sql_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn execute_script_allowing_failure(
    database: &Path,
    script: &str,
) -> Result<(bool, String, String), Box<dyn Error>> {
    let sqlite = env::var_os("LITHOGRAPH_SQLITE3").unwrap_or_else(|| "sqlite3".into());
    let mut child = Command::new(sqlite)
        .arg("-batch")
        .arg("-noheader")
        .arg(database)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .as_mut()
        .ok_or("sqlite3 stdin unavailable")?
        .write_all(script.as_bytes())?;
    drop(child.stdin.take());
    let output = child.wait_with_output()?;
    Ok((
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).trim().to_owned(),
        String::from_utf8_lossy(&output.stderr).trim().to_owned(),
    ))
}

fn output_value<'a>(value: &'a str, prefix: &str) -> Result<&'a str, Box<dyn Error>> {
    value
        .lines()
        .find_map(|line| line.trim().strip_prefix(prefix))
        .ok_or_else(|| format!("missing {prefix:?} in {value:?}").into())
}

fn require_contains(value: &str, expected: &str) -> Result<(), Box<dyn Error>> {
    require(
        value.lines().any(|line| line.trim() == expected),
        &format!("expected output line {expected:?}, got {value:?}"),
    )
}

fn require_positive_value(value: &str, prefix: &str) -> Result<(), Box<dyn Error>> {
    let number = value
        .lines()
        .find_map(|line| line.trim().strip_prefix(prefix))
        .ok_or_else(|| format!("missing {prefix:?} in {value:?}"))?
        .parse::<i64>()?;
    require(
        number > 0,
        &format!("expected positive {prefix}, got {number}"),
    )
}

fn require_nonzero_pair(value: &str, prefix: &str) -> Result<(), Box<dyn Error>> {
    let payload = value
        .lines()
        .find_map(|line| line.trim().strip_prefix(prefix))
        .ok_or_else(|| format!("missing {prefix:?} in {value:?}"))?;
    let mut values = payload.split(':').map(str::parse::<i64>);
    let first = values.next().ok_or("missing first counter")??;
    let second = values.next().ok_or("missing second counter")??;
    require(
        first > 0 && second > 0,
        &format!("expected non-zero pair for {prefix}, got {payload}"),
    )
}

fn require(condition: bool, message: &str) -> Result<(), Box<dyn Error>> {
    if condition {
        Ok(())
    } else {
        Err(message.to_owned().into())
    }
}
