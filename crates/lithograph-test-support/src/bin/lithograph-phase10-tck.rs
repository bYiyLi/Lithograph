use std::error::Error;
use std::path::Path;

use lithograph_test_support::tck_execution::execute_vendored_tck;

fn main() -> Result<(), Box<dyn Error>> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()?;
    let report = execute_vendored_tck(&root).map_err(std::io::Error::other)?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    if report.is_success() {
        Ok(())
    } else {
        Err(format!(
            "{} applicable openCypher TCK scenario(s) failed",
            report.failed
        )
        .into())
    }
}
