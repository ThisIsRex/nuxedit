A NUX MG-30 Gen2 firmware resource extractor and editor. Extract images and audio from firmware, edit PNGs, and repack them back — any image in the bundle can be replaced, including the boot screen. Made just for fun, without any guarantees. The author is not responsible for damaged devices — flash modified firmware at your own risk.

## Usage

Build and run with Cargo:

```bash
cargo build --release
cargo run --release -- <COMMAND> [OPTIONS] <FIRMWARE> <OUT_DIR>
```

The binary is named `mg30_extract`. It reads a NUX DFU firmware `.bin` file and writes extracted resources to `<OUT_DIR>`.

### Commands

| Command | Description |
|---------|-------------|
| `images` | Extract palette + RLE encoded images from the BINA section as PNG files |
| `waves` | Extract embedded RIFF/WAVE audio from the BINA section as `.wav` files |
| `pack` | Repack edited PNGs from an extraction directory back into a new firmware file |

### Extract images

```bash
mg30_extract images firmware.bin ./out
```

Images are located through the firmware's own **bank index tables** (see [Image banks](#image-banks)), so every
extracted file carries the exact ID the firmware uses to draw it.

**Output layout:**

- `out/sections/` — raw firmware sections (`TEXT.bin`, `BINA.bin`, `EXTR.bin`) and `BINA.clean.bin` (depacketized BINA)
- `out/images/font/` — glyph bank (opaque RGB PNGs)
- `out/images/small/` — small bank: knobs, switches, icons (RGBA PNGs; the firmware colour key is exported as alpha 0)
- `out/images/large/` — large bank: strips, panels, photos, full screens (opaque RGB PNGs)
- `out/manifest.ron` — metadata for every extracted image (bank + table index, offsets, dimensions, `bpp`,
  `palette_entries`, `record_size_bytes`, `color_key`, SHA-512) and a per-bank summary (table address, used/free bytes)

File names are `<bank>_<table index>_<w>x<h>_bpp<N>_pal<M>.png`, e.g. `small_0056_84x84_bpp6_pal36.png`.

**Options:**

| Flag | Default | Description |
|------|---------|-------------|
| `--dump-sections <BOOL>` | `true` | Dump TEXT/BINA/EXTR sections to `out_dir/sections/` |
| `--no-depacketize` | off | Skip depacketizing the BINA tail frame wrapper |
| `--scan` | off | Ignore the bank tables and scan the whole BINA for records (fallback for unknown layouts) |
| `--scan-start <HEX>` | `141f4a0` | BINA-relative scan start offset (scan mode only) |
| `--min-area <N>` | `1` | Minimum image area in pixels (scan mode only) |
| `--large-threshold <N>` | `50000` | Pixel area threshold for `images/large/` (scan mode only) |

If the bank tables cannot be read (different firmware layout), the tool prints a warning and falls back to scan mode
automatically. In scan mode images are named `scan_<n>_...` and split into `images/small/` / `images/large/` by area;
raw (bpp 0) images cannot be found this way.

### Image banks

The depacketized BINA tail is a byte-exact image of external flash starting at flash offset `0x40000`
(memory-mapped at `0x60040000`). It holds three resource banks, each an index table followed by packed records:

| Bank | Table (ext-image offset / flash) | Entry | Records | Max entries | Transparency |
|------|----------------------------------|-------|---------|-------------|--------------|
| `font` | `0x480000` / `0x604C0000` | `u32 height, u32 width, u32 offset` | `0x488000` | 2048 | none |
| `small` | `0x580000` / `0x605C0000` | `u32 offset` | `0x588000` | 2800 | colour key `0xF8FF` |
| `large` | `0xD00000` / `0x60D40000` | `u32 offset` | `0xD08000` | 2048 | none |

Offsets are relative to the table base. Record format:

```text
u16 bpp              0 = raw RGB565 pixels, 1..=14 = palette index width
u16 width            pixels along a framebuffer row (vertical on screen)
u16 height           framebuffer rows (horizontal on screen)
u16 palette_entries  up to 2^bpp RGB565 colours
u32 rle_words        RLE word count (ignored for raw images)
u16 palette[palette_entries]
u16 rle[rle_words]   word = run-1 << bpp | palette_index   (or u16 pixels[width*height] for raw)
```

