//! CSS colors in text, for the editor's color swatches and picker
//! (`editor/colors.rs`): finding them (`find`), reading one (`parse`), and
//! writing a color back in the same notation (`Format::write`), plus the
//! HSV the picker works in.

use std::ops::Range;

/// A color, each part from 0 to 1.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Color {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

/// How a color was written, to write a new one the same way.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Format {
    /// `#rgb`, `#rgba`, `#rrggbb` or `#rrggbbaa`: its digits and case.
    Hex { long: bool, upper: bool },
    /// `rgb(…)` / `rgba(…)`; `commas`: the older `rgb(1, 2, 3)` syntax.
    Rgb { commas: bool },
    /// `hsl(…)` / `hsla(…)`.
    Hsl { commas: bool },
}

impl Color {
    pub fn rgba(r: f32, g: f32, b: f32, a: f32) -> Self {
        Color { r, g, b, a }
    }

    /// Hue (0–360), saturation and value (0–1).
    pub fn to_hsv(self) -> (f32, f32, f32) {
        let max = self.r.max(self.g).max(self.b);
        let min = self.r.min(self.g).min(self.b);
        let d = max - min;
        let h = if d == 0. {
            0.
        } else if max == self.r {
            60. * ((self.g - self.b) / d).rem_euclid(6.)
        } else if max == self.g {
            60. * ((self.b - self.r) / d + 2.)
        } else {
            60. * ((self.r - self.g) / d + 4.)
        };
        let s = if max == 0. { 0. } else { d / max };
        (h, s, max)
    }

    pub fn from_hsv(h: f32, s: f32, v: f32, a: f32) -> Self {
        let h = h.rem_euclid(360.);
        let c = v * s;
        let x = c * (1. - ((h / 60.) % 2. - 1.).abs());
        let m = v - c;
        let (r, g, b) = match (h / 60.) as u32 {
            0 => (c, x, 0.),
            1 => (x, c, 0.),
            2 => (0., c, x),
            3 => (0., x, c),
            4 => (x, 0., c),
            _ => (c, 0., x),
        };
        Color { r: r + m, g: g + m, b: b + m, a }
    }

    fn to_hsl(self) -> (f32, f32, f32) {
        let (h, _, _) = self.to_hsv();
        let max = self.r.max(self.g).max(self.b);
        let min = self.r.min(self.g).min(self.b);
        let l = (max + min) / 2.;
        let s = if max == min { 0. } else { (max - min) / (1. - (2. * l - 1.).abs()) };
        (h, s, l)
    }

    fn from_hsl(h: f32, s: f32, l: f32, a: f32) -> Self {
        let v = l + s * l.min(1. - l);
        let sv = if v == 0. { 0. } else { 2. * (1. - l / v) };
        Color::from_hsv(h, sv, v, a)
    }

    fn byte(part: f32) -> u8 {
        (part.clamp(0., 1.) * 255.).round() as u8
    }
}

impl Format {
    /// `color` written this way: alpha only when it isn't opaque.
    pub fn write(self, color: Color) -> String {
        let opaque = color.a >= 0.999;
        let alpha = trim(color.a.clamp(0., 1.), 2);
        match self {
            Format::Hex { long, upper } => {
                let [r, g, b, a] = [color.r, color.g, color.b, color.a].map(Color::byte);
                // Short hex only when every part has two equal digits.
                let short = !long && [r, g, b, a].iter().all(|v| v % 17 == 0);
                let text = match (short, opaque) {
                    (true, true) => format!("#{:x}{:x}{:x}", r / 17, g / 17, b / 17),
                    (true, false) => format!("#{:x}{:x}{:x}{:x}", r / 17, g / 17, b / 17, a / 17),
                    (false, true) => format!("#{r:02x}{g:02x}{b:02x}"),
                    (false, false) => format!("#{r:02x}{g:02x}{b:02x}{a:02x}"),
                };
                if upper { text.to_uppercase() } else { text }
            }
            Format::Rgb { commas } => {
                let [r, g, b] = [color.r, color.g, color.b].map(Color::byte);
                match (commas, opaque) {
                    (true, true) => format!("rgb({r}, {g}, {b})"),
                    (true, false) => format!("rgba({r}, {g}, {b}, {alpha})"),
                    (false, true) => format!("rgb({r} {g} {b})"),
                    (false, false) => format!("rgb({r} {g} {b} / {alpha})"),
                }
            }
            Format::Hsl { commas } => {
                let (h, s, l) = color.to_hsl();
                let (h, s, l) = (h.round() as u32 % 360, (s * 100.).round() as u32, (l * 100.).round() as u32);
                match (commas, opaque) {
                    (true, true) => format!("hsl({h}, {s}%, {l}%)"),
                    (true, false) => format!("hsla({h}, {s}%, {l}%, {alpha})"),
                    (false, true) => format!("hsl({h} {s}% {l}%)"),
                    (false, false) => format!("hsl({h} {s}% {l}% / {alpha})"),
                }
            }
        }
    }
}

