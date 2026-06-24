use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde::Serialize;

use crate::container::{read_u16_le, read_u32_le};
use crate::depacketize::clean_bina_rel_to_original_abs;

#[derive(Debug, Clone, Copy, Serialize)]
pub struct WavFormat {
    pub audio_format: u16,
    pub channels: u16,
    pub sample_rate: u32,
    pub byte_rate: u32,
    pub block_align: u16,
    pub bits_per_sample: u16,
}

#[derive(Debug, Clone, Serialize)]
pub struct WavRecord {
    pub index: usize,

    pub clean_bina_offset: usize,
    pub clean_bina_offset_hex: String,
    pub clean_abs_offset: usize,
    pub clean_abs_offset_hex: String,
    pub original_abs_offset: usize,
    pub original_abs_offset_hex: String,
    pub end_clean_bina_offset: usize,

    pub wav_size: usize,
    pub riff_size: u32,

    pub fmt_chunk_offset: usize,
    pub data_chunk_offset: usize,
    pub data_size: usize,

    pub audio_format: u16,
    pub channels: u16,
    pub sample_rate: u32,
    pub byte_rate: u32,
    pub block_align: u16,
    pub bits_per_sample: u16,

    pub sample_frames: usize,
    pub duration_seconds: f64,

    pub path: String,
}

#[derive(Debug, Serialize)]
pub struct WaveManifestSummary {
    pub firmware_product: String,
    pub firmware_version: String,
    pub clean_bina_size: usize,
    pub wav_count: usize,
    pub total_duration_seconds: f64,
    pub min_duration_ms: u64,
}

#[derive(Debug, Serialize)]
pub struct WaveManifest {
    pub summary: WaveManifestSummary,
    pub waves: Vec<WavRecord>,
}

fn hex_offset(value: usize) -> String {
    format!("0x{value:08x}")
}

pub fn parse_fmt_chunk(data: &[u8]) -> Result<WavFormat> {
    ensure!(data.len() >= 16, "fmt chunk too small");

    let audio_format = read_u16_le(data, 0)?;
    let channels = read_u16_le(data, 2)?;
    let sample_rate = read_u32_le(data, 4)?;
    let byte_rate = read_u32_le(data, 8)?;
    let block_align = read_u16_le(data, 12)?;
    let bits_per_sample = read_u16_le(data, 14)?;

    ensure!(audio_format == 1, "unsupported audio format: {audio_format}");
    ensure!(
        (1..=2).contains(&channels),
        "invalid channel count: {channels}"
    );
    ensure!(
        (8000..=192_000).contains(&sample_rate),
        "invalid sample rate: {sample_rate}"
    );
    ensure!(
        [8, 16, 24, 32].contains(&bits_per_sample),
        "invalid bits per sample: {bits_per_sample}"
    );

    let expected_block_align = channels as u32 * bits_per_sample as u32 / 8;
    ensure!(
        block_align as u32 == expected_block_align,
        "block_align mismatch: got {block_align}, expected {expected_block_align}"
    );

    let expected_byte_rate = sample_rate * expected_block_align;
    ensure!(
        byte_rate == expected_byte_rate,
        "byte_rate mismatch: got {byte_rate}, expected {expected_byte_rate}"
    );

    Ok(WavFormat {
        audio_format,
        channels,
        sample_rate,
        byte_rate,
        block_align,
        bits_per_sample,
    })
}

