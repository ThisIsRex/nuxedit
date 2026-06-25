use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};

pub const SECTION_TABLE_OFFSET: usize = 0x224;
pub const SECTION_ENTRY_SIZE: usize = 12;

#[derive(Debug, Clone)]
pub struct NuxDfuHeader {
    pub magic: [u8; 7],
    pub version_byte: u8,
    pub total_size: u32,
    pub unknown_0c: u32,
    pub unknown_10: u32,
    pub product_name: String,
    pub vendor: String,
    pub version: String,
}

#[derive(Debug, Clone)]
pub struct SectionEntry {
    pub tag: String,
    pub size: usize,
    pub offset: usize,
}

#[derive(Debug, Clone)]
pub struct Firmware {
    pub header: NuxDfuHeader,
    pub sections: Vec<SectionEntry>,
}

pub fn read_u16_le(buf: &[u8], off: usize) -> Result<u16> {
    let bytes: [u8; 2] = buf
        .get(off..off + 2)
        .context("read_u16_le out of bounds")?
        .try_into()?;
    Ok(u16::from_le_bytes(bytes))
}

pub fn read_u32_le(buf: &[u8], off: usize) -> Result<u32> {
    let bytes: [u8; 4] = buf
        .get(off..off + 4)
        .context("read_u32_le out of bounds")?
        .try_into()?;
    Ok(u32::from_le_bytes(bytes))
}

fn read_zero_padded_ascii(buf: &[u8], off: usize, len: usize) -> Result<String> {
    let slice = buf
        .get(off..off + len)
        .context("read_zero_padded_ascii out of bounds")?;
    let end = slice.iter().position(|&b| b == 0).unwrap_or(len);
    Ok(String::from_utf8_lossy(&slice[..end]).into_owned())
}

fn is_printable_ascii_tag(tag: &[u8; 4]) -> bool {
    tag.iter().all(|&b| b == 0 || (0x20..=0x7e).contains(&b))
}

pub fn parse_nux_dfu_header(data: &[u8]) -> Result<NuxDfuHeader> {
    ensure!(
        data.len() >= SECTION_TABLE_OFFSET,
        "firmware file too small"
    );

    let magic: [u8; 7] = data[0..7]
        .try_into()
        .context("failed to read NUX DFU magic")?;
    ensure!(
        &magic == b"NUX DFU",
        "invalid magic: expected b\"NUX DFU\", got {:?}",
        String::from_utf8_lossy(&magic)
    );

    let version_byte = data[7];
    let total_size = read_u32_le(data, 0x08)?;
    ensure!(
        total_size as usize == data.len(),
        "total_size mismatch: header says {total_size}, file is {}",
        data.len()
    );

    Ok(NuxDfuHeader {
        magic,
        version_byte,
        total_size,
        unknown_0c: read_u32_le(data, 0x0c)?,
        unknown_10: read_u32_le(data, 0x10)?,
        product_name: read_zero_padded_ascii(data, 0x14, 0x100)?,
        vendor: read_zero_padded_ascii(data, 0x114, 0x100)?,
        version: read_zero_padded_ascii(data, 0x214, 0x10)?,
    })
}

pub fn parse_sections(data: &[u8]) -> Result<Vec<SectionEntry>> {
    let mut sections = Vec::new();
    let mut off = SECTION_TABLE_OFFSET;

    while off + SECTION_ENTRY_SIZE <= data.len() {
        let tag_bytes: [u8; 4] = data[off..off + 4].try_into()?;
        if tag_bytes == [0, 0, 0, 0] {
            break;
        }

        if !is_printable_ascii_tag(&tag_bytes) {
            break;
        }

        let size = read_u32_le(data, off + 4)? as usize;
        let file_offset = read_u32_le(data, off + 8)? as usize;

        if file_offset + size > data.len() {
            bail!(
                "invalid section {:?}: offset 0x{file_offset:x} + size 0x{size:x} exceeds file",
                String::from_utf8_lossy(&tag_bytes)
            );
        }

        sections.push(SectionEntry {
            tag: String::from_utf8_lossy(&tag_bytes).into_owned(),
            size,
            offset: file_offset,
        });

        off += SECTION_ENTRY_SIZE;
    }

    Ok(sections)
}

pub fn parse_nux_dfu(data: &[u8]) -> Result<Firmware> {
    let header = parse_nux_dfu_header(data)?;
    let sections = parse_sections(data)?;
    Ok(Firmware { header, sections })
}

impl Firmware {
    pub fn section(&self, tag: &str) -> Option<&SectionEntry> {
        self.sections.iter().find(|s| s.tag == tag)
    }
}

pub fn dump_sections(data: &[u8], sections: &[SectionEntry], out_dir: &Path) -> Result<()> {
    fs::create_dir_all(out_dir)?;

    for section in sections {
        let path = out_dir.join(format!("{}.bin", section.tag));
        let bytes = &data[section.offset..section.offset + section.size];
        fs::write(&path, bytes)
            .with_context(|| format!("failed to write section dump {}", path.display()))?;
    }

    Ok(())
}
