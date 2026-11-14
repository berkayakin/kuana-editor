//! Text encodings: how a file's bytes become the editor's text and back, so
//! files that aren't UTF-8 survive a save unchanged. Detection is simple: a
//! byte order mark, else UTF-8 if the bytes are valid, else Windows-1252
//! (which maps every byte, so even unknown legacy text round-trips).

use encoding_rs::{Encoding, UTF_8, UTF_16BE, UTF_16LE, WINDOWS_1252};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TextEncoding {
    pub encoding: &'static Encoding,
    /// Starts with a byte order mark (always for UTF-16).
    pub bom: bool,
}

impl TextEncoding {
    pub const UTF8: TextEncoding = TextEncoding { encoding: UTF_8, bom: false };

    /// The name for the status bar.
    pub fn label(self) -> String {
        match (self.encoding.name(), self.bom) {
            ("UTF-8", true) => "UTF-8 with BOM".into(),
            ("UTF-16LE", _) => "UTF-16 LE".into(),
            ("UTF-16BE", _) => "UTF-16 BE".into(),
            ("Shift_JIS", _) => "Shift JIS".into(),
            (name, _) => name.replace("windows", "Windows"),
        }
    }

    /// How sessions store it ("windows-1254", "UTF-8 BOM").
    pub fn key(self) -> String {
        if self.bom { format!("{} BOM", self.encoding.name()) } else { self.encoding.name().to_string() }
    }

    pub fn from_key(key: &str) -> Option<TextEncoding> {
        let (name, bom) = match key.strip_suffix(" BOM") {
            Some(name) => (name, true),
            None => (key, false),
        };
        Some(TextEncoding { encoding: Encoding::for_label(name.as_bytes())?, bom })
    }

    /// The name with what it's for, for the encoding menu.
    pub fn menu_label(self) -> String {
        let region = match self.encoding.name() {
            "windows-1252" => "Western",
            "ISO-8859-2" => "Central European",
            "windows-1251" => "Cyrillic",
            "windows-1254" => "Turkish",
            "Shift_JIS" => "Japanese",
            "EUC-KR" => "Korean",
            "GBK" => "Simplified Chinese",
            "Big5" => "Traditional Chinese",
            _ => return self.label(),
        };
        format!("{} ({region})", self.label())
    }

    fn is_utf16(self) -> bool {
        self.encoding == UTF_16LE || self.encoding == UTF_16BE
    }
}

/// What the status bar's encoding menu offers.
pub fn choices() -> Vec<TextEncoding> {
    let plain = |encoding| TextEncoding { encoding, bom: false };
    vec![
        TextEncoding::UTF8,
        TextEncoding { encoding: UTF_8, bom: true },
        TextEncoding { encoding: UTF_16LE, bom: true },
        TextEncoding { encoding: UTF_16BE, bom: true },
        plain(WINDOWS_1252),
        plain(encoding_rs::ISO_8859_2),
        plain(encoding_rs::WINDOWS_1251),
        plain(encoding_rs::WINDOWS_1254),
        plain(encoding_rs::SHIFT_JIS),
        plain(encoding_rs::EUC_KR),
        plain(encoding_rs::GBK),
        plain(encoding_rs::BIG5),
    ]
}

/// Decodes a file. `hint`: the encoding it was in before (a reload keeps a
/// legacy encoding the user picked rather than guessing again).
pub fn decode(bytes: &[u8], hint: Option<TextEncoding>) -> (String, TextEncoding) {
    if let Some((encoding, bom_len)) = Encoding::for_bom(bytes) {
        let (text, _) = encoding.decode_without_bom_handling(&bytes[bom_len..]);
        return (text.into_owned(), TextEncoding { encoding, bom: true });
    }
    if let Some(hint) = hint.filter(|h| h.encoding != UTF_8 && !h.is_utf16()) {
        return (decode_as(bytes, hint), hint);
    }
    match std::str::from_utf8(bytes) {
        Ok(text) => (text.to_string(), TextEncoding::UTF8),
        Err(_) => (decode_as(bytes, TextEncoding { encoding: WINDOWS_1252, bom: false }), TextEncoding { encoding: WINDOWS_1252, bom: false }),
    }
}

/// Decodes as `encoding` (Reload with Encoding), skipping its BOM if present.
pub fn decode_as(bytes: &[u8], encoding: TextEncoding) -> String {
    let body = match Encoding::for_bom(bytes) {
        Some((found, len)) if found == encoding.encoding => &bytes[len..],
        _ => bytes,
    };
    encoding.encoding.decode_without_bom_handling(body).0.into_owned()
}

