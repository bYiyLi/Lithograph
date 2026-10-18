#[path = "children.rs"]
mod children;
#[path = "server.rs"]
mod server;

use super::{
    ENTRY_TABLE, META_TABLE, provider_config_with_timeout, require, require_cache_state,
    sql_literal,
};
use children::{SqliteChild, start, wait_for_paths};
use lithograph_test_support::sqlite::FileDatabaseFixture;
use rusqlite::{Connection, params};
use server::EmbeddingServer;
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

const DEADLINE: Duration = Duration::from_secs(4);

pub(super) fn check(
    provider_load: &str,
    probe_load: &str,
) -> Result<Vec<&'static str>, Box<dyn Error>> {
    for precreated in [false, true] {
        check_first_use(provider_load, probe_load, precreated)?;
    }
    check_foreign_database_races(provider_load, probe_load)?;
    check_initial_wait(provider_load, probe_load, true)?;
    check_initial_wait(provider_load, probe_load, false)?;
    check_existing_foreign_objects(provider_load, probe_load)?;
    Ok(vec![
        "provider-cache-first-use-missing-multi-process",
        "provider-cache-first-use-empty-multi-process",
        "provider-cache-first-use-foreign-schema-races",
        "provider-cache-first-use-cancel-no-partial-schema",
        "provider-cache-first-use-timeout-no-partial-schema",
        "provider-cache-existing-foreign-objects-rejected",
        "provider-cache-offline-warm-read-under-writer",
    ])
}

struct Worker {
    database: PathBuf,
    ready: PathBuf,
    release: PathBuf,
}

impl Worker {
    fn new(directory: &Path, name: &str) -> Self {
        Self {
            database: directory.join(format!("{name}.db")),
            ready: directory.join(format!("{name}.ready")),
            release: directory.join(format!("{name}.release")),
        }
    }

    fn start(
        &self,
        loads: &str,
        config: &str,
        text: &str,
        cancel_on_retry: bool,
    ) -> Result<SqliteChild, Box<dyn Error>> {
        start(
            &self.database,
            &format!(
                "{loads}\nSELECT provider_cache_probe_barrier({},{},2,{},{},{});",
                sql_literal(config),
                sql_literal(text),
                sql_literal(&self.ready.to_string_lossy()),
                sql_literal(&self.release.to_string_lossy()),
                i32::from(cancel_on_retry),
            ),
        )
    }

    fn release(&self) -> Result<(), Box<dyn Error>> {
        fs::write(&self.release, b"go")?;
        Ok(())
    }
}

fn check_first_use(
    provider_load: &str,
    probe_load: &str,
    precreated: bool,
) -> Result<(), Box<dyn Error>> {
    let fixture = FileDatabaseFixture::new(0x1505_0100 + u64::from(precreated))?;
    let cache = fixture.directory().join("first-use.db");
    require(!cache.exists(), "fresh cache path already exists")?;
    let locker = if precreated {
        let connection = Connection::open(&cache)?;
        connection.execute_batch("BEGIN IMMEDIATE")?;
        Some(connection)
    } else {
        None
    };
    let server = EmbeddingServer::start(&[1, 3])?;
    let config = provider_config_with_timeout(server.base_url(), &cache, 24, 5_000);
    let loads = format!("{provider_load}\n{probe_load}");
    let (workers, mut children) = start_first_workers(fixture.directory(), &loads, &config)?;
    let markers = workers
        .iter()
        .map(|worker| worker.ready.clone())
        .collect::<Vec<_>>();
    wait_for_paths(&markers, &mut children, DEADLINE)?;
    require_empty_schema(&cache)?;
    if let Some(locker) = locker {
        locker.execute_batch("ROLLBACK")?;
    }
    // All four independent processes have completed the empty-schema read.
    // Finish one initialization and publish before resuming the stale readers.
    workers[0].release()?;
    let winner = children.remove(0).wait(DEADLINE)?;
    require_vector(&winner, "1,2")?;
    require_cache_state(&cache, 1, 8)?;
    let winner_id = entry_id(&cache, "winner")?;
    finish_stale_readers(&workers[1..], children)?;
    check_first_use_results(&cache, server, winner_id)?;
    check_offline_warm_reads(fixture.path(), &cache, &loads, &config)
}

