use serde_json::{Value, json};

use super::{BILATERAL_HOT_ROWS, BILATERAL_NODES, Distribution, SMALL_ROWS};

pub(super) fn left_key(offset: i64, hot_rows: i64) -> i64 {
    if offset < hot_rows { 1 } else { offset + 1 }
}

pub(super) fn bilateral_left_key(offset: i64) -> i64 {
    if offset < BILATERAL_HOT_ROWS {
        1
    } else if offset == BILATERAL_HOT_ROWS {
        2
    } else {
        offset - 99_999
    }
}

pub(super) fn bilateral_right_key(offset: i64) -> i64 {
    if offset < 2 {
        1
    } else if offset <= 100_002 {
        2
    } else {
        offset - 100_000
    }
}

pub(super) fn cases(nodes: i64, first_node: i64, distribution: Distribution) -> Vec<Value> {
    if matches!(distribution, Distribution::BilateralSkew) {
        return vec![
            bilateral_case("full", first_node),
            bilateral_case("small24", first_node),
        ];
    }
    let hot_rows = distribution.hot_rows(nodes);
    vec![
        case("full", nodes, first_node, hot_rows),
        case("small24", 24, first_node, hot_rows),
    ]
}

fn bilateral_case(name: &str, first_node: i64) -> Value {
    let small = if name == "small24" { ":S5Small" } else { "" };
    let small_offsets = std::iter::once(0).chain(2..=SMALL_ROWS).collect::<Vec<_>>();
    let mut result = json!({
        "name":name,
        "query":format!("MATCH (l:S5Left{small}),(r:S5Right{small}) WHERE l.leftKey=r.rightKey RETURN 0,elementId(l),elementId(r),l.leftKey,r.rightKey"),
        "columns":["0","elementId(l)","elementId(r)","l.leftKey","r.rightKey"],
        "expected_rows":if name == "small24" { SMALL_ROWS } else { 400_002 },
        "recipe":{"kind":"bilateral-hot-equijoin-v1","first_node":first_node,
            "node_count":BILATERAL_NODES,"hot_rows":BILATERAL_HOT_ROWS,
            "offset_start":0,"offset_end":BILATERAL_NODES,"case":name,
            "left_key":"offset <= 100000 ? 1 : offset == 100001 ? 2 : offset - 99999",
            "right_key":"offset <= 1 ? 1 : offset <= 100002 ? 2 : offset - 100000",
            "small_label_offsets":small_offsets,
            "full_match_segments":[
                {"left_start":0,"left_end_exclusive":100001,"right_offsets":[0,1],"key":1,"rows":200002},
                {"left_offset":100001,"right_start":2,"right_end_exclusive":100003,"key":2,"rows":100001},
                {"left_start":100002,"left_end_exclusive":200001,"right_offset_delta":1,"left_key_offset_delta":-99999,"rows":99999}
            ],
            "unmatched_left_offsets":[200001],
            "small24_matches":"each listed S5Small left offset matches only right offset 0; both keys are Integer 1",
            "comparison":"complete bag of exact Node ID pairs and Integer keys; no output-order assumption"}
    });
    if name == "small24" {
        result["rows"] = json!(
            std::iter::once(0)
                .chain(2..=SMALL_ROWS)
                .map(|offset| {
                    json!([
                        0,
                        format!("n:{}", first_node + offset),
                        format!("n:{first_node}"),
                        1,
                        1
                    ])
                })
                .collect::<Vec<_>>()
        );
    }
    result
}

fn case(name: &str, output_rows: i64, first_node: i64, hot_rows: i64) -> Value {
    let small = if name == "small24" { ":S5Small" } else { "" };
    let mut result = json!({
        "name":name,
        "query":format!("MATCH (l:S5Left{small}),(r:S5Right{small}) WHERE l.leftKey=r.rightKey RETURN 0,elementId(l),elementId(r),l.leftKey,r.rightKey"),
        "columns":["0","elementId(l)","elementId(r)","l.leftKey","r.rightKey"],
        "expected_rows":output_rows,
        "recipe":{"kind":"offset-equijoin-v1","first_node":first_node,
            "offset_start":0,"offset_end":output_rows,"hot_rows":hot_rows,
            "left_key":"offset < hot_rows ? 1 : offset + 1",
            "right_offset":"left_key - 1",
            "row":"[0, n:first_node+offset, n:first_node+right_offset, left_key, left_key]",
            "comparison":"complete bag including both actual Node IDs and Integer keys; no output-order assumption"}
    });
    if name == "small24" {
        result["rows"] = json!(
            (0..output_rows)
                .map(|offset| {
                    let key = left_key(offset, hot_rows);
                    json!([
                        0,
                        format!("n:{}", first_node + offset),
                        format!("n:{}", first_node + key - 1),
                        key,
                        key
                    ])
                })
                .collect::<Vec<_>>()
        );
    }
    result
}
