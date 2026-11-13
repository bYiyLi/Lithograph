use super::*;
use crate::cypher::Value;
use join::PartOrientation::{Reversed, Written};

#[test]
fn exchanging_proven_arms_reverses_hops_and_preserves_columns_and_asp_identity() {
    let mut plan = FactorizedPlan {
        clause_start: 7,
        shared: "h".into(),
        single_shared: true,
        shared_predicate: Expr::Literal(Value::Boolean(true)),
        accumulate: FactorArm {
            hops: vec![
                FactorHop {
                    part: 0,
                    orientation: Reversed,
                },
                FactorHop {
                    part: 1,
                    orientation: Written,
                },
            ],
        },
        build: FactorArm {
            hops: vec![FactorHop {
                part: 2,
                orientation: Written,
            }],
        },
        bindings: [
            ("a", FactorGroup::Accumulate),
            ("h", FactorGroup::Shared),
            ("b", FactorGroup::Build),
            ("d", FactorGroup::Accumulate),
        ]
        .into_iter()
        .map(|(name, group)| FactorBinding {
            name: name.into(),
            group,
            kind: FactorBindingKind::Node { labels: vec![3] },
        })
        .collect(),
        asp: Some(FactorAsp {
            part: 3,
            probe_variable: "a".into(),
            build_variable: "d".into(),
            probe_group: FactorGroup::Accumulate,
        }),
    };
    let original = format!("{plan:?}");
    plan.swap_arms();
    assert_eq!(
        plan.accumulate
            .hops
            .iter()
            .map(|h| (h.part, h.orientation))
            .collect::<Vec<_>>(),
        vec![(2, Reversed)]
    );
    assert_eq!(
        plan.build
            .hops
            .iter()
            .map(|h| (h.part, h.orientation))
            .collect::<Vec<_>>(),
        vec![(1, Reversed), (0, Written)]
    );
    assert_eq!(
        plan.bindings
            .iter()
            .map(|b| (b.name.as_str(), b.group))
            .collect::<Vec<_>>(),
        vec![
            ("a", FactorGroup::Build),
            ("h", FactorGroup::Shared),
            ("b", FactorGroup::Accumulate),
            ("d", FactorGroup::Build)
        ]
    );
    let asp = plan.asp.as_ref().unwrap();
    assert_eq!(
        (
            asp.probe_variable.as_str(),
            asp.build_variable.as_str(),
            asp.part,
            asp.probe_group
        ),
        ("a", "d", 3, FactorGroup::Build)
    );
    plan.swap_arms();
    assert_eq!(format!("{plan:?}"), original);
}
