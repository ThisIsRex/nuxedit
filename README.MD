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

**Output layout:**

- `out/sections/` — raw firmware sections (`TEXT.bin`, `BINA.bin`, `EXTR.bin`) and `BINA.clean.bin` (depacketized BINA)
- `out/images/small/` — PNGs below the large threshold
- `out/images/large/` — PNGs at or above the large threshold
- `out/manifest.yaml` — metadata for every extracted image (offsets, dimensions, `bpp`, `palette_entries`, `record_size_bytes`, SHA-512)

**Options:**

| Flag | Default | Description |
|------|---------|-------------|
| `--dump-sections` | on | Dump TEXT/BINA/EXTR sections to `out_dir/sections/` |
| `--no-depacketize` | off | Skip depacketizing the BINA tail frame wrapper |
| `--scan-start <HEX>` | `141f4a0` | BINA-relative scan start offset |
| `--min-area <N>` | `1` | Minimum image area in pixels |
| `--large-threshold <N>` | `50000` | Pixel area threshold for `images/large/` |

Example with custom scan range and large-image threshold:

```bash
mg30_extract images firmware.bin ./out \
  --scan-start 0x141f4a0 \
  --min-area 100 \
  --large-threshold 100000
```

### Extract WAV audio

```bash
mg30_extract waves firmware.bin ./out_waves
```

**Output layout:**

- `out_waves/waves/` — extracted `.wav` files
- `out_waves/waves_manifest.yaml` — metadata for every extracted WAV
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
| `<OUT_DIR>` | Extraction directory with `manifest.yaml` and `images/` |
| `<OUTPUT>` | Path for the patched output firmware |

**How it works:**

1. Reads `manifest.yaml` and checks every PNG against its stored SHA-512 hash.
2. Skips images whose hash matches the manifest (unchanged).
3. Re-encodes changed PNGs into the firmware palette + RLE format and patches them in-place inside the BINA section (frame headers in the BINA tail are preserved automatically).
4. Writes the full firmware to `<OUTPUT>` — all other bytes (header, TEXT, unmodified images, WAV data, EXTR, checksum field) stay as in the original.

**Editing constraints:**

- PNG dimensions must match the manifest (`output_width` × `output_height` in the PNG file; `width` / `height` are the source dimensions in firmware layout).
- Each re-encoded image record **must not exceed** `record_size_bytes` from the manifest. The slot size in firmware is fixed; if the new RLE stream is larger, `pack` fails with a "too large" error.
- If the encoded result is smaller, the remainder of the slot is zero-padded.
- `bpp` and `palette_entries` are taken from the manifest (not inferred from the PNG). They must satisfy `1 <= bpp <= 8` and `1 <= palette_entries <= min(2^bpp, 256)`.

**If the image is too large**, try lowering `bpp` and/or `palette_entries` in `manifest.yaml` for that entry before running `pack`. Fewer palette colors mean a smaller palette block and often a more compact RLE stream, which may fit within `record_size_bytes`. Ensure the new values still cover the colors in your edited PNG (excess colors are quantized to the nearest palette entry).

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
