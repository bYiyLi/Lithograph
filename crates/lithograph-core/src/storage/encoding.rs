use super::{HashId, StorageError, StorageResult};

const MAGIC: &[u8; 4] = b"LCE1";

pub(crate) fn record(domain: &str, fields: &[Vec<u8>]) -> Vec<u8> {
    let mut output = Vec::new();
    output.extend_from_slice(MAGIC);
    put_uleb(domain.len() as u64, &mut output);
    output.extend_from_slice(domain.as_bytes());
    put_uleb(fields.len() as u64, &mut output);
    for field in fields {
        put_uleb(field.len() as u64, &mut output);
        output.extend_from_slice(field);
    }
    output
}

pub(crate) struct RecordHasher {
    hasher: blake3::Hasher,
}

impl RecordHasher {
    pub(crate) fn new(domain: &str, field_count: usize) -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.update(MAGIC);
        hash_uleb(domain.len() as u64, &mut hasher);
        hasher.update(domain.as_bytes());
        hash_uleb(field_count as u64, &mut hasher);
        Self { hasher }
    }

    pub(crate) fn field(&mut self, field: &[u8]) {
        hash_uleb(field.len() as u64, &mut self.hasher);
        self.hasher.update(field);
    }

    #[cfg(feature = "test-support")]
    pub(crate) fn record_field(&mut self, domain: &str, fields: &[&[u8]]) {
        let encoded_len = MAGIC.len()
            + uleb_len(domain.len() as u64)
            + domain.len()
            + uleb_len(fields.len() as u64)
            + fields
                .iter()
                .map(|field| uleb_len(field.len() as u64) + field.len())
                .sum::<usize>();
        hash_uleb(encoded_len as u64, &mut self.hasher);
        self.hasher.update(MAGIC);
        hash_uleb(domain.len() as u64, &mut self.hasher);
        self.hasher.update(domain.as_bytes());
        hash_uleb(fields.len() as u64, &mut self.hasher);
        for field in fields {
            hash_uleb(field.len() as u64, &mut self.hasher);
            self.hasher.update(field);
        }
    }

    pub(crate) fn finish(self) -> HashId {
        HashId::from_bytes(*self.hasher.finalize().as_bytes())
    }
}

fn hash_uleb(value: u64, hasher: &mut blake3::Hasher) {
    let (bytes, len) = uleb_array(value);
    hasher.update(&bytes[..len]);
}

fn uleb_array(mut value: u64) -> ([u8; 10], usize) {
    let mut bytes = [0_u8; 10];
    let mut len = 0;
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        bytes[len] = byte;
        len += 1;
        if value == 0 {
            return (bytes, len);
        }
    }
}

pub(crate) fn hash(bytes: &[u8]) -> HashId {
    HashId::from_bytes(*blake3::hash(bytes).as_bytes())
}

pub(crate) fn i64_bytes(value: i64) -> Vec<u8> {
    value.to_le_bytes().to_vec()
}

pub(crate) fn i32_bytes(value: i32) -> Vec<u8> {
    value.to_le_bytes().to_vec()
}

pub(crate) fn u64_bytes(value: u64) -> Vec<u8> {
    value.to_le_bytes().to_vec()
}

pub(crate) fn u32_bytes(value: u32) -> Vec<u8> {
    value.to_le_bytes().to_vec()
}

pub(crate) fn uleb_bytes(value: u64) -> Vec<u8> {
    let mut output = Vec::new();
    put_uleb(value, &mut output);
    output
}

pub(crate) fn decode_uleb_field(bytes: &[u8]) -> StorageResult<u64> {
    let mut cursor = 0;
    let value = read_uleb(bytes, &mut cursor)?;
    if cursor != bytes.len() {
        return Err(StorageError::corrupt("ULEB128 field has trailing bytes"));
    }
    Ok(value)
}

pub(crate) fn optional_bytes(value: Option<&[u8]>) -> Vec<u8> {
    match value {
        Some(value) => {
            let mut output = Vec::with_capacity(value.len() + 1);
            output.push(1);
            output.extend_from_slice(value);
            output
        }
        None => vec![0],
    }
}

pub(crate) fn optional_string(value: Option<&str>) -> Vec<u8> {
    optional_bytes(value.map(str::as_bytes))
}

pub(crate) fn canonical_f64_bits(value: f64) -> u64 {
    if value.is_nan() {
        0x7ff8_0000_0000_0000
    } else {
        value.to_bits()
    }
}

pub(crate) fn canonical_f32_bits(value: f32) -> u32 {
    if value.is_nan() {
        0x7fc0_0000
    } else {
        value.to_bits()
    }
}

