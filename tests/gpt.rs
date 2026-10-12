// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use hyper_block::{Guid, Selector, gpt};

const SECTORS: u64 = 128;
const DISK_ID: Guid = Guid([0x42; 16]);
const TYPE_ID: Guid = Guid([0x11; 16]);

struct Disk {
    bytes: Vec<u8>,
    unit: usize,
}

impl Disk {
    fn new(unit: usize) -> Self {
        let mut disk = Self {
            bytes: vec![0; SECTORS as usize * unit],
            unit,
        };
        for backup in [false, true] {
            let own = if backup { SECTORS - 1 } else { 1 };
            let other = if backup { 1 } else { SECTORS - 1 };
            let table = if backup { SECTORS - 2 } else { 2 };
            let header = disk.header(backup);
            header[..8].copy_from_slice(b"EFI PART");
            put32(header, 8, 0x0001_0000);
            put32(header, 12, 92);
            put64(header, 24, own);
            put64(header, 32, other);
            put64(header, 40, 3);
            put64(header, 48, SECTORS - 3);
            header[56..72].copy_from_slice(&DISK_ID.0);
            put64(header, 72, table);
            put32(header, 80, 4);
            put32(header, 84, 128);
        }
        disk.partition(0, Guid([1; 16]), 8, 23, "hyper-data");
        disk.refresh();
        disk
    }

    fn header(&mut self, backup: bool) -> &mut [u8] {
        let offset = if backup { SECTORS as usize - 1 } else { 1 } * self.unit;
        &mut self.bytes[offset..offset + self.unit]
    }

    fn table(&mut self, backup: bool) -> &mut [u8] {
        let offset = if backup { SECTORS as usize - 2 } else { 2 } * self.unit;
        &mut self.bytes[offset..offset + 512]
    }

    fn partition(&mut self, index: usize, id: Guid, first: u64, last: u64, label: &str) {
        for backup in [false, true] {
            let entry = &mut self.table(backup)[index * 128..(index + 1) * 128];
            entry.fill(0);
            entry[..16].copy_from_slice(&TYPE_ID.0);
            entry[16..32].copy_from_slice(&id.0);
            put64(entry, 32, first);
            put64(entry, 40, last);
            for (offset, word) in label.encode_utf16().enumerate() {
                entry[56 + offset * 2..58 + offset * 2].copy_from_slice(&word.to_le_bytes());
            }
        }
    }

    fn refresh(&mut self) {
        for backup in [false, true] {
            let table_crc = crc32(self.table(backup));
            let header = self.header(backup);
            put32(header, 88, table_crc);
            put32(header, 16, 0);
            let header_crc = crc32(&header[..92]);
            put32(header, 16, header_crc);
        }
    }

    fn read(&self) -> Result<gpt::Table, gpt::Error<&'static str>> {
        self.read_traced(&mut Vec::new())
    }

    fn read_traced(&self, reads: &mut Vec<u64>) -> Result<gpt::Table, gpt::Error<&'static str>> {
        gpt::read(
            (self.bytes.len() / self.unit) as u64,
            self.unit as u32,
            |offset, destination| {
                reads.push(offset / self.unit as u64);
                if offset % self.unit as u64 != 0 || destination.len() % self.unit != 0 {
                    return Err("unaligned read");
                }
                let offset = usize::try_from(offset).map_err(|_| "offset overflow")?;
                let end = offset
                    .checked_add(destination.len())
                    .ok_or("length overflow")?;
                destination
                    .copy_from_slice(self.bytes.get(offset..end).ok_or("out of disk bounds")?);
                Ok(())
            },
        )
    }
}

#[test]
fn disk_images_on_larger_media_keep_their_original_backup() -> Result<(), String> {
    for unit in [512, 4096] {
        for bad_table in [None, Some(false), Some(true)] {
            let mut disk = Disk::new(unit);
            disk.bytes.resize(SECTORS as usize * unit * 2, 0);
            if let Some(backup) = bad_table {
                disk.table(backup)[56] ^= 1;
            }
            let table = disk.read().map_err(|error| format!("{error:?}"))?;
            assert_eq!(table.degraded, bad_table.is_some());
            assert_eq!(table.partitions[0].label, "hyper-data");
            assert_eq!(table.partitions[0].first_lba, 8);
            assert_eq!(table.partitions[0].sectors, 16);
        }
    }
    Ok(())
}

