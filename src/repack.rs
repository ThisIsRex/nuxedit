use std::collections::HashMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use image::{DynamicImage, GenericImageView};

use crate::banks::{Bank, build_bank_region, read_banks};
use crate::container::parse_nux_dfu;
use crate::depacketize::{depacketize_bina, patch_clean_range};
use crate::export::{MANIFEST_FILE, Manifest, ManifestImageEntry, load_manifest};
use crate::hash::sha512_file;
use crate::image_format::{
    IMAGE_HEADER_SIZE, MAX_BPP, SCREEN_HEIGHT, SCREEN_WIDTH, decode_pixels, read_image_record,
    rgb565_to_rgb888, validate_bpp_and_palette,
};

pub struct BankPackStats {
    pub name: String,
    pub rebuilt: bool,
    pub reencoded: usize,
    pub dimensions_changed: usize,
    pub used_bytes_before: usize,
    pub used_bytes_after: usize,
    pub free_bytes_after: usize,
}

pub struct PackSummary {
    pub total: usize,
    pub skipped_unchanged: usize,
    pub patched: usize,
    pub banks: Vec<BankPackStats>,
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

/// Firmware record dimensions for a PNG: `(width, height)` = `(vertical, horizontal)`.
///
/// The PNG is in screen orientation, so its width is the horizontal extent and its height the
/// vertical one. Both must fit the panel.
fn firmware_dimensions(png: &DynamicImage, entry: &ManifestImageEntry) -> Result<(u16, u16)> {
    let horizontal = png.width();
    let vertical = png.height();
    ensure!(
        (1..=u32::from(SCREEN_WIDTH)).contains(&horizontal)
            && (1..=u32::from(SCREEN_HEIGHT)).contains(&vertical),
        "PNG for index {} ({}) is {horizontal}x{vertical}, must be 1..={SCREEN_WIDTH} horizontal by 1..={SCREEN_HEIGHT} vertical",
        entry.index,
        entry.png
    );
    Ok((vertical as u16, horizontal as u16))
}

/// Converts the PNG into RGB565 pixels in firmware stream order (column-major on screen).
///
/// Returns `(record width, record height, pixels)`. Transparent pixels (alpha < 128) become the
/// bank colour key when the entry has one.
fn extract_source_pixels(
    png: &DynamicImage,
    entry: &ManifestImageEntry,
) -> Result<(u16, u16, Vec<u16>)> {
    let (src_w, src_h) = firmware_dimensions(png, entry)?;

    let mut pixels = Vec::with_capacity(usize::from(src_w) * usize::from(src_h));
    for y in 0..u32::from(src_h) {
        for x in 0..u32::from(src_w) {
            let p = png.get_pixel(y, x).0;
            let c = match entry.color_key {
                Some(key) if p[3] < 128 => key,
                _ => rgb888_to_rgb565([p[0], p[1], p[2]]),
            };
            pixels.push(c);
        }
    }

    Ok((src_w, src_h, pixels))
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

/// Encodes a PNG into a firmware record.
///
/// Dimensions come from the PNG. `bpp`, `palette_entries` and `color_key` come from the manifest.
/// The result is the exact record, with no slot padding.
pub fn encode_record(entry: &ManifestImageEntry, png: &DynamicImage) -> Result<Vec<u8>> {
    validate_entry_format(entry).with_context(|| {
        format!(
            "invalid bpp/palette_entries for index {} ({})",
            entry.index, entry.png
        )
    })?;

    let (width, height, pixels) = extract_source_pixels(png, entry)?;

    let mut out = Vec::new();
    out.extend_from_slice(&entry.bpp.to_le_bytes());
    out.extend_from_slice(&width.to_le_bytes());
    out.extend_from_slice(&height.to_le_bytes());
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

    Ok(out)
}

/// Scan-mode packing: the record must still fit the original slot, and a short record is padded
/// with zeros so the following record stays put.
fn fit_in_slot(entry: &ManifestImageEntry, mut record: Vec<u8>) -> Result<Vec<u8>> {
    if record.len() > entry.record_size_bytes {
        bail!(
            "re-encoded image index {} ({}) is too large: {} bytes, limit {} bytes",
            entry.index,
            entry.png,
            record.len(),
            entry.record_size_bytes
        );
    }
    record.resize(entry.record_size_bytes, 0);
    Ok(record)
}

fn load_changed_pngs(
    manifest: &Manifest,
    out_dir: &Path,
) -> Result<(HashMap<usize, DynamicImage>, usize)> {
    let mut changed = HashMap::new();
    let mut skipped_unchanged = 0usize;

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
        ensure!(
            changed.insert(entry.index, png).is_none(),
            "duplicate manifest index {}",
            entry.index
        );
    }

    Ok((changed, skipped_unchanged))
}

/// Scan manifests have no index table, so each record is patched inside its original slot.
fn patch_in_place(
    firmware: &mut [u8],
    bina_abs_offset: usize,
    manifest: &Manifest,
    changed: &HashMap<usize, DynamicImage>,
) -> Result<()> {
    for entry in &manifest.images {
        let Some(png) = changed.get(&entry.index) else {
            continue;
        };
        ensure!(
            entry.record_size_bytes >= IMAGE_HEADER_SIZE,
            "invalid record_size_bytes for index {}",
            entry.index
        );
        let record = encode_record(entry, png)?;
        let padded = fit_in_slot(entry, record)?;
        patch_clean_range(firmware, bina_abs_offset, entry.clean_bina_offset, &padded)
            .with_context(|| format!("failed to patch index {} ({})", entry.index, entry.png))?;
    }
    Ok(())
}

struct RebuiltBank {
    encoded: HashMap<usize, Vec<u8>>,
    reencoded: usize,
    dimensions_changed: usize,
}

/// Table manifests: any bank with a changed image is rewritten from scratch (records packed by
/// `bank_index`, index table regenerated). Banks with nothing changed are left byte for byte.
fn rebuild_tables(
    firmware: &mut [u8],
    bina_abs_offset: usize,
    manifest: &Manifest,
    changed: &HashMap<usize, DynamicImage>,
) -> Result<Vec<BankPackStats>> {
    let bina_end = bina_abs_offset
        + parse_nux_dfu(firmware)?
            .section("BINA")
            .map(|s| s.size)
            .ok_or_else(|| anyhow::anyhow!("BINA section not found"))?;
    let original_clean = depacketize_bina(&firmware[bina_abs_offset..bina_end]);
    let banks = read_banks(&original_clean)?;

    let mut by_name: HashMap<&str, &Bank> = HashMap::new();
    for bank in &banks {
        by_name.insert(bank.name(), bank);
    }

    let mut manifest_by_bank: HashMap<&str, HashMap<usize, &ManifestImageEntry>> = HashMap::new();
    for entry in &manifest.images {
        let bank = by_name.get(entry.bank.as_str()).with_context(|| {
            format!(
                "manifest entry {} refers to unknown bank \"{}\"",
                entry.index, entry.bank
            )
        })?;
        ensure!(
            entry.bank_index < bank.entries.len(),
            "manifest entry {} bank_index {} is outside the {} bank ({} entries)",
            entry.index,
            entry.bank_index,
            bank.name(),
            bank.entries.len()
        );
        let slots = manifest_by_bank.entry(bank.name()).or_default();
        ensure!(
            slots.insert(entry.bank_index, entry).is_none(),
            "{} bank index {} is listed twice in the manifest",
            bank.name(),
            entry.bank_index
        );
    }

    let mut rebuilt: HashMap<&str, RebuiltBank> = HashMap::new();
    for bank in &banks {
        let listed = manifest_by_bank.get(bank.name());
        let mut encoded = HashMap::new();
        let mut dimensions_changed = 0usize;
        for entry_in_bank in &bank.entries {
            let Some(entry) = listed.and_then(|slots| slots.get(&entry_in_bank.index)) else {
                continue;
            };
            let Some(png) = changed.get(&entry.index) else {
                continue;
            };
            if png.width() != u32::from(entry.output_width)
                || png.height() != u32::from(entry.output_height)
            {
                dimensions_changed += 1;
            }
            encoded.insert(entry_in_bank.index, encode_record(entry, png)?);
        }
        if encoded.is_empty() {
            continue;
        }

        let mut records = Vec::with_capacity(bank.entries.len());
        for entry_in_bank in &bank.entries {
            let bytes = if let Some(encoded) = encoded.get(&entry_in_bank.index) {
                encoded.clone()
            } else {
                let record = &entry_in_bank.record;
                original_clean[record.offset..record.end()].to_vec()
            };
            records.push(bytes);
        }
        let record_refs: Vec<&[u8]> = records.iter().map(Vec::as_slice).collect();
        let region = build_bank_region(&bank.layout, &record_refs)
            .with_context(|| format!("failed to rebuild the {} bank", bank.name()))?;
        patch_clean_range(firmware, bina_abs_offset, bank.region_bina_start(), &region)
            .with_context(|| format!("failed to write the rebuilt {} bank", bank.name()))?;

        let reencoded = encoded.len();
        rebuilt.insert(
            bank.name(),
            RebuiltBank {
                encoded,
                reencoded,
                dimensions_changed,
            },
        );
    }

    verify_rebuilt_banks(firmware, bina_abs_offset, &original_clean, &banks, &rebuilt)
}

fn verify_rebuilt_banks(
    firmware: &[u8],
    bina_abs_offset: usize,
    original_clean: &[u8],
    banks: &[Bank],
    rebuilt: &HashMap<&str, RebuiltBank>,
) -> Result<Vec<BankPackStats>> {
    let bina_end = bina_abs_offset
        + parse_nux_dfu(firmware)?
            .section("BINA")
            .map(|section| section.size)
            .ok_or_else(|| anyhow::anyhow!("BINA section not found"))?;
    let patched_clean = depacketize_bina(&firmware[bina_abs_offset..bina_end]);
    let patched_banks =
        read_banks(&patched_clean).context("patched firmware banks could not be read back")?;

    let mut stats = Vec::with_capacity(banks.len());
    for (original, patched) in banks.iter().zip(patched_banks.iter()) {
        ensure!(
            original.name() == patched.name(),
            "bank order changed while rebuilding"
        );
        let Some(job) = rebuilt.get(original.name()) else {
            let start = original.region_bina_start();
            let end = start + original.layout.region_bytes();
            let original_region = original_clean.get(start..end).with_context(|| {
                format!(
                    "{} bank region is outside the original image",
                    original.name()
                )
            })?;
            let patched_region = patched_clean.get(start..end).with_context(|| {
                format!(
                    "{} bank region is outside the patched image",
                    original.name()
                )
            })?;
            ensure!(
                original_region == patched_region,
                "{} bank changed even though none of its images did",
                original.name()
            );
            stats.push(BankPackStats {
                name: original.name().to_owned(),
                rebuilt: false,
                reencoded: 0,
                dimensions_changed: 0,
                used_bytes_before: original.used_bytes(),
                used_bytes_after: original.used_bytes(),
                free_bytes_after: original.free_bytes(),
            });
            continue;
        };

        ensure!(
            original.entries.len() == patched.entries.len(),
            "{} bank entry count changed from {} to {}",
            original.name(),
            original.entries.len(),
            patched.entries.len()
        );
        for (before, after) in original.entries.iter().zip(patched.entries.iter()) {
            ensure!(
                before.index == after.index,
                "{} bank indices shifted during rebuild",
                original.name()
            );
            let actual = patched_clean
                .get(after.record.offset..after.record.end())
                .with_context(|| {
                    format!(
                        "{} #{} record is outside the patched image",
                        original.name(),
                        after.index
                    )
                })?;
            if let Some(encoded) = job.encoded.get(&before.index) {
                ensure!(
                    actual == encoded.as_slice(),
                    "{} #{} record bytes differ after rebuild",
                    original.name(),
                    before.index
                );
                let decoded = decode_pixels(&patched_clean, &after.record)?;
                let expected_record = read_image_record(encoded, 0)?;
                let expected = decode_pixels(encoded, &expected_record)?;
                ensure!(
                    decoded.pixels == expected.pixels,
                    "{} #{} pixels differ after rebuild",
                    original.name(),
                    before.index
                );
            } else {
                let untouched = &original_clean[before.record.offset..before.record.end()];
                ensure!(
                    actual == untouched,
                    "{} #{} was not edited but its record bytes changed",
                    original.name(),
                    before.index
                );
            }
        }

        stats.push(BankPackStats {
            name: original.name().to_owned(),
            rebuilt: true,
            reencoded: job.reencoded,
            dimensions_changed: job.dimensions_changed,
            used_bytes_before: original.used_bytes(),
            used_bytes_after: patched.used_bytes(),
            free_bytes_after: patched.free_bytes(),
        });
    }

    Ok(stats)
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
    }

