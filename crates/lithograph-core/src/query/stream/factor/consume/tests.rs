use std::rc::Rc;

use super::*;
use crate::query::QueryResources;
use crate::query::expression::BinaryOp;
use crate::query::stream::factor::FactorTableBuilder;

fn packet(control: &ExecutionControl<'_>) -> GroupPacket {
    let mut prefix = BindingRow::default();
    prefix.insert("a".to_owned(), BindingValue::Node(1));
    prefix.insert("b".to_owned(), BindingValue::Node(2));
    let mut builder = FactorTableBuilder::new(prefix, 2, control).unwrap();
    for (arm, first, last) in [(0, 1, 2), (1, 3, 5)] {
        for relationship in first..=last {
            let mut row = BindingRow::default();
            row.used_relationships.insert(relationship);
            builder.push(arm, row, control).unwrap();
        }
    }
    GroupPacket::new(Rc::new(builder.finish(control).unwrap()), control).unwrap()
}

fn column(input: &str, output: &str, element_id: bool) -> Projection {
    let variable = Expr::Variable(input.to_owned());
    Projection {
        column: output.to_owned(),
        expression: if element_id {
            Expr::Function {
                name: "elementId".to_owned(),
                args: vec![variable],
                distinct: false,
                star: false,
            }
        } else {
            variable
        },
    }
}

#[test]
fn simultaneous_renames_compose_element_id_without_reading_changed_output_slots() {
    let resources = QueryResources::default();
    let control = ExecutionControl::new(&resources, &|| false);
    let mut group = packet(&control);
    let (projections, memory) = compile_projection(
        &group,
        &[column("a", "b", false), column("b", "a", false)],
        &control,
    )
    .unwrap()
    .unwrap();
    assert!(
        matches!(&projections[0], GroupProjection::Slot { name, output } if name == "a" && output == "b")
    );
    assert!(
        matches!(&projections[1], GroupProjection::Slot { name, output } if name == "b" && output == "a")
    );
    group.set_projections(&projections, &control).unwrap();
    drop((projections, memory));
    let (projections, memory) = compile_projection(&group, &[column("b", "id", true)], &control)
        .unwrap()
        .unwrap();
    assert!(
        matches!(&projections[0], GroupProjection::ElementId { name, output } if name == "a" && output == "id")
    );
    group.set_projections(&projections, &control).unwrap();
    drop((projections, memory));
    assert!(
        compile_projection(&group, &[column("id", "wrong", true)], &control)
            .unwrap()
            .is_none()
    );
    drop(group);
    assert_eq!(resources.used(), 0);
}

#[test]
fn dropping_arm_columns_keeps_the_original_factor_members_and_multiplicity() {
    let resources = QueryResources::default();
    let control = ExecutionControl::new(&resources, &|| false);
    let mut group = packet(&control);
    let table = group.table.clone();
    let (projections, memory) = compile_projection(&group, &[column("a", "a", false)], &control)
        .unwrap()
        .unwrap();
    group.set_projections(&projections, &control).unwrap();
    assert!(Rc::ptr_eq(&table, &group.table));
    assert_eq!(group.table.arm_len(0), 2);
    assert_eq!(group.table.arm_len(1), 3);
    drop((projections, memory, group, table));
    assert_eq!(resources.used(), 0);
}

#[test]
fn unsupported_expression_declines_before_spending_optional_description_memory() {
    let resources = QueryResources::new(Some(64 * 1024));
    let control = ExecutionControl::new(&resources, &|| false);
    let group = packet(&control);
    let grant = resources
        .reserve(resources.limit().unwrap() - resources.used())
        .unwrap();
    let unsupported = Projection {
        column: "bad".to_owned(),
        expression: Expr::Binary(
            BinaryOp::Divide,
            Box::new(Expr::Literal(Value::Integer(1))),
            Box::new(Expr::Literal(Value::Integer(0))),
        ),
    };
    assert!(
        compile_projection(&group, &[unsupported], &control)
            .unwrap()
            .is_none()
    );
    assert!(group.projection.is_none());
    drop((grant, group));
    assert_eq!(resources.used(), 0);
}
