// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Validate both GPT copies before publishing partition identities or ranges.
//!
//! Readers supply byte-addressed, sector-aligned I/O. Counts and allocation are
//! bounded before reading untrusted metadata; no filesystem content is probed.

use crate::Guid;
use alloc::{string::String, vec::Vec};

const MAX_TABLE_BYTES: usize = 1024 * 1024;
const MAX_PARTITIONS: usize = 1024;

#[derive(Debug)]
pub enum Error<E> {
    Io(E),
    Invalid(&'static str),
    Memory,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Partition {
    pub id: Guid,
    pub kind: Guid,
    pub label: String,
    pub first_lba: u64,
    pub sectors: u64,
    pub attributes: u64,
}

#[derive(Debug)]
pub struct Table {
    pub disk_id: Guid,
    pub partitions: Vec<Partition>,
    /// Exactly one copy was valid; callers can report degraded redundancy.
    pub degraded: bool,
}

#[derive(Eq, PartialEq)]
struct Copy {
    disk_id: Guid,
    backup_lba: u64,
    first_usable: u64,
    last_usable: u64,
    table_crc: u32,
    count: u32,
    stride: u32,
    partitions: Vec<Partition>,
}

/// Only a CRC-checked header with bounded metadata may direct another read.
struct Header {
    disk_id: Guid,
    backup_lba: u64,
    first_usable: u64,
    last_usable: u64,
    table_lba: u64,
    table_crc: u32,
    count: u32,
    stride: u32,
    bytes: usize,
    blocks: u64,
}

/// Read a GPT using the actual backend geometry, rather than build-time sizes.
/// A valid alternate copy permits discovery after interrupted metadata writes;
/// two independently valid but inconsistent copies are rejected as ambiguous.
/// A disk image written to larger media retains its original alternate LBA.
/// Follow that pointer only after validating the primary header; a damaged
/// primary header permits recovery only from the physical end, never a scan.
pub fn read<E>(
    sectors: u64,
    sector_bytes: u32,
    mut read: impl FnMut(u64, &mut [u8]) -> Result<(), E>,
) -> Result<Table, Error<E>> {
    if !(512..=65536).contains(&sector_bytes)
        || !sector_bytes.is_power_of_two()
        || sectors < 6
        || sectors.checked_mul(u64::from(sector_bytes)).is_none()
    {
        return Err(Error::Invalid("unsupported disk geometry"));
    }
    let primary = read_header(sectors, sector_bytes, 1, false, &mut read);
    let backup_lba = primary
        .as_ref()
        .ok()
        .map(|header| header.backup_lba)
        .unwrap_or(sectors - 1);
    let primary = primary.and_then(|header| read_table(header, sector_bytes, &mut read));
    // A valid primary selects its GPT pair. The physical end of a larger card
    // may still contain an unrelated GPT from before this image was flashed.
    let backup = read_header(sectors, sector_bytes, backup_lba, true, &mut read)
        .and_then(|header| read_table(header, sector_bytes, &mut read));
    let (valid, degraded) = match (primary, backup) {
        (Ok(primary), Ok(backup)) if primary == backup => (primary, false),
        (Ok(_), Ok(_)) => return Err(Error::Invalid("GPT copies disagree")),
        (Ok(valid), Err(_)) | (Err(_), Ok(valid)) => (valid, true),
        (Err(error), Err(_)) => return Err(error),
    };
    Ok(Table {
        disk_id: valid.disk_id,
        partitions: valid.partitions,
        degraded,
    })
}

fn read_header<E>(
    sectors: u64,
    sector_bytes: u32,
    own: u64,
    backup: bool,
    read: &mut impl FnMut(u64, &mut [u8]) -> Result<(), E>,
) -> Result<Header, Error<E>> {
    let unit = u64::from(sector_bytes);
    let mut header = buffer(sector_bytes as usize)?;
    read(own * unit, &mut header).map_err(Error::Io)?;
    let header_size = word(&header, 12) as usize;
    if &header[..8] != b"EFI PART"
        || word(&header, 8) != 0x0001_0000
        || !(92..=header.len()).contains(&header_size)
        || word(&header, 20) != 0
        || wide(&header, 24) != own
        || (backup && wide(&header, 32) != 1)
    {
        return Err(Error::Invalid("invalid GPT header"));
    }
    let expected_crc = word(&header, 16);
    header[16..20].fill(0);
    if crc32(&header[..header_size]) != expected_crc {
        return Err(Error::Invalid("GPT header checksum"));
    }
    let first_usable = wide(&header, 40);
    let last_usable = wide(&header, 48);
    let backup_lba = if backup { own } else { wide(&header, 32) };
    let disk_id = guid(&header, 56);
    let table_lba = wide(&header, 72);
    let count = word(&header, 80);
    let stride = word(&header, 84);
    if disk_id == Guid::NIL
        || count == 0
        || count as usize > MAX_PARTITIONS
        || !(128..=4096).contains(&stride)
        || !stride.is_power_of_two()
        || first_usable < 3
        || first_usable > last_usable
        || backup_lba >= sectors
        || last_usable >= backup_lba
    {
        return Err(Error::Invalid("invalid GPT geometry"));
    }
    let bytes = (count as usize)
        .checked_mul(stride as usize)
        .filter(|bytes| *bytes <= MAX_TABLE_BYTES)
        .ok_or(Error::Invalid("GPT table exceeds discovery limit"))?;
    let blocks = (bytes as u64).div_ceil(unit);
    let end = table_lba
        .checked_add(blocks)
        .ok_or(Error::Invalid("GPT table overflow"))?;
    if first_usable < 2 + blocks
        || backup_lba - last_usable <= blocks
        || if backup {
            table_lba <= last_usable || end > own
        } else {
            table_lba < 2 || end > first_usable
        }
    {
        return Err(Error::Invalid("GPT table overlaps usable disk space"));
    }
    Ok(Header {
        disk_id,
        backup_lba,
        first_usable,
        last_usable,
        table_lba,
        table_crc: word(&header, 88),
        count,
        stride,
        bytes,
        blocks,
    })
}

fn read_table<E>(
    header: Header,
    sector_bytes: u32,
    read: &mut impl FnMut(u64, &mut [u8]) -> Result<(), E>,
) -> Result<Copy, Error<E>> {
    let Header {
        disk_id,
        backup_lba,
        first_usable,
        last_usable,
        table_lba,
        table_crc,
        count,
        stride,
        bytes,
        blocks,
    } = header;
    let unit = u64::from(sector_bytes);
    let mut entries = buffer((blocks * unit) as usize)?;
    read(table_lba * unit, &mut entries).map_err(Error::Io)?;
    if crc32(&entries[..bytes]) != table_crc {
        return Err(Error::Invalid("GPT table checksum"));
    }
    let mut partitions: Vec<Partition> = Vec::new();
    partitions
        .try_reserve(count as usize)
        .map_err(|_| Error::Memory)?;
    for entry in entries[..bytes].chunks_exact(stride as usize) {
        let kind = guid(entry, 0);
        if kind == Guid::NIL {
            continue;
        }
        let id = guid(entry, 16);
        let first_lba = wide(entry, 32);
        let last_lba = wide(entry, 40);
        if id == Guid::NIL
            || first_lba < first_usable
            || first_lba > last_lba
            || last_lba > last_usable
            || partitions.iter().any(|part| {
                part.id == id
                    || (first_lba < part.first_lba + part.sectors && part.first_lba <= last_lba)
            })
        {
            return Err(Error::Invalid(
                "invalid, duplicate or overlapping GPT partition",
            ));
        }
        let mut label = String::new();
        label.try_reserve(108).map_err(|_| Error::Memory)?;
        let words = entry[56..128]
            .chunks_exact(2)
            .map(|bytes| u16::from_le_bytes([bytes[0], bytes[1]]))
            .take_while(|word| *word != 0);
        for character in char::decode_utf16(words) {
            label.push(character.map_err(|_| Error::Invalid("invalid UTF-16 GPT label"))?);
        }
        partitions.push(Partition {
            id,
            kind,
            label,
            first_lba,
            sectors: last_lba - first_lba + 1,
            attributes: wide(entry, 48),
        });
    }
    Ok(Copy {
        disk_id,
        backup_lba,
        first_usable,
        last_usable,
        table_crc,
        count,
        stride,
        partitions,
    })
}

fn buffer<E>(length: usize) -> Result<Vec<u8>, Error<E>> {
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(length).map_err(|_| Error::Memory)?;
    bytes.resize(length, 0);
    Ok(bytes)
}

fn word(bytes: &[u8], offset: usize) -> u32 {
    let mut value = [0; 4];
    value.copy_from_slice(&bytes[offset..offset + 4]);
    u32::from_le_bytes(value)
}

fn wide(bytes: &[u8], offset: usize) -> u64 {
    let mut value = [0; 8];
    value.copy_from_slice(&bytes[offset..offset + 8]);
    u64::from_le_bytes(value)
}

fn guid(bytes: &[u8], offset: usize) -> Guid {
    let mut value = [0; 16];
    value.copy_from_slice(&bytes[offset..offset + 16]);
    Guid(value)
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & 0u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}
