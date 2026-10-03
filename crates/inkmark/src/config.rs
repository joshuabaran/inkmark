//! inkmark's settings file, `$XDG_CONFIG_HOME/inkmark/config.toml`, and the
//! system fonts it falls back to. Today it sets fonts:
//!
//! ```toml
//! [font]
//! code = "JetBrains Mono"   # code pane and code spans; default: fontconfig's monospace
//! text = "Inter"            # live pane; default: fontconfig's sans-serif
//! code_size = 14            # points
//! text_size = 16
//! ```
//!
//! On Omarchy, `omarchy font set` changes fontconfig's monospace font, so
//! the code pane follows it unless `code` is set here.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Sizes are clamped to this range, in points.
const SIZES: std::ops::RangeInclusive<f32> = 6.0..=72.0;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Settings {
    pub code_font: Option<String>,
    pub text_font: Option<String>,
    pub code_size: Option<f32>,
    pub text_size: Option<f32>,
}

/// Parses config.toml. Unknown keys are ignored, so older inkmarks read
/// newer files; a wrong type is an error naming the key.
pub fn parse(text: &str) -> Result<Settings, String> {
    let table: toml::Table = text.parse().map_err(|e: toml::de::Error| e.to_string())?;
    let mut settings = Settings::default();
    let Some(font) = table.get("font") else {
        return Ok(settings);
    };
    let font = font.as_table().ok_or("[font] must be a table".to_owned())?;
    let name = |key: &str| -> Result<Option<String>, String> {
        match font.get(key) {
            None => Ok(None),
            Some(toml::Value::String(s)) if !s.trim().is_empty() => Ok(Some(s.trim().to_owned())),
            Some(_) => Err(format!("font.{key} must be a font name")),
        }
    };
    let size = |key: &str| -> Result<Option<f32>, String> {
        let value = match font.get(key) {
            None => return Ok(None),
            Some(toml::Value::Integer(n)) => *n as f32,
            Some(toml::Value::Float(f)) => *f as f32,
            Some(_) => return Err(format!("font.{key} must be a number")),
        };
        Ok(Some(value.clamp(*SIZES.start(), *SIZES.end())))
    };
    settings.code_font = name("code")?;
    settings.text_font = name("text")?;
    settings.code_size = size("code_size")?;
    settings.text_size = size("text_size")?;
    Ok(settings)
}

pub fn config_path() -> Option<PathBuf> {
    config_home().map(|c| c.join("inkmark/config.toml"))
}

/// The file `omarchy font set` (and other font tools) write; a change means
/// the system fonts may have changed.
pub fn fontconfig_path() -> Option<PathBuf> {
    config_home().map(|c| c.join("fontconfig/fonts.conf"))
}

fn config_home() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
}

/// The family fontconfig picks for `alias` (`monospace`, `sans-serif`), as
/// `omarchy font current` asks it. `None` without fontconfig's tools.
pub fn system_font(alias: &str) -> Option<String> {
    let out = std::process::Command::new("fc-match")
        .args([alias, "-f", "%{family}"])
        .output()
        .ok()?;
    let families = String::from_utf8(out.stdout).ok()?;
    // "JetBrainsMono Nerd Font,JetBrainsMono NF": the first name.
    let first = families.split(',').next()?.trim();
    (out.status.success() && !first.is_empty()).then(|| first.to_owned())
}

/// Modification times of the files fonts depend on, to notice a change.
pub fn stamps(paths: &[Option<PathBuf>]) -> Vec<Option<SystemTime>> {
    paths
        .iter()
        .map(|p| {
            p.as_deref()
                .and_then(|p: &Path| std::fs::metadata(p).ok())
                .and_then(|m| m.modified().ok())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn font_settings_parse() {
        assert_eq!(parse("").unwrap(), Settings::default());
        let s = parse(
            "[font]\ncode = \"JetBrains Mono\"\ntext = \" Inter \"\ncode_size = 13\ntext_size = 17.5\nfuture = true\n",
        )
        .unwrap();
        assert_eq!(s.code_font.as_deref(), Some("JetBrains Mono"));
        assert_eq!(s.text_font.as_deref(), Some("Inter"));
        assert_eq!(s.code_size, Some(13.0));
        assert_eq!(s.text_size, Some(17.5));
        assert_eq!(
            parse("[font]\ncode_size = 400").unwrap().code_size,
            Some(72.0)
        );
        assert!(parse("[font]\ncode = 3").unwrap_err().contains("font.code"));
        assert!(
            parse("[font]\ntext_size = \"big\"")
                .unwrap_err()
                .contains("text_size")
        );
        assert!(parse("font = 1").is_err());
        assert!(parse("[font").is_err());
        // Other tables are for later.
        assert_eq!(parse("[editor]\ntab = 4\n").unwrap(), Settings::default());
    }
}