The LCD framebuffer is portrait (480x800) while the panel is landscape, so the stream describes the image column by
column; PNGs are written in normal screen orientation (`output_width` x `output_height` = `height` x `width`).

### Extract WAV audio

```bash
mg30_extract waves firmware.bin ./out_waves
```

**Output layout:**

- `out_waves/waves/` — extracted `.wav` files
- `out_waves/waves_manifest.ron` — metadata for every extracted WAV
- `out_waves/sections/BINA.clean.bin` — only when `--dump-clean-bina` is set

**Options:**

| Flag | Default | Description |
|------|---------|-------------|
| `--scan-start <HEX>` | `0` | BINA-relative scan start offset |
| `--min-duration-ms <N>` | `0` | Skip WAVs shorter than this duration |
| `--dump-clean-bina` | off | Write depacketized BINA to `out_dir/sections/BINA.clean.bin` |

Example filtering out very short clips:

```bash
mg30_extract waves firmware.bin ./out_waves \
  --min-duration-ms 50 \
  --dump-clean-bina
```

### Repack edited images

After extracting images, edit PNGs in `out/images/` and write them back into a **new** firmware file. The original `.bin` is not modified.

```bash
mg30_extract pack firmware.bin ./out ./firmware/patched.bin
```

**Arguments:**

| Argument | Description |
|----------|-------------|
| `<FIRMWARE>` | Original firmware `.bin` used as the base |
| `<OUT_DIR>` | Extraction directory with `manifest.ron` and `images/` |
| `<OUTPUT>` | Path for the patched output firmware |

**How it works:**

1. Reads `manifest.ron` and checks every PNG against its stored SHA-512 hash.
2. Skips images whose hash matches the manifest (unchanged).
3. Re-encodes changed PNGs into the firmware palette + RLE format and patches them in-place inside the BINA section (frame headers in the BINA tail are preserved automatically).
4. Writes the full firmware to `<OUTPUT>` — all other bytes (header, TEXT, unmodified images, WAV data, EXTR, checksum field) stay as in the original.

**Editing constraints:**

- PNG dimensions must match the manifest (`output_width` × `output_height` in the PNG file; `width` / `height` are the source dimensions in firmware layout).
- Each re-encoded image record **must not exceed** `record_size_bytes` from the manifest. The slot size in firmware is fixed; if the new RLE stream is larger, `pack` fails with a "too large" error.
- If the encoded result is smaller, the remainder of the slot is zero-padded.
- `bpp` and `palette_entries` are taken from the manifest (not inferred from the PNG). They must satisfy `1 <= bpp <= 14` and `1 <= palette_entries <= 2^bpp`, or `bpp = 0` with `palette_entries = 0` for a raw RGB565 image (`12 + 2 * width * height` bytes).
- For entries with a `color_key` (the `small` bank) the PNG is RGBA: pixels with alpha below 128 are written as the colour key and will be transparent on the device. Fully opaque magenta `(255, 28, 255)` is also the key colour and will be transparent too.

**If the image is too large**, try lowering `bpp` and/or `palette_entries` in `manifest.ron` for that entry before running `pack`. Fewer palette colors mean a smaller palette block and often a more compact RLE stream, which may fit within `record_size_bytes`. Ensure the new values still cover the colors in your edited PNG (excess colors are quantized to the nearest palette entry).

```ron
ManifestImageEntry(
    index: 12,
    bank: "large",
    bank_index: 12,
    png: "images/large/large_0012_....png",
    bpp: 4,
    palette_entries: 8,
    // other fields stay as extracted
)
```

To trigger repacking for an entry, the on-disk PNG hash must differ from `sha512` in the manifest — edit the PNG file, or update the hash after you are done editing.

Example workflow:

```bash
# 1. Extract
mg30_extract images work_notes/data/firmware/MG-30_EN_GEN2_V1.0.2.bin work_notes/data/out

# 2. Edit PNGs in work_notes/data/out/images/

# 3. Repack
mg30_extract pack \
  work_notes/data/firmware/MG-30_EN_GEN2_V1.0.2.bin \
  work_notes/data/out \
  work_notes/data/firmware/patched.bin

# 4. Verify (optional)
mg30_extract images work_notes/data/firmware/patched.bin work_notes/data/out_check
```

### Help

```bash
mg30_extract --help
mg30_extract images --help
mg30_extract waves --help
mg30_extract pack --help
```
