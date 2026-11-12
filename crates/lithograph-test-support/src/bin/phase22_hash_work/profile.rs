use std::collections::BTreeMap;

use lithograph_core::performance;
use lithograph_core::query::QuerySummary;
use serde_json::{Value as JsonValue, json};

use crate::Result;

pub(super) fn report(summary: &QuerySummary, require_hash: bool) -> Result<JsonValue> {
    let operators = summary
        .metrics
        .operator_profile()
        .ok_or("PROFILE missing operators")?;
    let mut output = Vec::new();
    let mut sum = 0_u64;
    let mut hash_rows = 0_u64;
    for operator in operators {
        sum = sum
            .checked_add(operator.db_hits)
            .ok_or("profile dbHits overflow")?;
        let debug = format!("{operator:?}");
        let hash = hash_metrics(&debug)?;
        if operator.operator == "HashJoin" {
            if hash.is_none() || operator.rows == 0 {
                return Err("actual HashJoin lacks its real typed runtime metrics/rows".into());
            }
            hash_rows = hash_rows
                .checked_add(operator.rows)
                .ok_or("Hash rows overflow")?;
        }
        output.push(json!({"id": operator.id, "operator": operator.operator,
            "rows": operator.rows, "dbHits": operator.db_hits, "hash": hash,
            "runtime_debug": debug}));
    }
    if sum != summary.metrics.db_hits {
        return Err(format!(
            "PROFILE sum {sum} differs from summary {}",
            summary.metrics.db_hits
        )
        .into());
    }
    if require_hash && hash_rows == 0 {
        return Err("full After workload did not execute a real HashJoin".into());
    }
    Ok(
        json!({"operators": output, "exact_db_hits_sum": sum, "sum_verified": true,
              "actual_hash_output_rows": hash_rows}),
    )
}

fn hash_metrics(debug: &str) -> Result<Option<JsonValue>> {
    // A narrow harness decoder permits direct-linking this same source against
    // the retained S4 rlib, whose public metrics struct has no hash field.
    let Some((_, value)) = debug.split_once("hash: Some(") else {
        return Ok(None);
    };
    let (_, body) = value
        .split_once('{')
        .ok_or("malformed Hash metrics Debug")?;
    let (fields, _) = body
        .split_once('}')
        .ok_or("unterminated Hash metrics Debug")?;
    let fields = parse_fields(fields)?;
    for name in [
        "build_rows",
        "probe_rows",
        "hash_collisions",
        "spill_partitions",
        "spill_batches",
        "spill_vm_steps",
        "spill_encoded_bytes",
        "memory_peak_bytes",
        "temp_allocated_bytes",
    ] {
        if !fields.contains_key(name) {
            return Err(format!("Hash metrics missing {name}").into());
        }
    }
    if fields["build_rows"] == 0 || fields["probe_rows"] == 0 {
        return Err("nonempty Hash workload has empty build/probe accounting".into());
    }
    Ok(Some(
        json!({"buildRows": fields["build_rows"], "probeRows": fields["probe_rows"],
        "hashCollisions": fields["hash_collisions"], "spillPartitions": fields["spill_partitions"],
        "spillBatches": fields["spill_batches"], "spillVmSteps": fields["spill_vm_steps"],
        "spillEncodedBytes": fields["spill_encoded_bytes"], "memoryPeakBytes": fields["memory_peak_bytes"],
        "tempAllocatedBytes": fields["temp_allocated_bytes"]}),
    ))
}

pub(super) fn counters() -> Result<JsonValue> {
    let debug = format!("{:?}", performance::snapshot());
    let body = debug
        .strip_prefix("PerformanceCounters {")
        .and_then(|value| value.strip_suffix('}'))
        .ok_or("unexpected counter Debug")?;
    let fields = parse_fields(body)?;
    if fields.get("resolved_state_builds") != Some(&1)
        || fields.get("standard_index_builds") != Some(&0)
    {
        return Err("probe must resolve its Snapshot once and build no Standard Index".into());
    }
    Ok(json!({"values": fields, "debug": debug}))
}

fn parse_fields(body: &str) -> Result<BTreeMap<String, u64>> {
    let mut fields = BTreeMap::new();
    for field in body
        .split(',')
        .map(str::trim)
        .filter(|field| !field.is_empty())
    {
        let (key, value) = field
            .split_once(':')
            .ok_or("malformed numeric Debug field")?;
        if fields
            .insert(key.trim().to_owned(), value.trim().parse()?)
            .is_some()
        {
            return Err("duplicate numeric Debug field".into());
        }
    }
    Ok(fields)
}
