use serde::Serialize;
use std::env;
use std::error::Error;
use std::fmt::{self, Display, Formatter};
use std::fs::OpenOptions;
use std::io::{self, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use tempfile::{Builder, TempDir};

#[derive(Debug)]
pub enum FixtureError {
    Io(io::Error),
    Sqlite { code: Option<i32>, stderr: String },
    InvalidOutput(String),
}

impl Display for FixtureError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "I/O error: {error}"),
            Self::Sqlite { code, stderr } => {
                write!(formatter, "sqlite3 exited with {code:?}: {stderr}")
            }
            Self::InvalidOutput(message) => write!(formatter, "invalid sqlite3 output: {message}"),
        }
    }
}

impl Error for FixtureError {}

impl From<io::Error> for FixtureError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

pub type Result<T> = std::result::Result<T, FixtureError>;

#[derive(Debug, Clone, Serialize)]
pub struct SqliteRuntimeInfo {
    pub version: String,
    pub compile_options: Vec<String>,
}

#[derive(Debug)]
pub struct InMemoryDatabaseFixture {
    seed: u64,
}

impl InMemoryDatabaseFixture {
    pub fn new(seed: u64) -> Self {
        Self { seed }
    }

    pub fn seed(&self) -> u64 {
        self.seed
    }

    /// Executes one complete script against one in-memory SQLite session.
    pub fn execute_script(&self, script: &str) -> Result<String> {
        run_sqlite(":memory:", script)
    }
}

#[derive(Debug)]
pub struct FileDatabaseFixture {
    directory: TempDir,
    path: PathBuf,
    seed: u64,
}

