mod container;
mod depacketize;
mod export;
mod hash;
mod image_format;
mod scanner;
mod waves;

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use container::{Firmware, dump_sections, parse_nux_dfu};
use depacketize::depacketize_bina;
use export::{
    Manifest, ManifestSummary, build_manifest_entry, image_subdir, images_dir, png_filename,
    write_manifest, write_png,
};
use hash::sha512_file;
use image_format::decode_image;
use scanner::find_image_records;
use waves::{
    WaveManifest, WaveManifestSummary, export_waves, filter_by_min_duration, find_waves, waves_dir,
    write_wave_manifest,
};

#[derive(Parser, Debug)]
#[command(name = "mg30_extract")]
#[command(about = "Extract resources from NUX MG-30 firmware")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Extract palette+RLE images from BINA
    Images(ImagesArgs),
    /// Extract RIFF/WAVE audio from BINA
    Waves(WavesArgs),
}

#[derive(Parser, Debug)]
struct ImagesArgs {
    /// Path to firmware .bin file
    firmware: PathBuf,

    /// Output directory
    out_dir: PathBuf,

    /// Dump TEXT/BINA/EXTR sections to out_dir/sections/
    #[arg(long, default_value_t = true)]
    dump_sections: bool,

    /// Skip depacketizing the BINA tail frame wrapper
    #[arg(long)]
    no_depacketize: bool,

    /// BINA-relative scan start offset (hex)
    #[arg(long, default_value = "141f4a0", value_parser = parse_hex_usize)]
    scan_start: usize,

    /// Minimum image area in pixels
    #[arg(long, default_value_t = 1)]
    min_area: usize,

    /// Pixel area threshold for images/large/
    #[arg(long, default_value_t = 50_000)]
    large_threshold: usize,
}

#[derive(Parser, Debug)]
struct WavesArgs {
    /// Path to firmware .bin file
    firmware: PathBuf,

    /// Output directory
    out_dir: PathBuf,

    /// BINA-relative scan start offset (hex)
    #[arg(long, default_value = "0", value_parser = parse_hex_usize)]
    scan_start: usize,

    /// Minimum WAV duration in milliseconds
    #[arg(long, default_value_t = 0)]
    min_duration_ms: u64,

    /// Write depacketized BINA to out_dir/sections/BINA.clean.bin
    #[arg(long)]
    dump_clean_bina: bool,
}

struct LoadedBina {
    fw: Firmware,
    data: Vec<u8>,
    clean_bina: Vec<u8>,
    bina_abs_offset: usize,
}

fn parse_hex_usize(s: &str) -> Result<usize, String> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    usize::from_str_radix(s, 16).map_err(|e| e.to_string())
}

fn load_firmware(firmware_path: &Path) -> Result<Vec<u8>> {
    fs::read(firmware_path)
        .with_context(|| format!("failed to read firmware {}", firmware_path.display()))
}

fn load_bina(firmware_path: &Path, depacketize: bool) -> Result<LoadedBina> {
    let data = load_firmware(firmware_path)?;
    let fw = parse_nux_dfu(&data)?;

    let bina = fw
        .section("BINA")
        .ok_or_else(|| anyhow::anyhow!("BINA section not found"))?;

    let bina_abs_offset = bina.offset;
    let bina_bytes = &data[bina.offset..bina.offset + bina.size];
    let clean_bina = if depacketize {
        depacketize_bina(bina_bytes)
    } else {
        bina_bytes.to_vec()
    };

    Ok(LoadedBina {
        fw,
        data,
        clean_bina,
        bina_abs_offset,
    })
}

fn print_firmware_header(fw: &Firmware) {
    println!("NUX DFU firmware");
    println!("  product: {}", fw.header.product_name);
    println!("  vendor:  {}", fw.header.vendor);
    println!(
        "  version: {} (header byte 0x{:02x})",
        fw.header.version, fw.header.version_byte
    );
    println!("  total size: 0x{:08x}", fw.header.total_size);
    println!("  checksum field (0x10): 0x{:08x}", fw.header.unknown_10);
    println!();

    println!("Sections:");
    for section in &fw.sections {
        println!(
            "  {}: offset 0x{:08x}, size 0x{:08x} ({})",
            section.tag, section.offset, section.size, section.size
        );
    }
    println!();
}

