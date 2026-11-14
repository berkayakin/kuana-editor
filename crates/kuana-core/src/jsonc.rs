//! JSON with comments and trailing commas (`settings.json`, `devcontainer.json`).

/// Turns JSON with comments and trailing commas into plain JSON. Removed
/// characters become spaces, so error positions still match the original.
pub fn strip(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = text.as_bytes().to_vec();
    let mut i = 0;
    // Index of the last comma not yet followed by a value.
    let mut pending_comma: Option<usize> = None;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                pending_comma = None;
                i = skip_string(bytes, i);
                continue;
            }
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    out[i] = b' ';
                    i += 1;
                }
                continue;
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                let end = text[i + 2..].find("*/").map_or(bytes.len(), |e| i + 2 + e + 2);
                for b in &mut out[i..end] {
                    if *b != b'\n' {
                        *b = b' ';
                    }
                }
                i = end;
                continue;
            }
            b',' => pending_comma = Some(i),
            b'}' | b']' => {
                if let Some(c) = pending_comma.take() {
                    out[c] = b' ';
                }
            }
            b if b.is_ascii_whitespace() => {}
            _ => pending_comma = None,
        }
        i += 1;
    }
    // Only ASCII bytes were replaced, so this stays valid UTF-8.
    String::from_utf8(out).unwrap_or_default()
}

/// The index just past the string literal starting at `start`.
fn skip_string(bytes: &[u8], start: usize) -> usize {
    let mut i = start + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'"' => return i + 1,
            _ => i += 1,
        }
    }
    bytes.len()
}

#[cfg(test)]
mod tests {
    #[test]
    fn comments_and_trailing_commas_go() {
        let text = "{\n  // a\n  \"a\": \"//not\", /* b */\n  \"b\": [1, 2,],\n}";
        let value: serde_json::Value = serde_json::from_str(&super::strip(text)).unwrap();
        assert_eq!(value, serde_json::json!({ "a": "//not", "b": [1, 2] }));
    }
}