pub(crate) fn parse_record<'a>(
    bytes: &'a [u8],
    expected_domain: &str,
) -> StorageResult<Vec<&'a [u8]>> {
    if bytes.get(..MAGIC.len()) != Some(MAGIC.as_slice()) {
        return Err(StorageError::corrupt(
            "LCE1 record is missing its magic prefix",
        ));
    }
    let mut cursor = MAGIC.len();
    let domain_len = usize_from_u64(read_uleb(bytes, &mut cursor)?, "LCE1 domain length")?;
    let domain = take(bytes, &mut cursor, domain_len)?;
    if domain != expected_domain.as_bytes() {
        return Err(StorageError::corrupt(format!(
            "expected LCE1 {expected_domain} record"
        )));
    }
    let field_count = usize_from_u64(read_uleb(bytes, &mut cursor)?, "LCE1 field count")?;
    if field_count > bytes.len().saturating_sub(cursor) {
        return Err(StorageError::corrupt(
            "LCE1 field count exceeds remaining encoded bytes",
        ));
    }
    let mut fields = Vec::with_capacity(field_count);
    for _ in 0..field_count {
        let field_len = usize_from_u64(read_uleb(bytes, &mut cursor)?, "LCE1 field length")?;
        fields.push(take(bytes, &mut cursor, field_len)?);
    }
    if cursor != bytes.len() {
        return Err(StorageError::corrupt("LCE1 record has trailing bytes"));
    }
    Ok(fields)
}

pub(crate) fn decode_i64(bytes: &[u8]) -> StorageResult<i64> {
    Ok(i64::from_le_bytes(fixed(bytes)?))
}

pub(crate) fn decode_i32(bytes: &[u8]) -> StorageResult<i32> {
    Ok(i32::from_le_bytes(fixed(bytes)?))
}

pub(crate) fn decode_u64(bytes: &[u8]) -> StorageResult<u64> {
    Ok(u64::from_le_bytes(fixed(bytes)?))
}

pub(crate) fn decode_u32(bytes: &[u8]) -> StorageResult<u32> {
    Ok(u32::from_le_bytes(fixed(bytes)?))
}

pub(crate) fn encode_counted(items: &[Vec<u8>]) -> Vec<u8> {
    let mut output = Vec::new();
    put_uleb(items.len() as u64, &mut output);
    for item in items {
        put_uleb(item.len() as u64, &mut output);
        output.extend_from_slice(item);
    }
    output
}

pub(crate) fn decode_counted(bytes: &[u8]) -> StorageResult<Vec<&[u8]>> {
    let mut cursor = 0;
    let count = usize_from_u64(read_uleb(bytes, &mut cursor)?, "counted LCE1 item count")?;
    if count > bytes.len().saturating_sub(cursor) {
        return Err(StorageError::corrupt(
            "counted LCE1 item count exceeds remaining encoded bytes",
        ));
    }
    let mut output = Vec::with_capacity(count);
    for _ in 0..count {
        let len = usize_from_u64(read_uleb(bytes, &mut cursor)?, "counted LCE1 item length")?;
        output.push(take(bytes, &mut cursor, len)?);
    }
    if cursor != bytes.len() {
        return Err(StorageError::corrupt(
            "counted LCE1 payload has trailing bytes",
        ));
    }
    Ok(output)
}

fn fixed<const N: usize>(bytes: &[u8]) -> StorageResult<[u8; N]> {
    bytes
        .try_into()
        .map_err(|_| StorageError::corrupt(format!("expected fixed-width LCE1 field of {N} bytes")))
}

fn put_uleb(mut value: u64, output: &mut Vec<u8>) {
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        output.push(byte);
        if value == 0 {
            break;
        }
    }
}

fn read_uleb(bytes: &[u8], cursor: &mut usize) -> StorageResult<u64> {
    let mut value = 0_u64;
    let start = *cursor;
    for shift in (0..=63).step_by(7) {
        let byte = *bytes
            .get(*cursor)
            .ok_or_else(|| StorageError::corrupt("truncated LCE1 ULEB128"))?;
        *cursor += 1;
        let payload = u64::from(byte & 0x7f);
        if shift == 63 && payload > 1 {
            return Err(StorageError::corrupt("LCE1 ULEB128 overflows u64"));
        }
        value |= payload << shift;
        if byte & 0x80 == 0 {
            if *cursor - start != uleb_len(value) {
                return Err(StorageError::corrupt(
                    "LCE1 ULEB128 is not minimally encoded",
                ));
            }
            return Ok(value);
        }
    }
    Err(StorageError::corrupt("LCE1 ULEB128 overflows u64"))
}

fn uleb_len(mut value: u64) -> usize {
    let mut length = 1;
    while value >= 0x80 {
        value >>= 7;
        length += 1;
    }
    length
}

fn usize_from_u64(value: u64, name: &str) -> StorageResult<usize> {
    usize::try_from(value)
        .map_err(|_| StorageError::corrupt(format!("{name} exceeds addressable size")))
}

fn take<'a>(bytes: &'a [u8], cursor: &mut usize, len: usize) -> StorageResult<&'a [u8]> {
    let end = cursor
        .checked_add(len)
        .ok_or_else(|| StorageError::corrupt("LCE1 field length overflow"))?;
    let field = bytes
        .get(*cursor..end)
        .ok_or_else(|| StorageError::corrupt("truncated LCE1 field"))?;
    *cursor = end;
    Ok(field)
}
