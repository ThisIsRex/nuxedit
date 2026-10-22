use std::collections::HashMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use image::{DynamicImage, GenericImageView};

use crate::container::parse_nux_dfu;
use crate::depacketize::clean_bina_rel_to_original_abs;
use crate::export::{MANIFEST_FILE, ManifestImageEntry, load_manifest};
use crate::hash::sha512_file;
use crate::image_format::{IMAGE_HEADER_SIZE, MAX_BPP, rgb565_to_rgb888, validate_bpp_and_palette};

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

/// Validates the `bpp`/`palette_entries` pair of a manifest entry (raw images allowed).
pub fn validate_entry_format(entry: &ManifestImageEntry) -> Result<()> {
    if entry.bpp == 0 {
        ensure!(
            entry.palette_entries == 0,
            "raw (bpp=0) images must have palette_entries=0, got {}",
            entry.palette_entries
        );
        return Ok(());
    }
    ensure!(
        entry.bpp <= MAX_BPP,
        "bpp must be 0..={MAX_BPP}, got {}",
        entry.bpp
    );
    validate_bpp_and_palette(entry.bpp, entry.palette_entries)
}

/// Converts the PNG into RGB565 pixels in firmware stream order (column-major on screen).
///
/// Transparent pixels (alpha < 128) become the bank colour key when the entry has one.
fn extract_source_pixels(png: &DynamicImage, entry: &ManifestImageEntry) -> Result<Vec<u16>> {
    let src_w = u32::from(entry.width);
    let src_h = u32::from(entry.height);

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
            let p = png.get_pixel(y, x).0;
            let c = match entry.color_key {
                Some(key) if p[3] < 128 => key,
                _ => rgb888_to_rgb565([p[0], p[1], p[2]]),
            };
            pixels.push(c);
        }
    }

    Ok(pixels)
}

/// Builds a palette of at most `palette_entries` colours (most frequent first) and maps every
/// pixel to its nearest palette entry.
fn build_palette(pixels: &[u16], palette_entries: usize) -> (Vec<u16>, Vec<u16>) {
    let mut freq: HashMap<u16, usize> = HashMap::new();
    for &c in pixels {
        *freq.entry(c).or_default() += 1;
    }

    let mut ranked: Vec<(u16, usize)> = freq.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    let mut palette: Vec<u16> = ranked
        .iter()
        .take(palette_entries)
        .map(|(c, _)| *c)
        .collect();
    while palette.len() < palette_entries {
        palette.push(0);
    }

    let exact: HashMap<u16, u16> = palette
        .iter()
        .enumerate()
        .map(|(i, &c)| (c, i as u16))
        .collect();
    let palette_rgb: Vec<[u8; 3]> = palette.iter().copied().map(rgb565_to_rgb888).collect();

    let mut nearest_cache: HashMap<u16, u16> = HashMap::new();
    let indices = pixels
        .iter()
        .map(|&c| {
            if let Some(&i) = exact.get(&c) {
                return i;
            }
            *nearest_cache.entry(c).or_insert_with(|| {
                let px = rgb565_to_rgb888(c);
                let mut best_idx = 0u16;
                let mut best_dist = u32::MAX;
                for (idx, &pal_rgb) in palette_rgb.iter().enumerate() {
                    let dist = color_distance_sq(px, pal_rgb);
                    if dist < best_dist {
                        best_dist = dist;
                        best_idx = idx as u16;
                    }
                }
                best_idx
            })
        })
        .collect();

    (palette, indices)
}

fn rle_encode(indices: &[u16], bpp: u16) -> Result<Vec<u16>> {
    if indices.is_empty() {
        return Ok(Vec::new());
    }

    let max_index = 1u32 << bpp;
    let max_run = 1usize << (16 - bpp);
    let mut words = Vec::new();
    let mut i = 0;

    while i < indices.len() {
        let idx = indices[i];
        ensure!(
            u32::from(idx) < max_index,
            "palette index {idx} does not fit in bpp={bpp} (max index {})",
            max_index - 1
        );

        let mut run = 1usize;
        while i + run < indices.len() && indices[i + run] == idx && run < max_run {
            run += 1;
        }
        words.push((((run - 1) as u16) << bpp) | idx);
        i += run;
    }

    Ok(words)
}