/// `value` with at most `places` decimals, without trailing zeros.
fn trim(value: f32, places: usize) -> String {
    let text = format!("{value:.places$}");
    let text = text.trim_end_matches('0').trim_end_matches('.');
    if text.is_empty() { "0".to_string() } else { text.to_string() }
}

/// A whole color in `text`: `#…`, `rgb(…)`, `rgba(…)`, `hsl(…)`, `hsla(…)`.
pub fn parse(text: &str) -> Option<(Color, Format)> {
    let text = text.trim();
    if let Some(hex) = text.strip_prefix('#') {
        return parse_hex(hex);
    }
    let open = text.find('(')?;
    let name = text[..open].trim().to_ascii_lowercase();
    let inner = text[open + 1..].strip_suffix(')')?;
    let commas = inner.contains(',');
    let parts: Vec<&str> = inner.split([',', ' ', '/']).map(str::trim).filter(|p| !p.is_empty()).collect();
    if !(3..=4).contains(&parts.len()) {
        return None;
    }
    let alpha = match parts.get(3) {
        Some(a) => number_or_percent(a, 1.)?,
        None => 1.,
    };
    match name.as_str() {
        "rgb" | "rgba" => {
            let [r, g, b] = [parts[0], parts[1], parts[2]].map(|p| number_or_percent(p, 255.).map(|v| v / 255.));
            Some((Color::rgba(r?, g?, b?, alpha), Format::Rgb { commas }))
        }
        "hsl" | "hsla" => {
            let h: f32 = parts[0].trim_end_matches("deg").parse().ok()?;
            let s = parts[1].strip_suffix('%')?.parse::<f32>().ok()? / 100.;
            let l = parts[2].strip_suffix('%')?.parse::<f32>().ok()? / 100.;
            Some((Color::from_hsl(h, s.clamp(0., 1.), l.clamp(0., 1.), alpha), Format::Hsl { commas }))
        }
        _ => None,
    }
}

/// `12` as is, `50%` as that share of `full`.
fn number_or_percent(text: &str, full: f32) -> Option<f32> {
    match text.strip_suffix('%') {
        Some(p) => Some(p.parse::<f32>().ok()? / 100. * full),
        None => text.parse().ok(),
    }
    .filter(|v: &f32| v.is_finite())
}

fn parse_hex(hex: &str) -> Option<(Color, Format)> {
    if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let digit = |i: usize| u8::from_str_radix(&hex[i..i + 1], 16).ok();
    let pair = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
    let (bytes, long) = match hex.len() {
        3 | 4 => ((0..hex.len()).map(|i| digit(i).map(|d| d * 17)).collect::<Option<Vec<u8>>>()?, false),
        6 | 8 => ((0..hex.len() / 2).map(|i| pair(i * 2)).collect::<Option<Vec<u8>>>()?, true),
        _ => return None,
    };
    let part = |i: usize| bytes.get(i).map_or(1., |&b| b as f32 / 255.);
    let upper = hex.chars().any(|c| c.is_ascii_uppercase());
    Some((Color::rgba(part(0), part(1), part(2), part(3)), Format::Hex { long, upper }))
}

/// The colors written in `line` (positions within it). A `#…` counts in a
/// value, not as a selector (`#main {`): after a `:` or `=` on the line,
/// with no `{` before the next `;`.
pub fn find(line: &str) -> Vec<(Range<usize>, Color, Format)> {
    let mut found = Vec::new();
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        let word_start = i == 0 || !(bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'-' || bytes[i - 1] == b'_');
        if c == b'#' {
            let end = i + 1 + bytes[i + 1..].iter().take_while(|b| b.is_ascii_alphanumeric()).count();
            if is_value(line, i, end)
                && let Some((color, format)) = parse_hex(&line[i + 1..end])
            {
                found.push((i..end, color, format));
            }
            i = end.max(i + 1);
            continue;
        }
        if word_start && (c == b'r' || c == b'h' || c == b'R' || c == b'H') {
            let lower = line[i..].to_ascii_lowercase();
            if ["rgb(", "rgba(", "hsl(", "hsla("].iter().any(|p| lower.starts_with(p))
                && let Some(close) = line[i..].find(')')
                && let Some((color, format)) = parse(&line[i..i + close + 1])
            {
                found.push((i..i + close + 1, color, format));
                i += close + 1;
                continue;
            }
        }
        i += 1;
    }
    found
}