    let mut firmware = fs::read(firmware_path)
        .with_context(|| format!("failed to read firmware {}", firmware_path.display()))?;
    let fw = parse_nux_dfu(&firmware)?;
    let bina = fw
        .section("BINA")
        .ok_or_else(|| anyhow::anyhow!("BINA section not found"))?;
    let bina_abs_offset = bina.offset;

    let (changed, skipped_unchanged) = load_changed_pngs(&manifest, out_dir)?;
    let patched = changed.len();

    let banks = match manifest.summary.discovery.as_str() {
        "table" => rebuild_tables(&mut firmware, bina_abs_offset, &manifest, &changed)?,
        "scan" => {
            patch_in_place(&mut firmware, bina_abs_offset, &manifest, &changed)?;
            Vec::new()
        }
        other => bail!("manifest discovery mode \"{other}\" is not supported"),
    };

    if let Some(parent) = output_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(output_path, &firmware)
        .with_context(|| format!("failed to write {}", output_path.display()))?;

    Ok(PackSummary {
        total: manifest.images.len(),
        skipped_unchanged,
        patched,
        banks,
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

    #[test]
    fn round_trips_high_bpp_image() {
        let (w, h) = (40u32, 25u32);
        let png = gradient(w, h);
        let e = entry(12, 4096, w as u16, h as u16, None);
        let record = encode_record(&e, &png).unwrap();
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
        let record = encode_record(&e, &png).unwrap();
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
        let record = encode_record(&e, &DynamicImage::ImageRgba8(img)).unwrap();
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
    fn png_dimensions_override_the_manifest() {
        let png = gradient(5, 3);
        // Manifest still describes the original 9x9 slot; the record follows the PNG.
        let entry = entry(4, 16, 9, 9, None);
        let record = encode_record(&entry, &png).unwrap();
        let rec = read_image_record(&record, 0).unwrap();
        assert_eq!(rec.header.width, 3);
        assert_eq!(rec.header.height, 5);
        assert_eq!(record.len(), rec.record_size_bytes());
    }

    #[test]
    fn rejects_png_larger_than_the_screen() {
        let png = DynamicImage::ImageRgba8(RgbaImage::new(801, 1));
        let err = encode_record(&entry(4, 16, 1, 1, None), &png).unwrap_err();
        assert!(err.to_string().contains("800"), "{err}");
    }

    #[test]
    fn too_large_record_is_rejected_for_in_place_slots() {
        let png = gradient(40, 25);
        let mut entry = entry(8, 256, 40, 25, None);
        entry.record_size_bytes = 64;
        let record = encode_record(&entry, &png).unwrap();
        let err = fit_in_slot(&entry, record).unwrap_err();
        assert!(err.to_string().contains("too large"), "{err}");
    }
}
