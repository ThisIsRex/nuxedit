//! MG-30 image record format.
//!
//! Every UI image in the external-flash part of the firmware is stored as a record:
//!
//! ```text
//! offset  size  field
//! 0x00    u16   bpp              0 = raw RGB565 pixels, 1..=14 = palette index width in bits
//! 0x02    u16   width            pixels along the framebuffer row = screen *vertical* extent
//! 0x04    u16   height           number of framebuffer rows      = screen *horizontal* extent
//! 0x06    u16   palette_entries  RGB565 palette size (0 for raw images)
//! 0x08    u32   rle_words        number of u16 RLE words (ignored for raw images)
//! 0x0c    u16[palette_entries]   palette
//! ...     u16[rle_words]         RLE stream, or u16[width*height] raw pixels when bpp == 0
//! ```
//!
//! An RLE word packs `palette_index = word & ((1 << bpp) - 1)` and
//! `run_length = (word >> bpp) + 1`. The firmware blitter (`gfx_blit_opaque`) takes the mask from a
//! 15-entry table (`bpp` 0..=14), and copies the palette into a 6000-byte DTCM buffer, so a palette
//! holds at most [`MAX_PALETTE_ENTRIES`] colours and never more than `1 << bpp` (the firmware ships
//! images with bpp 9..=12 and up to 2611 colours).
//!
//! The pixel stream runs along framebuffer rows. The LCD framebuffer is 480x800 portrait and the
//! panel is mounted landscape, so one framebuffer row is one screen *column*: the stream describes
//! the image column by column (top to bottom, then left to right). The decoder transposes it into
//! an ordinary row-major `height x width` (horizontal x vertical) image.
//!
//! Images blitted through the "small" bank blitter (`FUN_0000ff58`) treat palette colour
//! [`COLOR_KEY_RGB565`] as transparent; everything else is drawn opaque.

use anyhow::{Context, Result, bail, ensure};
use image::{RgbImage, RgbaImage};

use crate::container::{read_u16_le, read_u32_le};

pub const IMAGE_HEADER_SIZE: usize = 12;

/// Largest palette index width supported by the firmware mask table at code address 0x101aa.
pub const MAX_BPP: u16 = 14;

/// Largest palette the firmware blitters can load.
///
/// `gfx_blit_opaque` and `gfx_blit_colorkey` memcpy `palette_entries * 2` bytes into the DTCM
/// buffer at `0x2001d8dc` (`g_blit_palette`) with no bounds check. That buffer is 6000 bytes —
/// the next live variable sits at `0x2001f04c` — so a palette holds at most 3000 RGB565 colours.
/// The stock MG-30 image never exceeds 2611.
pub const MAX_PALETTE_ENTRIES: u16 = 3000;

/// RGB565 magenta (255, 28, 255 in RGB888) that the small-bank blitter skips.
pub const COLOR_KEY_RGB565: u16 = 0xF8FF;

/// Framebuffer dimensions: 800 rows (screen width) x 480 pixels per row (screen height).
pub const SCREEN_WIDTH: u16 = 800;
pub const SCREEN_HEIGHT: u16 = 480;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageRecordHeader {
    pub bpp: u16,
    pub width: u16,
    pub height: u16,
    pub palette_entries: u16,
    pub rle_words: u32,
}

impl ImageRecordHeader {
    pub fn parse(buf: &[u8], off: usize) -> Result<Self> {
        Ok(ImageRecordHeader {
            bpp: read_u16_le(buf, off)?,
            width: read_u16_le(buf, off + 2)?,
            height: read_u16_le(buf, off + 4)?,
            palette_entries: read_u16_le(buf, off + 6)?,
            rle_words: read_u32_le(buf, off + 8)?,
        })
    }

    pub fn is_raw(&self) -> bool {
        self.bpp == 0
    }

    pub fn pixel_count(&self) -> usize {
        self.width as usize * self.height as usize
    }

    pub fn palette_size_bytes(&self) -> usize {
        self.palette_entries as usize * 2
    }

