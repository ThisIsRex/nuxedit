//! Palette selection for re-encoded images.
//!
//! A [`ColorHistogram`] counts RGB565 colours. [`lossless`] keeps every colour that occurs.
//! [`median_cut`] folds a histogram down to at most `k` colours, optionally reserving the firmware
//! colour key so transparent pixels stay transparent and opaque pixels never map onto it.

use crate::image_format::rgb565_to_rgb888;

/// How many index bits cover `colors` distinct palette entries.
///
/// `needed_bpp(1) == 1` and `needed_bpp(n) == bits(n - 1)`: the smallest `bpp` with
/// `n <= 2^bpp`. 3000 colours need 12 bits; 2048 fit in 11.
pub fn needed_bpp(colors: usize) -> u16 {
    match colors {
        0 | 1 => 1,
        n => (n - 1).ilog2() as u16 + 1,
    }
}

/// Occurrence count of every RGB565 colour. The table is 65536 entries; only `distinct` are used.
#[derive(Debug, Clone)]
pub struct ColorHistogram {
    counts: Box<[u32; 65536]>,
    distinct: usize,
}

impl ColorHistogram {
    pub fn from_pixels(pixels: &[u16]) -> Self {
        let mut counts: Box<[u32; 65536]> = vec![0u32; 65536]
            .try_into()
            .expect("histogram length is 65536");
        let mut distinct = 0usize;
        for &pixel in pixels {
            let slot = &mut counts[usize::from(pixel)];
            if *slot == 0 {
                distinct += 1;
            }
            *slot += 1;
        }
        Self { counts, distinct }
    }

    pub fn distinct(&self) -> usize {
        self.distinct
    }

    pub fn count(&self, color: u16) -> u32 {
        self.counts[usize::from(color)]
    }

    pub fn iter_nonzero(&self) -> impl Iterator<Item = (u16, u32)> + '_ {
        self.counts
            .iter()
            .enumerate()
            .filter_map(|(color, &count)| {
                (count > 0).then_some((u16::try_from(color).expect("color index fits u16"), count))
            })
    }
}

/// Palette plus a lookup from every RGB565 colour to a palette index.
///
/// `map` is defined for colours present in the source histogram. Other entries are 0.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Quantized {
    pub palette: Vec<u16>,
    pub map: Box<[u16; 65536]>,
}

/// Absolute per-channel error of mapping the histogram through `quantized`, in 8-bit levels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChannelError {
    pub max_channel_error: u8,
    /// Mean of `|dr| + |dg| + |db|` over pixels, divided by 3.
    pub mean_channel_error: f64,
}

pub fn channel_error(hist: &ColorHistogram, quantized: &Quantized) -> ChannelError {
    let mut max_channel_error = 0u8;
    let mut sum = 0u64;
    let mut pixels = 0u64;
    for (color, count) in hist.iter_nonzero() {
        let idx = usize::from(quantized.map[usize::from(color)]);
        let mapped = quantized.palette.get(idx).copied().unwrap_or(color);
        let src = rgb565_to_rgb888(color);
        let dst = rgb565_to_rgb888(mapped);
        for channel in 0..3 {
            let err = src[channel].abs_diff(dst[channel]);
            max_channel_error = max_channel_error.max(err);
            sum += u64::from(err) * u64::from(count);
        }
        pixels += u64::from(count);
    }
    let mean_channel_error = if pixels == 0 {
        0.0
    } else {
        sum as f64 / (pixels as f64 * 3.0)
    };
    ChannelError {
        max_channel_error,
        mean_channel_error,
    }
}

/// Every colour that occurs, most frequent first. Equal counts break toward the smaller colour.
pub fn lossless(hist: &ColorHistogram) -> Quantized {
    let mut ranked: Vec<(u16, u32)> = hist.iter_nonzero().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));

    let mut quantized = Quantized::empty();
    for (color, _) in ranked {
        let idx = u16::try_from(quantized.palette.len()).expect("palette index fits u16");
        quantized.map[usize::from(color)] = idx;
        quantized.palette.push(color);
    }
    quantized
}

