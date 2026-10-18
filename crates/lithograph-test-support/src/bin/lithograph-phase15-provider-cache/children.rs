use std::error::Error;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

pub(super) struct SqliteChild {
    child: Option<Child>,
}

pub(super) fn start(database: &Path, script: &str) -> Result<SqliteChild, Box<dyn Error>> {
    let executable = std::env::var_os("LITHOGRAPH_SQLITE3").unwrap_or_else(|| "sqlite3".into());
    let child = Command::new(executable)
        .args(["-batch", "-bail", "-noheader"])
        .arg(database)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut guarded = SqliteChild { child: Some(child) };
    let mut input = guarded
        .child
        .as_mut()
        .and_then(|child| child.stdin.take())
        .ok_or("SQLite child has no stdin pipe")?;
    input.write_all(script.as_bytes())?;
    drop(input);
    Ok(guarded)
}

impl SqliteChild {
    pub(super) fn wait(mut self, timeout: Duration) -> Result<String, Box<dyn Error>> {
        let started = Instant::now();
        loop {
            if self
                .child
                .as_mut()
                .ok_or("SQLite child was already collected")?
                .try_wait()?
                .is_some()
            {
                let output = self.collect(false)?;
                if output.status.success() {
                    return Ok(String::from_utf8(output.stdout)?);
                }
                return Err(format!("SQLite child failed: {}", describe_output(&output)).into());
            }
            if started.elapsed() >= timeout {
                let output = self.collect(true)?;
                return Err(format!(
                    "SQLite child timed out after {timeout:?}: {}",
                    describe_output(&output)
                )
                .into());
            }
            thread::sleep(Duration::from_millis(2));
        }
    }

    fn collect(&mut self, terminate: bool) -> Result<Output, Box<dyn Error>> {
        if terminate && let Some(child) = self.child.as_mut() {
            // A child may exit between the last poll and this kill attempt.
            if let Err(error) = child.kill()
                && child.try_wait()?.is_none()
            {
                return Err(format!("failed to terminate SQLite child: {error}").into());
            }
        }
        Ok(self
            .child
            .take()
            .ok_or("SQLite child was already collected")?
            .wait_with_output()?)
    }
}

impl Drop for SqliteChild {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

pub(super) fn wait_for_paths(
    paths: &[PathBuf],
    children: &mut [SqliteChild],
    timeout: Duration,
) -> Result<(), Box<dyn Error>> {
    let started = Instant::now();
    loop {
        if paths.iter().all(|path| path.exists()) {
            return Ok(());
        }
        for (index, child) in children.iter_mut().enumerate() {
            if child
                .child
                .as_mut()
                .ok_or("SQLite child was already collected")?
                .try_wait()?
                .is_some()
            {
                let output = child.collect(false)?;
                return Err(format!(
                    "SQLite child {index} exited before the marker barrier: {}",
                    describe_output(&output)
                )
                .into());
            }
        }
        if started.elapsed() >= timeout {
            let missing: Vec<_> = paths.iter().filter(|path| !path.exists()).collect();
            return Err(format!(
                "SQLite marker barrier timed out after {timeout:?}; missing {missing:?}"
            )
            .into());
        }
        thread::sleep(Duration::from_millis(2));
    }
}

fn describe_output(output: &Output) -> String {
    format!(
        "status={}; stdout={:?}; stderr={:?}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}
