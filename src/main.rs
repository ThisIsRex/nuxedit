mod container;
mod depacketize;
mod export;
mod image_format;
mod scanner;

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;

use container::{dump_sections, parse_nux_dfu};
use depacketize::depacketize_bina;
use export::{
    Manifest, ManifestSummary, build_manifest_entry, image_subdir, images_dir,
    png_filename, write_manifest, write_png,
};
use image_format::decode_image;
use scanner::find_image_records;

#[derive(Parser, Debug)]
#[command(name = "mg30_extract")]
#[command(about = "Extract palette+RLE images from NUX MG-30 firmware")]
struct Args {
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

fn parse_hex_usize(s: &str) -> Result<usize, String> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    usize::from_str_radix(s, 16).map_err(|e| e.to_string())
}

fn main() -> Result<()> {
    let args = Args::parse();

    let data = fs::read(&args.firmware)
        .with_context(|| format!("failed to read firmware {}", args.firmware.display()))?;

    let fw = parse_nux_dfu(&data)?;
    fs::create_dir_all(&args.out_dir)?;

    println!("NUX DFU firmware");
    println!("  product: {}", fw.header.product_name);
    println!("  vendor:  {}", fw.header.vendor);
    println!("  version: {} (header byte 0x{:02x})", fw.header.version, fw.header.version_byte);
    println!("  total size: 0x{:08x}", fw.header.total_size);
    println!("  checksum field (0x10): 0x{:08x}", fw.header.unknown_10);
    println!();

    println!("Sections:");
    for section in &fw.sections {
        println!(
            "  {}: offset 0x{:08x}, size 0x{:08x} ({})",
            section.tag,
            section.offset,
            section.size,
            section.size
        );
    }
    println!();

    let sections_dir = args.out_dir.join("sections");
    if args.dump_sections {
        dump_sections(&data, &fw.sections, &sections_dir)?;
    }

    let bina = fw
        .section("BINA")
        .ok_or_else(|| anyhow::anyhow!("BINA section not found"))?;

    let bina_bytes = &data[bina.offset..bina.offset + bina.size];
    let clean_bina = if args.no_depacketize {
        bina_bytes.to_vec()
    } else {
        depacketize_bina(bina_bytes)
    };

    if args.dump_sections {
        let clean_path = sections_dir.join("BINA.clean.bin");
        fs::write(&clean_path, &clean_bina)
            .with_context(|| format!("failed to write {}", clean_path.display()))?;
    }

    println!(
        "BINA: raw size 0x{:x}, clean size 0x{:x}",
        bina_bytes.len(),
        clean_bina.len()
    );

    let records = find_image_records(&clean_bina, args.scan_start, args.min_area);
    println!("Found {} image records", records.len());

    let mut manifest_entries = Vec::with_capacity(records.len());
    let mut small_count = 0usize;
    let mut large_count = 0usize;

    for (index, record) in records.iter().enumerate() {
        let image = decode_image(&clean_bina, record)?;

        let clean_abs = bina.offset + record.offset;
        let original_abs =
            depacketize::clean_bina_rel_to_original_abs(record.offset, bina.offset);
        let filename = png_filename(index, clean_abs, original_abs, record);
        let subdir = image_subdir(record.area(), args.large_threshold);

        if subdir == "large" {
            large_count += 1;
        } else {
            small_count += 1;
        }

        let png_path = images_dir(&args.out_dir, subdir).join(&filename);
        write_png(&png_path, &image)?;

        let png_rel = format!("images/{subdir}/{filename}");
        manifest_entries.push(build_manifest_entry(
            index,
            record,
            bina.offset,
            &png_rel,
        ));
    }

    let manifest = Manifest {
        summary: ManifestSummary {
            firmware_product: fw.header.product_name.clone(),
            firmware_version: fw.header.version.clone(),
            sections: fw.sections.iter().map(|s| s.tag.clone()).collect(),
            clean_bina_size: clean_bina.len(),
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
    println!("  clean BINA size: 0x{:x}", clean_bina.len());
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