pub fn parse_wav_at(
    clean_bina: &[u8],
    offset: usize,
    bina_abs_offset: usize,
) -> Result<Option<WavRecord>> {
    if offset + 12 > clean_bina.len() {
        return Ok(None);
    }

    if &clean_bina[offset..offset + 4] != b"RIFF" {
        return Ok(None);
    }
    if &clean_bina[offset + 8..offset + 12] != b"WAVE" {
        return Ok(None);
    }

    let riff_size = read_u32_le(clean_bina, offset + 4)?;
    let wav_size = riff_size as usize + 8;
    if wav_size < 44 {
        return Ok(None);
    }

    let end_clean_bina_offset = offset + wav_size;
    if end_clean_bina_offset > clean_bina.len() {
        return Ok(None);
    }

    let riff_end = offset + 8 + riff_size as usize;
    let mut pos = offset + 12;

    let mut fmt: Option<WavFormat> = None;
    let mut fmt_chunk_offset = 0usize;
    let mut data_chunk_offset = 0usize;
    let mut data_size = 0usize;

    while pos + 8 <= riff_end {
        let id = &clean_bina[pos..pos + 4];
        let size = read_u32_le(clean_bina, pos + 4)? as usize;
        let data_start = pos + 8;
        let data_end = data_start + size;

        if data_end > riff_end {
            return Ok(None);
        }

        if id == b"fmt " {
            if size < 16 {
                return Ok(None);
            }
            fmt = Some(parse_fmt_chunk(&clean_bina[data_start..data_end])?);
            fmt_chunk_offset = pos;
        } else if id == b"data" {
            data_chunk_offset = pos;
            data_size = size;
        }

        pos = data_end;
        if size % 2 == 1 {
            pos += 1;
        }
    }

    let Some(fmt) = fmt else {
        return Ok(None);
    };
    if data_chunk_offset == 0 || data_size == 0 {
        return Ok(None);
    }

    let bytes_per_frame = fmt.block_align as usize;
    if data_size % bytes_per_frame != 0 {
        return Ok(None);
    }

    let sample_frames = data_size / bytes_per_frame;
    let duration_seconds = sample_frames as f64 / fmt.sample_rate as f64;
    if duration_seconds <= 0.0 {
        return Ok(None);
    }

    let clean_bina_offset = offset;
    let clean_abs_offset = bina_abs_offset + clean_bina_offset;
    let original_abs_offset =
        clean_bina_rel_to_original_abs(clean_bina_offset, bina_abs_offset);

    let duration_ms = (duration_seconds * 1000.0).round() as u64;
    let path = wav_filename(
        0,
        clean_abs_offset,
        original_abs_offset,
        fmt.sample_rate,
        fmt.channels,
        fmt.bits_per_sample,
        duration_ms,
    );

    Ok(Some(WavRecord {
        index: 0,
        clean_bina_offset,
        clean_bina_offset_hex: hex_offset(clean_bina_offset),
        clean_abs_offset,
        clean_abs_offset_hex: hex_offset(clean_abs_offset),
        original_abs_offset,
        original_abs_offset_hex: hex_offset(original_abs_offset),
        end_clean_bina_offset,
        wav_size,
        riff_size,
        fmt_chunk_offset,
        data_chunk_offset,
        data_size,
        audio_format: fmt.audio_format,
        channels: fmt.channels,
        sample_rate: fmt.sample_rate,
        byte_rate: fmt.byte_rate,
        block_align: fmt.block_align,
        bits_per_sample: fmt.bits_per_sample,
        sample_frames,
        duration_seconds,
        path,
    }))
}

pub fn find_waves(
    clean_bina: &[u8],
    bina_abs_offset: usize,
    scan_start: usize,
) -> Result<Vec<WavRecord>> {
    let mut records = Vec::new();
    let mut off = scan_start;

    while off + 12 <= clean_bina.len() {
        if &clean_bina[off..off + 4] == b"RIFF" {
            if let Some(mut rec) = parse_wav_at(clean_bina, off, bina_abs_offset)? {
                rec.index = records.len();
                rec.path = wav_filename(
                    rec.index,
                    rec.clean_abs_offset,
                    rec.original_abs_offset,
                    rec.sample_rate,
                    rec.channels,
                    rec.bits_per_sample,
                    (rec.duration_seconds * 1000.0).round() as u64,
                );
                off = rec.end_clean_bina_offset;
                records.push(rec);
                continue;
            }
        }

        off += 1;
    }

    Ok(records)
}

pub fn wav_filename(
    index: usize,
    clean_abs: usize,
    original_abs: usize,
    sample_rate: u32,
    channels: u16,
    bits_per_sample: u16,
    duration_ms: u64,
) -> String {
    format!(
        "wav_{index:03}_clean_{clean_abs:08x}_orig_{original_abs:08x}_{sample_rate}hz_{channels}ch_{bits_per_sample}bit_{duration_ms}ms.wav"
    )
}

pub fn export_waves(clean_bina: &[u8], records: &[WavRecord], out_dir: &Path) -> Result<()> {
    fs::create_dir_all(out_dir)?;

    for record in records {
        let path = out_dir.join(&record.path);
        let bytes = &clean_bina[record.clean_bina_offset..record.end_clean_bina_offset];
        fs::write(&path, bytes)
            .with_context(|| format!("failed to write WAV {}", path.display()))?;
    }

    Ok(())
}

pub fn write_wave_manifest(path: &Path, manifest: &WaveManifest) -> Result<()> {
    let yaml = serde_yaml::to_string(manifest).context("failed to serialize wave manifest YAML")?;
    fs::write(path, yaml).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

pub fn waves_dir(out_dir: &Path) -> PathBuf {
    out_dir.join("waves")
}

pub fn filter_by_min_duration(records: Vec<WavRecord>, min_duration_ms: u64) -> Vec<WavRecord> {
    let min_seconds = min_duration_ms as f64 / 1000.0;
    let mut filtered: Vec<WavRecord> = records
        .into_iter()
        .filter(|r| r.duration_seconds >= min_seconds)
        .collect();

    for (index, record) in filtered.iter_mut().enumerate() {
        record.index = index;
        record.path = wav_filename(
            index,
            record.clean_abs_offset,
            record.original_abs_offset,
            record.sample_rate,
            record.channels,
            record.bits_per_sample,
            (record.duration_seconds * 1000.0).round() as u64,
        );
    }

    filtered
}
