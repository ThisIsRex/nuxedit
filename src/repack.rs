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
    IMAGE_HEADER_SIZE, MAX_BPP, MAX_PALETTE_ENTRIES, SCREEN_HEIGHT, SCREEN_WIDTH, decode_pixels,
    read_image_record, validate_bpp_and_palette,
};
use crate::quantize::{ColorHistogram, Quantized, channel_error, lossless, median_cut, needed_bpp};

pub struct BankPackStats {
    pub name: String,
    pub rebuilt: bool,
    pub reencoded: usize,
    pub dimensions_changed: usize,
    pub used_bytes_before: usize,
    pub used_bytes_after: usize,
    pub free_bytes_after: usize,
}

/// Highest `bpp` [`PaletteMode::Auto`] will emit.
///
/// 3000 colours need 12 bits (`2^11` holds 2048) and the firmware palette buffer holds 3000,
/// so a wider index cannot store a larger palette.
pub const MAX_AUTO_BPP: u16 = 12;

/// Where `bpp` and `palette_entries` come from when a PNG is re-encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PaletteMode {
    /// Infer both from the PNG. Lossless when the colour count fits the cap; otherwise median-cut.
    #[default]
    Auto,
    /// Keep the manifest `bpp` and quantize down to its `palette_entries` (no zero padding).
    Manifest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PackOptions {
    pub palette: PaletteMode,
    pub max_bpp: u16,
}

impl Default for PackOptions {
    fn default() -> Self {
        Self {
            palette: PaletteMode::Auto,
            max_bpp: MAX_AUTO_BPP,
        }
    }
}

/// What the encoder did with one changed PNG, for the warning lines and the pack summary.
#[derive(Debug, Clone)]
pub struct EncodeReport {
    pub bank: String,
    pub bank_index: usize,
    pub png: String,
    pub distinct_colors: usize,
    pub bpp: u16,
    pub palette_entries: u16,
    pub lossless: bool,
    pub manifest_bpp: u16,
    pub manifest_palette_entries: u16,
    pub max_channel_error: u8,
    pub mean_channel_error: f64,
    pub record_bytes: usize,
    pub original_record_bytes: usize,
    mode: PaletteMode,
}

impl EncodeReport {
    /// True when the encoded depth differs from the manifest, or colours were quantized away.
    pub fn is_notable(&self) -> bool {
        !self.lossless
            || self.bpp != self.manifest_bpp
            || self.palette_entries != self.manifest_palette_entries
    }

    pub fn warning(&self) -> String {
        let old = format_bytes(self.original_record_bytes);
        let new = format_bytes(self.record_bytes);
        if self.lossless {
            format!(
                "warning: {}#{} ({}): {} colours in PNG, manifest bpp {} / {} -> bpp {} / {} (lossless); record {old} -> {new}",
                self.bank,
                self.bank_index,
                self.png,
                self.distinct_colors,
                self.manifest_bpp,
                self.manifest_palette_entries,
                self.bpp,
                self.palette_entries,
            )
        } else {
            let hint = if self.mode == PaletteMode::Auto {
                " (--max-bpp to lower)"
            } else {
                " (lower bpp/palette_entries in the manifest)"
            };
            format!(
                "warning: {}#{} ({}): {} colours in PNG -> quantized to {} (bpp {}), max channel error {}/255, mean {:.1}; record {old} -> {new}{hint}",
                self.bank,
                self.bank_index,
                self.png,
                self.distinct_colors,
                self.palette_entries,
                self.bpp,
                self.max_channel_error,
                self.mean_channel_error,
            )
        }
    }
}

pub struct PackSummary {
    pub total: usize,
    pub skipped_unchanged: usize,
    pub patched: usize,
    pub banks: Vec<BankPackStats>,
    /// One report per re-encoded image.
    pub reports: Vec<EncodeReport>,
    /// Inferred palettes that kept every colour of the PNG.
    pub auto_lossless: usize,
    /// Inferred palettes that were quantized down to the cap.
    pub auto_quantized: usize,
}

