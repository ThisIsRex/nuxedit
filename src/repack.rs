use std::collections::HashMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use image::RgbImage;

use crate::container::parse_nux_dfu;
use crate::depacketize::clean_bina_rel_to_original_abs;
use crate::export::{ManifestImageEntry, load_manifest};
use crate::hash::sha512_file;
use crate::image_format::{IMAGE_HEADER_SIZE, rgb565_to_rgb888, validate_bpp_and_palette};

pub struct PackSummary {
    pub total: usize,
    pub skipped_unchanged: usize,
    pub patched: usize,
}

pub fn rgb888_to_rgb565(c: [u8; 3]) -> u16 {
    let r5 = (u16::from(c[0]) * 31 + 127) / 255;
    let g6 = (u16::from(c[1]) * 63 + 127) / 255;
    let b5 = (u16::from(c[2]) * 31 + 127) / 255;
    (r5 << 11) | (g6 << 5) | b5
}

fn color_distance_sq(a: [u8; 3], b: [u8; 3]) -> u32 {
    let dr = i32::from(a[0]) - i32::from(b[0]);
    let dg = i32::from(a[1]) - i32::from(b[1]);
    let db = i32::from(a[2]) - i32::from(b[2]);
    (dr * dr + dg * dg + db * db) as u32
}

fn extract_source_pixels(png: &RgbImage, entry: &ManifestImageEntry) -> Result<Vec<[u8; 3]>> {
    let src_w = entry.width as u32;
    let src_h = entry.height as u32;

    ensure!(
        png.width() == src_h && png.height() == src_w,
        "PNG dimensions mismatch for index {} ({}): expected {}x{} (output), got {}x{}",
        entry.index,
        entry.png,
        src_h,
        src_w,
        png.width(),
        png.height()
    );

    let mut pixels = Vec::with_capacity((src_w * src_h) as usize);
    for y in 0..src_h {
        for x in 0..src_w {
            let pixel = png.get_pixel(y, x);
            pixels.push([pixel[0], pixel[1], pixel[2]]);
        }
    }

    Ok(pixels)
}

fn build_palette(pixels: &[[u8; 3]], palette_entries: usize) -> (Vec<u16>, Vec<u8>) {
    let mut freq: HashMap<u16, usize> = HashMap::new();
    let mut rgb565_pixels = Vec::with_capacity(pixels.len());

    for &px in pixels {
        let c = rgb888_to_rgb565(px);
        rgb565_pixels.push(c);
        *freq.entry(c).or_default() += 1;
    }

    let mut ranked: Vec<(u16, usize)> = freq.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    let mut palette = Vec::with_capacity(palette_entries);
    for (color, _) in ranked.into_iter().take(palette_entries) {
        palette.push(color);
    }

    while palette.len() < palette_entries {
        palette.push(0);
    }

    let palette_rgb: Vec<[u8; 3]> = palette.iter().copied().map(rgb565_to_rgb888).collect();

    let mut indices = Vec::with_capacity(pixels.len());
    for &c565 in &rgb565_pixels {
        let px = rgb565_to_rgb888(c565);
        let mut best_idx = 0u8;
        let mut best_dist = u32::MAX;
        for (idx, &pal_rgb) in palette_rgb.iter().enumerate() {
            let dist = color_distance_sq(px, pal_rgb);
            if dist < best_dist {
                best_dist = dist;
                best_idx = idx as u8;
            }
        }
        indices.push(best_idx);
    }

    (palette, indices)
}

fn rle_encode(indices: &[u8], bpp: u16) -> Result<Vec<u16>> {
    if indices.is_empty() {
        return Ok(Vec::new());
    }

    let max_index = 1u16 << bpp;
    let max_run = 1usize << (16 - bpp);
    let mut words = Vec::new();
    let mut i = 0;

    while i < indices.len() {
        let idx = indices[i];
        ensure!(
            u16::from(idx) < max_index,
            "palette index {idx} does not fit in bpp={bpp} (max index {})",
            max_index - 1
        );

        let mut run = 1usize;
        while i + run < indices.len() && indices[i + run] == idx && run < max_run {
            run += 1;
        }
        words.push((((run - 1) as u16) << bpp) | u16::from(idx));
        i += run;
    }

    Ok(words)
}

