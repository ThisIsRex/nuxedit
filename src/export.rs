use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use image::RgbImage;
use serde::{Deserialize, Serialize};

use crate::depacketize::clean_bina_rel_to_original_abs;
use crate::image_format::ImageRecord;

#[derive(Debug, Serialize, Deserialize)]
pub struct ManifestImageEntry {
    pub index: usize,
    pub png: String,
    pub clean_bina_offset: usize,
    pub clean_abs_offset: usize,
    pub original_abs_offset: usize,
    pub width: u16,
    pub height: u16,
    pub output_width: u16,
    pub output_height: u16,
    pub bpp: u16,
    pub palette_entries: u16,
    pub rle_words: u32,
    pub record_size_bytes: usize,
    pub sha512: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ManifestSummary {
    pub firmware_product: String,
    pub firmware_version: String,
    pub sections: Vec<String>,
    pub clean_bina_size: usize,
    pub image_count: usize,
    pub small_count: usize,
    pub large_count: usize,
    pub large_threshold: usize,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub summary: ManifestSummary,
    pub images: Vec<ManifestImageEntry>,
}

pub fn png_filename(
    index: usize,
    clean_abs: usize,
    original_abs: usize,
    record: &ImageRecord,
) -> String {
    let (width, height) = record.output_dimensions();
    format!(
        "img_{index:04}_clean_{clean_abs:08x}_orig_{original_abs:08x}_{width}x{height}_bpp{bpp}_pal{palette_entries}.png",
        bpp = record.header.bpp,
        palette_entries = record.header.palette_entries,
    )
}

pub fn write_png(path: &Path, image: &RgbImage) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    image
        .save(path)
        .with_context(|| format!("failed to write PNG {}", path.display()))?;
    Ok(())
}

pub fn write_manifest(path: &Path, manifest: &Manifest) -> Result<()> {
    let yaml = serde_yaml::to_string(manifest).context("failed to serialize manifest YAML")?;
    fs::write(path, yaml).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

pub fn load_manifest(path: &Path) -> Result<Manifest> {
    let yaml = fs::read_to_string(path)
        .with_context(|| format!("failed to read manifest {}", path.display()))?;
    serde_yaml::from_str(&yaml).with_context(|| format!("failed to parse manifest {}", path.display()))
}

pub fn image_subdir(area: usize, large_threshold: usize) -> &'static str {
    if area >= large_threshold {
        "large"
    } else {
        "small"
    }
}

pub fn build_manifest_entry(
    index: usize,
    record: &ImageRecord,
    bina_abs_offset: usize,
    png_rel_path: &str,
    sha512: String,
) -> ManifestImageEntry {
    let clean_bina_offset = record.offset;
    let clean_abs_offset = bina_abs_offset + clean_bina_offset;
    let original_abs_offset = clean_bina_rel_to_original_abs(clean_bina_offset, bina_abs_offset);

    let (output_width, output_height) = record.output_dimensions();

    ManifestImageEntry {
        index,
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
        sha512,
    }
}

pub fn images_dir(out_dir: &Path, subdir: &str) -> PathBuf {
    out_dir.join("images").join(subdir)
}
