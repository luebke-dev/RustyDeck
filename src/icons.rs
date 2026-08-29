//! Named icons from the embedded Material Design Icons font.
//!
//! In the configuration an icon is either a file path or a name such as
//! `mdi:volume-high`. The Pictogrammers webfont and its name table ship inside
//! the binary, so nothing has to be installed on the system.

use ab_glyph::FontArc;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Material Design Icons release the embedded assets were taken from.
pub const VERSION: &str = "7.4.47";

const FONT_DATA: &[u8] = include_bytes!("../assets/materialdesignicons-webfont.ttf");
/// One `name codepoint` pair per line, generated from the release's CSS.
const CODEPOINTS: &str = include_str!("../assets/mdi-codepoints.txt");

/// Prefix marking an icon name rather than a file path.
pub const PREFIX: &str = "mdi:";

/// Where a key image gets its picture from.
#[derive(Debug, Clone)]
pub enum IconRef {
    File(PathBuf),
    /// An encoded image handed over directly, as the REST API does.
    Data(std::sync::Arc<Vec<u8>>),
    /// A glyph from the icon font, with the name it was written as — the name
    /// is kept for error messages.
    Glyph {
        name: String,
        glyph: char,
    },
    /// A prefixed name that matches no icon.
    Unknown(String),
}

/// The embedded icon font, parsed once.
pub fn font() -> &'static FontArc {
    static FONT: OnceLock<FontArc> = OnceLock::new();
    FONT.get_or_init(|| {
        FontArc::try_from_slice(FONT_DATA).expect("the embedded icon font is valid")
    })
}

/// Name → glyph, parsed once from the embedded table.
fn table() -> &'static HashMap<&'static str, char> {
    static TABLE: OnceLock<HashMap<&'static str, char>> = OnceLock::new();
    TABLE.get_or_init(|| {
        CODEPOINTS
            .lines()
            .filter_map(|line| {
                let (name, hex) = line.split_once(' ')?;
                let code = u32::from_str_radix(hex.trim(), 16).ok()?;
                Some((name, char::from_u32(code)?))
            })
            .collect()
    })
}

/// Look up an icon name. Underscores and spaces work as well as hyphens, and a
/// leading `mdi-` is ignored, so `volume_high` and `mdi-volume-high` both land
/// on `volume-high`.
pub fn lookup(name: &str) -> Option<char> {
    table().get(normalise(name).as_str()).copied()
}

fn normalise(name: &str) -> String {
    let lower = name.trim().to_ascii_lowercase().replace(['_', ' '], "-");
    lower.strip_prefix("mdi-").unwrap_or(&lower).to_string()
}

/// Icon names containing `needle`, alphabetically sorted. An empty needle
/// returns every name.
pub fn search(needle: &str) -> Vec<&'static str> {
    let needle = normalise(needle);
    let mut names: Vec<&'static str> = table()
        .keys()
        .copied()
        .filter(|name| needle.is_empty() || name.contains(needle.as_str()))
        .collect();
    names.sort_unstable();
    names
}

/// Up to five names close to `name`, for "did you mean …" hints.
pub fn suggestions(name: &str) -> Vec<&'static str> {
    let normalised = normalise(name);
    // Try the parts of the name longest-first: for `volume-hiiigh` the part
    // `volume` still finds the `volume-*` family. Parts of equal length keep
    // their written order, so the leading word wins.
    let mut parts: Vec<&str> = normalised.split('-').filter(|p| p.len() >= 3).collect();
    parts.sort_by_key(|p| std::cmp::Reverse(p.len()));

    for part in parts {
        let mut names = search(part);
        if !names.is_empty() {
            names.truncate(5);
            return names;
        }
    }
    Vec::new()
}

/// How many icons the embedded font provides.
pub fn count() -> usize {
    table().len()
}

/// Decide whether an `icon:` value names an icon or points at a file.
/// `resolve_path` turns a relative path into an absolute one.
pub fn parse(raw: &str, resolve_path: impl FnOnce(&str) -> PathBuf) -> IconRef {
    match raw.strip_prefix(PREFIX) {
        Some(name) => match lookup(name) {
            Some(glyph) => IconRef::Glyph {
                name: name.trim().to_string(),
                glyph,
            },
            None => IconRef::Unknown(name.trim().to_string()),
        },
        None => IconRef::File(resolve_path(raw)),
    }
}

impl IconRef {
    /// Path of a file icon, for existence checks.
    pub fn as_file(&self) -> Option<&Path> {
        match self {
            IconRef::File(path) => Some(path),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_covers_the_whole_release() {
        assert_eq!(count(), 7448);
    }

    #[test]
    fn names_are_forgiving() {
        let expected = lookup("volume-high").expect("volume-high exists");
        assert_eq!(lookup("volume_high"), Some(expected));
        assert_eq!(lookup("mdi-volume-high"), Some(expected));
        assert_eq!(lookup("  Volume High  "), Some(expected));
    }

    #[test]
    fn unknown_names_get_suggestions() {
        assert!(lookup("volume-hiiigh").is_none());
        assert!(suggestions("volume-hiiigh").contains(&"volume-high"));
    }

    #[test]
    fn prefix_separates_names_from_paths() {
        let path_of = |raw: &str| PathBuf::from("/base").join(raw);
        assert!(matches!(parse("mdi:play", path_of), IconRef::Glyph { .. }));
        assert!(matches!(parse("icons/play.png", path_of), IconRef::File(_)));
        assert!(matches!(
            parse("mdi:nope-nope", path_of),
            IconRef::Unknown(_)
        ));
    }
}
