use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use image::{RgbImage, RgbaImage};
use serde::{Deserialize, Serialize};

use crate::banks::Bank;
use crate::depacketize::clean_bina_rel_to_original_abs;
use crate::image_format::ImageRecord;

/// Bank name used for records found by the heuristic scanner (no index table).
pub const SCAN_BANK_NAME: &str = "scan";

#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub struct ManifestImageEntry {
    /// Sequential number in this manifest.
    pub index: usize,
    /// Resource bank: `font`, `small`, `large` (or `scan` for heuristic extraction).
    pub bank: String,
    /// Index in the bank table — the ID the firmware uses to address the image.
    pub bank_index: usize,
    pub png: String,
    pub clean_bina_offset: usize,
    pub clean_abs_offset: usize,
    pub original_abs_offset: usize,
    /// Record header `width` in the original firmware: pixels along a framebuffer row (vertical on
    /// screen). `pack` takes the replacement dimensions from the PNG.
    pub width: u16,
    /// Record header `height` in the original firmware: framebuffer rows (horizontal on screen).
    pub height: u16,
    /// PNG width (horizontal) of the extracted image.
    pub output_width: u16,
    /// PNG height (vertical) of the extracted image.
    pub output_height: u16,
    /// 0 = raw RGB565, 1..=14 = palette index bits.
    pub bpp: u16,
    pub palette_entries: u16,
    pub rle_words: u32,
    /// Size of this record in the original firmware. Table manifests are relaid out, so this is not
    /// a limit there; scan manifests are still patched in place and must fit this slot.
    pub record_size_bytes: usize,
    /// RGB565 colour the firmware treats as transparent for this bank. When set, the PNG is RGBA
    /// and key pixels have alpha 0.
    pub color_key: Option<u16>,
    pub sha512: String,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub struct BankSummary {
    pub name: String,
    pub table_clean_bina_offset: usize,
    pub table_flash_address: u32,
    pub entries: usize,
    pub max_entries: usize,
    pub records_clean_bina_start: usize,
    pub records_clean_bina_end: usize,
    pub used_bytes: usize,
    pub free_bytes: usize,
    pub color_key: Option<u16>,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub struct ManifestSummary {
    pub firmware_product: String,
    pub firmware_version: String,
    pub sections: Vec<String>,
    pub clean_bina_size: usize,
    /// `table` (bank index tables) or `scan` (heuristic search).
    pub discovery: String,
    pub image_count: usize,
    #[serde(default)]
    pub banks: Vec<BankSummary>,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub summary: ManifestSummary,
    pub images: Vec<ManifestImageEntry>,
}

pub fn png_filename(bank: &str, bank_index: usize, record: &ImageRecord) -> String {
    let (width, height) = record.output_dimensions();
    format!(
        "{bank}_{bank_index:04}_{width}x{height}_bpp{bpp}_pal{palette_entries}.png",
        bpp = record.header.bpp,
        palette_entries = record.header.palette_entries,
    )
}

fn ensure_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    Ok(())
}

pub fn write_png(path: &Path, image: &RgbImage) -> Result<()> {
    ensure_parent(path)?;
    image
        .save(path)
        .with_context(|| format!("failed to write PNG {}", path.display()))?;
    Ok(())
}

pub fn write_png_rgba(path: &Path, image: &RgbaImage) -> Result<()> {
    ensure_parent(path)?;
    image
        .save(path)
        .with_context(|| format!("failed to write PNG {}", path.display()))?;
    Ok(())
}

pub const MANIFEST_FILE: &str = "manifest.ron";

pub fn write_manifest(path: &Path, manifest: &Manifest) -> Result<()> {
    crate::ron_io::write_ron(path, manifest, "manifest")
}

pub fn load_manifest(path: &Path) -> Result<Manifest> {
    crate::ron_io::read_ron(path, "manifest")
}

/// Sub-directory for scan-mode extraction (no bank information available).
pub fn image_subdir(area: usize, large_threshold: usize) -> &'static str {
    if area >= large_threshold {
        "large"
    } else {
        "small"
    }
}

#[allow(clippy::too_many_arguments)]
pub fn build_manifest_entry(
    index: usize,
    bank: &str,
    bank_index: usize,
    record: &ImageRecord,
    bina_abs_offset: usize,
    png_rel_path: &str,
    color_key: Option<u16>,
    sha512: String,
) -> ManifestImageEntry {
    let clean_bina_offset = record.offset;
    let clean_abs_offset = bina_abs_offset + clean_bina_offset;
    let original_abs_offset = clean_bina_rel_to_original_abs(clean_bina_offset, bina_abs_offset);

    let (output_width, output_height) = record.output_dimensions();

    ManifestImageEntry {
        index,
        bank: bank.to_owned(),
        bank_index,
        png: png_rel_path.to_owned(),
        clean_bina_offset,
        clean_abs_offset,
        original_abs_offset,
        width: record.header.width,
        height: record.header.height,
        output_width,
        output_height,
        bpp: record.header.bpp,
        palette_entries: record.header.palette_entries,
        rle_words: record.header.rle_words,
        record_size_bytes: record.record_size_bytes(),
        color_key,
        sha512,
    }
}

