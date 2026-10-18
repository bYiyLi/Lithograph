//! Allocation-free preflight for copying and decoding canonical property values.

use std::mem::size_of;

use super::encoding::{borrowed_counted_fields, borrowed_length_prefixed_field, decode_uleb_field};
use super::value::borrowed_value_fields;
use super::{PropertyValue, StorageError, StorageResult};

type Check<'a> = &'a dyn Fn() -> StorageResult<()>;

/// Size of a cloned property, including its inline value and owned descendants.
pub(crate) fn property_value_bytes(value: &PropertyValue, check: Check<'_>) -> StorageResult<u64> {
    check()?;
    let mut bytes = size_of::<PropertyValue>() as u64;
    match value {
        PropertyValue::String(value) => add_payload(&mut bytes, value.len(), check)?,
        PropertyValue::List(values) => {
            for value in values {
                checked_add(&mut bytes, property_value_bytes(value, check)?)?;
            }
        }
        PropertyValue::ZonedDateTime(value) => {
            add_payload(&mut bytes, value.zone_id.len(), check)?;
        }
        PropertyValue::Point(value) => {
            add_payload(
                &mut bytes,
                value.coordinates.len() * size_of::<f64>(),
                check,
            )?;
        }
        PropertyValue::Vector(value) => add_payload(&mut bytes, value.packed.len(), check)?,
        _ => {}
    }
    Ok(bytes)
}

/// Returns `(retained, temporary)` before allocating a decoded PropertyValue.
///
/// `retained` includes the root value, List slots, and decoded payloads. `temporary`
/// also covers the canonical re-encoding check's field and child-record buffers.
/// The caller separately reserves the owned SQLite column copy
/// and any later conversion to query values. This deliberately overestimates peak
/// lifetimes, without copying bytes or allocating a traversal stack.
pub(super) fn canonical_value_bytes(bytes: &[u8], check: Check<'_>) -> StorageResult<(u64, u64)> {
    check()?;
    let mut retained = size_of::<PropertyValue>() as u64;
    // While validating a List, child VALUE buffers, encode_counted's output,
    // and the final record can coexist. Checked builders reserve exact capacity;
    // four times the complete encoded input conservatively bounds these payload
    // copies, while field and List slot buffers are charged separately.
    let mut temporary = checked_mul(bytes.len(), 4)?;
    let mut current = bytes;
    loop {
        let (fields, count) = value_fields(current, check)?;
        checked_add(&mut temporary, checked_mul(count, size_of::<Vec<u8>>())?)?;
        let next = value_memory(&fields, &mut retained, &mut temporary, check)?;
        if let Some(first_child) = next {
            current = first_child;
            continue;
        }
        let end = slice_offset(bytes, current)?
            .checked_add(current.len())
            .ok_or_else(memory_overflow)?;
        if end == bytes.len() {
            break;
        }
        // A List's counted payload is its last field. Once a nested value ends,
        // the next bytes are a sibling's length prefix, even across List levels.
        current = borrowed_length_prefixed_field(&bytes[end..])?;
    }
    Ok((retained, temporary))
}

fn value_memory<'a>(
    fields: &[&'a [u8]; 5],
    retained: &mut u64,
    temporary: &mut u64,
    check: Check<'_>,
) -> StorageResult<Option<&'a [u8]>> {
    match fields[0][0] {
        4 => add_payload(retained, fields[1].len(), check)?,
        5 => return list_memory(fields[1], retained, temporary, check),
        10 => add_payload(retained, fields[3].len(), check)?,
        12 => packed_memory(fields[2], fields[3], size_of::<f64>(), retained, check)?,
        13 => packed_memory(
            fields[2],
            fields[3],
            vector_width(fields[1])?,
            retained,
            check,
        )?,
        _ => {}
    }
    Ok(None)
}

fn value_fields<'a>(bytes: &'a [u8], check: Check<'_>) -> StorageResult<([&'a [u8]; 5], usize)> {
    let (fields, count) = borrowed_value_fields(bytes, check)?;
    if count < 2 {
        return Err(StorageError::corrupt(
            "VALUE record has invalid field count",
        ));
    }
    let [tag] = fields[0] else {
        return Err(StorageError::corrupt("VALUE type tag must be one byte"));
    };
    let expected = match tag {
        1..=7 | 14 => 2,
        8 | 9 => 3,
        10 | 12 | 13 => 4,
        11 => 5,
        _ => return Err(StorageError::corrupt("unknown property type tag")),
    };
    if count != expected {
        return Err(StorageError::corrupt(
            "VALUE type tag has invalid field count",
        ));
    }
    Ok((fields, count))
}

fn list_memory<'a>(
    bytes: &'a [u8],
    retained: &mut u64,
    temporary: &mut u64,
    check: Check<'_>,
) -> StorageResult<Option<&'a [u8]>> {
    let mut items = borrowed_counted_fields(bytes)?;
    let count = items.len();
    // The checked decoder reserves this exact count before visiting children.
    checked_add(retained, checked_mul(count, size_of::<PropertyValue>())?)?;
    checked_add(temporary, checked_mul(count, size_of::<Vec<u8>>())?)?;
    let mut first = None;
    while let Some(item) = items.next_field()? {
        check()?;
        if first.is_none() {
            first = Some(item);
        }
    }
    items.finish()?;
    Ok(first)
}