pub fn encode_record(entry: &ManifestImageEntry, png: &RgbImage) -> Result<Vec<u8>> {
    validate_bpp_and_palette(entry.bpp, entry.palette_entries).with_context(|| {
        format!(
            "invalid bpp/palette_entries for index {} ({})",
            entry.index, entry.png
        )
    })?;

    let pixels = extract_source_pixels(png, entry)?;
    let palette_entries = entry.palette_entries as usize;
    let (palette, indices) = build_palette(&pixels, palette_entries);
    let rle_words = rle_encode(&indices, entry.bpp)?;
    let rle_word_count = rle_words.len();

    let mut out = Vec::with_capacity(entry.record_size_bytes);
    out.extend_from_slice(&entry.bpp.to_le_bytes());
    out.extend_from_slice(&entry.width.to_le_bytes());
    out.extend_from_slice(&entry.height.to_le_bytes());
    out.extend_from_slice(&entry.palette_entries.to_le_bytes());
    out.extend_from_slice(&(rle_word_count as u32).to_le_bytes());

    for color in palette {
        out.extend_from_slice(&color.to_le_bytes());
    }
    for word in rle_words {
        out.extend_from_slice(&word.to_le_bytes());
    }

    let header_and_payload = IMAGE_HEADER_SIZE + palette_entries * 2 + rle_word_count * 2;
    ensure!(
        out.len() == header_and_payload,
        "internal size mismatch for index {}",
        entry.index
    );

    if out.len() > entry.record_size_bytes {
        bail!(
            "re-encoded image index {} ({}) is too large: {} bytes, limit {} bytes",
            entry.index,
            entry.png,
            out.len(),
            entry.record_size_bytes
        );
    }

    out.resize(entry.record_size_bytes, 0);
    Ok(out)
}

fn patch_record(
    firmware: &mut [u8],
    entry: &ManifestImageEntry,
    record_bytes: &[u8],
    bina_abs_offset: usize,
) -> Result<()> {
    ensure!(
        record_bytes.len() == entry.record_size_bytes,
        "record byte length mismatch for index {}",
        entry.index
    );

    for (i, &byte) in record_bytes.iter().enumerate() {
        let clean_rel = entry.clean_bina_offset + i;
        let abs = clean_bina_rel_to_original_abs(clean_rel, bina_abs_offset);
        let slot = firmware
            .get_mut(abs)
            .with_context(|| {
                format!(
                    "patch offset out of bounds for index {} at clean_rel=0x{clean_rel:x}, abs=0x{abs:x}",
                    entry.index
                )
            })?;
        *slot = byte;
    }

    Ok(())
}

pub fn pack(firmware_path: &Path, out_dir: &Path, output_path: &Path) -> Result<PackSummary> {
    let manifest_path = out_dir.join("manifest.yaml");
    let manifest = load_manifest(&manifest_path)?;

    for entry in &manifest.images {
        validate_bpp_and_palette(entry.bpp, entry.palette_entries).with_context(|| {
            format!(
                "invalid manifest entry index {} ({}): bpp={}, palette_entries={}",
                entry.index, entry.png, entry.bpp, entry.palette_entries
            )
        })?;
    }

    let mut firmware = fs::read(firmware_path)
        .with_context(|| format!("failed to read firmware {}", firmware_path.display()))?;
    let fw = parse_nux_dfu(&firmware)?;
    let bina = fw
        .section("BINA")
        .ok_or_else(|| anyhow::anyhow!("BINA section not found"))?;
    let bina_abs_offset = bina.offset;

    let mut skipped_unchanged = 0usize;
    let mut patched = 0usize;

    for entry in &manifest.images {
        let png_path = out_dir.join(&entry.png);
        let current_sha512 = sha512_file(&png_path).with_context(|| {
            format!(
                "failed to hash PNG for index {} at {}",
                entry.index,
                png_path.display()
            )
        })?;

        if current_sha512 == entry.sha512 {
            skipped_unchanged += 1;
            continue;
        }

        let png = image::open(&png_path)
            .with_context(|| {
                format!(
                    "failed to load PNG for index {} at {}",
                    entry.index,
                    png_path.display()
                )
            })?
            .to_rgb8();

        let record_bytes = encode_record(entry, &png)?;
        patch_record(&mut firmware, entry, &record_bytes, bina_abs_offset)?;
        patched += 1;
    }

    if let Some(parent) = output_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(output_path, &firmware)
        .with_context(|| format!("failed to write {}", output_path.display()))?;

    Ok(PackSummary {
        total: manifest.images.len(),
        skipped_unchanged,
        patched,
    })
}
