use anyhow::{Context, Result, ensure};

use image::RgbImage;

use crate::container::{read_u16_le, read_u32_le};

pub const IMAGE_HEADER_SIZE: usize = 12;

#[derive(Debug, Clone, Copy)]
pub struct ImageRecordHeader {
    pub bpp: u16,
    pub width: u16,
    pub height: u16,
    pub palette_entries: u16,
    pub rle_words: u32,
}

#[derive(Debug, Clone)]
pub struct ImageRecord {
    pub offset: usize,
    pub header: ImageRecordHeader,
}

impl ImageRecord {
    pub fn end(&self) -> usize {
        self.offset + self.record_size_bytes()
    }

    pub fn record_size_bytes(&self) -> usize {
        IMAGE_HEADER_SIZE
            + self.header.palette_entries as usize * 2
            + self.header.rle_words as usize * 2
    }

    pub fn area(&self) -> usize {
        self.header.width as usize * self.header.height as usize
    }
}

pub fn rgb565_to_rgb888(c: u16) -> [u8; 3] {
    let r5 = ((c >> 11) & 0x1f) as u32;
    let g6 = ((c >> 5) & 0x3f) as u32;
    let b5 = (c & 0x1f) as u32;

    [
        ((r5 * 255) / 31) as u8,
        ((g6 * 255) / 63) as u8,
        ((b5 * 255) / 31) as u8,
    ]
}

pub fn decode_rle_word(word: u16, bpp: u16) -> (usize, usize) {
    let mask = (1u16 << bpp) - 1;
    let palette_index = (word & mask) as usize;
    let run_length = ((word >> bpp) as usize) + 1;
    (palette_index, run_length)
}

fn parse_header(buf: &[u8], off: usize) -> Result<ImageRecordHeader> {
    Ok(ImageRecordHeader {
        bpp: read_u16_le(buf, off)?,
        width: read_u16_le(buf, off + 2)?,
        height: read_u16_le(buf, off + 4)?,
        palette_entries: read_u16_le(buf, off + 6)?,
        rle_words: read_u32_le(buf, off + 8)?,
    })
}

fn header_bounds_ok(header: &ImageRecordHeader, buf_len: usize, off: usize) -> bool {
    if !(1..=8).contains(&header.bpp) {
        return false;
    }
    if !(1..=800).contains(&header.width) || !(1..=1000).contains(&header.height) {
        return false;
    }

    let max_palette = (1usize << header.bpp).min(256);
    if !(1..=max_palette).contains(&(header.palette_entries as usize)) {
        return false;
    }
    if header.rle_words == 0 || header.rle_words > 200_000 {
        return false;
    }

    let record_end = off
        + IMAGE_HEADER_SIZE
        + header.palette_entries as usize * 2
        + header.rle_words as usize * 2;
    record_end <= buf_len
}

fn validate_rle_stream(buf: &[u8], record: &ImageRecord) -> bool {
    let header = record.header;
    let palette_offset = record.offset + IMAGE_HEADER_SIZE;
    let rle_offset = palette_offset + header.palette_entries as usize * 2;
    let expected_pixels = header.width as u64 * header.height as u64;
    let mask = (1u16 << header.bpp) - 1;

    let mut pixel_total: u64 = 0;

    for i in 0..header.rle_words as usize {
        let word = match read_u16_le(buf, rle_offset + i * 2) {
            Ok(w) => w,
            Err(_) => return false,
        };

        let palette_index = (word & mask) as u64;
        let run_length = ((word >> header.bpp) as u64) + 1;

        if palette_index >= header.palette_entries as u64 {
            return false;
        }

        pixel_total = match pixel_total.checked_add(run_length) {
            Some(v) => v,
            None => return false,
        };

        if pixel_total > expected_pixels {
            return false;
        }
    }

    pixel_total == expected_pixels
}

pub fn validate_image_record(buf: &[u8], off: usize) -> Option<ImageRecord> {
    if off + IMAGE_HEADER_SIZE > buf.len() {
        return None;
    }

    let header = parse_header(buf, off).ok()?;
    if !header_bounds_ok(&header, buf.len(), off) {
        return None;
    }

    let record = ImageRecord { offset: off, header };
    if !validate_rle_stream(buf, &record) {
        return None;
    }

    Some(record)
}

pub fn decode_image(clean_bina: &[u8], rec: &ImageRecord) -> Result<RgbImage> {
    let header = rec.header;
    let palette_offset = rec.offset + IMAGE_HEADER_SIZE;
    let rle_offset = palette_offset + header.palette_entries as usize * 2;

    let mut palette = Vec::with_capacity(header.palette_entries as usize);
    for i in 0..header.palette_entries as usize {
        let c = read_u16_le(clean_bina, palette_offset + i * 2)?;
        palette.push(rgb565_to_rgb888(c));
    }

    let expected_pixels = header.width as usize * header.height as usize;
    let mut rgb = Vec::with_capacity(expected_pixels * 3);

    for i in 0..header.rle_words as usize {
        let word = read_u16_le(clean_bina, rle_offset + i * 2)?;
        let (idx, run) = decode_rle_word(word, header.bpp);
        let color = palette[idx];

        for _ in 0..run {
            rgb.extend_from_slice(&color);
        }
    }

    ensure!(
        rgb.len() == expected_pixels * 3,
        "decoded pixel count mismatch: got {}, expected {}",
        rgb.len() / 3,
        expected_pixels
    );

    RgbImage::from_raw(header.width as u32, header.height as u32, rgb)
        .ok_or_else(|| anyhow::anyhow!("invalid RGB image buffer"))
        .context("failed to build RGB image")
}
