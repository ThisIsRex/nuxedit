use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use sha2::{Digest, Sha512};

pub fn sha512_hex(data: &[u8]) -> String {
    let digest = Sha512::digest(data);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn sha512_file(path: &Path) -> Result<String> {
    let data = fs::read(path)
        .with_context(|| format!("failed to read file for sha512: {}", path.display()))?;
    Ok(sha512_hex(&data))
}