/// Median-cut quantization to at most `k` colours.
///
/// The box with the widest 8-bit channel is split at its count-weighted median until there are `k`
/// boxes or every box is a single colour. Each box becomes the count-weighted mean of its colours.
/// When `reserved` occurs in the histogram it takes its own palette slot, and no other colour maps
/// to it; a mean that lands on that key is nudged off it. `k` is clamped to `1..=65536`.
pub fn median_cut(hist: &ColorHistogram, k: usize, reserved: Option<u16>) -> Quantized {
    let k = k.clamp(1, 65536);
    let reserved_color = reserved.filter(|&key| hist.count(key) > 0);

    let mut colors: Vec<(u16, u32)> = hist
        .iter_nonzero()
        .filter(|(color, _)| Some(*color) != reserved_color)
        .collect();
    colors.sort_by_key(|(color, _)| *color);

    let mut quantized = Quantized::empty();
    let rest_slots = if reserved_color.is_some() { k - 1 } else { k };

    if let Some(key) = reserved_color {
        quantized.map[usize::from(key)] = 0;
        quantized.palette.push(key);
    }

    if rest_slots == 0 {
        for (color, _) in colors {
            quantized.map[usize::from(color)] = 0;
        }
        return quantized;
    }
    if colors.is_empty() {
        return quantized;
    }

    let mut boxes = vec![ColorBox::new(colors)];
    while boxes.len() < rest_slots {
        let Some(index) = widest_splittable(&boxes) else {
            break;
        };
        let (left, right) = boxes[index].split();
        boxes[index] = left;
        boxes.push(right);
    }

    for bbox in boxes {
        let mut mean = weighted_mean(&bbox.colors);
        if reserved_color == Some(mean) {
            // One bit off stays a valid RGB565 colour and is never the reserved key.
            mean ^= 1;
        }
        let idx = u16::try_from(quantized.palette.len()).expect("palette index fits u16");
        for &(color, _) in &bbox.colors {
            quantized.map[usize::from(color)] = idx;
        }
        quantized.palette.push(mean);
    }
    quantized
}

impl Quantized {
    fn empty() -> Self {
        Self {
            palette: Vec::new(),
            map: vec![0u16; 65536].try_into().expect("map length is 65536"),
        }
    }
}

struct ColorBox {
    colors: Vec<(u16, u32)>,
    count: u64,
    widest: u8,
}

impl ColorBox {
    fn new(colors: Vec<(u16, u32)>) -> Self {
        let count = colors.iter().map(|(_, n)| u64::from(*n)).sum();
        let widest = axis_ranges(&colors).into_iter().max().unwrap_or(0);
        Self {
            colors,
            count,
            widest,
        }
    }

    fn split(&self) -> (Self, Self) {
        let ranges = axis_ranges(&self.colors);
        let axis = ranges
            .iter()
            .enumerate()
            .max_by_key(|(axis, range)| (*range, std::cmp::Reverse(*axis)))
            .map(|(axis, _)| axis)
            .unwrap_or(0);

        let mut ordered = self.colors.clone();
        ordered.sort_by(|a, b| {
            channel8(a.0)[axis]
                .cmp(&channel8(b.0)[axis])
                .then(a.0.cmp(&b.0))
        });

        let mut accumulated = 0u64;
        let mut split_at = 1usize;
        for (index, &(_, count)) in ordered.iter().enumerate().take(ordered.len() - 1) {
            accumulated += u64::from(count);
            split_at = index + 1;
            if accumulated * 2 >= self.count {
                break;
            }
        }
        let right = ordered.split_off(split_at);
        (Self::new(ordered), Self::new(right))
    }
}

fn widest_splittable(boxes: &[ColorBox]) -> Option<usize> {
    boxes
        .iter()
        .enumerate()
        .filter(|(_, bbox)| bbox.colors.len() >= 2)
        .max_by_key(|(index, bbox)| (bbox.widest, bbox.count, std::cmp::Reverse(*index)))
        .map(|(index, _)| index)
}

fn channel8(color: u16) -> [u8; 3] {
    rgb565_to_rgb888(color)
}

fn axis_ranges(colors: &[(u16, u32)]) -> [u8; 3] {
    let mut min = [255u8; 3];
    let mut max = [0u8; 3];
    for &(color, _) in colors {
        let channel = channel8(color);
        for i in 0..3 {
            min[i] = min[i].min(channel[i]);
            max[i] = max[i].max(channel[i]);
        }
    }
    [max[0] - min[0], max[1] - min[1], max[2] - min[2]]
}

fn weighted_mean(colors: &[(u16, u32)]) -> u16 {
    let mut sum = [0u64; 3];
    let mut total = 0u64;
    for &(color, count) in colors {
        let channel = channel8(color);
        let weight = u64::from(count);
        for i in 0..3 {
            sum[i] += u64::from(channel[i]) * weight;
        }
        total += weight;
    }
    let rgb = [
        ((sum[0] + total / 2) / total) as u8,
        ((sum[1] + total / 2) / total) as u8,
        ((sum[2] + total / 2) / total) as u8,
    ];
    rgb888_to_rgb565(rgb)
}