fn start_first_workers(
    directory: &Path,
    loads: &str,
    config: &str,
) -> Result<(Vec<Worker>, Vec<SqliteChild>), Box<dyn Error>> {
    let texts = ["winner", "shared", "shared", "different"];
    let workers = texts
        .iter()
        .enumerate()
        .map(|(index, _)| Worker::new(directory, &format!("worker-{index}")))
        .collect::<Vec<_>>();
    let children = workers
        .iter()
        .zip(texts)
        .map(|(worker, text)| worker.start(loads, config, text, false))
        .collect::<Result<Vec<_>, _>>()?;
    Ok((workers, children))
}

fn finish_stale_readers(
    workers: &[Worker],
    children: Vec<SqliteChild>,
) -> Result<(), Box<dyn Error>> {
    for worker in workers {
        worker.release()?;
    }
    for (child, vector) in children.into_iter().zip(["3,4", "3,4", "5,6"]) {
        require_vector(&child.wait(DEADLINE)?, vector)?;
    }
    Ok(())
}

fn check_first_use_results(
    cache: &Path,
    server: EmbeddingServer,
    winner_id: i64,
) -> Result<(), Box<dyn Error>> {
    let mut requests = server.finish()?;
    requests.sort();
    require(
        requests == ["different", "shared", "shared", "winner"],
        &format!("expected exactly four HTTP requests with max_retries=0, got {requests:?}"),
    )?;
    require_cache_state(cache, 3, 24)?;
    require(
        entry_id(cache, "winner")? == winner_id,
        "late initializer replaced the winner entry",
    )?;
    require_accounting_and_unique(cache)
}

fn check_offline_warm_reads(
    database: &Path,
    cache: &Path,
    loads: &str,
    config: &str,
) -> Result<(), Box<dyn Error>> {
    // The endpoint is now stopped. Each warm read uses another real process;
    // a RESERVED writer lock must not force a valid, in-budget hit to write.
    let before_warm = fs::read(cache)?;
    let writer = Connection::open(cache)?;
    writer.execute_batch("BEGIN IMMEDIATE")?;
    for (text, vector) in [("winner", "1,2"), ("shared", "3,4"), ("different", "5,6")] {
        let output = start(database, &vector_script(loads, config, text))?.wait(DEADLINE)?;
        require_vector(&output, vector)?;
    }
    writer.execute_batch("ROLLBACK")?;
    require(
        fs::read(cache)? == before_warm,
        "in-budget warm hit modified cache bytes",
    )?;
    require_cache_state(cache, 3, 24)
}

fn check_foreign_database_races(
    provider_load: &str,
    probe_load: &str,
) -> Result<(), Box<dyn Error>> {
    let partial = format!("CREATE TABLE {META_TABLE}(id INTEGER PRIMARY KEY,magic TEXT);");
    let bad_marker = marker_schema("wrong-cache-kind", 1, 0, -1);
    let bad_version = marker_schema("lithograph-openai-compatible-cache", 99, 0, -1);
    let bad_accounting = marker_schema("lithograph-openai-compatible-cache", 1, 8, 0);
    for (index, (schema, expected)) in [
        (
            "CREATE TABLE user_data(value TEXT); INSERT INTO user_data VALUES('preserve');"
                .to_owned(),
            1,
        ),
        (
            "CREATE VIEW sqlitex AS SELECT 'preserve' AS value;".to_owned(),
            1,
        ),
        (partial, 1),
        (bad_marker, 1),
        (bad_version, 1),
        (bad_accounting, 2),
    ]
    .iter()
    .enumerate()
    {
        let fixture = FileDatabaseFixture::new(0x1505_0200 + index as u64)?;
        let cache = fixture.directory().join("foreign-race.db");
        let locker = Connection::open(&cache)?;
        locker.execute_batch("BEGIN IMMEDIATE")?;
        let server = EmbeddingServer::start(&[1])?;
        let config = provider_config_with_timeout(server.base_url(), &cache, 24, 5_000);
        let loads = format!("{provider_load}\n{probe_load}");
        let worker = Worker::new(fixture.directory(), "foreign-worker");
        let mut children = vec![worker.start(&loads, &config, "winner", false)?];
        wait_for_paths(std::slice::from_ref(&worker.ready), &mut children, DEADLINE)?;
        locker.execute_batch(schema)?;
        locker.execute_batch("COMMIT")?;
        let before = fs::read(&cache)?;
        worker.release()?;
        let output = children.remove(0).wait(DEADLINE)?;
        require_status(&output, *expected)?;
        require(
            server.finish()?.is_empty(),
            "rejected concurrent schema caused an HTTP request",
        )?;
        require(
            fs::read(&cache)? == before,
            "rejected concurrent schema was modified",
        )?;
        locker.execute_batch("BEGIN IMMEDIATE; ROLLBACK")?;
    }
    Ok(())
}

