//! Optional user configuration: a flat TOML subset at
//! `$XDG_CONFIG_HOME/frameit/config.toml`.
//!
//! Every key has a default and a bad value only costs a warning on stderr, so
//! a typo never leaves the tool unusable. Parsed by hand: seven keys do not
//! justify a TOML dependency.
//!
//! `include = "path"` reads another file at that line, so a theme can ship
//! the colours while the user's own file keeps the rest. Keys after the
//! include win over it.

use std::path::{Path, PathBuf};

/// How deep `include` may nest before it is refused, which also stops a
/// file that includes itself.
const MAX_INCLUDE_DEPTH: usize = 8;

/// Straight (non-premultiplied) RGBA in 0..=1.
pub type Rgba = [f64; 4];

/// Alpha-premultiplied, which is what every Wayland buffer format expects.
pub fn premultiplied([r, g, b, a]: Rgba) -> Rgba {
    [r * a, g * a, b * a, a]
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cursor {
    Crosshair,
    Default,
    Hidden,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub fill: Rgba,
    pub border: Rgba,
    /// Logical pixels.
    pub border_width: i32,
    /// Logical pixels; 0 for square corners.
    pub border_radius: i32,
    pub cursor: Cursor,
    /// Key chord for `frameit bind`, e.g. "SUPER + SHIFT + Z".
    pub trigger: String,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            fill: [0.26, 0.53, 0.96, 0.25],
            border: [0.26, 0.53, 0.96, 1.0],
            border_width: 2,
            border_radius: 0,
            cursor: Cursor::Crosshair,
            trigger: "SUPER + SHIFT + Z".into(),
        }
    }
}

impl Config {
    /// `$XDG_CONFIG_HOME/frameit/config.toml`, falling back to `~/.config`.
    pub fn default_path() -> Option<PathBuf> {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
        Some(base.join("frameit").join("config.toml"))
    }

    /// Load `path`. A missing file is the default config. A present file with
    /// bad lines keeps the defaults for those keys and warns about each one.
    pub fn load(path: &Path) -> Config {
        let mut config = Config::default();
        config.apply_file(path, 0, true);
        config
    }

    /// Apply the lines of `path` on top of `self`. A missing root file is
    /// silent, since the config is optional; a missing include is an error
    /// on the line that named it.
    fn apply_file(&mut self, path: &Path, depth: usize, root: bool) {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if root && e.kind() == std::io::ErrorKind::NotFound => return,
            Err(e) => {
                eprintln!("frameit: cannot read {}: {e}", path.display());
                return;
            }
        };
        for (index, line) in text.lines().enumerate() {
            let line = strip_comment(line).trim();
            if line.is_empty() {
                continue;
            }
            let result = match line.split_once('=') {
                Some((key, value)) => match key.trim() {
                    "include" => self.include(path, unquote(value.trim()), depth),
                    key => self.set(key, unquote(value.trim())),
                },
                None => Err("expected `key = value`".to_string()),
            };
            if let Err(message) = result {
                eprintln!("frameit: {}:{}: {message}", path.display(), index + 1);
            }
        }
    }

    /// Read another file in place of this line. A relative path resolves
    /// against the including file's directory and a leading `~/` against
    /// `$HOME`.
    fn include(&mut self, from: &Path, value: &str, depth: usize) -> Result<(), String> {
        if depth >= MAX_INCLUDE_DEPTH {
            return Err(format!(
                "include nested deeper than {MAX_INCLUDE_DEPTH} files"
            ));
        }
        let target = resolve(from, value);
        if !target.is_file() {
            return Err(format!("include '{}' is not a file", target.display()));
        }
        self.apply_file(&target, depth + 1, false);
        Ok(())
    }

    fn set(&mut self, key: &str, value: &str) -> Result<(), String> {
        match key {
            "fill" => self.fill = parse_color(value)?,
            "border" => self.border = parse_color(value)?,
            "border_width" => self.border_width = parse_int(value, 0, 64)?,
            "border_radius" => self.border_radius = parse_int(value, 0, 256)?,
            "cursor" => self.cursor = parse_cursor(value)?,
            "trigger" => self.trigger = value.to_string(),
            _ => return Err(format!("unknown key '{key}'")),
        }
        Ok(())
    }
}

/// Path named by an `include` line, resolved from the including file.
fn resolve(from: &Path, value: &str) -> PathBuf {
    if let Some(rest) = value.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    let path = Path::new(value);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        from.parent().unwrap_or(Path::new(".")).join(path)
    }
}

/// Drop a `#` comment, but not a `#` inside quotes: colours start with one.
fn strip_comment(line: &str) -> &str {
    let mut quoted = false;
    for (i, ch) in line.char_indices() {
        match ch {
            '"' => quoted = !quoted,
            '#' if !quoted => return &line[..i],
            _ => {}
        }
    }
    line
}

