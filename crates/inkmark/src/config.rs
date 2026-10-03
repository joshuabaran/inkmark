//! inkmark's settings file, `$XDG_CONFIG_HOME/inkmark/config.toml`, and the
//! system fonts it falls back to. It sets fonts and key bindings:
//!
//! ```toml
//! [font]
//! code = "JetBrains Mono"   # code pane and code spans; default: fontconfig's monospace
//! text = "Inter"            # live pane; default: fontconfig's sans-serif
//! code_size = 14            # points
//! text_size = 16
//!
//! [keys]
//! bold = "Ctrl+B"           # one chord, or a list; [] unbinds
//! ```
//!
//! On Omarchy, `omarchy font set` changes fontconfig's monospace font, so
//! the code pane follows it unless `code` is set here. A problem in `[keys]`
//! is reported and that entry keeps its default; it does not reject the
//! rest of the file.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use inkmark_view::keys::KeyMap;

/// Sizes are clamped to this range, in points.
const SIZES: std::ops::RangeInclusive<f32> = 6.0..=72.0;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Settings {
    pub code_font: Option<String>,
    pub text_font: Option<String>,
    pub code_size: Option<f32>,
    pub text_size: Option<f32>,
    /// Effective chords: defaults, with the `[keys]` entries that parsed.
    pub keys: KeyMap,
    /// Unknown actions, bad chords, conflicts, and desktop-key warnings.
    /// These do not fail the parse; the banner shows them.
    pub key_problems: Vec<String>,
}

/// Parses config.toml. Unknown keys are ignored, so older inkmarks read
/// newer files; a wrong type in `[font]` is an error naming the key. A
/// problem in `[keys]` is recorded on [`Settings::key_problems`] and that
/// entry keeps its default.
pub fn parse(text: &str) -> Result<Settings, String> {
    let table: toml::Table = text.parse().map_err(|e: toml::de::Error| e.to_string())?;
    let mut settings = Settings::default();
    if let Some(font) = table.get("font") {
        apply_font(&mut settings, font)?;
    }
    if let Some(keys) = table.get("keys") {
        apply_keys(&mut settings, keys);
    }
    Ok(settings)
}

fn apply_font(settings: &mut Settings, font: &toml::Value) -> Result<(), String> {
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
    Ok(())
}

/// A chord string, or a list of them. A wrong type names the key and leaves
/// that action out, so it keeps its default.
fn chord_list(name: &str, value: &toml::Value) -> Result<Vec<String>, String> {
    let bad = || format!("keys.{name} must be a chord or a list of chords");
    match value {
        toml::Value::String(text) => Ok(vec![text.clone()]),
        toml::Value::Array(items) => items
            .iter()
            .map(|item| item.as_str().map(str::to_owned).ok_or_else(bad))
            .collect(),
        _ => Err(bad()),
    }
}

fn apply_keys(settings: &mut Settings, keys: &toml::Value) {
    let Some(table) = keys.as_table() else {
        settings
            .key_problems
            .push("[keys] must be a table".to_owned());
        return;
    };
    let mut entries = Vec::new();
    for (name, value) in table {
        match chord_list(name, value) {
            Ok(chords) => entries.push((name.clone(), chords)),
            Err(problem) => settings.key_problems.push(problem),
        }
    }
    let applied = KeyMap::apply(&entries);
    settings.keys = applied.map;
    settings.key_problems.extend(applied.errors);
    settings.key_problems.extend(applied.warnings);
}

/// The bindings `--list-keys` prints, and the problems that go to stderr.
/// `Ok(None)` is a missing file: the defaults, and nothing to report.
pub fn list_keys_from(read: Result<Option<String>, String>) -> (String, Vec<String>) {
    match read {
        Ok(None) => (KeyMap::builtin().to_toml(), Vec::new()),
        Err(error) => (
            KeyMap::builtin().to_toml(),
            vec![format!(
                "Couldn't read config.toml: {error} (keeping the defaults)"
            )],
        ),
        Ok(Some(text)) => match parse(&text) {
            Ok(settings) => (settings.keys.to_toml(), settings.key_problems),
            Err(error) => (
                KeyMap::builtin().to_toml(),
                vec![format!(
                    "Couldn't read config.toml: {error} (keeping the defaults)"
                )],
            ),
        },
    }
}

