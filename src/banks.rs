//! Table-driven discovery of the three image banks in the MG-30 external-flash image.
//!
//! The BINA tail (after the frame wrapper is stripped) is a byte-exact image of external flash
//! starting at flash offset 0x40000 (memory-mapped at `0x60040000`). It contains three resource
//! banks, each made of an index table followed by tightly packed image records:
//!
//! | bank    | table (ext-image offset) | entry                              | records start   | limit |
//! |---------|--------------------------|------------------------------------|-----------------|-------|
//! | `font`  | `0x480000` (`0x604C0000`)| `u32 glyph_height, u32 glyph_width, u32 offset` | `0x488000` | 2048 |
//! | `small` | `0x580000` (`0x605C0000`)| `u32 offset`                       | `0x588000`      | 2800  |
//! | `large` | `0xD00000` (`0x60D40000`)| `u32 offset`                       | `0xD08000`      | 2048  |
//!
//! Offsets are relative to the table base. The firmware copies the tables to SDRAM at boot
//! (`FUN_0000fd74`) and the compositor (`FUN_000101d6`) addresses images by `(bank, index)`, so
//! the table index is the stable identity of an image. The `small` bank is drawn with colour-key
//! transparency; `font` and `large` are opaque.

use anyhow::{Context, Result, bail, ensure};

use crate::container::read_u32_le;
use crate::depacketize::FRAME_START_BINA_REL;
use crate::image_format::{COLOR_KEY_RGB565, ImageRecord, read_image_record};

/// Flash address the external image (clean BINA tail) is programmed to.
pub const EXT_IMAGE_FLASH_BASE: u32 = 0x6004_0000;

/// Offset of the external-flash image inside the depacketized BINA section.
pub const EXT_IMAGE_BINA_REL: usize = FRAME_START_BINA_REL;

/// Index tables are followed by the record area at this distance.
pub const RECORDS_OFFSET_FROM_TABLE: usize = 0x8000;

/// Unused table slots are either zero or 0xFFFFFFFF.
const TABLE_END_MARKER: u32 = 0xffff_ffff;

/// Allowed slack between consecutive records (records are 2-byte aligned).
const MAX_RECORD_GAP: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableEntryKind {
    /// `u32 offset`
    Offset,
    /// `u32 glyph_height, u32 glyph_width, u32 offset`
    Glyph,
}

#[derive(Debug, Clone, Copy)]
pub struct BankLayout {
    pub name: &'static str,
    /// Table offset relative to the external-flash image.
    pub table_offset: usize,
    pub entry_kind: TableEntryKind,
    pub max_entries: usize,
    /// End of the flash region reserved for this bank (start of the next region).
    pub region_end: usize,
    pub color_key: Option<u16>,
}

impl BankLayout {
    pub fn entry_size(&self) -> usize {
        match self.entry_kind {
            TableEntryKind::Offset => 4,
            TableEntryKind::Glyph => 12,
        }
    }

    pub fn table_flash_address(&self) -> u32 {
        EXT_IMAGE_FLASH_BASE + self.table_offset as u32
    }

    /// Bytes reserved for this bank, from the table base to the next flash region.
    pub fn region_bytes(&self) -> usize {
        self.region_end.saturating_sub(self.table_offset)
    }
}

