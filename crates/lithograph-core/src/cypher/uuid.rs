use super::value::ValueError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct UuidValue([u8; 16]);

impl UuidValue {
    pub fn parse(text: &str) -> Result<Self, ValueError> {
        if text.len() != 36 || !matches_hyphens(text.as_bytes()) {
            return Err(ValueError::new(
                "UUID must use canonical 8-4-4-4-12 text form",
            ));
        }
        let mut bytes = [0_u8; 16];
        let mut digit = 0_usize;
        let compact: Vec<u8> = text.bytes().filter(|byte| *byte != b'-').collect();
        if compact.len() != 32 {
            return Err(ValueError::new("UUID must contain 32 hexadecimal digits"));
        }
        while digit < 16 {
            let high = hex(compact[digit * 2])?;
            let low = hex(compact[digit * 2 + 1])?;
            bytes[digit] = high << 4 | low;
            digit += 1;
        }
        Ok(Self(bytes))
    }

    pub fn to_canonical(self) -> String {
        use std::fmt::Write as _;
        let mut output = String::with_capacity(36);
        for (index, byte) in self.0.iter().enumerate() {
            if matches!(index, 4 | 6 | 8 | 10) {
                output.push('-');
            }
            let _ = write!(output, "{byte:02x}");
        }
        output
    }

    pub fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

fn matches_hyphens(bytes: &[u8]) -> bool {
    [8, 13, 18, 23]
        .into_iter()
        .all(|index| bytes.get(index) == Some(&b'-'))
}

fn hex(byte: u8) -> Result<u8, ValueError> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(ValueError::new("UUID contains a non-hexadecimal digit")),
    }
}