fn check_initial_wait(
    provider_load: &str,
    probe_load: &str,
    cancel: bool,
) -> Result<(), Box<dyn Error>> {
    let fixture = FileDatabaseFixture::new(0x1505_0300 + u64::from(cancel))?;
    let cache = fixture.directory().join("locked-empty.db");
    let locker = Connection::open(&cache)?;
    locker.execute_batch("BEGIN IMMEDIATE")?;
    let before = fs::read(&cache)?;
    let server = EmbeddingServer::start(&[1])?;
    let config = provider_config_with_timeout(server.base_url(), &cache, 24, 5_000);
    let loads = format!("{provider_load}\n{probe_load}");
    let worker = Worker::new(fixture.directory(), "waiting-worker");
    let mut children = vec![worker.start(&loads, &config, "winner", cancel)?];
    wait_for_paths(std::slice::from_ref(&worker.ready), &mut children, DEADLINE)?;
    worker.release()?;
    let output = children.remove(0).wait(DEADLINE)?;
    require_status(&output, if cancel { 4 } else { 2 })?;
    require(
        server.finish()?.is_empty(),
        "failed initial lock wait caused HTTP",
    )?;
    locker.execute_batch("ROLLBACK")?;
    require(
        fs::read(&cache)? == before,
        "failed initial lock wait left schema or data",
    )?;
    require_empty_schema(&cache)?;
    locker.execute_batch("BEGIN IMMEDIATE; ROLLBACK")?;
    check_recovery_after_wait(fixture.path(), &cache, &loads)
}

fn check_recovery_after_wait(
    database: &Path,
    cache: &Path,
    loads: &str,
) -> Result<(), Box<dyn Error>> {
    // Retrying after the failed call must initialize and populate normally.
    let server = EmbeddingServer::start(&[1])?;
    let config = provider_config_with_timeout(server.base_url(), cache, 24, 5_000);
    let output = start(database, &vector_script(loads, &config, "winner"))?.wait(DEADLINE)?;
    require_vector(&output, "1,2")?;
    require(
        server.finish()? == ["winner"],
        "post-failure initialization missed its HTTP request",
    )?;
    require_cache_state(cache, 1, 8)
}

fn check_existing_foreign_objects(
    provider_load: &str,
    probe_load: &str,
) -> Result<(), Box<dyn Error>> {
    for (index, schema) in [
        "CREATE VIEW sqlitex AS SELECT 1;".to_owned(),
        format!("CREATE VIEW {META_TABLE} AS SELECT 1; CREATE VIEW {ENTRY_TABLE} AS SELECT 1;"),
    ]
    .iter()
    .enumerate()
    {
        let fixture = FileDatabaseFixture::new(0x1505_0400 + index as u64)?;
        let cache = fixture.directory().join("foreign.db");
        Connection::open(&cache)?.execute_batch(schema)?;
        let before = fs::read(&cache)?;
        let server = EmbeddingServer::start(&[1])?;
        let config = provider_config_with_timeout(server.base_url(), &cache, 24, 5_000);
        let loads = format!("{provider_load}\n{probe_load}");
        let output =
            start(fixture.path(), &vector_script(&loads, &config, "winner"))?.wait(DEADLINE)?;
        require_status(&output, 1)?;
        require(server.finish()?.is_empty(), "foreign objects caused HTTP")?;
        require(fs::read(&cache)? == before, "foreign objects were modified")?;
    }
    check_augmented_cache(provider_load, probe_load)
}