/// Whether a file's `bytes` look binary rather than like text: a NUL byte
/// in the first 8000 (as Git and Find in Files tell), without a byte order
/// mark (UTF-16 text has NULs all over).
pub fn looks_binary(bytes: &[u8]) -> bool {
    Encoding::for_bom(bytes).is_none() && bytes[..bytes.len().min(8000)].contains(&0)
}

/// Text with LF line ends, as editors hold it: CRLF and a lone CR (old Mac
/// files, redrawn progress lines) both become LF. Lines then end where
/// language servers end them too (LSP counts a lone CR as a line end, the
/// rope and tree-sitter don't). A save writes LF, or CRLF for a file that had
/// any (`Document::uses_crlf`).
pub fn to_lf(text: String) -> String {
    if !text.contains('\r') {
        return text;
    }
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// Encodes for saving. Fails when the text has characters the encoding
/// can't represent, rather than writing something else in their place.
pub fn encode(text: &str, encoding: TextEncoding) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(text.len() + 3);
    if encoding.is_utf16() {
        let big = encoding.encoding == UTF_16BE;
        out.extend_from_slice(if big { &[0xFE, 0xFF] } else { &[0xFF, 0xFE] });
        for unit in text.encode_utf16() {
            out.extend_from_slice(&if big { unit.to_be_bytes() } else { unit.to_le_bytes() });
        }
        return Ok(out);
    }
    if encoding.bom && encoding.encoding == UTF_8 {
        out.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
    }
    let (bytes, _, unmappable) = encoding.encoding.encode(text);
    if unmappable {
        let bad = text.chars().find(|c| encoding.encoding.encode(&c.to_string()).2).unwrap_or('?');
        return Err(format!("“{bad}” can't be saved in {}. Choose another encoding in the status bar.", encoding.label()));
    }
    out.extend_from_slice(&bytes);
    Ok(out)
}

/// Writes text, given in pieces, to `path` in `encoding`: UTF-8 and UTF-16
/// piece by piece as they come, since they can hold any text; other
/// encodings encoded whole first, so a character one lacks fails the save
/// before the file is touched (see `encode`).
pub fn write_file<'a>(path: &std::path::Path, text: impl Iterator<Item = &'a str>, encoding: TextEncoding) -> std::io::Result<()> {
    use std::io::Write;
    if encoding.encoding != UTF_8 && !encoding.is_utf16() {
        let text: String = text.collect();
        return std::fs::write(path, encode(&text, encoding).map_err(std::io::Error::other)?);
    }
    let mut out = std::io::BufWriter::new(std::fs::File::create(path)?);
    if encoding.is_utf16() {
        let big = encoding.encoding == UTF_16BE;
        out.write_all(if big { &[0xFE, 0xFF] } else { &[0xFF, 0xFE] })?;
        let mut units = Vec::new();
        for piece in text {
            units.clear();
            units.extend(piece.encode_utf16().flat_map(|unit| if big { unit.to_be_bytes() } else { unit.to_le_bytes() }));
            out.write_all(&units)?;
        }
    } else {
        if encoding.bom {
            out.write_all(&[0xEF, 0xBB, 0xBF])?;
        }
        for piece in text {
            out.write_all(piece.as_bytes())?;
        }
    }
    out.into_inner().map_err(std::io::IntoInnerError::into_error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_each_kind_of_file() {
        let cases: [(&[u8], &str); 4] = [
            (b"plain \xc3\xa9", "UTF-8"),
            (b"\xef\xbb\xbfwith bom", "UTF-8 with BOM"),
            (b"\xff\xfeh\x00i\x00", "UTF-16 LE"),
            // Latin-1 é and the Windows-1252 euro sign: not UTF-8.
            (b"caf\xe9 \x80", "Windows-1252"),
        ];
        for (bytes, label) in cases {
            let (text, encoding) = decode(bytes, None);
            assert_eq!(encoding.label(), label);
            assert_eq!(encode(&text, encoding).unwrap(), bytes, "{label}");
        }
        assert_eq!(decode(b"caf\xe9 \x80", None).0, "café €");
    }

    #[test]
    fn keeps_a_chosen_legacy_encoding_and_refuses_unmappable_text() {
        let turkish = TextEncoding { encoding: encoding_rs::WINDOWS_1254, bom: false };
        // In Windows-1254, 0xFE is ş (not þ as in Windows-1252).
        assert_eq!(decode(b"\xfe", Some(turkish)), ("ş".to_string(), turkish));
        assert_eq!(decode_as(b"\xfe", turkish), "ş");
        assert!(encode("日本", turkish).unwrap_err().contains("日"));
        for encoding in choices() {
            assert_eq!(TextEncoding::from_key(&encoding.key()), Some(encoding));
        }
    }
}
