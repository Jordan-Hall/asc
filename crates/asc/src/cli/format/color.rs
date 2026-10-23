//! Colour specifications (`%C(…)`, git-config "Values": colours and attributes) as SGR codes.

pub const RESET: &str = "\x1b[m";

/// A parsed colour specification.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Spec {
    reset: bool,
    attrs: Vec<u8>,
    fg: Option<String>,
    bg: Option<String>,
}

fn base_color(name: &str) -> Option<u8> {
    ["black", "red", "green", "yellow", "blue", "magenta", "cyan", "white"]
        .iter()
        .position(|c| *c == name)
        .map(|p| p as u8)
}

/// SGR parameters of one colour word; `foreground` picks 30/90/38 or 40/100/48.
fn color_code(word: &str, foreground: bool) -> Option<Option<String>> {
    let (base, bright, ext) = if foreground { (30, 90, 38) } else { (40, 100, 48) };
    if word == "normal" {
        return Some(None);
    }
    if word == "default" {
        return Some(Some((base + 9).to_string()));
    }
    if let Some(n) = base_color(word) {
        return Some(Some((base + n).to_string()));
    }
    if let Some(n) = word.strip_prefix("bright").and_then(base_color) {
        return Some(Some((bright + n).to_string()));
    }
    if let Ok(n) = word.parse::<u16>() {
        return match n {
            0..=7 => Some(Some((base + n as u8).to_string())),
            8..=15 => Some(Some((bright + n as u8 - 8).to_string())),
            16..=255 => Some(Some(format!("{ext};5;{n}"))),
            _ => None,
        };
    }
    let hex = word.strip_prefix('#').filter(|h| h.len() == 6)?;
    let v = u32::from_str_radix(hex, 16).ok()?;
    Some(Some(format!("{ext};2;{};{};{}", v >> 16, (v >> 8) & 0xff, v & 0xff)))
}

/// Attribute words and their SGR numbers, in the order git emits them.
const ATTRS: [(&str, u8); 7] =
    [("bold", 1), ("dim", 2), ("italic", 3), ("ul", 4), ("blink", 5), ("reverse", 7), ("strike", 9)];

impl Spec {
    /// Parses `red`, `bold blue`, `reset`, `ul green black`… `None` for an invalid word.
    pub fn parse(text: &str) -> Option<Self> {
        let mut spec = Self::default();
        let mut seen_fg = false;
        for word in text.split_whitespace() {
            let lower = word.to_ascii_lowercase();
            if lower == "reset" {
                spec.reset = true;
                continue;
            }
            if let Some(&(_, n)) = ATTRS.iter().find(|(a, _)| *a == lower) {
                spec.attrs.push(n);
            } else if let Some(&(_, n)) = ATTRS.iter().find(|(a, _)| lower.strip_prefix("no").is_some_and(|r| r == *a)
                || lower.strip_prefix("no-").is_some_and(|r| r == *a))
            {
                spec.attrs.push(20 + n + u8::from(n == 1));
            } else if !seen_fg {
                spec.fg = color_code(&lower, true)?;
                seen_fg = true;
            } else {
                spec.bg = color_code(&lower, false)?;
            }
        }
        spec.attrs.sort_by_key(|n| ATTRS.iter().position(|(_, a)| a == n).unwrap_or(99));
        Some(spec)
    }

    /// The escape sequence, empty for a specification that sets nothing.
    pub fn sgr(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if self.reset && self.attrs.is_empty() && self.fg.is_none() && self.bg.is_none() {
            return RESET.to_owned();
        }
        parts.extend(self.attrs.iter().map(u8::to_string));
        parts.extend(self.fg.clone());
        parts.extend(self.bg.clone());
        if parts.is_empty() {
            return String::new();
        }
        format!("\x1b[{}m", parts.join(";"))
    }
}

/// The SGR sequence of a plain colour spec, or empty when it is invalid or sets nothing.
pub fn sgr(text: &str) -> String {
    Spec::parse(text).map(|s| s.sgr()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequences() {
        assert_eq!(sgr("red"), "\x1b[31m");
        assert_eq!(sgr("bold blue"), "\x1b[1;34m");
        assert_eq!(sgr("reset"), "\x1b[m");
        assert_eq!(sgr("ul green black"), "\x1b[4;32;40m");
        assert_eq!(sgr("brightred"), "\x1b[91m");
        assert_eq!(sgr("#ff0000"), "\x1b[38;2;255;0;0m");
        assert_eq!(sgr("200"), "\x1b[38;5;200m");
        assert_eq!(sgr("nobold"), "\x1b[22m");
        assert_eq!(sgr("nonsense"), "");
    }
}
