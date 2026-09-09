#![forbid(unsafe_code)]

use lithograph_test_support::compatibility::{
    inventory_report, load_fixture_directory, self_check_report,
};
use lithograph_test_support::tck::{read_feature, read_suite_inventory};
use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

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
    let mut args = env::args().skip(1);
    match args.next().as_deref() {
        Some("inventory") => {
            let path = args
                .next()
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("tests/fixtures/cypher25"));
            let fixtures = load_fixture_directory(&path)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&inventory_report(&fixtures)?)?
            );
        }
        Some("self-check") => {
            let report = self_check_report();
            if report.passed != 1 || report.failed != 1 || report.planned != 0 {
                return Err("compatibility harness self-check produced unexpected counts".into());
            }
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        Some("tck-smoke") => {
            let path = args.next().map(PathBuf::from).unwrap_or_else(|| {
                PathBuf::from("vendor/opencypher-tck/features/clauses/return/Return1.feature")
            });
            println!("{}", serde_json::to_string_pretty(&read_feature(&path)?)?);
        }
        Some("tck-inventory") => {
            let path = args
                .next()
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("vendor/opencypher-tck/features"));
            println!(
                "{}",
                serde_json::to_string_pretty(&read_suite_inventory(&path)?)?
            );
        }
        _ => {
            return Err(
                "usage: lithograph-compat <inventory [dir] | self-check | tck-smoke [feature] | tck-inventory [dir]>"
                    .into(),
            );
        }
    }
    Ok(())
}