#[test]
fn untrusted_or_out_of_bounds_alternates_never_direct_reads() {
    for (alternate, valid_crc) in [
        (64, false),
        (64, true),
        (SECTORS * 2, true),
        (u64::MAX, true),
    ] {
        let mut disk = Disk::new(512);
        disk.bytes.resize(SECTORS as usize * 512 * 2, 0);
        put64(disk.header(false), 32, alternate);
        if valid_crc {
            disk.refresh();
        }
        let mut reads = Vec::new();
        assert!(disk.read_traced(&mut reads).is_err());
        assert_eq!(reads, [1, SECTORS * 2 - 1]);
    }
    // With no trustworthy primary pointer, do not scan for the old backup.
    let mut disk = Disk::new(512);
    disk.bytes.resize(SECTORS as usize * 512 * 2, 0);
    disk.header(false)[56] ^= 1;
    let mut reads = Vec::new();
    assert!(disk.read_traced(&mut reads).is_err());
    assert_eq!(reads, [1, SECTORS * 2 - 1]);
}

#[test]
fn trusted_primary_selects_its_backup_instead_of_stale_media_end() -> Result<(), String> {
    let mut disk = Disk::new(512);
    disk.bytes.resize(SECTORS as usize * 512 * 2, 0);
    let last = SECTORS as usize * 2 - 1;
    let table = disk.table(true).to_vec();
    let mut header = disk.header(true).to_vec();
    put64(&mut header, 24, last as u64);
    put64(&mut header, 72, last as u64 - 1);
    put32(&mut header, 16, 0);
    let checksum = crc32(&header[..92]);
    put32(&mut header, 16, checksum);
    disk.bytes[(last - 1) * 512..last * 512].copy_from_slice(&table);
    disk.bytes[last * 512..(last + 1) * 512].copy_from_slice(&header);
    // A previous disk layout may survive outside the newly flashed image. It
    // does not belong to the new primary's pair and must not affect discovery.
    let mut reads = Vec::new();
    let selected = disk
        .read_traced(&mut reads)
        .map_err(|error| format!("{error:?}"))?;
    assert!(!selected.degraded);
    assert!(!reads.contains(&(last as u64)));
    // A damaged primary instead permits the independently valid end backup.
    disk.header(false)[56] ^= 1;
    let recovered = disk.read().map_err(|error| format!("{error:?}"))?;
    assert!(recovered.degraded);
    assert_eq!(recovered.partitions[0].sectors, 16);
    Ok(())
}

#[test]
fn conflicting_declared_backup_on_larger_media_is_rejected() {
    let mut disk = Disk::new(512);
    disk.bytes.resize(SECTORS as usize * 512 * 2, 0);
    disk.table(true)[56] ^= 1;
    disk.refresh();
    assert!(matches!(
        disk.read(),
        Err(gpt::Error::Invalid("GPT copies disagree"))
    ));
}

