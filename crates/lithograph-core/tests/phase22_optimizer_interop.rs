//! One query passes real Hash/Factorized output into completed-list SIP reads.

use std::collections::BTreeMap;

use lithograph_core::cypher::Value;
use lithograph_core::query::{ExecutionOptions, QueryCounters};
use lithograph_core::storage;

#[path = "phase22_factorized/support.rs"]
pub mod support;

#[path = "phase22_optimizer_interop/factor_sip.rs"]
mod factor_sip;
#[path = "phase22_optimizer_interop/fixture.rs"]
mod fixture;
#[path = "phase22_optimizer_interop/hash_sip.rs"]
mod hash_sip;

use support::{Observed, actual_operator, assert_bag, read};

fn assert_sip(actual: &Observed, keys: u64) {
    let owners = actual
        .profile
        .iter()
        .filter(|operator| operator.semi_mask.is_some())
        .collect::<Vec<_>>();
    assert_eq!(
        owners.len(),
        1,
        "one finite-read site: {:?}",
        actual.profile
    );
    let mask = owners[0].semi_mask.as_ref().unwrap();
    assert_eq!(
        mask.keys, keys,
        "every completed-list owner contributes its key"
    );
    assert_eq!(mask.seeks, keys, "each actual finite Node read is observed");
    assert_eq!(
        owners[0].rows, keys,
        "SIP must not multiply repeated input IDs"
    );
    assert!(
        owners[0].db_hits > 0,
        "a static access name is not execution"
    );
}

fn assert_no_sip(actual: &Observed) {
    assert!(
        actual
            .profile
            .iter()
            .all(|operator| operator.semi_mask.is_none()),
        "the original OR predicate must execute its scan fallback: {:?}",
        actual.profile
    );
}

fn fallback(text: &str) -> String {
    let original = text.replace(
        "WHERE elementId(x) IN ids",
        "WHERE (elementId(x) IN ids) OR false",
    );
    assert_ne!(
        original, text,
        "only the final membership predicate is changed"
    );
    original
}

fn assert_pinned(actual: &Observed, commit: storage::HashId) {
    assert_eq!(actual.commit, Some(format!("commit/{}", commit.to_hex())));
    assert_eq!(actual.counters, QueryCounters::default());
}

fn node_id(value: &Value) -> &str {
    let Value::String(id) = value else {
        panic!("public CREATE/SET returns a typed NodeId: {value:?}")
    };
    assert!(id.starts_with("n:"));
    id
}