fn extract_images(args: ImagesArgs) -> Result<()> {
    let loaded = load_bina(&args.firmware, !args.no_depacketize)?;
    fs::create_dir_all(&args.out_dir)?;

    print_firmware_header(&loaded.fw);

    let sections_dir = args.out_dir.join("sections");
    if args.dump_sections {
        dump_sections(&loaded.data, &loaded.fw.sections, &sections_dir)?;
    }

    if args.dump_sections {
        let clean_path = sections_dir.join("BINA.clean.bin");
        fs::write(&clean_path, &loaded.clean_bina)
            .with_context(|| format!("failed to write {}", clean_path.display()))?;
    }

    let bina_bytes = loaded.fw.section("BINA").map(|b| b.size).unwrap_or(0);

    println!(
        "BINA: raw size 0x{:x}, clean size 0x{:x}",
        bina_bytes,
        loaded.clean_bina.len()
    );

    let records = find_image_records(&loaded.clean_bina, args.scan_start, args.min_area);
    println!("Found {} image records", records.len());

    let mut manifest_entries = Vec::with_capacity(records.len());
    let mut small_count = 0usize;
    let mut large_count = 0usize;

    for (index, record) in records.iter().enumerate() {
        let image = decode_image(&loaded.clean_bina, record)?;

        let clean_abs = loaded.bina_abs_offset + record.offset;
        let original_abs =
            depacketize::clean_bina_rel_to_original_abs(record.offset, loaded.bina_abs_offset);
        let filename = png_filename(index, clean_abs, original_abs, record);
        let subdir = image_subdir(record.area(), args.large_threshold);

        if subdir == "large" {
            large_count += 1;
        } else {
            small_count += 1;
        }

        let png_path = images_dir(&args.out_dir, subdir).join(&filename);
        write_png(&png_path, &image)?;
        let sha512 = sha512_file(&png_path)?;

        let png_rel = format!("images/{subdir}/{filename}");
        manifest_entries.push(build_manifest_entry(
            index,
            record,
            loaded.bina_abs_offset,
            &png_rel,
            sha512,
        ));
    }

    let manifest = Manifest {
        summary: ManifestSummary {
            firmware_product: loaded.fw.header.product_name.clone(),
            firmware_version: loaded.fw.header.version.clone(),
            sections: loaded.fw.sections.iter().map(|s| s.tag.clone()).collect(),
            clean_bina_size: loaded.clean_bina.len(),
            image_count: manifest_entries.len(),
            small_count,
            large_count,
            large_threshold: args.large_threshold,
        },
        images: manifest_entries,
    };

    let manifest_path = args.out_dir.join("manifest.yaml");
    write_manifest(&manifest_path, &manifest)?;

    println!();
    println!("Summary:");
    println!("  sections dumped: {}", args.dump_sections);
    println!("  clean BINA size: 0x{:x}", loaded.clean_bina.len());
    println!("  images total:    {}", manifest.summary.image_count);
    println!("  small:           {}", small_count);
    println!("  large:           {}", large_count);
    println!("  manifest:        {}", manifest_path.display());

    if let Some(first) = manifest.images.first() {
        println!();
        println!("First image:");
        println!(
            "  {}x{}, bpp={}, palette_entries={}",
            first.width, first.height, first.bpp, first.palette_entries
        );
        println!("  clean_abs_offset:    0x{:08x}", first.clean_abs_offset);
        println!("  original_abs_offset: 0x{:08x}", first.original_abs_offset);
        println!("  png: {}", first.png);
    }

    Ok(())
}

fn extract_waves(args: WavesArgs) -> Result<()> {
    let loaded = load_bina(&args.firmware, true)?;
    fs::create_dir_all(&args.out_dir)?;

    print_firmware_header(&loaded.fw);

    if args.dump_clean_bina {
        let sections_dir = args.out_dir.join("sections");
        fs::create_dir_all(&sections_dir)?;
        let clean_path = sections_dir.join("BINA.clean.bin");
        fs::write(&clean_path, &loaded.clean_bina)
            .with_context(|| format!("failed to write {}", clean_path.display()))?;
    }

    let bina_bytes = loaded.fw.section("BINA").map(|b| b.size).unwrap_or(0);

    println!(
        "BINA: raw size 0x{:x}, clean size 0x{:x}",
        bina_bytes,
        loaded.clean_bina.len()
    );

    let mut records = find_waves(&loaded.clean_bina, loaded.bina_abs_offset, args.scan_start)?;
    records = filter_by_min_duration(records, args.min_duration_ms);

    println!("Found {} WAV files", records.len());

    let out_waves_dir = waves_dir(&args.out_dir);
    export_waves(&loaded.clean_bina, &mut records, &out_waves_dir)?;

    let total_duration_seconds: f64 = records.iter().map(|r| r.duration_seconds).sum();

    let manifest = WaveManifest {
        summary: WaveManifestSummary {
            firmware_product: loaded.fw.header.product_name.clone(),
            firmware_version: loaded.fw.header.version.clone(),
            clean_bina_size: loaded.clean_bina.len(),
            wav_count: records.len(),
            total_duration_seconds,
            min_duration_ms: args.min_duration_ms,
        },
        waves: records,
    };

    let manifest_path = args.out_dir.join("waves_manifest.yaml");
    write_wave_manifest(&manifest_path, &manifest)?;

    println!();
    println!("Summary:");
    println!("  clean BINA dumped: {}", args.dump_clean_bina);
    println!("  clean BINA size:   0x{:x}", loaded.clean_bina.len());
    println!("  WAV count:         {}", manifest.summary.wav_count);
    println!(
        "  total duration:    {:.3} s",
        manifest.summary.total_duration_seconds
    );
    println!("  manifest:          {}", manifest_path.display());

    if let Some(first) = manifest.waves.first() {
        println!();
        println!("First WAV:");
        println!(
            "  {} Hz, {} ch, {} bit, {:.6} s",
            first.sample_rate, first.channels, first.bits_per_sample, first.duration_seconds
        );
        println!("  clean_abs_offset:    0x{:08x}", first.clean_abs_offset);
        println!("  original_abs_offset: 0x{:08x}", first.original_abs_offset);
        println!("  path: {}", first.path);
    }

    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Images(args) => extract_images(args),
        Command::Waves(args) => extract_waves(args),
    }
}
