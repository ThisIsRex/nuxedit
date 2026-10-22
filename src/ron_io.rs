use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Serialize;
use serde::de::DeserializeOwned;

fn pretty_config() -> ron::ser::PrettyConfig {
    ron::ser::PrettyConfig::new().struct_names(true)
}

pub fn write_ron<T: Serialize>(path: &Path, value: &T, what: &str) -> Result<()> {
    let text = ron::ser::to_string_pretty(value, pretty_config())
        .with_context(|| format!("failed to serialize {what}"))?;
    fs::write(path, text).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

pub fn read_ron<T: DeserializeOwned>(path: &Path, what: &str) -> Result<T> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("failed to read {what} {}", path.display()))?;
    ron::from_str(&text).with_context(|| format!("failed to parse {what} {}", path.display()))
}
