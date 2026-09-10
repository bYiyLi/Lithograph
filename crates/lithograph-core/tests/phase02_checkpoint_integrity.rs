use lithograph_core::storage::{
    CommitMetadata, LayerBuilder, OwnerKind, PropertyValue, RelationshipRecord, Snapshot,
    allocate_node_id, allocate_relationship_id, branch_head, commit_layer, create_branch,
    create_checkpoint, create_storage_schema, initialize_root, integrity_check, intern_label,
    intern_property_key, intern_relationship_type, root_commit,
};
use rusqlite::{Connection, params};

fn fresh_storage() -> Connection {
    let connection = Connection::open_in_memory().expect("in-memory SQLite must open");
    connection
        .execute_batch(
            "CREATE TABLE main._lithograph_meta(\
                id INTEGER PRIMARY KEY CHECK(id = 1),\
                magic TEXT NOT NULL,\
                database_id TEXT NOT NULL,\
                storage_format INTEGER NOT NULL\
            );",
        )
        .expect("test metadata table must be created");
    create_storage_schema(&connection).expect("storage schema must initialize");
    initialize_root(&connection).expect("Root Commit must initialize");
    connection
}

fn metadata(message: &str, committed_at: i64) -> CommitMetadata {
    CommitMetadata {
        author: Some("phase02-test".to_owned()),
        message: Some(message.to_owned()),
        committed_at,
    }
}

#[test]
fn checkpoint_overlay_streams_removes_replacements_and_additions() {
    let connection = fresh_storage();
    let root = root_commit(&connection).expect("Root");
    let (base, ids) = checkpoint_stream_base(&connection);
    let first = commit_layer(
        &connection,
        "main",
        root,
        None,
        &base,
        &metadata("stream base", 90),
    )
    .expect("base commit");
    create_checkpoint(&connection, first).expect("base checkpoint");

    let (overlay, added_node, added_label, added_key, added_rel) =
        checkpoint_stream_overlay(&connection, &ids);
    let second = commit_layer(
        &connection,
        "main",
        first,
        None,
        &overlay,
        &metadata("stream overlay", 91),
    )
    .expect("overlay commit");
    let snapshot = Snapshot::resolve(&connection, second).expect("snapshot");

    let mut nodes = Vec::new();
    snapshot
        .visit_nodes(|node| {
            nodes.push(node);
            Ok(())
        })
        .expect("nodes");
    assert_eq!(nodes, vec![ids.keep_node, added_node]);
    let mut labels = Vec::new();
    snapshot
        .visit_labels(|node, label| {
            labels.push((node, label));
            Ok(())
        })
        .expect("labels");
    assert_eq!(labels, vec![(ids.keep_node, added_label)]);
    let mut relationships = Vec::new();
    snapshot
        .visit_relationships(|rel| {
            relationships.push(rel);
            Ok(())
        })
        .expect("relationships");
    assert_eq!(relationships, vec![added_rel]);
    let mut properties = Vec::new();
    snapshot
        .visit_properties(|kind, owner, key, value| {
            properties.push((kind, owner, key, value));
            Ok(())
        })
        .expect("properties");
    assert_eq!(
        properties,
        vec![(
            OwnerKind::Node,
            ids.keep_node,
            added_key,
            PropertyValue::Integer(22)
        )]
    );
}

struct CheckpointStreamIds {
    remove_node: i64,
    keep_node: i64,
    base_label: i64,
    base_key: i64,
    rel_type: i64,
    base_rel: RelationshipRecord,
}

fn checkpoint_stream_base(connection: &Connection) -> (LayerBuilder, CheckpointStreamIds) {
    let remove_node = allocate_node_id(connection).expect("remove node");
    let keep_node = allocate_node_id(connection).expect("keep node");
    let base_label = intern_label(connection, "Base").expect("label");
    let base_key = intern_property_key(connection, "base").expect("key");
    let rel_type = intern_relationship_type(connection, "LINK").expect("type");
    let base_rel = RelationshipRecord {
        id: allocate_relationship_id(connection).expect("relationship"),
        source: remove_node,
        type_id: rel_type,
        target: keep_node,
    };
    let mut layer = LayerBuilder::default();
    layer.add_node(remove_node).expect("node");
    layer.add_node(keep_node).expect("node");
    layer.add_label(remove_node, base_label).expect("label");
    layer
        .set_property(
            OwnerKind::Node,
            remove_node,
            base_key,
            PropertyValue::Integer(11),
        )
        .expect("property");
    layer.add_relationship(base_rel).expect("relationship");
    (
        layer,
        CheckpointStreamIds {
            remove_node,
            keep_node,
            base_label,
            base_key,
            rel_type,
            base_rel,
        },
    )
}