    /// Size of the pixel payload that follows the palette.
    pub fn pixel_data_size_bytes(&self) -> usize {
        if self.is_raw() {
            self.pixel_count() * 2
        } else {
            self.rle_words as usize * 2
        }
    }

    pub fn record_size_bytes(&self) -> usize {
        IMAGE_HEADER_SIZE + self.palette_size_bytes() + self.pixel_data_size_bytes()
    }

    /// Structural validation mirroring what the firmware blitter can actually consume.
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.bpp <= MAX_BPP,
            "bpp must be 0..={MAX_BPP}, got {}",
            self.bpp
        );
        ensure!(
            self.width > 0 && self.height > 0,
            "image dimensions must be non-zero, got {}x{}",
            self.width,
            self.height
        );
        if self.is_raw() {
            return Ok(());
        }
        validate_bpp_and_palette(self.bpp, self.palette_entries)?;
        let pixels = self.pixel_count() as u64;
        ensure!(
            self.rle_words > 0 && u64::from(self.rle_words) <= pixels,
            "rle_words must be between 1 and width*height ({pixels}), got {}",
            self.rle_words
        );
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageRecord {
    /// Offset of the header inside the buffer the record was read from (clean BINA).
    pub offset: usize,
    pub header: ImageRecordHeader,
}

impl ImageRecord {
    pub fn end(&self) -> usize {
        self.offset + self.record_size_bytes()
    }

    pub fn record_size_bytes(&self) -> usize {
        self.header.record_size_bytes()
    }

    pub fn area(&self) -> usize {
        self.header.pixel_count()
    }

    /// Dimensions of the decoded (screen-oriented) image: `(horizontal, vertical)`.
    pub fn output_dimensions(&self) -> (u16, u16) {
        (self.header.height, self.header.width)
    }

    fn palette_offset(&self) -> usize {
        self.offset + IMAGE_HEADER_SIZE
    }

    fn pixel_data_offset(&self) -> usize {
        self.palette_offset() + self.header.palette_size_bytes()
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
    let mask = (1u32 << bpp) - 1;
    let palette_index = (u32::from(word) & mask) as usize;
    let run_length = ((word >> bpp) as usize) + 1;
    (palette_index, run_length)
}

/// Maximum palette size allowed for a given `bpp`: `min(2^bpp, MAX_PALETTE_ENTRIES)`.
pub fn max_palette_entries(bpp: u16) -> usize {
    if bpp == 0 || bpp > MAX_BPP {
        0
    } else {
        (1usize << bpp).min(usize::from(MAX_PALETTE_ENTRIES))
    }
}

/// Validates `bpp`/`palette_entries` for a palette+RLE image (`bpp` 1..=14).
pub fn validate_bpp_and_palette(bpp: u16, palette_entries: u16) -> Result<()> {
    ensure!(
        (1..=MAX_BPP).contains(&bpp),
        "bpp must be between 1 and {MAX_BPP}, got {bpp}"
    );

    let max_palette = max_palette_entries(bpp);
    let entries = palette_entries as usize;
    ensure!(
        (1..=max_palette).contains(&entries),
        "palette_entries must be between 1 and {max_palette} for bpp={bpp} (at most 2^bpp and {MAX_PALETTE_ENTRIES}, the firmware palette buffer), got {palette_entries}"
    );

    Ok(())
}

/// Reads a record at a known position (e.g. from a bank index table) and validates it
/// structurally. Does not apply any heuristics on dimensions.
pub fn read_image_record(buf: &[u8], off: usize) -> Result<ImageRecord> {
    let header = ImageRecordHeader::parse(buf, off)
        .with_context(|| format!("image header at 0x{off:x} is out of bounds"))?;
    header
        .validate()
        .with_context(|| format!("invalid image header at 0x{off:x}"))?;

    let record = ImageRecord {
        offset: off,
        header,
    };
    ensure!(
        record.end() <= buf.len(),
        "image record at 0x{off:x} ({} bytes) exceeds buffer",
        record.record_size_bytes()
    );
    Ok(record)
}

/// Heuristic bounds used when scanning a buffer for records without an index table.
fn header_plausible_for_scan(header: &ImageRecordHeader) -> bool {
    if header.validate().is_err() {
        return false;
    }
    // Raw images are indistinguishable from random data without an index table.
    if header.is_raw() {
        return false;
    }
    header.width <= SCREEN_HEIGHT && header.height <= SCREEN_WIDTH
}

fn validate_rle_stream(buf: &[u8], record: &ImageRecord) -> bool {
    let header = record.header;
    let rle_offset = record.pixel_data_offset();
    let expected_pixels = header.pixel_count() as u64;

    let mut pixel_total: u64 = 0;

    for i in 0..header.rle_words as usize {
        let word = match read_u16_le(buf, rle_offset + i * 2) {
            Ok(w) => w,
            Err(_) => return false,
        };

        let (palette_index, run_length) = decode_rle_word(word, header.bpp);

        if palette_index >= header.palette_entries as usize {
            return false;
        }

        pixel_total += run_length as u64;
        if pixel_total > expected_pixels {
            return false;
        }
    }

    pixel_total == expected_pixels
}

/// Scan-mode validation: structural checks plus plausibility heuristics and a full RLE pass.
pub fn validate_image_record(buf: &[u8], off: usize) -> Option<ImageRecord> {
    if off + IMAGE_HEADER_SIZE > buf.len() {
        return None;
    }

    let header = ImageRecordHeader::parse(buf, off).ok()?;
    if !header_plausible_for_scan(&header) {
        return None;
    }

    let record = ImageRecord {
        offset: off,
        header,
    };
    if record.end() > buf.len() || !validate_rle_stream(buf, &record) {
        return None;
    }

    Some(record)
}

/// Decoded image in screen orientation: row-major, `width` horizontal x `height` vertical.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedImage {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u16>,
}