fn packed_memory(
    count: &[u8],
    payload: &[u8],
    width: usize,
    retained: &mut u64,
    check: Check<'_>,
) -> StorageResult<()> {
    let count = usize::try_from(decode_uleb_field(count)?).map_err(|_| memory_overflow())?;
    let expected = count.checked_mul(width).ok_or_else(memory_overflow)?;
    if payload.len() != expected {
        return Err(StorageError::corrupt(
            "canonical coordinate payload has invalid length",
        ));
    }
    add_payload(retained, payload.len(), check)
}

fn vector_width(tag: &[u8]) -> StorageResult<usize> {
    match tag {
        [1] => Ok(1),
        [2] => Ok(2),
        [3 | 5] => Ok(4),
        [4 | 6] => Ok(8),
        _ => Err(StorageError::corrupt("unknown vector coordinate tag")),
    }
}

fn add_payload(bytes: &mut u64, len: usize, check: Check<'_>) -> StorageResult<()> {
    // Long payload work must offer cancellation before the eventual owned copy.
    for _ in (0..len).step_by(64 * 1024) {
        check()?;
    }
    checked_add(bytes, len as u64)
}

fn slice_offset(whole: &[u8], part: &[u8]) -> StorageResult<usize> {
    (part.as_ptr() as usize)
        .checked_sub(whole.as_ptr() as usize)
        .filter(|offset| *offset <= whole.len())
        .ok_or_else(|| StorageError::corrupt("VALUE field lies outside its canonical record"))
}

fn checked_add(bytes: &mut u64, extra: u64) -> StorageResult<()> {
    *bytes = bytes.checked_add(extra).ok_or_else(memory_overflow)?;
    Ok(())
}

fn checked_mul(count: usize, width: usize) -> StorageResult<u64> {
    (count as u64)
        .checked_mul(width as u64)
        .ok_or_else(memory_overflow)
}

fn memory_overflow() -> StorageError {
    StorageError::corrupt("canonical property memory size overflows addressable size")
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;
    use crate::storage::{PointValue, VectorCoordinateType, VectorValue, ZonedDateTimeValue};

    #[test]
    fn preflight_matches_decoded_retained_sizes_without_owned_decode() {
        let values = [
            PropertyValue::String("人物".repeat(25_000)),
            PropertyValue::List(vec![PropertyValue::Integer(3); 2_000]),
            PropertyValue::ZonedDateTime(ZonedDateTimeValue {
                epoch_seconds: 0,
                nanoseconds: 0,
                zone_id: "Asia/Shanghai".to_owned(),
            }),
            PropertyValue::Point(PointValue {
                crs: 1,
                coordinates: vec![1.0, 2.0],
            }),
            PropertyValue::Vector(VectorValue {
                coordinate_type: VectorCoordinateType::I16,
                dimension: 30_000,
                packed: vec![0; 60_000],
            }),
        ];
        for value in values {
            let encoded = value.canonical_bytes().unwrap();
            let (retained, temporary) = canonical_value_bytes(&encoded, &|| Ok(())).unwrap();
            assert!(retained >= property_value_bytes(&value, &|| Ok(())).unwrap());
            assert!(temporary >= encoded.len() as u64);
        }
    }

    #[test]
    fn preflight_visits_nested_lists_without_a_traversal_stack() {
        let value = PropertyValue::List(vec![
            PropertyValue::List(vec![PropertyValue::String("first".to_owned())]),
            PropertyValue::List(vec![]),
            PropertyValue::List(vec![
                PropertyValue::List(vec![PropertyValue::Integer(2)]),
                PropertyValue::String("last".to_owned()),
            ]),
        ]);
        let encoded = value.canonical_bytes().unwrap();
        let (retained, _) = canonical_value_bytes(&encoded, &|| Ok(())).unwrap();
        assert!(retained >= property_value_bytes(&value, &|| Ok(())).unwrap());
    }

    #[test]
    fn preflight_can_size_deep_canonical_lists_before_recursive_decode() {
        use crate::storage::encoding::{encode_counted, record};

        let mut encoded = PropertyValue::Integer(1).canonical_bytes().unwrap();
        for _ in 0..1_024 {
            encoded = record("VALUE", &[vec![5], encode_counted(&[encoded])]);
        }
        let checks = Cell::new(0);
        let result = canonical_value_bytes(&encoded, &|| {
            checks.set(checks.get() + 1);
            Ok(())
        });
        assert!(result.is_ok());
        assert!(checks.get() >= 1_024);
    }

    #[test]
    fn preflight_checks_each_list_item_and_propagates_cancellation() {
        let encoded = PropertyValue::List(vec![PropertyValue::Integer(0); 1_000])
            .canonical_bytes()
            .unwrap();
        let calls = Cell::new(0);
        let result = canonical_value_bytes(&encoded, &|| {
            calls.set(calls.get() + 1);
            if calls.get() == 257 {
                Err(StorageError::not_found("cancelled preflight"))
            } else {
                Ok(())
            }
        });
        assert!(matches!(result, Err(StorageError::NotFound(_))));
        assert_eq!(calls.get(), 257);
    }

    #[test]
    fn borrowed_counted_fields_reject_impossible_count_without_allocating() {
        let payload = [0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x7f];
        assert!(borrowed_counted_fields(&payload).is_err());
        assert!(canonical_value_bytes(b"LCE1", &|| Ok(())).is_err());
    }
}