pub const MG30_BANKS: [BankLayout; 3] = [
    BankLayout {
        name: "font",
        table_offset: 0x48_0000,
        entry_kind: TableEntryKind::Glyph,
        max_entries: 2048,
        region_end: 0x58_0000,
        color_key: None,
    },
    BankLayout {
        name: "small",
        table_offset: 0x58_0000,
        entry_kind: TableEntryKind::Offset,
        max_entries: 2800,
        region_end: 0xD0_0000,
        color_key: Some(COLOR_KEY_RGB565),
    },
    BankLayout {
        name: "large",
        table_offset: 0xD0_0000,
        entry_kind: TableEntryKind::Offset,
        max_entries: 2048,
        region_end: 0x178_0000,
        color_key: None,
    },
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GlyphInfo {
    pub height: u32,
    pub width: u32,
}

#[derive(Debug, Clone)]
pub struct BankEntry {
    /// Index in the bank table (the ID the firmware uses).
    pub index: usize,
    /// Glyph dimensions from the font table (`font` bank only).
    pub glyph: Option<GlyphInfo>,
    pub record: ImageRecord,
}

#[derive(Debug, Clone)]
pub struct Bank {
    pub layout: BankLayout,
    /// Table position relative to the clean BINA buffer.
    pub table_bina_offset: usize,
    pub entries: Vec<BankEntry>,
}

impl Bank {
    pub fn name(&self) -> &'static str {
        self.layout.name
    }

    /// Clean-BINA offset where this bank's region (index table included) starts.
    pub fn region_bina_start(&self) -> usize {
        self.table_bina_offset
    }

    pub fn records_start(&self) -> usize {
        self.table_bina_offset + RECORDS_OFFSET_FROM_TABLE
    }

    /// End of the last record (clean BINA offset).
    pub fn records_end(&self) -> usize {
        self.entries
            .last()
            .map(|e| e.record.end())
            .unwrap_or_else(|| self.records_start())
    }

    pub fn used_bytes(&self) -> usize {
        self.records_end() - self.records_start()
    }

    /// Bytes left before the next flash region.
    pub fn free_bytes(&self) -> usize {
        let region_end = self.region_bina_start() + self.layout.region_bytes();
        region_end.saturating_sub(self.records_end())
    }

    pub fn free_table_slots(&self) -> usize {
        self.layout.max_entries.saturating_sub(self.entries.len())
    }

    /// Distinct glyph heights (font bank), ascending.
    pub fn glyph_heights(&self) -> Vec<u32> {
        let mut heights: Vec<u32> = self
            .entries
            .iter()
            .filter_map(|e| e.glyph.map(|g| g.height))
            .collect();
        heights.sort_unstable();
        heights.dedup();
        heights
    }
}

fn read_table_entry(
    clean_bina: &[u8],
    layout: &BankLayout,
    table_off: usize,
    index: usize,
) -> Result<(u32, Option<GlyphInfo>)> {
    let entry_off = table_off + index * layout.entry_size();
    match layout.entry_kind {
        TableEntryKind::Offset => Ok((read_u32_le(clean_bina, entry_off)?, None)),
        TableEntryKind::Glyph => {
            let height = read_u32_le(clean_bina, entry_off)?;
            let width = read_u32_le(clean_bina, entry_off + 4)?;
            let offset = read_u32_le(clean_bina, entry_off + 8)?;
            Ok((offset, Some(GlyphInfo { height, width })))
        }
    }
}

fn is_unused_entry(index: usize, value: u32, glyph: Option<GlyphInfo>) -> bool {
    if value == TABLE_END_MARKER {
        return true;
    }
    // Index 0 legitimately points at the first record right after the table (`0x8000`); any
    // later zero entry means the table has ended.
    value == 0 && index > 0 && glyph.is_none_or(|g| g.height == 0 && g.width == 0)
}

/// Reads one bank. Fails if the table does not look like a valid MG-30 bank.
pub fn read_bank(clean_bina: &[u8], layout: &BankLayout, ext_base: usize) -> Result<Bank> {
    let table_off = ext_base + layout.table_offset;
    ensure!(
        table_off + RECORDS_OFFSET_FROM_TABLE <= clean_bina.len(),
        "{} bank table at clean BINA offset 0x{table_off:x} is outside the buffer",
        layout.name
    );

    let mut entries: Vec<BankEntry> = Vec::new();
    for index in 0..layout.max_entries {
        let (value, glyph) = read_table_entry(clean_bina, layout, table_off, index)?;
        if is_unused_entry(index, value, glyph) {
            break;
        }

        let record_off = table_off + value as usize;
        if let Some(prev) = entries.last() {
            ensure!(
                record_off >= prev.record.end(),
                "{} bank: record {index} at 0x{record_off:x} overlaps record {} ending at 0x{:x}",
                layout.name,
                prev.index,
                prev.record.end()
            );
            ensure!(
                record_off - prev.record.end() <= MAX_RECORD_GAP,
                "{} bank: unexpected gap of {} bytes before record {index}",
                layout.name,
                record_off - prev.record.end()
            );
        } else {
            ensure!(
                value as usize == RECORDS_OFFSET_FROM_TABLE,
                "{} bank: first record expected at table+0x{RECORDS_OFFSET_FROM_TABLE:x}, got table+0x{value:x}",
                layout.name
            );
        }

        let record = read_image_record(clean_bina, record_off)
            .with_context(|| format!("{} bank: bad record {index}", layout.name))?;

        if let Some(g) = glyph {
            ensure!(
                g.height == u32::from(record.header.width)
                    && g.width == u32::from(record.header.height),
                "{} bank: glyph {index} table says {}x{} but record is {}x{}",
                layout.name,
                g.width,
                g.height,
                record.header.height,
                record.header.width
            );
        }

        entries.push(BankEntry {
            index,
            glyph,
            record,
        });
    }

    if entries.is_empty() {
        bail!("{} bank table at 0x{table_off:x} is empty", layout.name);
    }

    Ok(Bank {
        layout: *layout,
        table_bina_offset: table_off,
        entries,
    })
}