fn checkpoint_stream_overlay(
    connection: &Connection,
    ids: &CheckpointStreamIds,
) -> (LayerBuilder, i64, i64, i64, RelationshipRecord) {
    let added_node = allocate_node_id(connection).expect("added node");
    let added_label = intern_label(connection, "Added").expect("label");
    let added_key = intern_property_key(connection, "added").expect("key");
    let added_rel = RelationshipRecord {
        id: allocate_relationship_id(connection).expect("relationship"),
        source: ids.keep_node,
        type_id: ids.rel_type,
        target: added_node,
    };
    let mut layer = LayerBuilder::default();
    layer
        .remove_relationship(ids.base_rel)
        .expect("relationship remove");
    layer
        .remove_label(ids.remove_node, ids.base_label)
        .expect("label remove");
    layer
        .remove_property(OwnerKind::Node, ids.remove_node, ids.base_key)
        .expect("property remove");
    layer.remove_node(ids.remove_node).expect("node remove");
    layer.add_node(added_node).expect("node add");
    layer
        .add_label(ids.keep_node, added_label)
        .expect("label add");
    layer
        .set_property(
            OwnerKind::Node,
            ids.keep_node,
            added_key,
            PropertyValue::Integer(22),
        )
        .expect("property add");
    layer.add_relationship(added_rel).expect("relationship add");
    (layer, added_node, added_label, added_key, added_rel)
}