/// Reads the settings file and lists the bindings that would apply.
pub fn list_keys() -> (String, Vec<String>) {
    let Some(path) = config_path() else {
        return list_keys_from(Ok(None));
    };
    let read = match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.to_string()),
    };
    list_keys_from(read)
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

    #[test]
    fn key_bindings_apply_and_a_bad_entry_keeps_its_default() {
        use inkmark_view::keys::Action;

        let settings = parse(
            "[font]\ntext_size = 18\n[keys]\nbold = \"Ctrl+L\"\nitalic = []\ninsert_row_below = [\"Ctrl+Alt+Down\", \"F6\"]\n",
        )
        .unwrap();
        assert_eq!(settings.text_size, Some(18.0));
        assert!(
            settings.key_problems.is_empty(),
            "{:?}",
            settings.key_problems
        );
        assert_eq!(settings.keys.shortcut_text(Action::Bold), "Ctrl+L");
        assert!(settings.keys.chords(Action::Italic).is_empty());
        assert_eq!(
            settings.keys.shortcut_text(Action::InsertRowBelow),
            "Ctrl+Alt+Down, F6"
        );
        assert_eq!(settings.keys.shortcut_text(Action::Save), "Ctrl+S");

        // A bad chord, an unknown action, a shared chord, and a wrong type
        // are reported. The font still applies, and those actions stay put.
        let settings = parse(
            "[font]\ncode_size = 15\n[keys]\nbold = \"nope\"\nnope = \"Ctrl+Q\"\nitalic = \"Ctrl+K\"\nlink = 1\n",
        )
        .unwrap();
        assert_eq!(settings.code_size, Some(15.0));
        let problems = settings.key_problems.join("\n");
        assert!(problems.contains("keys.bold"), "{problems}");
        assert!(problems.contains("Unknown key action"), "{problems}");
        assert!(problems.contains("keys.link"), "{problems}");
        assert!(problems.contains("keys.italic"), "{problems}");
        assert_eq!(settings.keys.shortcut_text(Action::Bold), "Ctrl+B");
        assert_eq!(settings.keys.shortcut_text(Action::Italic), "Ctrl+I");
        assert_eq!(settings.keys.shortcut_text(Action::Link), "Ctrl+K");

        // `keys` at the root, not a key of [font].
        let settings = parse("keys = true\n[font]\ntext_size = 19\n").unwrap();
        assert_eq!(settings.text_size, Some(19.0));
        assert_eq!(
            settings.key_problems,
            vec!["[keys] must be a table".to_owned()]
        );
        assert_eq!(settings.keys, KeyMap::builtin());

        // A desktop chord is kept, and the banner can warn about it.
        let settings = parse("[keys]\nbold = \"Ctrl+Alt+Delete\"\n").unwrap();
        assert_eq!(settings.keys.shortcut_text(Action::Bold), "Ctrl+Alt+Delete");
        assert!(
            settings
                .key_problems
                .iter()
                .any(|p| p.contains("closes all windows")),
            "{:?}",
            settings.key_problems
        );
    }

    #[test]
    fn listed_keys_are_the_effective_bindings() {
        let (text, problems) = list_keys_from(Ok(None));
        assert!(problems.is_empty());
        assert_eq!(parse(&text).unwrap().keys, KeyMap::builtin());

        let (text, problems) = list_keys_from(Ok(Some("[keys]\nbold = \"Ctrl+L\"\n".into())));
        assert!(problems.is_empty(), "{problems:?}");
        assert!(text.contains("bold = \"Ctrl+L\""));
        assert!(text.contains("italic = \"Ctrl+I\""));
        assert!(!text.contains("bold = \"Ctrl+B\""));

        let (text, problems) = list_keys_from(Ok(Some("[keys]\nbold = []\n".into())));
        assert!(text.contains("bold = []"));
        assert!(problems.is_empty());

        let (_, problems) = list_keys_from(Ok(Some("[keys]\nbold = \"nope\"\n".into())));
        assert!(problems.iter().any(|p| p.contains("keys.bold")));

        let (text, problems) = list_keys_from(Ok(Some("[keys".into())));
        assert_eq!(parse(&text).unwrap().keys, KeyMap::builtin());
        assert!(problems.iter().any(|p| p.contains("keeping the defaults")));

        let (text, problems) = list_keys_from(Err("permission denied".into()));
        assert_eq!(parse(&text).unwrap().keys, KeyMap::builtin());
        assert!(problems[0].contains("permission denied"));
    }
}
