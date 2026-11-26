//! Following the system theme.
//!
//! Checked in order of how specific the source is. Stylix comes first because
//! it is the one that actually tracks a user's choice across the whole desktop.
//! Omarchy comes next: it rewrites the active theme's `colors.toml` on every
//! theme switch, so reading it again follows the switch. COSMIC's own config
//! is the fallback for a COSMIC user running neither.

use std::path::PathBuf;

use super::base16::{self, Base16Scheme};

/// Where a detected system scheme came from, for reporting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Stylix(PathBuf),
    Omarchy(PathBuf),
    Cosmic,
    None,
}

impl std::fmt::Display for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Source::Stylix(p) => write!(f, "stylix ({})", p.display()),
            Source::Omarchy(p) => write!(f, "omarchy ({})", p.display()),
            Source::Cosmic => f.write_str("cosmic"),
            Source::None => f.write_str("none"),
        }
    }
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Candidate files, most authoritative first.
fn stylix_candidates() -> Vec<PathBuf> {
    let Some(h) = home() else { return Vec::new() };
    vec![
        h.join(".config/stylix/palette.json"),
        h.join(".config/stylix/palette.yaml"),
    ]
}

/// The active Omarchy theme's palette.
fn omarchy_colors() -> Option<PathBuf> {
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| home().map(|h| h.join(".local/state")))?;
    Some(state.join("omarchy/current/theme/colors.toml"))
}

/// Read an Omarchy `colors.toml` as a base16 scheme, with its light or dark
/// mode when the theme declares one.
///
/// Omarchy names its colours by role rather than by base16 slot. The
/// background and foreground ramps map onto base00..base07 and the accents onto
/// base08..base0F. Older themes leave out `orange` and `brown`, so those fall
/// back to the nearest colour every theme has.
pub fn parse_omarchy(text: &str) -> (Base16Scheme, Option<bool>) {
    let table = text.parse::<toml::Table>().unwrap_or_default();
    let color = |key: &str| {
        table
            .get(key)
            .and_then(|v| v.as_str())
            .and_then(|v| super::color::Rgb::parse_hex(v).ok())
    };
    let slots: [(&str, &[&str]); 16] = [
        ("base00", &["background"]),
        ("base01", &["lighter_background", "selection"]),
        ("base02", &["selection", "lighter_background"]),
        ("base03", &["muted", "dark_foreground"]),
        ("base04", &["dark_foreground", "muted"]),
        ("base05", &["foreground"]),
        ("base06", &["light_foreground", "foreground"]),
        ("base07", &["bright_foreground", "foreground"]),
        ("base08", &["red"]),
        ("base09", &["orange", "bright_red", "yellow"]),
        ("base0a", &["yellow"]),
        ("base0b", &["green"]),
        ("base0c", &["cyan"]),
        ("base0d", &["blue"]),
        ("base0e", &["magenta"]),
        ("base0f", &["brown", "red"]),
    ];
    let colors = slots
        .iter()
        .filter_map(|(slot, keys)| {
            keys.iter()
                .find_map(|key| color(key))
                .map(|c| (slot.to_string(), c))
        })
        .collect();
    let dark = match table.get("mode").and_then(|v| v.as_str()) {
        Some("dark") => Some(true),
        Some("light") => Some(false),
        _ => None,
    };
    (
        Base16Scheme {
            name: Some("omarchy".into()),
            author: None,
            colors,
        },
        dark,
    )
}

fn omarchy() -> Option<(Base16Scheme, Option<bool>, PathBuf)> {
    let path = omarchy_colors()?;
    let text = std::fs::read_to_string(&path).ok()?;
    let (scheme, dark) = parse_omarchy(&text);
    scheme.is_complete().then_some((scheme, dark, path))
}

/// Is the desktop asking for a dark theme?
///
/// COSMIC records this separately from the palette, and it decides which way
/// the base16 lightness ends are read. An Omarchy theme declares its own mode,
/// which wins when its palette is the one in use.
pub fn prefers_dark() -> bool {
    if !stylix_candidates().iter().any(|p| p.is_file()) {
        if let Some((_, Some(dark), _)) = omarchy() {
            return dark;
        }
    }
    if let Some(h) = home() {
        let mode = h.join(".config/cosmic/com.system76.CosmicTheme.Mode/v1/is_dark");
        if let Ok(s) = std::fs::read_to_string(&mode) {
            return s.trim() == "true";
        }
    }
    // Dark is the right default for a terminal application.
    true
}