fn rgb888_to_rgb565(color: [u8; 3]) -> u16 {
    let r5 = (u16::from(color[0]) * 31 + 127) / 255;
    let g6 = (u16::from(color[1]) * 63 + 127) / 255;
    let b5 = (u16::from(color[2]) * 31 + 127) / 255;
    (r5 << 11) | (g6 << 5) | b5
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image_format::COLOR_KEY_RGB565;

    #[test]
    fn needed_bpp_covers_the_colour_count() {
        assert_eq!(needed_bpp(0), 1);
        assert_eq!(needed_bpp(1), 1);
        assert_eq!(needed_bpp(2), 1);
        assert_eq!(needed_bpp(3), 2);
        assert_eq!(needed_bpp(4), 2);
        assert_eq!(needed_bpp(5), 3);
        assert_eq!(needed_bpp(256), 8);
        assert_eq!(needed_bpp(257), 9);
        assert_eq!(needed_bpp(2048), 11);
        assert_eq!(needed_bpp(2049), 12);
        assert_eq!(needed_bpp(3000), 12);
    }

    #[test]
    fn lossless_orders_by_frequency_and_is_exact() {
        let mut pixels = vec![0x001Fu16; 2];
        pixels.extend(std::iter::repeat_n(0xF800, 5));
        pixels.push(0x07E0);
        let hist = ColorHistogram::from_pixels(&pixels);
        assert_eq!(hist.distinct(), 3);

        let quantized = lossless(&hist);
        assert_eq!(quantized.palette, vec![0xF800, 0x001F, 0x07E0]);
        for (color, _) in hist.iter_nonzero() {
            let idx = usize::from(quantized.map[usize::from(color)]);
            assert_eq!(quantized.palette[idx], color);
        }
        let error = channel_error(&hist, &quantized);
        assert_eq!(error.max_channel_error, 0);
        assert_eq!(error.mean_channel_error, 0.0);
    }

    #[test]
    fn lossless_breaks_frequency_ties_by_colour() {
        let hist = ColorHistogram::from_pixels(&[3, 1, 2]);
        assert_eq!(lossless(&hist).palette, vec![1, 2, 3]);
    }

    #[test]
    fn median_cut_keeps_a_single_colour() {
        let hist = ColorHistogram::from_pixels(&[0xABCDu16, 0xABCD, 0xABCD]);
        let quantized = median_cut(&hist, 8, None);
        assert_eq!(quantized.palette, vec![0xABCD]);
        assert_eq!(quantized.map[0xABCD], 0);
    }

    #[test]
    fn median_cut_limits_palette_and_maps_every_colour() {
        let pixels: Vec<u16> = (0..300).collect();
        let hist = ColorHistogram::from_pixels(&pixels);
        let quantized = median_cut(&hist, 16, None);
        assert_eq!(quantized.palette.len(), 16);
        for (color, _) in hist.iter_nonzero() {
            let idx = usize::from(quantized.map[usize::from(color)]);
            assert!(idx < quantized.palette.len());
        }
        let error = channel_error(&hist, &quantized);
        assert!(error.max_channel_error > 0);
        assert!(error.mean_channel_error > 0.0);
    }

    #[test]
    fn median_cut_is_deterministic() {
        let pixels: Vec<u16> = (0..80).map(|i| i * 17 + 3).collect();
        let hist = ColorHistogram::from_pixels(&pixels);
        assert_eq!(
            median_cut(&hist, 7, Some(COLOR_KEY_RGB565)),
            median_cut(&hist, 7, Some(COLOR_KEY_RGB565))
        );
    }

    #[test]
    fn reserved_key_is_a_private_slot() {
        let mut pixels: Vec<u16> = (1..50).map(|i| i * 100).collect();
        pixels.extend(std::iter::repeat_n(COLOR_KEY_RGB565, 10));
        let hist = ColorHistogram::from_pixels(&pixels);
        let quantized = median_cut(&hist, 8, Some(COLOR_KEY_RGB565));

        assert!(quantized.palette.len() <= 8);
        let key_idx = quantized.map[usize::from(COLOR_KEY_RGB565)];
        assert_eq!(quantized.palette[usize::from(key_idx)], COLOR_KEY_RGB565);
        for (color, _) in hist.iter_nonzero() {
            if color != COLOR_KEY_RGB565 {
                assert_ne!(quantized.map[usize::from(color)], key_idx);
            }
        }
    }
}
