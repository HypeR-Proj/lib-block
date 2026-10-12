// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use core::fmt;

/// GPT GUID in its on-disk mixed-endian encoding.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Guid(pub [u8; 16]);

impl Guid {
    pub const NIL: Self = Self([0; 16]);

    pub fn parse(text: &str) -> Option<Self> {
        let bytes = text.as_bytes();
        if bytes.len() != 36 {
            return None;
        }
        let mut result = [0; 16];
        let mut cursor = 0;
        for offset in [3, 2, 1, 0, 5, 4, 7, 6, 8, 9, 10, 11, 12, 13, 14, 15] {
            if matches!(cursor, 8 | 13 | 18 | 23) {
                if bytes[cursor] != b'-' {
                    return None;
                }
                cursor += 1;
            }
            result[offset] = hex(bytes[cursor])? * 16 + hex(bytes[cursor + 1])?;
            cursor += 2;
        }
        Some(Self(result))
    }
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

impl fmt::Display for Guid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, offset) in [3, 2, 1, 0, 5, 4, 7, 6, 8, 9, 10, 11, 12, 13, 14, 15]
            .into_iter()
            .enumerate()
        {
            if matches!(index, 4 | 6 | 8 | 10) {
                f.write_str("-")?;
            }
            write!(f, "{:02x}", self.0[offset])?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Selector<'a> {
    PartUuid(Guid),
    PartLabel(&'a str),
}

impl<'a> Selector<'a> {
    pub fn parse(value: &'a str) -> Option<Self> {
        if let Some(uuid) = value.strip_prefix("PARTUUID=") {
            return Guid::parse(uuid)
                .filter(|uuid| *uuid != Guid::NIL)
                .map(Self::PartUuid);
        }
        let label = value.strip_prefix("PARTLABEL=")?;
        (!label.is_empty()
            && label.encode_utf16().count() <= 36
            && !label.chars().any(char::is_control))
        .then_some(Self::PartLabel(label))
    }

    pub fn matches(self, uuid: Guid, label: &str) -> bool {
        match self {
            Self::PartUuid(expected) => uuid == expected,
            Self::PartLabel(expected) => label == expected,
        }
    }
}