fn is_value(line: &str, start: usize, end: usize) -> bool {
    let before = &line[..start];
    let after = &line[end..];
    let in_value = before.contains(':') || before.contains('=');
    let opens_block = match (after.find('{'), after.find(';')) {
        (Some(brace), Some(semi)) => brace < semi,
        (Some(_), None) => true,
        _ => false,
    };
    in_value && !opens_block
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: Color, b: Color) -> bool {
        [a.r - b.r, a.g - b.g, a.b - b.b, a.a - b.a].iter().all(|d| d.abs() < 0.01)
    }

    #[test]
    fn reads_and_writes_each_notation() {
        for text in ["#abc", "#ABCD", "#a1b2c3", "#A1B2C3D4", "rgb(10, 20, 30)", "rgba(10, 20, 30, 0.5)", "rgb(10 20 30)", "rgb(10 20 30 / 0.5)", "hsl(120, 50%, 40%)", "hsla(120, 50%, 40%, 0.25)", "hsl(200 30% 60%)"] {
            let (color, format) = parse(text).unwrap_or_else(|| panic!("{text}"));
            assert_eq!(format.write(color), text, "{text}");
        }
        assert_eq!(parse("rgb(100%, 0%, 50%)").unwrap().0, Color::rgba(1., 0., 0.5, 1.));
        assert_eq!(parse("#12345"), None);
        assert_eq!(parse("#ggg"), None);
        assert_eq!(parse("rgb(1, 2)"), None);
        assert_eq!(parse("cmyk(1, 2, 3)"), None);
    }

    #[test]
    fn writing_keeps_the_notation_and_adds_alpha_when_needed() {
        let red = Color::rgba(1., 0., 0., 1.);
        let half = Color::rgba(1., 0., 0., 0.5);
        assert_eq!(Format::Hex { long: false, upper: false }.write(red), "#f00");
        assert_eq!(Format::Hex { long: false, upper: true }.write(half), "#FF000080");
        assert_eq!(Format::Hex { long: true, upper: false }.write(red), "#ff0000");
        // Not expressible in short hex: long.
        assert_eq!(Format::Hex { long: false, upper: false }.write(Color::rgba(0.1, 0., 0., 1.)), "#1a0000");
        assert_eq!(Format::Rgb { commas: true }.write(half), "rgba(255, 0, 0, 0.5)");
        assert_eq!(Format::Rgb { commas: false }.write(half), "rgb(255 0 0 / 0.5)");
        assert_eq!(Format::Hsl { commas: true }.write(red), "hsl(0, 100%, 50%)");
    }

    #[test]
    fn hsv_round_trips() {
        for hex in ["#ff0000", "#00ff00", "#0000ff", "#123456", "#fedcba", "#808080", "#000000", "#ffffff", "#ff8800"] {
            let (color, _) = parse(hex).unwrap();
            let (h, s, v) = color.to_hsv();
            assert!(close(Color::from_hsv(h, s, v, 1.), color), "{hex}");
        }
        assert!(close(Color::from_hsv(240., 1., 1., 1.), Color::rgba(0., 0., 1., 1.)));
    }

    #[test]
    fn finds_colors_in_values_not_selectors() {
        let at = |line: &str| find(line).into_iter().map(|(r, _, _)| line[r].to_string()).collect::<Vec<_>>();
        assert_eq!(at("  color: #fff;"), ["#fff"]);
        assert_eq!(at("  border: 1px solid #A0B0C0; background: rgba(0, 0, 0, 0.5);"), ["#A0B0C0", "rgba(0, 0, 0, 0.5)"]);
        assert_eq!(at("#main { color: red; }"), Vec::<String>::new());
        assert_eq!(at("a:hover #nav {"), Vec::<String>::new());
        assert_eq!(at("<div style=\"color: hsl(10, 20%, 30%)\">"), ["hsl(10, 20%, 30%)"]);
        assert_eq!(at("  color: #ffff1;"), Vec::<String>::new());
        // A function whose name ends like one isn't one.
        assert_eq!(at("  x: torgb(1, 2, 3);"), Vec::<String>::new());
        assert_eq!(at("  --accent: #0af; --bg: RGB(1, 2, 3)"), ["#0af", "RGB(1, 2, 3)"]);
    }
}
