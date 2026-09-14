use super::*;

#[test]
fn reset_revert_and_gc_respect_tag_roots() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (:N {v:0}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("base");
    let base = branch_head(&connection, "main").expect("base");
    execute(
        &connection,
        "MATCH (n:N) SET n.v=1 FINISH",
        ExecutionOptions::default(),
    )
    .expect("v1");
    let one = branch_head(&connection, "main").expect("one");
    execute(
        &connection,
        "MATCH (n:N) SET n.v=2 FINISH",
        ExecutionOptions::default(),
    )
    .expect("v2");
    let old_head = branch_head(&connection, "main").expect("old head");
    call(
        &connection,
        &format!(
            "CALL lithograph.commit.data.set('{}', {{gc:'sidecar'}}) YIELD commit RETURN commit",
            descriptor(old_head)
        ),
    );
    call(
        &connection,
        &format!(
            "CALL lithograph.reset('{}') YIELD to RETURN to",
            descriptor(one)
        ),
    );
    assert_eq!(branch_head(&connection, "main").expect("reset head"), one);
    let reverted = call(
        &connection,
        &format!(
            "CALL lithograph.revert('{}') YIELD commit RETURN commit",
            descriptor(one)
        ),
    );
    let reverted_id = HashId::from_hex(&string(&reverted[0][0])[7..]).expect("reverted id");
    assert_eq!(
        snapshot(&connection, reverted_id),
        snapshot(&connection, base)
    );

    call(
        &connection,
        &format!(
            "CALL lithograph.tag.create('protect', '{}') YIELD name RETURN name",
            descriptor(old_head)
        ),
    );
    call(
        &connection,
        "CALL lithograph.gc() YIELD commits RETURN commits",
    );
    assert!(commit_exists(&connection, old_head).expect("tag-protected history"));
    let protected_data: i64 = connection
        .query_row(
            "SELECT count(*) FROM main._lithograph_commit_data WHERE commit_id=?1",
            [old_head.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .expect("tag-protected Commit Data");
    assert_eq!(protected_data, 1);
    call(
        &connection,
        "CALL lithograph.tag.delete('protect') YIELD name RETURN name",
    );
    call(
        &connection,
        "CALL lithograph.gc() YIELD commits RETURN commits",
    );
    assert!(!commit_exists(&connection, old_head).expect("unreachable history collected"));
    let collected_data: i64 = connection
        .query_row(
            "SELECT count(*) FROM main._lithograph_commit_data WHERE commit_id=?1",
            [old_head.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .expect("collected Commit Data");
    assert_eq!(collected_data, 0);
}