fn put32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}
fn put64(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

// IEEE CRC-32 fixture encoder, checked against the published check vector below.
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

#[test]
fn publishes_exact_ranges_and_attributes_for_512_and_4096_byte_disks() -> Result<(), String> {
    assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
    for unit in [512, 4096] {
        let mut disk = Disk::new(unit);
        for backup in [false, true] {
            put64(disk.table(backup), 48, 1 << 60);
        }
        disk.refresh();
        let table = disk.read().map_err(|error| format!("{error:?}"))?;
        assert_eq!(table.disk_id, DISK_ID);
        assert!(!table.degraded);
        assert_eq!(
            table.partitions,
            vec![gpt::Partition {
                id: Guid([1; 16]),
                kind: TYPE_ID,
                label: "hyper-data".into(),
                first_lba: 8,
                sectors: 16,
                attributes: 1 << 60,
            }]
        );
    }
    Ok(())
}

#[test]
fn corrupt_header_or_entry_crc_uses_the_other_copy_and_reports_degradation() -> Result<(), String> {
    for backup in [false, true] {
        for corrupt_header in [false, true] {
            let mut disk = Disk::new(512);
            if corrupt_header {
                disk.header(backup)[56] ^= 1;
            } else {
                disk.table(backup)[56] ^= 1;
            }
            let table = disk.read().map_err(|error| format!("{error:?}"))?;
            assert!(table.degraded);
            assert_eq!(table.partitions[0].label, "hyper-data");
        }
    }
    let mut disk = Disk::new(512);
    disk.header(false)[56] ^= 1;
    disk.table(true)[56] ^= 1;
    assert!(disk.read().is_err());
    Ok(())
}

#[test]
fn unreadable_primary_still_allows_a_valid_backup() -> Result<(), String> {
    let disk = Disk::new(512);
    let table = gpt::read(SECTORS, 512, |offset, destination| {
        if offset == 512 {
            return Err("primary I/O failure");
        }
        let start = offset as usize;
        destination.copy_from_slice(&disk.bytes[start..start + destination.len()]);
        Ok(())
    })
    .map_err(|error| format!("{error:?}"))?;
    assert!(table.degraded);
    assert_eq!(table.partitions[0].sectors, 16);
    Ok(())
}

#[test]
fn two_valid_but_different_copies_are_not_silently_resolved() {
    for identity in [false, true] {
        let mut disk = Disk::new(512);
        if identity {
            disk.header(true)[56] ^= 1;
        } else {
            disk.table(true)[56] ^= 1;
        }
        disk.refresh();
        assert!(matches!(
            disk.read(),
            Err(gpt::Error::Invalid("GPT copies disagree"))
        ));
    }
}

#[test]
fn rejects_overlapping_duplicate_nil_and_out_of_range_partitions() {
    for (id, first, last) in [
        (Guid([2; 16]), 20, 31),
        (Guid([1; 16]), 32, 40),
        (Guid::NIL, 32, 40),
        (Guid([2; 16]), 2, 7),
        (Guid([2; 16]), 32, SECTORS - 2),
        (Guid([2; 16]), 32, 31),
        (Guid([2; 16]), u64::MAX - 1, u64::MAX),
    ] {
        let mut disk = Disk::new(512);
        disk.partition(1, id, first, last, "other");
        disk.refresh();
        assert!(disk.read().is_err(), "id={id}, first={first}, last={last}");
    }
}

#[test]
fn rejects_unbounded_tables_and_invalid_metadata_placement() {
    for (offset, value) in [(72, u64::MAX), (40, 2), (48, SECTORS - 2), (24, 7), (32, 7)] {
        let mut disk = Disk::new(512);
        for backup in [false, true] {
            put64(disk.header(backup), offset, value);
        }
        disk.refresh();
        assert!(disk.read().is_err(), "field={offset}");
    }
    for (offset, value) in [
        (80, 0),
        (80, u32::MAX),
        (84, 0),
        (84, 127),
        (84, 192),
        (84, 8192),
        (12, 91),
        (12, 513),
    ] {
        let mut disk = Disk::new(512);
        for backup in [false, true] {
            put32(disk.header(backup), offset, value);
        }
        disk.refresh();
        assert!(disk.read().is_err(), "field={offset}, value={value}");
    }
    let mut disk = Disk::new(512);
    put64(disk.header(false), 72, 3);
    put64(disk.header(true), 72, SECTORS - 3);
    disk.refresh();
    assert!(disk.read().is_err());
}

#[test]
fn geometry_overflow_is_rejected_without_issuing_io() {
    for (sectors, unit) in [(5, 512), (SECTORS, 256), (SECTORS, 768), (u64::MAX, 512)] {
        let mut reads = 0;
        let result = gpt::read(sectors, unit, |_, _| {
            reads += 1;
            Ok::<(), ()>(())
        });
        assert!(result.is_err());
        assert_eq!(reads, 0);
    }
}

#[test]
fn labels_use_utf16_and_duplicate_labels_remain_ambiguous() -> Result<(), String> {
    let mut disk = Disk::new(4096);
    disk.partition(0, Guid([1; 16]), 8, 23, "持久化-🦀");
    disk.partition(1, Guid([2; 16]), 24, 31, "持久化-🦀");
    disk.refresh();
    let table = disk.read().map_err(|error| format!("{error:?}"))?;
    let selector = Selector::parse("PARTLABEL=持久化-🦀").ok_or("label selector")?;
    assert_eq!(
        table
            .partitions
            .iter()
            .filter(|p| selector.matches(p.id, &p.label))
            .count(),
        2
    );
    let id =
        Selector::parse("PARTUUID=01010101-0101-0101-0101-010101010101").ok_or("id selector")?;
    assert_eq!(
        table
            .partitions
            .iter()
            .filter(|p| id.matches(p.id, &p.label))
            .count(),
        1
    );
    for backup in [false, true] {
        disk.table(backup)[56..58].copy_from_slice(&0xd800u16.to_le_bytes());
    }
    disk.refresh();
    assert!(disk.read().is_err());
    Ok(())
}

#[test]
fn guid_encoding_and_selectors_are_strict() -> Result<(), String> {
    let guid = Guid::parse("12345678-9ABC-DEF0-1234-56789abcdef0").ok_or("guid")?;
    assert_eq!(
        guid.0,
        [
            0x78, 0x56, 0x34, 0x12, 0xbc, 0x9a, 0xf0, 0xde, 0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc,
            0xde, 0xf0
        ]
    );
    assert_eq!(guid.to_string(), "12345678-9abc-def0-1234-56789abcdef0");
    for text in [
        "config",
        "LABEL=hyper-data",
        "PARTLABEL=",
        "PARTLABEL=x\0y",
        "PARTUUID=00000000-0000-0000-0000-000000000000",
        "PARTUUID=12345678_9abc-def0-1234-56789abcdef0",
    ] {
        assert!(Selector::parse(text).is_none(), "{text:?}");
    }
    assert!(Selector::parse(&format!("PARTLABEL={}", "🦀".repeat(19))).is_none());
    assert!(Selector::parse(&format!("PARTLABEL={}", "🦀".repeat(18))).is_some());
    Ok(())
}