impl DecodedImage {
    pub fn to_rgb_image(&self) -> Result<RgbImage> {
        let mut rgb = Vec::with_capacity(self.pixels.len() * 3);
        for &c in &self.pixels {
            rgb.extend_from_slice(&rgb565_to_rgb888(c));
        }
        RgbImage::from_raw(self.width, self.height, rgb)
            .ok_or_else(|| anyhow::anyhow!("invalid RGB image buffer"))
    }

    /// RGBA image where pixels equal to `color_key` get alpha 0 (colour is preserved so that
    /// re-encoding is lossless).
    pub fn to_rgba_image(&self, color_key: u16) -> Result<RgbaImage> {
        let mut rgba = Vec::with_capacity(self.pixels.len() * 4);
        for &c in &self.pixels {
            rgba.extend_from_slice(&rgb565_to_rgb888(c));
            rgba.push(if c == color_key { 0 } else { 255 });
        }
        RgbaImage::from_raw(self.width, self.height, rgba)
            .ok_or_else(|| anyhow::anyhow!("invalid RGBA image buffer"))
    }
}

/// Decodes a record into RGB565 pixels in screen orientation.
pub fn decode_pixels(buf: &[u8], rec: &ImageRecord) -> Result<DecodedImage> {
    let header = rec.header;
    header
        .validate()
        .with_context(|| format!("invalid image header at 0x{:x}", rec.offset))?;
    ensure!(
        rec.end() <= buf.len(),
        "image record at 0x{:x} exceeds buffer",
        rec.offset
    );

    // Source stream geometry: `src_w` pixels per framebuffer row (vertical on screen),
    // `src_h` rows (horizontal on screen).
    let src_w = header.width as usize;
    let src_h = header.height as usize;
    let out_w = src_h;
    let out_h = src_w;
    let expected_pixels = header.pixel_count();

    let data_offset = rec.pixel_data_offset();
    let mut pixels = vec![0u16; expected_pixels];

    if header.is_raw() {
        for i in 0..expected_pixels {
            let c = read_u16_le(buf, data_offset + i * 2)?;
            let (x, y) = (i % src_w, i / src_w);
            pixels[x * out_w + y] = c;
        }
        return Ok(DecodedImage {
            width: out_w as u32,
            height: out_h as u32,
            pixels,
        });
    }

    let palette_offset = rec.palette_offset();
    let mut palette = Vec::with_capacity(header.palette_entries as usize);
    for i in 0..header.palette_entries as usize {
        palette.push(read_u16_le(buf, palette_offset + i * 2)?);
    }

    let mut src_idx = 0usize;
    for i in 0..header.rle_words as usize {
        let word = read_u16_le(buf, data_offset + i * 2)?;
        let (idx, run) = decode_rle_word(word, header.bpp);
        let color = *palette.get(idx).with_context(|| {
            format!(
                "RLE word {i} references palette index {idx} but the palette has {} entries (record at 0x{:x})",
                header.palette_entries, rec.offset
            )
        })?;

        ensure!(
            src_idx + run <= expected_pixels,
            "RLE stream produces more pixels than width*height ({expected_pixels}) in record at 0x{:x}",
            rec.offset
        );

        for _ in 0..run {
            // Stream order is column-major on screen; transpose into row-major.
            let (x, y) = (src_idx % src_w, src_idx / src_w);
            pixels[x * out_w + y] = color;
            src_idx += 1;
        }
    }

    if src_idx != expected_pixels {
        bail!(
            "decoded pixel count mismatch in record at 0x{:x}: got {src_idx}, expected {expected_pixels}",
            rec.offset
        );
    }

    Ok(DecodedImage {
        width: out_w as u32,
        height: out_h as u32,
        pixels,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Builds a palette+RLE record from screen-oriented pixels (`out_w` x `out_h`).
    pub(crate) fn build_record(
        bpp: u16,
        out_w: usize,
        out_h: usize,
        palette: &[u16],
        pixels: &[u16],
    ) -> Vec<u8> {
        assert_eq!(pixels.len(), out_w * out_h);
        let src_w = out_h;
        let src_h = out_w;
        let mut rec = Vec::new();
        let mut words: Vec<u16> = Vec::new();
        if bpp == 0 {
            // raw: stream is column-major
            for y in 0..src_h {
                for x in 0..src_w {
                    words.push(pixels[x * out_w + y]);
                }
            }
        } else {
            let mut indices = Vec::new();
            for y in 0..src_h {
                for x in 0..src_w {
                    let c = pixels[x * out_w + y];
                    indices.push(palette.iter().position(|&p| p == c).unwrap() as u16);
                }
            }
            let max_run = 1usize << (16 - bpp);
            let mut i = 0;
            while i < indices.len() {
                let mut run = 1;
                while i + run < indices.len() && indices[i + run] == indices[i] && run < max_run {
                    run += 1;
                }
                words.push((((run - 1) as u16) << bpp) | indices[i]);
                i += run;
            }
        }
        rec.extend_from_slice(&bpp.to_le_bytes());
        rec.extend_from_slice(&(src_w as u16).to_le_bytes());
        rec.extend_from_slice(&(src_h as u16).to_le_bytes());
        rec.extend_from_slice(&(palette.len() as u16).to_le_bytes());
        let rle_words = if bpp == 0 { 0 } else { words.len() as u32 };
        rec.extend_from_slice(&rle_words.to_le_bytes());
        for &c in palette {
            rec.extend_from_slice(&c.to_le_bytes());
        }
        for &w in &words {
            rec.extend_from_slice(&w.to_le_bytes());
        }
        rec
    }

    #[test]
    fn validate_bpp_and_palette_accepts_known_values() {
        validate_bpp_and_palette(1, 1).unwrap();
        validate_bpp_and_palette(4, 14).unwrap();
        validate_bpp_and_palette(8, 256).unwrap();
        validate_bpp_and_palette(12, 3000).unwrap();
        validate_bpp_and_palette(14, 3000).unwrap();
    }

    #[test]
    fn validate_bpp_and_palette_rejects_invalid_bpp() {
        assert!(validate_bpp_and_palette(0, 1).is_err());
        assert!(validate_bpp_and_palette(15, 1).is_err());
    }

    #[test]
    fn validate_bpp_and_palette_rejects_too_many_entries() {
        assert!(validate_bpp_and_palette(4, 17).is_err());
        assert!(validate_bpp_and_palette(8, 257).is_err());
        assert!(validate_bpp_and_palette(12, 3001).is_err());
        assert!(validate_bpp_and_palette(14, 16384).is_err());
    }

    #[test]
    fn header_validate_bounds_rle_words_by_pixel_count() {
        let mut h = ImageRecordHeader {
            bpp: 4,
            width: 3,
            height: 2,
            palette_entries: 2,
            rle_words: 6,
        };
        h.validate().unwrap();
        h.rle_words = 7;
        assert!(h.validate().is_err());
        h.rle_words = 0;
        assert!(h.validate().is_err());
    }

    #[test]
    fn decodes_and_transposes_rle_image() {
        // 3 wide x 2 tall on screen.
        let palette = [0x0000, 0xffff, 0xf800];
        let pixels = [0x0000, 0xffff, 0xf800, 0xf800, 0xffff, 0x0000];
        let rec = build_record(2, 3, 2, &palette, &pixels);
        let record = read_image_record(&rec, 0).unwrap();
        assert_eq!(record.header.width, 2);
        assert_eq!(record.header.height, 3);
        assert_eq!(record.record_size_bytes(), rec.len());
        assert_eq!(record.output_dimensions(), (3, 2));

        let decoded = decode_pixels(&rec, &record).unwrap();
        assert_eq!((decoded.width, decoded.height), (3, 2));
        assert_eq!(decoded.pixels, pixels);

        // Scan-mode validation agrees.
        assert_eq!(validate_image_record(&rec, 0), Some(record));
    }

    #[test]
    fn decodes_high_bpp_image_with_large_palette() {
        let palette: Vec<u16> = (0..600u16).collect();
        let pixels: Vec<u16> = (0..600u16).collect();
        let rec = build_record(10, 20, 30, &palette, &pixels);
        let record = read_image_record(&rec, 0).unwrap();
        assert_eq!(record.header.palette_entries, 600);
        let decoded = decode_pixels(&rec, &record).unwrap();
        assert_eq!(decoded.pixels, pixels);
    }

    #[test]
    fn decodes_raw_image() {
        let pixels = [0x1234, 0x5678, 0x9abc, 0xdef0];
        let rec = build_record(0, 2, 2, &[], &pixels);
        let record = read_image_record(&rec, 0).unwrap();
        assert!(record.header.is_raw());
        assert_eq!(record.record_size_bytes(), IMAGE_HEADER_SIZE + 8);
        let decoded = decode_pixels(&rec, &record).unwrap();
        assert_eq!(decoded.pixels, pixels);
        // Raw images are not accepted by the heuristic scanner.
        assert!(validate_image_record(&rec, 0).is_none());
    }

    #[test]
    fn palette_index_out_of_range_is_an_error_not_a_panic() {
        let palette = [0x0000];
        let pixels = [0x0000, 0x0000];
        let mut rec = build_record(1, 2, 1, &palette, &pixels);
        // Patch the single RLE word to reference palette index 1.
        let rle_off = IMAGE_HEADER_SIZE + 2;
        rec[rle_off] |= 0x01;
        let record = read_image_record(&rec, 0).unwrap();
        let err = decode_pixels(&rec, &record).unwrap_err();
        assert!(err.to_string().contains("palette index 1"), "{err}");
        assert!(validate_image_record(&rec, 0).is_none());
    }

    #[test]
    fn color_key_becomes_transparent_in_rgba() {
        let palette = [COLOR_KEY_RGB565, 0xffff];
        let pixels = [COLOR_KEY_RGB565, 0xffff];
        let rec = build_record(1, 2, 1, &palette, &pixels);
        let record = read_image_record(&rec, 0).unwrap();
        let decoded = decode_pixels(&rec, &record).unwrap();
        assert!(decoded.pixels.contains(&COLOR_KEY_RGB565));
        let rgba = decoded.to_rgba_image(COLOR_KEY_RGB565).unwrap();
        assert_eq!(rgba.get_pixel(0, 0).0, [255, 28, 255, 0]);
        assert_eq!(rgba.get_pixel(1, 0).0, [255, 255, 255, 255]);
    }
}
