A NUX MG-30 Gen2 firmware resource extractor. Made just for fun, without any guarantees.

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

### Extract images

```bash
mg30_extract images firmware.bin ./out
```

**Output layout:**

- `out/sections/` — raw firmware sections (`TEXT.bin`, `BINA.bin`, `EXTR.bin`) and `BINA.clean.bin` (depacketized BINA)
- `out/images/small/` — PNGs below the large threshold
- `out/images/large/` — PNGs at or above the large threshold
- `out/manifest.yaml` — metadata for every extracted image

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

### Help

```bash
mg30_extract --help
mg30_extract images --help
mg30_extract waves --help
```
