use lithograph_test_support::sqlite::{inspect_runtime, load_extension};
use serde::Serialize;
use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Serialize)]
struct ProbeResult {
    loaded: bool,
    sqlite: lithograph_test_support::sqlite::SqliteRuntimeInfo,
    extension: String,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let path = env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: lithograph-sqlite-probe <extension-path>")?;
    let sqlite = inspect_runtime()?;
    load_extension(&path)?;
    let result = ProbeResult {
        loaded: true,
        sqlite,
        extension: path.display().to_string(),
    };
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}