/// Reads all known banks from a depacketized BINA section.
pub fn read_banks(clean_bina: &[u8]) -> Result<Vec<Bank>> {
    read_banks_at(clean_bina, EXT_IMAGE_BINA_REL)
}

pub fn read_banks_at(clean_bina: &[u8], ext_base: usize) -> Result<Vec<Bank>> {
    MG30_BANKS
        .iter()
        .map(|layout| read_bank(clean_bina, layout, ext_base))
        .collect()
}

fn write_u32(buf: &mut [u8], off: usize, value: u32) {
    buf[off..off + 4].copy_from_slice(&value.to_le_bytes());
}

fn align_up(value: usize, align: usize) -> usize {
    value.div_ceil(align) * align
}

/// Builds one bank region: a zero-terminated index table at the start and the records packed from
/// offset `0x8000`, each record start rounded up to 4 bytes (the alignment every MG-30 record
/// has). The result is exactly [`BankLayout::region_bytes`] long, so it can replace the region
/// `[table_offset, region_end)` in the external-flash image.
///
/// `records` is in table order. For the font bank the glyph dimensions written to the table come
/// from the record itself (`height` = record `width`, `width` = record `height`), which is the
/// relation [`read_bank`] checks.
pub fn build_bank_region(layout: &BankLayout, records: &[&[u8]]) -> Result<Vec<u8>> {
    ensure!(
        records.len() <= layout.max_entries,
        "{} bank has {} records, table holds at most {}",
        layout.name,
        records.len(),
        layout.max_entries
    );
    ensure!(
        records.len() * layout.entry_size() <= RECORDS_OFFSET_FROM_TABLE,
        "{} bank table ({} entries) does not fit before the record area",
        layout.name,
        records.len()
    );

    let mut parsed = Vec::with_capacity(records.len());
    for (index, record) in records.iter().enumerate() {
        let header = read_image_record(record, 0).with_context(|| {
            format!(
                "{} bank: record {index} is not a valid image record",
                layout.name
            )
        })?;
        ensure!(
            record.len() == header.record_size_bytes(),
            "{} bank: record {index} is {} bytes but its header describes {}",
            layout.name,
            record.len(),
            header.record_size_bytes()
        );
        parsed.push(header);
    }

    let region_len = layout.region_bytes();
    let mut region = vec![0u8; region_len];
    let mut cursor = RECORDS_OFFSET_FROM_TABLE;

    for (index, record) in records.iter().enumerate() {
        let aligned = align_up(cursor, 4);
        let end = aligned + record.len();
        if end > region_len {
            bail!(
                "{} bank region exceeded by {} bytes (records need 0x{end:x}, region is 0x{region_len:x})",
                layout.name,
                end - region_len
            );
        }
        region[aligned..end].copy_from_slice(record);

        let entry_off = index * layout.entry_size();
        match layout.entry_kind {
            TableEntryKind::Offset => write_u32(&mut region, entry_off, aligned as u32),
            TableEntryKind::Glyph => {
                let header = parsed[index].header;
                // Glyph height is the vertical extent, which the record calls `width`.
                write_u32(&mut region, entry_off, u32::from(header.width));
                write_u32(&mut region, entry_off + 4, u32::from(header.height));
                write_u32(&mut region, entry_off + 8, aligned as u32);
            }
        }
        cursor = end;
    }

    Ok(region)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image_format::IMAGE_HEADER_SIZE;
    use crate::image_format::tests::build_record;

    fn put_u32(buf: &mut [u8], off: usize, v: u32) {
        buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
    }

    /// A tiny fake flash image with the `small` layout squeezed into a small buffer.
    fn layout_for_test() -> BankLayout {
        BankLayout {
            name: "test",
            table_offset: 0x100,
            entry_kind: TableEntryKind::Offset,
            max_entries: 8,
            region_end: 0x100 + RECORDS_OFFSET_FROM_TABLE + 0x400,
            color_key: Some(COLOR_KEY_RGB565),
        }
    }

    fn glyph_layout_for_test() -> BankLayout {
        BankLayout {
            name: "glyph",
            table_offset: 0x100,
            entry_kind: TableEntryKind::Glyph,
            max_entries: 8,
            region_end: 0x100 + RECORDS_OFFSET_FROM_TABLE + 0x400,
            color_key: None,
        }
    }

    #[test]
    fn reads_offset_table_and_stops_at_zero() {
        let layout = layout_for_test();
        let ext_base = 0x20;
        let table = ext_base + layout.table_offset;
        let records = table + RECORDS_OFFSET_FROM_TABLE;
        let mut buf = vec![0u8; records + 0x400];

        let rec_a = build_record(2, 3, 2, &[0, 1, 2], &[0, 1, 2, 2, 1, 0]);
        let rec_b = build_record(1, 2, 2, &[7, 8], &[7, 8, 8, 7]);
        buf[records..records + rec_a.len()].copy_from_slice(&rec_a);
        // Keep 2-byte alignment slack between records.
        let b_off = records + rec_a.len() + 2;
        buf[b_off..b_off + rec_b.len()].copy_from_slice(&rec_b);

        put_u32(&mut buf, table, RECORDS_OFFSET_FROM_TABLE as u32);
        put_u32(&mut buf, table + 4, (b_off - table) as u32);
        // table[2] == 0 -> end

        let bank = read_bank(&buf, &layout, ext_base).unwrap();
        assert_eq!(bank.entries.len(), 2);
        assert_eq!(bank.entries[0].index, 0);
        assert_eq!(bank.entries[0].record.offset, records);
        assert_eq!(bank.entries[1].record.offset, b_off);
        assert_eq!(bank.records_end(), b_off + rec_b.len());
        assert_eq!(bank.used_bytes(), rec_a.len() + 2 + rec_b.len());
        assert_eq!(bank.free_bytes(), 0x400 - bank.used_bytes());
        assert_eq!(bank.free_table_slots(), 6);
    }

    #[test]
    fn stops_at_ffffffff_marker() {
        let layout = layout_for_test();
        let ext_base = 0;
        let table = layout.table_offset;
        let records = table + RECORDS_OFFSET_FROM_TABLE;
        let mut buf = vec![0u8; records + 0x400];
        let rec = build_record(1, 1, 1, &[5], &[5]);
        buf[records..records + rec.len()].copy_from_slice(&rec);
        put_u32(&mut buf, table, RECORDS_OFFSET_FROM_TABLE as u32);
        put_u32(&mut buf, table + 4, TABLE_END_MARKER);
        let bank = read_bank(&buf, &layout, ext_base).unwrap();
        assert_eq!(bank.entries.len(), 1);
    }

    #[test]
    fn rejects_overlapping_records() {
        let layout = layout_for_test();
        let table = layout.table_offset;
        let records = table + RECORDS_OFFSET_FROM_TABLE;
        let mut buf = vec![0u8; records + 0x400];
        let rec = build_record(1, 2, 1, &[5, 6], &[5, 6]);
        buf[records..records + rec.len()].copy_from_slice(&rec);
        put_u32(&mut buf, table, RECORDS_OFFSET_FROM_TABLE as u32);
        put_u32(&mut buf, table + 4, (RECORDS_OFFSET_FROM_TABLE + 2) as u32);
        let err = read_bank(&buf, &layout, 0).unwrap_err();
        assert!(err.to_string().contains("overlaps"), "{err}");
    }

    #[test]
    fn reads_glyph_table_and_checks_dimensions() {
        let layout = glyph_layout_for_test();
        let table = layout.table_offset;
        let records = table + RECORDS_OFFSET_FROM_TABLE;
        let mut buf = vec![0u8; records + 0x400];
        // Glyph 4 px wide (horizontal) and 6 px tall (vertical) -> record width=6, height=4.
        let pixels: Vec<u16> = vec![1; 24];
        let rec = build_record(1, 4, 6, &[0, 1], &pixels);
        buf[records..records + rec.len()].copy_from_slice(&rec);
        put_u32(&mut buf, table, 6);
        put_u32(&mut buf, table + 4, 4);
        put_u32(&mut buf, table + 8, RECORDS_OFFSET_FROM_TABLE as u32);

        let bank = read_bank(&buf, &layout, 0).unwrap();
        assert_eq!(bank.entries.len(), 1);
        assert_eq!(
            bank.entries[0].glyph,
            Some(GlyphInfo {
                height: 6,
                width: 4
            })
        );

        // Mismatching glyph dimensions are rejected.
        put_u32(&mut buf, table, 7);
        let err = read_bank(&buf, &layout, 0).unwrap_err();
        assert!(err.to_string().contains("glyph 0"), "{err}");
    }

    #[test]
    fn empty_table_is_an_error() {
        let layout = layout_for_test();
        let buf = vec![0u8; layout.table_offset + RECORDS_OFFSET_FROM_TABLE + IMAGE_HEADER_SIZE];
        assert!(read_bank(&buf, &layout, 0).is_err());
    }

    #[test]
    fn flash_addresses_match_firmware_constants() {
        assert_eq!(MG30_BANKS[0].table_flash_address(), 0x604C_0000);
        assert_eq!(MG30_BANKS[1].table_flash_address(), 0x605C_0000);
        assert_eq!(MG30_BANKS[2].table_flash_address(), 0x60D4_0000);
    }

    fn region_as_image(layout: &BankLayout, region: &[u8]) -> Vec<u8> {
        let mut image = vec![0u8; layout.table_offset + region.len()];
        image[layout.table_offset..].copy_from_slice(region);
        image
    }

    #[test]
    fn build_bank_region_round_trips_and_aligns_to_4() {
        let layout = layout_for_test();
        // pal 2 + 3 rle words is odd, so the record ends 2 bytes past a 4-byte boundary.
        let rec_a = build_record(2, 3, 1, &[10, 20], &[10, 20, 10]);
        let rec_b = build_record(1, 1, 1, &[5], &[5]);
        assert_eq!((rec_a.len()) % 4, 2);

        let region = build_bank_region(&layout, &[&rec_a, &rec_b]).unwrap();
        assert_eq!(region.len(), layout.region_bytes());
        let bank = read_bank(&region_as_image(&layout, &region), &layout, 0).unwrap();

        assert_eq!(bank.entries.len(), 2);
        for (entry, rec) in bank.entries.iter().zip([&rec_a, &rec_b]) {
            let start = entry.record.offset - layout.table_offset;
            assert_eq!(start % 4, 0, "record start not 4-aligned");
            assert_eq!(&region[start..start + rec.len()], rec.as_slice());
        }
        let gap = bank.entries[1].record.offset - bank.entries[0].record.end();
        assert_eq!(gap, 2);
        // The slot after the last entry is the zero terminator.
        assert_eq!(&region[2 * 4..3 * 4], &[0, 0, 0, 0]);
    }

    #[test]
    fn build_bank_region_reports_overflow_amount() {
        let mut layout = layout_for_test();
        layout.region_end = layout.table_offset + RECORDS_OFFSET_FROM_TABLE + 4;
        let rec = build_record(1, 2, 2, &[7, 8], &[7, 8, 8, 7]);
        let overflow = rec.len() - 4;
        let err = build_bank_region(&layout, &[&rec]).unwrap_err();
        assert!(
            err.to_string().contains(&format!("{overflow} bytes")),
            "{err}"
        );
    }

    #[test]
    fn build_bank_region_rejects_too_many_entries() {
        let mut layout = layout_for_test();
        layout.max_entries = 1;
        let rec = build_record(1, 1, 1, &[5], &[5]);
        let err = build_bank_region(&layout, &[&rec, &rec]).unwrap_err();
        assert!(err.to_string().contains("at most 1"), "{err}");
    }

    #[test]
    fn build_bank_region_glyph_table_follows_record() {
        let layout = glyph_layout_for_test();
        // 4 px wide, 6 px tall on screen -> record width 6 (vertical), height 4 (horizontal).
        let pixels = vec![1u16; 24];
        let rec = build_record(1, 4, 6, &[0, 1], &pixels);
        let region = build_bank_region(&layout, &[&rec]).unwrap();

        let (height, width, off) = (
            u32::from_le_bytes(region[0..4].try_into().unwrap()),
            u32::from_le_bytes(region[4..8].try_into().unwrap()),
            u32::from_le_bytes(region[8..12].try_into().unwrap()),
        );
        assert_eq!(
            (height, width, off),
            (6, 4, RECORDS_OFFSET_FROM_TABLE as u32)
        );

        let bank = read_bank(&region_as_image(&layout, &region), &layout, 0).unwrap();
        assert_eq!(
            bank.entries[0].glyph,
            Some(GlyphInfo {
                height: 6,
                width: 4
            })
        );
    }
}