fn unquote(value: &str) -> &str {
    value
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .unwrap_or(value)
}

/// `#rrggbb` or `#rrggbbaa`, with or without the `#`.
fn parse_color(value: &str) -> Result<Rgba, String> {
    let hex = value.trim_start_matches('#');
    let valid = (hex.len() == 6 || hex.len() == 8) && hex.chars().all(|c| c.is_ascii_hexdigit());
    if !valid {
        return Err(format!(
            "expected a colour like #rrggbb or #rrggbbaa, got '{value}'"
        ));
    }
    let channel = |i: usize| f64::from(u8::from_str_radix(&hex[i..i + 2], 16).unwrap_or(0)) / 255.0;
    let alpha = if hex.len() == 8 { channel(6) } else { 1.0 };
    Ok([channel(0), channel(2), channel(4), alpha])
}

fn parse_int(value: &str, min: i32, max: i32) -> Result<i32, String> {
    value
        .parse::<i32>()
        .ok()
        .filter(|v| (min..=max).contains(v))
        .ok_or_else(|| format!("expected an integer from {min} to {max}, got '{value}'"))
}

fn parse_cursor(value: &str) -> Result<Cursor, String> {
    match value {
        "crosshair" => Ok(Cursor::Crosshair),
        "default" => Ok(Cursor::Default),
        "hidden" | "none" => Ok(Cursor::Hidden),
        _ => Err(format!(
            "expected crosshair, default or hidden, got '{value}'"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colours_parse_with_and_without_alpha() {
        assert_eq!(parse_color("#ff0000"), Ok([1.0, 0.0, 0.0, 1.0]));
        assert_eq!(parse_color("00ff0080"), Ok([0.0, 1.0, 0.0, 128.0 / 255.0]));
        assert!(parse_color("#12345").is_err());
        assert!(parse_color("red").is_err());
    }

    #[test]
    fn comments_are_stripped_but_not_inside_quotes() {
        assert_eq!(
            strip_comment("border = \"#4287f5\" # blue"),
            "border = \"#4287f5\" "
        );
        assert_eq!(strip_comment("# whole line"), "");
        assert_eq!(unquote("\"x\""), "x");
        assert_eq!(unquote("2"), "2");
    }

    #[test]
    fn integers_are_bounded() {
        assert_eq!(parse_int("3", 0, 64), Ok(3));
        assert!(parse_int("65", 0, 64).is_err());
        assert!(parse_int("-1", 0, 64).is_err());
        assert!(parse_int("two", 0, 64).is_err());
    }

    /// A scratch directory unique to one test, under the system temp dir.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("frameit-test-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn include_reads_the_other_file_and_later_keys_win() {
        let dir = scratch("include");
        std::fs::write(
            dir.join("theme.toml"),
            "border = \"#ff0000\"\nborder_width = 4\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("config.toml"),
            "border_width = 1\ninclude = \"theme.toml\"\nborder_width = 3\n",
        )
        .unwrap();
        let config = Config::load(&dir.join("config.toml"));
        assert_eq!(
            config.border,
            [1.0, 0.0, 0.0, 1.0],
            "included colour applies"
        );
        assert_eq!(
            config.border_width, 3,
            "a key after the include wins over it"
        );
    }

    #[test]
    fn missing_include_keeps_going() {
        let dir = scratch("missing");
        std::fs::write(
            dir.join("config.toml"),
            "include = \"nope.toml\"\nborder_radius = 9\n",
        )
        .unwrap();
        let config = Config::load(&dir.join("config.toml"));
        assert_eq!(
            config.border_radius, 9,
            "the rest of the file still applies"
        );
        assert_eq!(config.border, Config::default().border);
    }

    #[test]
    fn self_include_stops_at_the_depth_limit() {
        let dir = scratch("loop");
        std::fs::write(
            dir.join("config.toml"),
            "include = \"config.toml\"\ncursor = \"hidden\"\n",
        )
        .unwrap();
        let config = Config::load(&dir.join("config.toml"));
        assert_eq!(config.cursor, Cursor::Hidden);
    }

    #[test]
    fn set_applies_known_keys_and_rejects_unknown() {
        let mut config = Config::default();
        assert_eq!(config.set("border_radius", "6"), Ok(()));
        assert_eq!(config.border_radius, 6);
        assert_eq!(config.set("cursor", "hidden"), Ok(()));
        assert_eq!(config.cursor, Cursor::Hidden);
        assert!(config.set("radius", "6").is_err());
        assert!(config.set("border_width", "wide").is_err());
        assert_eq!(config.border_width, 2, "bad value keeps the default");
    }
}