/// Find the system's base16 scheme, if there is one.
pub fn detect() -> (Option<Base16Scheme>, Source) {
    for path in stylix_candidates() {
        if !path.is_file() {
            continue;
        }
        if let Ok(text) = std::fs::read_to_string(&path) {
            let scheme = base16::parse(&text);
            if scheme.is_complete() {
                return (Some(scheme), Source::Stylix(path));
            }
        }
    }
    if let Some((scheme, _, path)) = omarchy() {
        return (Some(scheme), Source::Omarchy(path));
    }
    (None, Source::None)
}

/// The desktop's scheme as a theme file, if one can be detected.
///
/// A file rather than a resolved theme: each application resolves its own
/// roles out of it, and handing back one application's struct would mean the
/// other could not follow the desktop at all.
pub fn theme() -> Option<(super::schema::ThemeFile, Source)> {
    let (scheme, source) = detect();
    let scheme = scheme?;
    let variant = if prefers_dark() { "dark" } else { "light" };
    let toml = base16::to_theme_toml(&scheme, "system", variant);
    let file = super::schema::ThemeFile::parse(&toml).ok()?;
    Some((file, source))
}

#[cfg(test)]
mod tests {
    use super::super::color::Rgb;
    use super::*;

    #[test]
    fn detection_never_panics_whatever_the_machine_looks_like() {
        let (_, source) = detect();
        // Only that it produced an answer; the machine running the tests may or
        // may not have Stylix.
        let _ = source.to_string();
    }

    #[test]
    fn a_detected_scheme_resolves_to_a_usable_theme() {
        if let Some((f, _)) = theme() {
            let t = super::super::resolve::Theme::resolve(&f);
            // Whatever the scheme, body text has to be readable.
            assert!(
                t.bg.contrast(t.fg) >= 3.0,
                "system theme contrast is only {:.2}:1",
                t.bg.contrast(t.fg)
            );
        }
    }

    #[test]
    fn an_omarchy_palette_fills_every_base16_slot() {
        let text = r##"
mode = "light"
accent = "#dcd7ba"
selection = "#ccd0da"
muted = "#acb0be"
background = "#eff1f5"
dark_background = "#e3e4e8"
darker_background = "#d7d8dc"
lighter_background = "#dce0e8"
foreground = "#4c4f69"
dark_foreground = "#9ca0b0"
light_foreground = "#5c5f77"
bright_foreground = "#4c4f69"
red = "#d20f39"
yellow = "#df8e1d"
green = "#40a02b"
cyan = "#179299"
blue = "#1e66f5"
magenta = "#8839ef"
bright_red = "#e64553"
"##;
        let (scheme, dark) = parse_omarchy(text);
        assert!(scheme.is_complete(), "missing {:?}", scheme.missing());
        assert_eq!(dark, Some(false));
        assert_eq!(scheme.get("base00"), Rgb::parse_hex("#eff1f5").ok());
        assert_eq!(scheme.get("base01"), Rgb::parse_hex("#dce0e8").ok());
        assert_eq!(scheme.get("base05"), Rgb::parse_hex("#4c4f69").ok());
        // No `orange` or `brown`: the nearest colours stand in.
        assert_eq!(scheme.get("base09"), Rgb::parse_hex("#e64553").ok());
        assert_eq!(scheme.get("base0F"), Rgb::parse_hex("#d20f39").ok());
        let toml = base16::to_theme_toml(&scheme, "system", "light");
        let theme = super::super::resolve::Theme::resolve(
            &super::super::schema::ThemeFile::parse(&toml).unwrap(),
        );
        assert!(theme.bg.contrast(theme.fg) >= 3.0);
    }

    #[test]
    fn an_incomplete_omarchy_palette_is_not_used() {
        let (scheme, dark) = parse_omarchy("background = \"#000000\"\nnot toml [");
        assert!(!scheme.is_complete());
        assert_eq!(dark, None);
    }

    #[test]
    fn source_renders_something_meaningful() {
        assert_eq!(Source::None.to_string(), "none");
        assert_eq!(Source::Cosmic.to_string(), "cosmic");
        assert!(Source::Stylix("/x/p.json".into())
            .to_string()
            .contains("stylix"));
    }
}