/// Encodes a PNG into a firmware record using the format parameters from the manifest entry.
pub fn encode_record(entry: &ManifestImageEntry, png: &DynamicImage) -> Result<Vec<u8>> {
    validate_entry_format(entry).with_context(|| {
        format!(
            "invalid bpp/palette_entries for index {} ({})",
            entry.index, entry.png
        )
    })?;

    let pixels = extract_source_pixels(png, entry)?;

    let mut out = Vec::with_capacity(entry.record_size_bytes);
    out.extend_from_slice(&entry.bpp.to_le_bytes());
    out.extend_from_slice(&entry.width.to_le_bytes());
    out.extend_from_slice(&entry.height.to_le_bytes());
    out.extend_from_slice(&entry.palette_entries.to_le_bytes());

    if entry.bpp == 0 {
        out.extend_from_slice(&0u32.to_le_bytes());
        for c in &pixels {
            out.extend_from_slice(&c.to_le_bytes());
        }
    } else {
        let palette_entries = entry.palette_entries as usize;
        let (palette, indices) = build_palette(&pixels, palette_entries);
        let rle_words = rle_encode(&indices, entry.bpp)?;

        out.extend_from_slice(&(rle_words.len() as u32).to_le_bytes());
        for color in palette {
            out.extend_from_slice(&color.to_le_bytes());
        }
        for word in rle_words {
            out.extend_from_slice(&word.to_le_bytes());
        }
    }

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
    let manifest_path = out_dir.join(MANIFEST_FILE);
    let manifest = load_manifest(&manifest_path)?;

    for entry in &manifest.images {
        validate_entry_format(entry).with_context(|| {
            format!(
                "invalid manifest entry index {} ({}): bpp={}, palette_entries={}",
                entry.index, entry.png, entry.bpp, entry.palette_entries
            )
        })?;
        ensure!(
            entry.record_size_bytes >= IMAGE_HEADER_SIZE,
            "invalid record_size_bytes for index {}",
            entry.index
        );
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

        let png = image::open(&png_path).with_context(|| {
            format!(
                "failed to load PNG for index {} at {}",
                entry.index,
                png_path.display()
            )
        })?;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image_format::{COLOR_KEY_RGB565, decode_pixels, read_image_record};
    use image::{Rgba, RgbaImage};

    fn entry(
        bpp: u16,
        palette_entries: u16,
        out_w: u16,
        out_h: u16,
        color_key: Option<u16>,
    ) -> ManifestImageEntry {
        ManifestImageEntry {
            index: 0,
            bank: "test".to_owned(),
            bank_index: 0,
            png: "x.png".to_owned(),
            clean_bina_offset: 0,
            clean_abs_offset: 0,
            original_abs_offset: 0,
            width: out_h,
            height: out_w,
            output_width: out_w,
            output_height: out_h,
            bpp,
            palette_entries,
            rle_words: 0,
            record_size_bytes: 1 << 20,
            color_key,
            sha512: String::new(),
        }
    }

    fn gradient(out_w: u32, out_h: u32) -> DynamicImage {
        let mut img = RgbaImage::new(out_w, out_h);
        for y in 0..out_h {
            for x in 0..out_w {
                let v = ((x * 7 + y * 13) % 256) as u8;
                img.put_pixel(x, y, Rgba([v, 255 - v, (x * 3) as u8, 255]));
            }
        }
        DynamicImage::ImageRgba8(img)
    }

    fn trim(record: Vec<u8>) -> Vec<u8> {
        let rec = read_image_record(&record, 0).unwrap();
        record[..rec.record_size_bytes()].to_vec()
    }

    #[test]
    fn round_trips_high_bpp_image() {
        let (w, h) = (40u32, 25u32);
        let png = gradient(w, h);
        let e = entry(12, 4096, w as u16, h as u16, None);
        let record = trim(encode_record(&e, &png).unwrap());
        let rec = read_image_record(&record, 0).unwrap();
        assert_eq!(rec.header.bpp, 12);
        let decoded = decode_pixels(&record, &rec).unwrap();
        let rgb = png.to_rgb8();
        for y in 0..h {
            for x in 0..w {
                let p = rgb.get_pixel(x, y).0;
                assert_eq!(decoded.pixels[(y * w + x) as usize], rgb888_to_rgb565(p));
            }
        }
    }

    #[test]
    fn round_trips_raw_image() {
        let (w, h) = (9u32, 4u32);
        let png = gradient(w, h);
        let e = entry(0, 0, w as u16, h as u16, None);
        let record = trim(encode_record(&e, &png).unwrap());
        let rec = read_image_record(&record, 0).unwrap();
        assert!(rec.header.is_raw());
        assert_eq!(
            rec.record_size_bytes(),
            IMAGE_HEADER_SIZE + (w * h * 2) as usize
        );
        let decoded = decode_pixels(&record, &rec).unwrap();
        let rgb = png.to_rgb8();
        assert_eq!(decoded.pixels[5], rgb888_to_rgb565(rgb.get_pixel(5, 0).0));
    }

    #[test]
    fn transparent_pixels_become_color_key() {
        let mut img = RgbaImage::new(2, 1);
        img.put_pixel(0, 0, Rgba([10, 20, 30, 0]));
        img.put_pixel(1, 0, Rgba([255, 255, 255, 255]));
        let e = entry(1, 2, 2, 1, Some(COLOR_KEY_RGB565));
        let record = trim(encode_record(&e, &DynamicImage::ImageRgba8(img)).unwrap());
        let rec = read_image_record(&record, 0).unwrap();
        let decoded = decode_pixels(&record, &rec).unwrap();
        assert_eq!(decoded.pixels, vec![COLOR_KEY_RGB565, 0xffff]);
    }

    #[test]
    fn rejects_raw_with_palette_and_bpp_above_14() {
        assert!(validate_entry_format(&entry(0, 3, 1, 1, None)).is_err());
        assert!(validate_entry_format(&entry(15, 1, 1, 1, None)).is_err());
        validate_entry_format(&entry(14, 16384, 1, 1, None)).unwrap();
    }

    #[test]
    fn too_large_record_is_rejected() {
        let png = gradient(40, 25);
        let mut e = entry(8, 256, 40, 25, None);
        e.record_size_bytes = 64;
        let err = encode_record(&e, &png).unwrap_err();
        assert!(err.to_string().contains("too large"), "{err}");
    }
}