pub fn bank_summary(bank: &Bank) -> BankSummary {
    BankSummary {
        name: bank.name().to_owned(),
        table_clean_bina_offset: bank.table_bina_offset,
        table_flash_address: bank.layout.table_flash_address(),
        entries: bank.entries.len(),
        max_entries: bank.layout.max_entries,
        records_clean_bina_start: bank.records_start(),
        records_clean_bina_end: bank.records_end(),
        used_bytes: bank.used_bytes(),
        free_bytes: bank.free_bytes(),
        color_key: bank.layout.color_key,
    }
}

pub fn images_dir(out_dir: &Path, subdir: &str) -> PathBuf {
    out_dir.join("images").join(subdir)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_manifest() -> Manifest {
        Manifest {
            summary: ManifestSummary {
                firmware_product: "NUX MG-30".to_owned(),
                firmware_version: "V1.0.2".to_owned(),
                sections: vec!["TEXT".to_owned(), "BINA".to_owned(), "EXTR".to_owned()],
                clean_bina_size: 4096,
                discovery: "table".to_owned(),
                image_count: 1,
                banks: vec![BankSummary {
                    name: "small".to_owned(),
                    table_clean_bina_offset: 0x1526e3c,
                    table_flash_address: 0x605c0000,
                    entries: 1,
                    max_entries: 2800,
                    records_clean_bina_start: 0x152ee3c,
                    records_clean_bina_end: 0x152ee7c,
                    used_bytes: 64,
                    free_bytes: 100,
                    color_key: Some(0xf8ff),
                }],
            },
            images: vec![ManifestImageEntry {
                index: 0,
                bank: "small".to_owned(),
                bank_index: 0,
                png: "images/small/small_0000_32x16_bpp4_pal14.png".to_owned(),
                clean_bina_offset: 100,
                clean_abs_offset: 200,
                original_abs_offset: 300,
                width: 16,
                height: 32,
                output_width: 32,
                output_height: 16,
                bpp: 4,
                palette_entries: 14,
                rle_words: 8,
                record_size_bytes: 64,
                color_key: Some(0xf8ff),
                sha512: "abc".to_owned(),
            }],
        }
    }

    #[test]
    fn manifest_round_trips_through_ron() {
        let dir = std::env::temp_dir().join(format!("nuxedit-ron-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(MANIFEST_FILE);
        let manifest = sample_manifest();

        write_manifest(&path, &manifest).unwrap();
        let loaded = load_manifest(&path).unwrap();

        assert_eq!(loaded, manifest);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn manifest_parses_without_struct_names() {
        let ron = r#"
(
    summary: (
        firmware_product: "NUX MG-30",
        firmware_version: "V1.0.2",
        sections: ["TEXT", "BINA", "EXTR"],
        clean_bina_size: 4096,
        discovery: "table",
        image_count: 1,
        banks: [
            (
                name: "small",
                table_clean_bina_offset: 22179388,
                table_flash_address: 1616642048,
                entries: 1,
                max_entries: 2800,
                records_clean_bina_start: 22212156,
                records_clean_bina_end: 22212220,
                used_bytes: 64,
                free_bytes: 100,
                color_key: Some(63743),
            ),
        ],
    ),
    images: [
        (
            index: 0,
            bank: "small",
            bank_index: 0,
            png: "images/small/small_0000_32x16_bpp4_pal14.png",
            clean_bina_offset: 100,
            clean_abs_offset: 200,
            original_abs_offset: 300,
            width: 16,
            height: 32,
            output_width: 32,
            output_height: 16,
            bpp: 4,
            palette_entries: 14,
            rle_words: 8,
            record_size_bytes: 64,
            color_key: Some(63743),
            sha512: "abc",
        ),
    ],
)
"#;

        let parsed: Manifest = ron::from_str(ron).unwrap();
        assert_eq!(parsed, sample_manifest());
    }

    #[test]
    fn png_filename_uses_bank_and_table_index() {
        let record = ImageRecord {
            offset: 0,
            header: crate::image_format::ImageRecordHeader {
                bpp: 6,
                width: 84,
                height: 85,
                palette_entries: 40,
                rle_words: 100,
            },
        };
        assert_eq!(
            png_filename("small", 7, &record),
            "small_0007_85x84_bpp6_pal40.png"
        );
    }
}