fn check_augmented_cache(provider_load: &str, probe_load: &str) -> Result<(), Box<dyn Error>> {
    let fixture = FileDatabaseFixture::new(0x1505_0500)?;
    let cache = fixture.directory().join("augmented.db");
    let server = EmbeddingServer::start(&[1])?;
    let config = provider_config_with_timeout(server.base_url(), &cache, 24, 5_000);
    let loads = format!("{provider_load}\n{probe_load}");
    let output =
        start(fixture.path(), &vector_script(&loads, &config, "winner"))?.wait(DEADLINE)?;
    require_vector(&output, "1,2")?;
    require(
        server.finish()? == ["winner"],
        "cache setup missed its HTTP request",
    )?;
    let connection = Connection::open(&cache)?;
    for (create, drop) in [
        (
            "CREATE VIEW sqlitex AS SELECT 1;".to_owned(),
            "DROP VIEW sqlitex;",
        ),
        (
            format!("CREATE INDEX user_index ON {ENTRY_TABLE}(text_bytes);"),
            "DROP INDEX user_index;",
        ),
        (
            format!(
                "CREATE TRIGGER user_trigger AFTER INSERT ON {ENTRY_TABLE} BEGIN SELECT 1; END;"
            ),
            "DROP TRIGGER user_trigger;",
        ),
    ] {
        check_foreign_cache_object(
            &connection,
            fixture.path(),
            &cache,
            &loads,
            &config,
            &create,
            drop,
        )?;
    }
    require_cache_state(&cache, 1, 8)
}

fn check_foreign_cache_object(
    connection: &Connection,
    database: &Path,
    cache: &Path,
    loads: &str,
    config: &str,
    create: &str,
    drop: &str,
) -> Result<(), Box<dyn Error>> {
    connection.execute_batch(create)?;
    let before = fs::read(cache)?;
    let output = start(database, &vector_script(loads, config, "winner"))?.wait(DEADLINE)?;
    require_status(&output, 1)?;
    require(fs::read(cache)? == before, "augmented cache was modified")?;
    connection.execute_batch(drop)?;
    let output = start(database, &vector_script(loads, config, "winner"))?.wait(DEADLINE)?;
    require_vector(&output, "1,2")
}

fn marker_schema(magic: &str, version: i64, used: i64, check: i64) -> String {
    format!(
        "CREATE TABLE {META_TABLE}(id INTEGER PRIMARY KEY,magic TEXT,schema_version INTEGER,\
         used_payload_bytes INTEGER,used_payload_bytes_check INTEGER);\
         INSERT INTO {META_TABLE} VALUES(1,'{magic}',{version},{used},{check});\
         CREATE TABLE {ENTRY_TABLE}(entry_id INTEGER PRIMARY KEY);"
    )
}

fn vector_script(loads: &str, config: &str, text: &str) -> String {
    format!(
        "{loads}\nSELECT provider_cache_probe_vector({},{},2);",
        sql_literal(config),
        sql_literal(text)
    )
}

fn require_vector(output: &str, values: &str) -> Result<(), Box<dyn Error>> {
    let expected = format!("status=0;dimensions=2;count=2;values={values};error=");
    require(
        output.trim() == expected,
        &format!("expected vector {expected:?}, got {output:?}"),
    )
}

fn require_status(output: &str, expected: i32) -> Result<(), Box<dyn Error>> {
    require(
        output.trim().starts_with(&format!("status={expected};")),
        &format!("expected Provider status {expected}, got {output:?}"),
    )
}

fn require_empty_schema(cache: &Path) -> Result<(), Box<dyn Error>> {
    let count = Connection::open(cache)?.query_row(
        "SELECT count(*) FROM sqlite_schema WHERE name NOT GLOB 'sqlite_*'",
        [],
        |row| row.get::<_, i64>(0),
    )?;
    require(count == 0, "initialization left partial schema")
}

fn entry_id(cache: &Path, text: &str) -> Result<i64, Box<dyn Error>> {
    let hash = blake3::hash(text.as_bytes());
    Ok(Connection::open(cache)?.query_row(
        &format!("SELECT entry_id FROM {ENTRY_TABLE} WHERE text_hash=?1"),
        params![hash.as_bytes().as_slice()],
        |row| row.get(0),
    )?)
}

fn require_accounting_and_unique(cache: &Path) -> Result<(), Box<dyn Error>> {
    let connection = Connection::open(cache)?;
    let (entries, keys, payload, used, check): (i64, i64, i64, i64, i64) = connection.query_row(
        &format!(
            "SELECT count(*),count(DISTINCT hex(space_hash)||hex(text_hash)),sum(payload_bytes),\
             (SELECT used_payload_bytes FROM {META_TABLE}),\
             (SELECT used_payload_bytes_check FROM {META_TABLE}) FROM {ENTRY_TABLE}"
        ),
        [],
        |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        },
    )?;
    require(
        (entries, keys, payload, used, check) == (3, 3, 24, 24, !24_i64),
        "concurrent publish violated UNIQUE/accounting",
    )?;
    let integrity: String = connection.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    require(integrity == "ok", "cache integrity_check failed")
}