impl FileDatabaseFixture {
    pub fn new(seed: u64) -> Result<Self> {
        let directory = Builder::new()
            .prefix(&format!("lithograph-{seed:016x}-"))
            .tempdir()?;
        let path = directory.path().join("fixture.db");
        run_sqlite(path.as_os_str(), "PRAGMA schema_version;")?;
        Ok(Self {
            directory,
            path,
            seed,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn directory(&self) -> &Path {
        self.directory.path()
    }

    pub fn seed(&self) -> u64 {
        self.seed
    }

    pub fn seeded_u64(&self, ordinal: u64) -> u64 {
        splitmix64(self.seed.wrapping_add(ordinal))
    }

    pub fn execute_script(&self, script: &str) -> Result<String> {
        run_sqlite(self.path.as_os_str(), script)
    }
}

/// Disposable file database intended only for corruption and crash fixtures.
#[derive(Debug)]
pub struct DisposableDatabaseFixture {
    inner: FileDatabaseFixture,
}

impl DisposableDatabaseFixture {
    pub fn new(seed: u64) -> Result<Self> {
        Ok(Self {
            inner: FileDatabaseFixture::new(seed)?,
        })
    }

    pub fn path(&self) -> &Path {
        self.inner.path()
    }

    pub fn execute_script(&self, script: &str) -> Result<String> {
        self.inner.execute_script(script)
    }

    /// Returns a sqlite3 process attached only to this disposable database.
    /// Callers may terminate it in crash tests without touching user data.
    pub fn spawn_session(&self) -> Result<std::process::Child> {
        Ok(Command::new(sqlite_binary())
            .arg("-batch")
            .arg(self.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?)
    }

    /// Intentionally damages the disposable database header for corruption tests.
    pub fn corrupt_header(&self) -> Result<()> {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(self.path())?;
        file.seek(SeekFrom::Start(0))?;
        file.write_all(b"LITHOBAD")?;
        file.sync_all()?;
        Ok(())
    }
}

pub fn inspect_runtime() -> Result<SqliteRuntimeInfo> {
    let output = run_sqlite(
        ":memory:",
        "SELECT sqlite_version();\nPRAGMA compile_options;\n",
    )?;
    let mut lines = output.lines();
    let version = lines
        .next()
        .ok_or_else(|| FixtureError::InvalidOutput("missing sqlite_version()".into()))?
        .to_string();
    let compile_options = lines.map(str::to_string).collect();
    Ok(SqliteRuntimeInfo {
        version,
        compile_options,
    })
}

pub fn load_extension(path: &Path) -> Result<()> {
    let load = extension_load_command(path)?;
    run_sqlite(":memory:", &format!("{load}\nSELECT 1;\n"))?;
    Ok(())
}

pub fn extension_load_command(path: &Path) -> Result<String> {
    let path = path
        .to_str()
        .ok_or_else(|| FixtureError::InvalidOutput("extension path is not UTF-8".into()))?;
    if path.contains(['\n', '\r']) {
        return Err(FixtureError::InvalidOutput(
            "extension path contains a newline".into(),
        ));
    }
    let path = if cfg!(windows) {
        path.replace('\\', "/")
    } else {
        path.to_owned()
    };
    let quoted = path.replace('"', "\"\"");
    Ok(format!(".load \"{quoted}\""))
}

fn run_sqlite(database: impl AsRef<std::ffi::OsStr>, script: &str) -> Result<String> {
    let mut child = Command::new(sqlite_binary())
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
        .ok_or_else(|| FixtureError::InvalidOutput("sqlite3 stdin unavailable".into()))?
        .write_all(script.as_bytes())?;
    drop(child.stdin.take());
    let output = child.wait_with_output()?;
    output_to_string(output)
}

fn output_to_string(output: Output) -> Result<String> {
    if !output.status.success() {
        return Err(FixtureError::Sqlite {
            code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    Ok(normalize_sqlite_stdout(&output.stdout))
}

fn normalize_sqlite_stdout(stdout: &[u8]) -> String {
    String::from_utf8_lossy(stdout)
        .replace("\r\n", "\n")
        .trim()
        .to_string()
}

fn sqlite_binary() -> std::ffi::OsString {
    env::var_os("LITHOGRAPH_SQLITE3").unwrap_or_else(|| "sqlite3".into())
}

fn splitmix64(mut value: u64) -> u64 {
    value = value.wrapping_add(0x9e37_79b9_7f4a_7c15);
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn sqlite_stdout_normalizes_windows_line_endings() {
        assert_eq!(
            normalize_sqlite_stdout(b"first\r\nsecond\r\n"),
            "first\nsecond"
        );
    }

    #[test]
    fn in_memory_fixture_executes_one_session() {
        let fixture = InMemoryDatabaseFixture::new(7);
        let output = fixture
            .execute_script(
                "CREATE TABLE t(v INTEGER); INSERT INTO t VALUES (1), (2); SELECT sum(v) FROM t;",
            )
            .expect("in-memory sqlite fixture should execute");
        assert_eq!(output, "3");
        assert_eq!(fixture.seed(), 7);
    }

    #[test]
    fn file_fixture_persists_and_cleans_up() {
        let path;
        {
            let fixture = FileDatabaseFixture::new(11).expect("file fixture should create");
            path = fixture.path().to_path_buf();
            fixture
                .execute_script("CREATE TABLE t(v TEXT); INSERT INTO t VALUES ('ok');")
                .expect("write should succeed");
            assert_eq!(
                fixture
                    .execute_script("SELECT v FROM t;")
                    .expect("read should succeed"),
                "ok"
            );
            assert!(path.starts_with(fixture.directory()));
            assert_eq!(fixture.seeded_u64(2), fixture.seeded_u64(2));
        }
        assert!(!path.exists(), "temporary database must be cleaned up");
    }

    #[test]
    fn corruption_fixture_is_disposable() {
        let fixture = DisposableDatabaseFixture::new(19).expect("fixture should create");
        fixture
            .execute_script("CREATE TABLE t(v INTEGER);")
            .expect("setup should succeed");
        fixture
            .corrupt_header()
            .expect("header corruption should succeed");
        let bytes = fs::read(fixture.path()).expect("database should remain readable as bytes");
        assert_eq!(&bytes[..8], b"LITHOBAD");
    }

    #[test]
    fn crash_fixture_can_terminate_a_disposable_sqlite_session() {
        let fixture = DisposableDatabaseFixture::new(23).expect("fixture should create");
        let mut child = fixture
            .spawn_session()
            .expect("disposable sqlite session should start");
        child.kill().expect("test sqlite process should terminate");
        child.wait().expect("terminated sqlite process should reap");
        assert!(fixture.path().exists());
    }

    #[test]
    fn runtime_inspection_reports_version_and_options() {
        let info = inspect_runtime().expect("runtime inspection should succeed");
        assert!(!info.version.is_empty());
        assert!(!info.compile_options.is_empty());
    }
}
