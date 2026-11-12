use super::*;
use crate::query::QueryErrorKind;

fn key(value: Value, control: &ExecutionControl<'_>) -> EvaluatedHashKey {
    let mut digest = [0; 32];
    if let Value::Integer(value) = &value {
        digest[..8].copy_from_slice(&value.to_le_bytes());
    }
    EvaluatedHashKey {
        digest,
        value: key::HashKeyValue::Scalar(value),
        memory: control
            .reserve(std::mem::size_of::<Value>() as u64)
            .unwrap(),
    }
}

#[test]
fn complete_integer_set_preserves_exact_keys_and_bounds_growth() {
    let resources = QueryResources::default();
    let control = ExecutionControl::new(&resources, &|| false);
    let mut set = KeyCollector::new(&control).unwrap().unwrap();
    for value in [i64::MAX, 0, i64::MIN, 0, i64::MAX] {
        assert!(
            set.observe(&key(Value::Integer(value), &control), &control)
                .unwrap()
        );
    }
    assert_eq!(
        set.keys.iter().map(|key| key.value).collect::<Vec<_>>(),
        [i64::MAX, 0, i64::MIN]
    );
    assert!(
        !set.observe(&key(Value::Float(0.0), &control), &control)
            .unwrap()
    );
    assert_eq!(
        set.keys.iter().map(|key| key.value).collect::<Vec<_>>(),
        [i64::MAX, 0, i64::MIN]
    );
    drop(set);
    assert_eq!(resources.used(), 0);

    let mut set = KeyCollector::new(&control).unwrap().unwrap();
    for value in 0..MAX_KEYS as i64 {
        assert!(
            set.observe(&key(Value::Integer(value), &control), &control)
                .unwrap()
        );
    }
    let used = resources.used();
    assert!(
        !set.observe(&key(Value::Integer(i64::MAX), &control), &control)
            .unwrap()
    );
    assert_eq!(set.keys.len(), MAX_KEYS);
    assert_eq!(resources.used(), used);
    assert!(set.peak_bytes <= 96 * MAX_KEYS as u64 + 512);
    drop(set);
    assert_eq!(resources.used(), 0);
}

#[test]
fn only_local_advisory_grant_is_abandoned_and_interrupt_is_preserved() {
    let resources = QueryResources::new(Some(8));
    let control = ExecutionControl::new(&resources, &|| false);
    let mut set = KeyCollector::new(&control).unwrap().unwrap();
    // This key already belongs to the actual Hash extraction. The advisory
    // allocation itself receives no quota and cannot turn into a Query error.
    let key = EvaluatedHashKey {
        digest: [0; 32],
        value: key::HashKeyValue::Scalar(Value::Integer(7)),
        memory: control.reserve(0).unwrap(),
    };
    assert!(!set.observe(&key, &control).unwrap());
    assert!(set.keys.is_empty());
    drop((set, key));
    assert_eq!(resources.used(), 0);

    let interrupted = ExecutionControl::new(&resources, &|| true);
    assert_eq!(
        KeyCollector::new(&interrupted).unwrap_err().kind,
        QueryErrorKind::Interrupted
    );
    assert_eq!(resources.used(), 0);
}

#[test]
fn adversarial_digest_chain_abandons_the_whole_advisory_set() {
    let resources = QueryResources::default();
    let control = ExecutionControl::new(&resources, &|| false);
    let mut set = KeyCollector::new(&control).unwrap().unwrap();
    for value in 0..MAX_PROBES as i64 {
        let mut key = key(Value::Integer(value), &control);
        key.digest = [0; 32];
        assert!(set.observe(&key, &control).unwrap());
    }
    let mut distinct = key(Value::Integer(i64::MAX), &control);
    distinct.digest = [0; 32];
    let before = set.probes;
    assert!(!set.observe(&distinct, &control).unwrap());
    assert_eq!(set.probes - before, MAX_PROBES as u64);
    drop((set, distinct));
    assert_eq!(resources.used(), 0);
}