#[test]
fn integrity_reports_sequence_dictionary_and_relationship_corruption() {
    let connection = fresh_storage();
    let root = root_commit(&connection).expect("Root");
    let source = allocate_node_id(&connection).expect("source");
    let target = allocate_node_id(&connection).expect("target");
    let alternate = allocate_node_id(&connection).expect("alternate");
    let label = intern_label(&connection, "Corruptible").expect("label");
    let rel_type = intern_relationship_type(&connection, "REL").expect("type");
    let rel = RelationshipRecord {
        id: allocate_relationship_id(&connection).expect("relationship"),
        source,
        type_id: rel_type,
        target,
    };
    let mut first_layer = LayerBuilder::default();
    for node in [source, target, alternate] {
        first_layer.add_node(node).expect("node");
    }
    first_layer.add_label(source, label).expect("label");
    first_layer.add_relationship(rel).expect("relationship");
    let first = commit_layer(
        &connection,
        "main",
        root,
        None,
        &first_layer,
        &metadata("corrupt base", 100),
    )
    .expect("first commit");
    let mut second_layer = LayerBuilder::default();
    second_layer
        .remove_relationship(rel)
        .expect("relationship remove");
    let second = commit_layer(
        &connection,
        "main",
        first,
        None,
        &second_layer,
        &metadata("corrupt second", 101),
    )
    .expect("second commit");
    let second_layer_id: i64 = connection
        .query_row(
            "SELECT layer_id FROM _lithograph_commits WHERE id = ?1",
            [second.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .expect("layer id");

    connection
        .execute(
            "UPDATE _lithograph_sequences SET next_id = 1 WHERE kind = 1",
            [],
        )
        .expect("sequence corruption");
    connection
        .execute(
            "UPDATE _lithograph_label_delta SET label_id = 9999 WHERE node_id = ?1",
            [source],
        )
        .expect("dictionary corruption");
    connection
        .execute(
            "UPDATE _lithograph_rel_delta SET target_id = ?2 WHERE layer_id = ?1 AND relationship_id = ?3",
            params![second_layer_id, alternate, rel.id],
        )
        .expect("identity corruption");
    let codes = issue_codes(&connection);
    assert!(codes.contains(&"identity.sequence_regressed".to_owned()));
    assert!(codes.contains(&"dictionary.dangling_id".to_owned()));
    assert!(codes.contains(&"identity.relationship_mutated".to_owned()));
    assert!(codes.contains(&"history.layer_hash_mismatch".to_owned()));
    assert!(codes.contains(&"graph.snapshot_invalid".to_owned()));
}

#[test]
fn integrity_reports_commit_schema_checkpoint_and_orphan_corruption() {
    let connection = fresh_storage();
    let root = root_commit(&connection).expect("Root");
    let node = allocate_node_id(&connection).expect("node");
    let mut layer = LayerBuilder::default();
    layer.add_node(node).expect("node add");
    let commit = commit_layer(
        &connection,
        "main",
        root,
        None,
        &layer,
        &metadata("integrity target", 110),
    )
    .expect("commit");
    create_checkpoint(&connection, commit).expect("checkpoint");

    connection
        .execute(
            "UPDATE _lithograph_commits SET message = 'tampered' WHERE id = ?1",
            [commit.as_bytes().as_slice()],
        )
        .expect("commit corruption");
    connection
        .execute(
            "UPDATE _lithograph_schema_objects SET canonical_blob = x'00'",
            [],
        )
        .expect("schema corruption");
    connection
        .execute(
            "DELETE FROM _lithograph_cp_nodes WHERE commit_id = ?1 AND node_id = ?2",
            params![commit.as_bytes().as_slice(), node],
        )
        .expect("checkpoint corruption");
    let fake = [0x55_u8; 32];
    connection.execute("INSERT INTO _lithograph_checkpoints(commit_id, created_at, metadata) VALUES(?1, 0, NULL)", [fake.as_slice()]).expect("dangling checkpoint");
    let orphan = [0x66_u8; 32];
    connection
        .execute(
            "INSERT INTO _lithograph_cp_nodes(commit_id, node_id) VALUES(?1, ?2)",
            params![orphan.as_slice(), node],
        )
        .expect("orphan row");

    let codes = issue_codes(&connection);
    assert!(codes.contains(&"history.commit_hash_mismatch".to_owned()));
    assert!(codes.contains(&"history.schema_hash_mismatch".to_owned()));
    assert!(codes.contains(&"checkpoint.snapshot_mismatch".to_owned()));
    assert!(codes.contains(&"checkpoint.dangling_commit".to_owned()));
    assert!(codes.contains(&"history.orphan_rows".to_owned()));
}

#[test]
fn checkpoint_never_changes_owner_delete_validation() {
    for checkpointed in [false, true] {
        let connection = fresh_storage();
        let root = root_commit(&connection).expect("Root");
        let node = allocate_node_id(&connection).expect("node");
        let label = intern_label(&connection, "Owned").expect("label");
        let key = intern_property_key(&connection, "owned").expect("key");
        let mut base = LayerBuilder::default();
        base.add_node(node).expect("node add");
        base.add_label(node, label).expect("label add");
        base.set_property(OwnerKind::Node, node, key, PropertyValue::Integer(1))
            .expect("property add");
        let first = commit_layer(
            &connection,
            "main",
            root,
            None,
            &base,
            &metadata("owner base", 120),
        )
        .expect("base commit");
        if checkpointed {
            create_checkpoint(&connection, first).expect("checkpoint");
        }

        let mut invalid = LayerBuilder::default();
        invalid.remove_node(node).expect("node remove");
        assert!(
            commit_layer(
                &connection,
                "main",
                first,
                None,
                &invalid,
                &metadata("invalid owner delete", 121),
            )
            .is_err()
        );
        assert_eq!(branch_head(&connection, "main").expect("head"), first);
    }
}

#[test]
fn checkpoint_never_changes_relationship_property_delete_validation() {
    for checkpointed in [false, true] {
        let connection = fresh_storage();
        let root = root_commit(&connection).expect("Root");
        let source = allocate_node_id(&connection).expect("source");
        let target = allocate_node_id(&connection).expect("target");
        let rel_type = intern_relationship_type(&connection, "OWNED").expect("type");
        let key = intern_property_key(&connection, "owned").expect("key");
        let relationship = RelationshipRecord {
            id: allocate_relationship_id(&connection).expect("relationship"),
            source,
            type_id: rel_type,
            target,
        };
        let mut base = LayerBuilder::default();
        base.add_node(source).expect("source add");
        base.add_node(target).expect("target add");
        base.add_relationship(relationship)
            .expect("relationship add");
        base.set_property(
            OwnerKind::Relationship,
            relationship.id,
            key,
            PropertyValue::Integer(1),
        )
        .expect("relationship property");
        let first = commit_layer(
            &connection,
            "main",
            root,
            None,
            &base,
            &metadata("relationship owner base", 130),
        )
        .expect("base commit");
        if checkpointed {
            create_checkpoint(&connection, first).expect("checkpoint");
        }

        let mut invalid = LayerBuilder::default();
        invalid
            .remove_relationship(relationship)
            .expect("relationship remove");
        assert!(
            commit_layer(
                &connection,
                "main",
                first,
                None,
                &invalid,
                &metadata("invalid relationship delete", 131),
            )
            .is_err()
        );
        assert_eq!(branch_head(&connection, "main").expect("head"), first);
    }
}

#[test]
fn integrity_rejects_noncanonical_lce1_and_extreme_counts_without_panicking() {
    for mutation in [
        Corruption::ExtraField,
        Corruption::HugeFieldCount,
        Corruption::NonCanonicalNan,
    ] {
        let connection = fresh_storage();
        let root = root_commit(&connection).expect("Root");
        let node = allocate_node_id(&connection).expect("node");
        let key = intern_property_key(&connection, "payload").expect("key");
        let value = match mutation {
            Corruption::NonCanonicalNan => PropertyValue::Float(f64::NAN),
            Corruption::ExtraField | Corruption::HugeFieldCount => {
                PropertyValue::List(vec![PropertyValue::Integer(1)])
            }
        };
        let mut layer = LayerBuilder::default();
        layer.add_node(node).expect("node add");
        layer
            .set_property(OwnerKind::Node, node, key, value)
            .expect("property set");
        let commit = commit_layer(
            &connection,
            "main",
            root,
            None,
            &layer,
            &metadata("canonical payload", 140),
        )
        .expect("commit");
        corrupt_property_payload(&connection, commit.as_bytes(), node, key, mutation);
        assert!(
            !integrity_check(&connection)
                .expect("integrity must not panic")
                .is_empty()
        );
    }
}

#[derive(Clone, Copy)]
enum Corruption {
    ExtraField,
    HugeFieldCount,
    NonCanonicalNan,
}

fn corrupt_property_payload(
    connection: &Connection,
    commit: &[u8; 32],
    node: i64,
    key: i64,
    mutation: Corruption,
) {
    let (layer_id, blob, aux): (i64, Option<Vec<u8>>, Option<Vec<u8>>) = connection
        .query_row(
            "SELECT d.layer_id, d.blob_value, d.aux_value FROM _lithograph_property_delta d JOIN _lithograph_commits c ON c.layer_id = d.layer_id WHERE c.id = ?1 AND d.owner_kind = 1 AND d.owner_id = ?2 AND d.key_id = ?3",
            params![commit.as_slice(), node, key],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("property payload");
    match mutation {
        Corruption::ExtraField => {
            let mut bytes = blob.expect("list blob");
            let field_count = 10;
            assert_eq!(bytes[field_count], 2);
            bytes[field_count] = 3;
            bytes.push(0);
            connection.execute(
                "UPDATE _lithograph_property_delta SET blob_value = ?1 WHERE layer_id = ?2 AND owner_kind = 1 AND owner_id = ?3 AND key_id = ?4",
                params![bytes, layer_id, node, key],
            ).expect("extra-field corruption");
        }
        Corruption::HugeFieldCount => {
            let bytes = blob.expect("list blob");
            let field_count = 10;
            let mut corrupted = bytes[..field_count].to_vec();
            corrupted
                .extend_from_slice(&[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01]);
            corrupted.extend_from_slice(&bytes[field_count + 1..]);
            connection.execute(
                "UPDATE _lithograph_property_delta SET blob_value = ?1 WHERE layer_id = ?2 AND owner_kind = 1 AND owner_id = ?3 AND key_id = ?4",
                params![corrupted, layer_id, node, key],
            ).expect("huge-count corruption");
        }
        Corruption::NonCanonicalNan => {
            assert_eq!(aux.expect("NaN aux").len(), 8);
            let corrupted = 0x7ff8_0000_0000_0001_u64.to_le_bytes();
            connection.execute(
                "UPDATE _lithograph_property_delta SET aux_value = ?1 WHERE layer_id = ?2 AND owner_kind = 1 AND owner_id = ?3 AND key_id = ?4",
                params![corrupted.as_slice(), layer_id, node, key],
            ).expect("NaN corruption");
        }
    }
}

#[test]
fn canonical_nan_layer_can_be_reused_by_content_hash() {
    let connection = fresh_storage();
    let root = root_commit(&connection).expect("Root");
    create_branch(&connection, "side", root).expect("side branch");
    let node = allocate_node_id(&connection).expect("node");
    let key = intern_property_key(&connection, "nan").expect("key");
    let mut layer = LayerBuilder::default();
    layer.add_node(node).expect("node add");
    layer
        .set_property(OwnerKind::Node, node, key, PropertyValue::Float(f64::NAN))
        .expect("NaN property");
    commit_layer(
        &connection,
        "main",
        root,
        None,
        &layer,
        &metadata("main NaN", 150),
    )
    .expect("main commit");
    commit_layer(
        &connection,
        "side",
        root,
        None,
        &layer,
        &metadata("side NaN", 151),
    )
    .expect("same canonical Layer must be reusable");
}

#[test]
fn integrity_requires_main_branch() {
    let connection = fresh_storage();
    connection
        .execute("DELETE FROM _lithograph_branches WHERE name = 'main'", [])
        .expect("delete main for corruption fixture");
    assert!(issue_codes(&connection).contains(&"refs.missing_main".to_owned()));
}

fn issue_codes(connection: &Connection) -> Vec<String> {
    integrity_check(connection)
        .expect("integrity check")
        .into_iter()
        .map(|issue| issue.code)
        .collect()
}