fn format_bytes(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    }
}

pub fn rgb888_to_rgb565(c: [u8; 3]) -> u16 {
    let r5 = (u16::from(c[0]) * 31 + 127) / 255;
    let g6 = (u16::from(c[1]) * 63 + 127) / 255;
    let b5 = (u16::from(c[2]) * 31 + 127) / 255;
    (r5 << 11) | (g6 << 5) | b5
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

/// Picks `bpp` and a palette for one changed PNG.
///
/// Raw (`bpp` 0) stays raw. [`PaletteMode::Auto`] uses the smallest bpp that holds every colour,
/// growing only up to `max(options.max_bpp, manifest bpp)` clamped to [`MAX_AUTO_BPP`] and to
/// [`MAX_PALETTE_ENTRIES`] colours, and never below the depth the firmware already used.
/// [`PaletteMode::Manifest`] keeps the manifest bpp and quantizes to at most its palette size.
fn select_palette(
    entry: &ManifestImageEntry,
    options: &PackOptions,
    hist: &ColorHistogram,
) -> (u16, Quantized, bool) {
    let distinct = hist.distinct();
    match options.palette {
        PaletteMode::Manifest => {
            let limit = usize::from(entry.palette_entries);
            if distinct <= limit {
                (entry.bpp, lossless(hist), true)
            } else {
                (entry.bpp, median_cut(hist, limit, entry.color_key), false)
            }
        }
        PaletteMode::Auto => {
            let cap_bpp = options.max_bpp.max(entry.bpp).clamp(1, MAX_AUTO_BPP);
            let cap_colors = (1usize << cap_bpp).min(usize::from(MAX_PALETTE_ENTRIES));
            if distinct <= cap_colors {
                let quantized = lossless(hist);
                (needed_bpp(quantized.palette.len()), quantized, true)
            } else {
                let quantized = median_cut(hist, cap_colors, entry.color_key);
                (needed_bpp(quantized.palette.len()), quantized, false)
            }
        }
    }
}

fn record_header(
    bpp: u16,
    width: u16,
    height: u16,
    palette_entries: u16,
    rle_words: u32,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(IMAGE_HEADER_SIZE);
    out.extend_from_slice(&bpp.to_le_bytes());
    out.extend_from_slice(&width.to_le_bytes());
    out.extend_from_slice(&height.to_le_bytes());
    out.extend_from_slice(&palette_entries.to_le_bytes());
    out.extend_from_slice(&rle_words.to_le_bytes());
    out
}

struct ChosenEncoding {
    bpp: u16,
    palette_entries: u16,
    lossless: bool,
    max_channel_error: u8,
    mean_channel_error: f64,
}

fn encode_report(
    entry: &ManifestImageEntry,
    options: &PackOptions,
    hist: &ColorHistogram,
    chosen: ChosenEncoding,
    record_bytes: usize,
) -> EncodeReport {
    EncodeReport {
        bank: entry.bank.clone(),
        bank_index: entry.bank_index,
        png: entry.png.clone(),
        distinct_colors: hist.distinct(),
        bpp: chosen.bpp,
        palette_entries: chosen.palette_entries,
        lossless: chosen.lossless,
        manifest_bpp: entry.bpp,
        manifest_palette_entries: entry.palette_entries,
        max_channel_error: chosen.max_channel_error,
        mean_channel_error: chosen.mean_channel_error,
        record_bytes,
        original_record_bytes: entry.record_size_bytes,
        mode: options.palette,
    }
}

/// Encodes a PNG into a firmware record.
///
/// Dimensions come from the PNG. `bpp` and `palette_entries` come from [`PackOptions`]: inferred
/// from the PNG's colours in [`PaletteMode::Auto`], or taken from the manifest in
/// [`PaletteMode::Manifest`]. A manifest `bpp` of 0 is always written as raw RGB565. The result
/// is the exact record, with no slot padding.
pub fn encode_record(
    entry: &ManifestImageEntry,
    png: &DynamicImage,
    options: &PackOptions,
) -> Result<(Vec<u8>, EncodeReport)> {
    ensure!(
        (1..=MAX_AUTO_BPP).contains(&options.max_bpp),
        "--max-bpp must be 1..={MAX_AUTO_BPP}, got {}",
        options.max_bpp
    );
    validate_entry_format(entry).with_context(|| {
        format!(
            "invalid bpp/palette_entries for index {} ({})",
            entry.index, entry.png
        )
    })?;

    let (width, height, pixels) = extract_source_pixels(png, entry)?;
    let hist = ColorHistogram::from_pixels(&pixels);

    if entry.bpp == 0 {
        let mut out = record_header(0, width, height, 0, 0);
        for color in &pixels {
            out.extend_from_slice(&color.to_le_bytes());
        }
        let report = encode_report(
            entry,
            options,
            &hist,
            ChosenEncoding {
                bpp: 0,
                palette_entries: 0,
                lossless: true,
                max_channel_error: 0,
                mean_channel_error: 0.0,
            },
            out.len(),
        );
        return Ok((out, report));
    }

    let (bpp, quantized, lossless_palette) = select_palette(entry, options, &hist);
    let palette_entries =
        u16::try_from(quantized.palette.len()).context("quantized palette does not fit in u16")?;
    let indices: Vec<u16> = pixels
        .iter()
        .map(|color| quantized.map[usize::from(*color)])
        .collect();
    let rle_words = rle_encode(&indices, bpp)?;
    let mut out = record_header(bpp, width, height, palette_entries, rle_words.len() as u32);
    for color in &quantized.palette {
        out.extend_from_slice(&color.to_le_bytes());
    }
    for word in &rle_words {
        out.extend_from_slice(&word.to_le_bytes());
    }

    let error = channel_error(&hist, &quantized);
    let report = encode_report(
        entry,
        options,
        &hist,
        ChosenEncoding {
            bpp,
            palette_entries,
            lossless: lossless_palette,
            max_channel_error: error.max_channel_error,
            mean_channel_error: error.mean_channel_error,
        },
        out.len(),
    );
    Ok((out, report))
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
    options: &PackOptions,
) -> Result<Vec<EncodeReport>> {
    let mut reports = Vec::new();
    for entry in &manifest.images {
        let Some(png) = changed.get(&entry.index) else {
            continue;
        };
        ensure!(
            entry.record_size_bytes >= IMAGE_HEADER_SIZE,
            "invalid record_size_bytes for index {}",
            entry.index
        );
        let (record, report) = encode_record(entry, png, options)?;
        let padded = fit_in_slot(entry, record)?;
        patch_clean_range(firmware, bina_abs_offset, entry.clean_bina_offset, &padded)
            .with_context(|| format!("failed to patch index {} ({})", entry.index, entry.png))?;
        reports.push(report);
    }
    Ok(reports)
}

struct RebuiltBank {
    encoded: HashMap<usize, Vec<u8>>,
    reencoded: usize,
    dimensions_changed: usize,
    reports: Vec<EncodeReport>,
}

/// Names the heaviest re-encoded records when a rebuilt bank no longer fits its flash region.
fn heavy_records_hint(reports: &[EncodeReport]) -> String {
    let mut heavy: Vec<&EncodeReport> = reports.iter().collect();
    heavy.sort_by_key(|report| std::cmp::Reverse(report.record_bytes));
    let listed = heavy
        .iter()
        .take(5)
        .map(|report| {
            format!(
                "{}#{} {} (bpp {}, {} colours)",
                report.bank,
                report.bank_index,
                format_bytes(report.record_bytes),
                report.bpp,
                report.palette_entries
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!("largest re-encoded records: {listed}. Lower --max-bpp or shrink those PNGs")
}

/// Table manifests: any bank with a changed image is rewritten from scratch (records packed by
/// `bank_index`, index table regenerated). Banks with nothing changed are left byte for byte.
fn rebuild_tables(
    firmware: &mut [u8],
    bina_abs_offset: usize,
    manifest: &Manifest,
    changed: &HashMap<usize, DynamicImage>,
    options: &PackOptions,
) -> Result<(Vec<BankPackStats>, Vec<EncodeReport>)> {
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
        let mut reports = Vec::new();
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
            let (record, report) = encode_record(entry, png, options)?;
            encoded.insert(entry_in_bank.index, record);
            reports.push(report);
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
        let region = build_bank_region(&bank.layout, &record_refs).with_context(|| {
            format!(
                "failed to rebuild the {} bank; {}",
                bank.name(),
                heavy_records_hint(&reports)
            )
        })?;
        patch_clean_range(firmware, bina_abs_offset, bank.region_bina_start(), &region)
            .with_context(|| format!("failed to write the rebuilt {} bank", bank.name()))?;

        let reencoded = encoded.len();
        rebuilt.insert(
            bank.name(),
            RebuiltBank {
                encoded,
                reencoded,
                dimensions_changed,
                reports,
            },
        );
    }

    let stats = verify_rebuilt_banks(firmware, bina_abs_offset, &original_clean, &banks, &rebuilt)?;
    let mut reports = Vec::new();
    for bank in &banks {
        if let Some(job) = rebuilt.get_mut(bank.name()) {
            reports.append(&mut job.reports);
        }
    }
    Ok((stats, reports))
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

pub fn pack(
    firmware_path: &Path,
    out_dir: &Path,
    output_path: &Path,
    options: &PackOptions,
) -> Result<PackSummary> {
    ensure!(
        (1..=MAX_AUTO_BPP).contains(&options.max_bpp),
        "--max-bpp must be 1..={MAX_AUTO_BPP}, got {}",
        options.max_bpp
    );
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

    let (banks, reports) = match manifest.summary.discovery.as_str() {
        "table" => rebuild_tables(&mut firmware, bina_abs_offset, &manifest, &changed, options)?,
        "scan" => (
            Vec::new(),
            patch_in_place(&mut firmware, bina_abs_offset, &manifest, &changed, options)?,
        ),
        other => bail!("manifest discovery mode \"{other}\" is not supported"),
    };
    let (auto_lossless, auto_quantized) = if options.palette == PaletteMode::Auto {
        (
            reports.iter().filter(|r| r.lossless && r.bpp != 0).count(),
            reports.iter().filter(|r| !r.lossless).count(),
        )
    } else {
        (0, 0)
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
        reports,
        auto_lossless,
        auto_quantized,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image_format::{
        COLOR_KEY_RGB565, decode_pixels, read_image_record, rgb565_to_rgb888,
    };

    fn manifest_mode() -> PackOptions {
        PackOptions {
            palette: PaletteMode::Manifest,
            ..PackOptions::default()
        }
    }

    fn auto_mode(max_bpp: u16) -> PackOptions {
        PackOptions {
            palette: PaletteMode::Auto,
            max_bpp,
        }
    }

    /// One pixel per colour, expanded through the same RGB565 mapping the encoder uses.
    fn image_from_colors(colors: &[u16]) -> DynamicImage {
        let mut img = RgbaImage::new(colors.len() as u32, 1);
        for (x, &color) in colors.iter().enumerate() {
            let [r, g, b] = rgb565_to_rgb888(color);
            img.put_pixel(x as u32, 0, Rgba([r, g, b, 255]));
        }
        DynamicImage::ImageRgba8(img)
    }
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
        let e = entry(12, 3000, w as u16, h as u16, None);
        let (record, _) = encode_record(&e, &png, &manifest_mode()).unwrap();
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
        let (record, report) = encode_record(&e, &png, &PackOptions::default()).unwrap();
        assert!(!report.is_notable());
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
        let (record, _) =
            encode_record(&e, &DynamicImage::ImageRgba8(img), &PackOptions::default()).unwrap();
        let rec = read_image_record(&record, 0).unwrap();
        let decoded = decode_pixels(&record, &rec).unwrap();
        assert_eq!(decoded.pixels, vec![COLOR_KEY_RGB565, 0xffff]);
    }

    #[test]
    fn rejects_raw_with_palette_and_bpp_above_14() {
        assert!(validate_entry_format(&entry(0, 3, 1, 1, None)).is_err());
        assert!(validate_entry_format(&entry(15, 1, 1, 1, None)).is_err());
        validate_entry_format(&entry(14, 3000, 1, 1, None)).unwrap();
    }

    #[test]
    fn png_dimensions_override_the_manifest() {
        let png = gradient(5, 3);
        // Manifest still describes the original 9x9 slot; the record follows the PNG.
        let entry = entry(4, 16, 9, 9, None);
        let (record, _) = encode_record(&entry, &png, &PackOptions::default()).unwrap();
        let rec = read_image_record(&record, 0).unwrap();
        assert_eq!(rec.header.width, 3);
        assert_eq!(rec.header.height, 5);
        assert_eq!(record.len(), rec.record_size_bytes());
    }

    #[test]
    fn rejects_png_larger_than_the_screen() {
        let png = DynamicImage::ImageRgba8(RgbaImage::new(801, 1));
        let err =
            encode_record(&entry(4, 16, 1, 1, None), &png, &PackOptions::default()).unwrap_err();
        assert!(err.to_string().contains("800"), "{err}");
    }

    #[test]
    fn too_large_record_is_rejected_for_in_place_slots() {
        let png = gradient(40, 25);
        let mut entry = entry(8, 256, 40, 25, None);
        entry.record_size_bytes = 64;
        let (record, _) = encode_record(&entry, &png, &PackOptions::default()).unwrap();
        let err = fit_in_slot(&entry, record).unwrap_err();
        assert!(err.to_string().contains("too large"), "{err}");
    }

    #[test]
    fn auto_shrinks_bpp_for_two_colour_png() {
        let png = image_from_colors(&[0x0000, 0xFFFF, 0x0000, 0xFFFF]);
        let mut entry = entry(8, 256, 4, 1, None);
        entry.bank = "small".to_owned();
        entry.bank_index = 1;
        entry.record_size_bytes = 486;
        let (record, report) = encode_record(&entry, &png, &PackOptions::default()).unwrap();
        assert_eq!(report.bpp, 1);
        assert_eq!(report.palette_entries, 2);
        assert!(report.lossless);
        assert!(report.is_notable());
        assert!(
            report
                .warning()
                .contains("manifest bpp 8 / 256 -> bpp 1 / 2 (lossless)"),
            "{}",
            report.warning()
        );
        let rec = read_image_record(&record, 0).unwrap();
        assert_eq!((rec.header.bpp, rec.header.palette_entries), (1, 2));
        let decoded = decode_pixels(&record, &rec).unwrap();
        assert_eq!(decoded.pixels, vec![0x0000, 0xFFFF, 0x0000, 0xFFFF]);
    }

    #[test]
    fn auto_grows_bpp_to_fit_all_colours() {
        let colors: Vec<u16> = (0..40).map(|i| i * 100).collect();
        let png = image_from_colors(&colors);
        let entry = entry(1, 2, 40, 1, None);
        let (_, report) = encode_record(&entry, &png, &PackOptions::default()).unwrap();
        assert_eq!(report.distinct_colors, 40);
        assert_eq!(report.bpp, 6);
        assert_eq!(report.palette_entries, 40);
        assert!(report.lossless);
        assert!(report.is_notable());
    }

    #[test]
    fn auto_quantizes_above_cap() {
        let colors: Vec<u16> = (0..300).collect();
        let png = image_from_colors(&colors);
        let entry = entry(1, 2, 300, 1, None);
        let (record, report) = encode_record(&entry, &png, &auto_mode(4)).unwrap();
        assert!(!report.lossless);
        assert!(report.palette_entries <= 16);
        assert!(report.bpp <= 4);
        assert!(report.max_channel_error > 0);
        assert!(report.mean_channel_error > 0.0);
        assert!(report.is_notable());
        assert!(report.warning().contains("--max-bpp to lower"));
        let rec = read_image_record(&record, 0).unwrap();
        assert_eq!(rec.header.bpp, report.bpp);
        assert_eq!(rec.header.palette_entries, report.palette_entries);
    }

    #[test]
    fn cap_never_below_manifest_bpp() {
        let colors: Vec<u16> = (0..300).collect();
        let png = image_from_colors(&colors);
        let entry = entry(8, 256, 300, 1, None);
        let (_, report) = encode_record(&entry, &png, &auto_mode(4)).unwrap();
        assert_eq!(report.bpp, 8);
        assert_eq!(report.palette_entries, 256);
        assert!(!report.lossless);
    }

    #[test]
    fn colour_key_survives_quantization() {
        let mut img = RgbaImage::new(20, 10);
        for y in 0..10 {
            for x in 0..20 {
                if x == 0 {
                    img.put_pixel(x, y, Rgba([255, 0, 0, 0]));
                } else {
                    let [r, g, b] = rgb565_to_rgb888(((y * 20 + x) * 30) as u16);
                    img.put_pixel(x, y, Rgba([r, g, b, 255]));
                }
            }
        }
        let entry = entry(1, 2, 20, 10, Some(COLOR_KEY_RGB565));
        let (record, report) =
            encode_record(&entry, &DynamicImage::ImageRgba8(img), &auto_mode(2)).unwrap();
        assert!(!report.lossless);
        let rec = read_image_record(&record, 0).unwrap();
        let decoded = decode_pixels(&record, &rec).unwrap();
        for y in 0..10u32 {
            for x in 0..20u32 {
                let pixel = decoded.pixels[(y * 20 + x) as usize];
                if x == 0 {
                    assert_eq!(pixel, COLOR_KEY_RGB565);
                } else {
                    assert_ne!(pixel, COLOR_KEY_RGB565);
                }
            }
        }
    }

    #[test]
    fn manifest_mode_keeps_manifest_bpp() {
        let png = image_from_colors(&[0x0000, 0xFFFF]);
        let entry = entry(8, 256, 2, 1, None);
        let (record, report) = encode_record(&entry, &png, &manifest_mode()).unwrap();
        assert_eq!(report.bpp, 8);
        assert_eq!(report.palette_entries, 2);
        assert!(report.lossless);
        assert!(report.is_notable());
        assert!(
            report
                .warning()
                .contains("manifest bpp 8 / 256 -> bpp 8 / 2 (lossless)"),
            "{}",
            report.warning()
        );
        let rec = read_image_record(&record, 0).unwrap();
        assert_eq!((rec.header.bpp, rec.header.palette_entries), (8, 2));
        let decoded = decode_pixels(&record, &rec).unwrap();
        assert_eq!(decoded.pixels, vec![0x0000, 0xFFFF]);
    }

    #[test]
    fn rejects_max_bpp_outside_1_to_12() {
        let png = image_from_colors(&[1]);
        let err = encode_record(&entry(4, 16, 1, 1, None), &png, &auto_mode(13)).unwrap_err();
        assert!(err.to_string().contains("max-bpp"), "{err}");
        let err = encode_record(&entry(4, 16, 1, 1, None), &png, &auto_mode(0)).unwrap_err();
        assert!(err.to_string().contains("max-bpp"), "{err}");
    }
}
