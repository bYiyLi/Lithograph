use lithograph_core::storage::{
    ConstraintDefinitionKind, HashId, PropertyRule, PropertyType, SchemaState, SchemaTarget,
    branch_head, integrity_check,
};
use rusqlite::{Connection, params};
use serde_json::{Value, json};

use super::{Config, Distribution, Result, SMALL_ROWS, Seeded};

pub(super) fn fixture(connection: &Connection, config: &Config, seeded: &Seeded) -> Result<Value> {
    let head = branch_head(connection, "main")?;
    if head != seeded.commit {
        return Err("checkpoint moved the Branch head".into());
    }
    let ids = seeded.ids;
    let nodes: (i64, i64, i64) = connection.query_row(
        "SELECT count(*),min(node_id),max(node_id) FROM main._lithograph_cp_nodes WHERE commit_id=?1",
        [head.as_bytes().as_slice()], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?)),
    )?;
    if nodes
        != (
            config.nodes,
            ids.first_node,
            ids.first_node + config.nodes - 1,
        )
    {
        return Err("checkpoint Node IDs/count differ".into());
    }
    verify_labels(connection, config, seeded)?;
    verify_properties(connection, config, seeded)?;
    verify_schema(connection, head)?;
    let (commits, checkpoints, relationships, generations, user_version): (
        i64,
        i64,
        i64,
        i64,
        i64,
    ) = connection.query_row(
        "SELECT (SELECT count(*) FROM main._lithograph_commits),\
         (SELECT count(*) FROM main._lithograph_checkpoints),\
         (SELECT count(*) FROM main._lithograph_cp_relationships),\
         (SELECT count(*) FROM main._lithograph_index_generations),\
         (SELECT user_version FROM pragma_user_version)",
        [],
        |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        },
    )?;
    if (
        commits,
        checkpoints,
        relationships,
        generations,
        user_version,
    ) != (seeded.graph_commits + 3, 1, 0, 0, 0)
    {
        return Err(
            "unexpected Commit/checkpoint/Relationship/Index/user_version dimensions".into(),
        );
    }
    let (quick_check, integrity_issues) = verify_integrity(connection)?;
    Ok(
        json!({"commits":commits,"checkpoints":checkpoints,"nodes":nodes.0,"relationships":0,
        "labels":config.nodes*2+SMALL_ROWS,"S5Left":config.nodes,"S5Right":config.nodes,"S5Small":SMALL_ROWS,
        "properties":config.nodes*2,"constraints":2,"property_indexes":0,"index_generations":generations,
        "user_version":user_version,"quick_check":quick_check,"integrity_issues":integrity_issues}),
    )
}

fn verify_integrity(connection: &Connection) -> Result<(String, usize)> {
    let quick_check: String = connection.query_row("PRAGMA quick_check", [], |row| row.get(0))?;
    let issues = integrity_check(connection)?;
    if quick_check != "ok" || !issues.is_empty() {
        return Err(format!("fixture integrity failed: {quick_check}; {issues:?}").into());
    }
    Ok((quick_check, issues.len()))
}

fn verify_labels(connection: &Connection, config: &Config, seeded: &Seeded) -> Result<()> {
    let ids = seeded.ids;
    let counts: (i64, i64, i64, i64, i64) = connection.query_row(
        "SELECT count(*),sum(label_id=?2),sum(label_id=?3),sum(label_id=?4),\
         coalesce(sum(node_id<?5 OR node_id>=?5+?6 OR (label_id=?4 AND \
         CASE WHEN ?8 THEN NOT(node_id=?5 OR node_id BETWEEN ?5+2 AND ?5+?7) \
         ELSE node_id>=?5+?7 END)),0) \
         FROM main._lithograph_cp_labels WHERE commit_id=?1",
        params![
            seeded.commit.as_bytes().as_slice(),
            ids.left_label,
            ids.right_label,
            ids.small_label,
            ids.first_node,
            config.nodes,
            SMALL_ROWS,
            matches!(config.distribution, Distribution::BilateralSkew)
        ],
        |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        },
    )?;
    if counts
        != (
            config.nodes * 2 + SMALL_ROWS,
            config.nodes,
            config.nodes,
            SMALL_ROWS,
            0,
        )
    {
        return Err("checkpoint dual-domain/small label memberships differ".into());
    }
    Ok(())
}

fn verify_properties(connection: &Connection, config: &Config, seeded: &Seeded) -> Result<()> {
    let ids = seeded.ids;
    let counts: (i64,i64) = connection.query_row(
        "SELECT count(*),coalesce(sum(owner_kind IS NOT 1 OR owner_id<?2 OR owner_id>=?2+?3 \
         OR key_id NOT IN (?4,?5) OR type_tag IS NOT 2 OR int_value IS NULL \
         OR int_value!=CASE WHEN ?7 THEN CASE WHEN key_id=?4 THEN \
           CASE WHEN owner_id-?2<=100000 THEN 1 WHEN owner_id-?2=100001 THEN 2 ELSE owner_id-?2-99999 END \
           ELSE CASE WHEN owner_id-?2<=1 THEN 1 WHEN owner_id-?2<=100002 THEN 2 ELSE owner_id-?2-100000 END END \
         ELSE CASE WHEN key_id=?4 AND owner_id-?2<?6 THEN 1 ELSE owner_id-?2+1 END END \
         OR real_value IS NOT NULL OR text_value IS NOT NULL OR blob_value IS NOT NULL OR aux_value IS NOT NULL),0) \
         FROM main._lithograph_cp_properties WHERE commit_id=?1",
        params![seeded.commit.as_bytes().as_slice(),ids.first_node,config.nodes,ids.left_key,ids.right_key,
            config.distribution.hot_rows(config.nodes),matches!(config.distribution, Distribution::BilateralSkew)],
        |row| Ok((row.get(0)?,row.get(1)?)),
    )?;
    if counts != (config.nodes * 2, 0) {
        return Err(
            "checkpoint typed property slots differ from the complete offset recipe".into(),
        );
    }
    Ok(())
}

fn verify_schema(connection: &Connection, commit: HashId) -> Result<()> {
    let schema = SchemaState::load(connection, commit)?;
    if schema.constraints.len() != 2 || !schema.indexes.is_empty() {
        return Err("expected exactly two Type Constraints and no Property Index".into());
    }
    for (name, label, key) in [
        ("s5_left_integer", "S5Left", "leftKey"),
        ("s5_right_integer", "S5Right", "rightKey"),
    ] {
        let definition = schema
            .constraints
            .get(name)
            .ok_or("missing integer Type Constraint")?;
        if definition.target
            != (SchemaTarget::Node {
                label: label.to_owned(),
            })
            || definition.properties != [key]
            || definition.kind
                != (ConstraintDefinitionKind::Type {
                    rule: PropertyRule {
                        property_type: PropertyType::Integer,
                        required: false,
                    },
                })
        {
            return Err("fixture Type Constraint differs from nullable INTEGER proof".into());
        }
    }
    Ok(())
}
