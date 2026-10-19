//! A small, tolerant HTML scanner that reports structural mistakes and a few
//! accessibility/best-practice issues. Inline `<script>` blocks are handed to
//! the JavaScript linter.

use std::collections::HashMap;

use super::{Diagnostic, Severity, js};

const VOID: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr",
];

/// Elements whose end tag may be omitted per the HTML spec.
const OPTIONAL_END: &[&str] = &[
    "html", "head", "body", "p", "li", "dt", "dd", "option", "optgroup", "tr", "td", "th",
    "thead", "tbody", "tfoot", "colgroup", "rb", "rt", "rp",
];

const DEPRECATED: &[&str] = &[
    "acronym", "applet", "basefont", "big", "blink", "center", "dir", "font", "frame",
    "frameset", "marquee", "nobr", "strike", "tt",
];

struct OpenTag {
    name: String,
    name_range: std::ops::Range<usize>,
}

struct Attr {
    name: String,
    name_range: std::ops::Range<usize>,
    value: Option<(String, std::ops::Range<usize>)>,
}

pub fn lint(src: &str) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    let mut diag = |range: std::ops::Range<usize>, severity, message: String, rule: &'static str| {
        out.push(Diagnostic {
            range,
            severity,
            message: message.into(),
            rule: rule.into(),
            unnecessary: false,
        })
    };

    let bytes = src.as_bytes();
    let mut stack: Vec<OpenTag> = Vec::new();
    let mut ids: HashMap<String, usize> = HashMap::new();
    let mut seen_doctype = false;
    let mut seen_content = false;
    let mut html_tag: Option<std::ops::Range<usize>> = None;
    let mut head_tag: Option<std::ops::Range<usize>> = None;
    let mut has_title = false;
    let mut scripts: Vec<(usize, usize)> = Vec::new();

    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'<' {
            if !bytes[i].is_ascii_whitespace() {
                seen_content = true;
            }
            i += 1;
            continue;
        }

        // Comment
        if src[i..].starts_with("<!--") {
            match src[i + 4..].find("-->") {
                Some(end) => i = i + 4 + end + 3,
                None => {
                    diag(i..i + 4, Severity::Error, "Unclosed comment".into(), "unclosed-comment");
                    break;
                }
            }
            continue;
        }

        // Doctype / other declarations
        if src[i..].starts_with("<!") {
            let end = src[i..].find('>').map(|e| i + e + 1).unwrap_or(bytes.len());
            if src[i + 2..end].to_ascii_lowercase().starts_with("doctype") {
                if seen_content {
                    diag(i..end, Severity::Warning, "<!DOCTYPE> must be the first thing in the document".into(), "doctype-first");
                }
                seen_doctype = true;
            }
            i = end;
            continue;
        }

        let closing = bytes.get(i + 1) == Some(&b'/');
        let name_start = if closing { i + 2 } else { i + 1 };
        let name_end = scan_while(bytes, name_start, |b| b.is_ascii_alphanumeric() || b == b'-' || b == b':');
        if name_end == name_start {
            // A literal '<' in text, e.g. `a < b`.
            seen_content = true;
            i += 1;
            continue;
        }
        seen_content = true;
        let name = src[name_start..name_end].to_ascii_lowercase();
        let name_range = name_start..name_end;

        // Parse attributes up to '>'.
        let mut attrs: Vec<Attr> = Vec::new();
        let mut j = name_end;
        let mut self_closing = false;
        let mut terminated = false;
        while j < bytes.len() {
            j = scan_while(bytes, j, |b| b.is_ascii_whitespace());
            match bytes.get(j) {
                None => break,
                Some(b'>') => {
                    terminated = true;
                    j += 1;
                    break;
                }
                Some(b'/') if bytes.get(j + 1) == Some(&b'>') => {
                    self_closing = true;
                    terminated = true;
                    j += 2;
                    break;
                }
                Some(b'<') => break,
                _ => {}
            }
            let a_start = j;
            let a_end = scan_while(bytes, j, |b| {
                !b.is_ascii_whitespace() && !matches!(b, b'=' | b'>' | b'/' | b'<' | b'"' | b'\'')
            });
            if a_end == a_start {
                j += 1;
                continue;
            }
            let mut attr = Attr {
                name: src[a_start..a_end].to_ascii_lowercase(),
                name_range: a_start..a_end,
                value: None,
            };
            j = scan_while(bytes, a_end, |b| b.is_ascii_whitespace());
            if bytes.get(j) == Some(&b'=') {
                j = scan_while(bytes, j + 1, |b| b.is_ascii_whitespace());
                match bytes.get(j) {
                    Some(&q @ (b'"' | b'\'')) => {
                        let v_start = j + 1;
                        match src[v_start..].find(q as char) {
                            Some(e) => {
                                attr.value = Some((src[v_start..v_start + e].to_string(), v_start..v_start + e));
                                j = v_start + e + 1;
                            }
                            None => {
                                diag(j..j + 1, Severity::Error, "Unterminated attribute value".into(), "unterminated-attr");
                                j = bytes.len();
                            }
                        }
                    }
                    _ => {
                        let v_end = scan_while(bytes, j, |b| !b.is_ascii_whitespace() && b != b'>');
                        attr.value = Some((src[j..v_end].to_string(), j..v_end));
                        j = v_end;
                    }
                }
            } else if j == a_end {
                // no whitespace consumed and not '=': advance to avoid loops
            }
            attrs.push(attr);
        }

        if !terminated {
            diag(i..name_end, Severity::Error, format!("Tag <{name}> is not terminated with '>'"), "unterminated-tag");
            i = j.max(name_end);
            continue;
        }
        let tag_end = j;

        if closing {
            if VOID.contains(&name.as_str()) {
                diag(i..tag_end, Severity::Error, format!("<{name}> is a void element and must not have a closing tag"), "void-closing-tag");
            } else if let Some(pos) = stack.iter().rposition(|t| t.name == name) {
                for open in stack.drain(pos + 1..) {
                    if !OPTIONAL_END.contains(&open.name.as_str()) {
                        diag(
                            open.name_range.clone(),
                            Severity::Error,
                            format!("<{}> is not closed before </{name}>", open.name),
                            "unclosed-tag",
                        );
                    }
                }
                stack.pop();
            } else {
                diag(name_range, Severity::Error, format!("Unexpected closing tag </{name}>"), "stray-closing-tag");
            }
            i = tag_end;
            continue;
        }

        // Opening tag checks.
        let mut seen_attrs: HashMap<&str, ()> = HashMap::new();
        for a in &attrs {
            if seen_attrs.insert(&a.name, ()).is_some() {
                diag(a.name_range.clone(), Severity::Error, format!("Duplicate attribute '{}'", a.name), "duplicate-attr");
            }
            if a.name == "id"
                && let Some((v, r)) = &a.value
            {
                if v.is_empty() {
                    diag(r.start.saturating_sub(1)..r.end + 1, Severity::Warning, "Empty id attribute".into(), "empty-id");
                } else if ids.insert(v.clone(), r.start).is_some() {
                    diag(r.clone(), Severity::Warning, format!("Duplicate id '{v}'"), "duplicate-id");
                }
            }
        }
        let attr = |n: &str| attrs.iter().find(|a| a.name == n);

        match name.as_str() {
            "img" if attr("alt").is_none() => {
                diag(name_range.clone(), Severity::Warning, "<img> is missing an alt attribute".into(), "img-alt");
            }
            "html" => {
                html_tag = Some(name_range.clone());
                if attr("lang").is_none() {
                    diag(name_range.clone(), Severity::Info, "<html> should declare a lang attribute".into(), "html-lang");
                }
            }
            "head" => head_tag = Some(name_range.clone()),
            "title" => has_title = true,
            "a" if attr("target").is_some_and(|t| t.value.as_ref().is_some_and(|v| v.0 == "_blank"))
                && attr("rel").is_none() =>
            {
                diag(name_range.clone(), Severity::Info, "target=\"_blank\" without rel=\"noopener\"".into(), "noopener");
            }
            _ => {}
        }
        if DEPRECATED.contains(&name.as_str()) {
            diag(name_range.clone(), Severity::Warning, format!("<{name}> is obsolete"), "obsolete-tag");
        }

        // Raw text elements: skip their content verbatim.
        if matches!(name.as_str(), "script" | "style" | "textarea" | "title") && !self_closing {
            let close = format!("</{name}");
            let rel = find_ci(&src[tag_end..], &close);
            let content_end = rel.map(|r| tag_end + r).unwrap_or(bytes.len());
            if name == "script" {
                let is_js = attr("type")
                    .and_then(|t| t.value.as_ref())
                    .map(|(v, _)| {
                        let v = v.trim().to_ascii_lowercase();
                        v.is_empty() || v == "module" || v.contains("javascript") || v.contains("ecmascript")
                    })
                    .unwrap_or(true);
                if is_js && attr("src").is_none() {
                    scripts.push((tag_end, content_end));
                }
            }
            match rel {
                Some(_) => {
                    let after = src[content_end..].find('>').map(|e| content_end + e + 1).unwrap_or(bytes.len());
                    i = after;
                }
                None => {
                    diag(name_range, Severity::Error, format!("<{name}> is not closed"), "unclosed-tag");
                    i = bytes.len();
                }
            }
            continue;
        }

        if !self_closing && !VOID.contains(&name.as_str()) {
            stack.push(OpenTag { name, name_range });
        }
        i = tag_end;
    }

    for open in stack {
        if !OPTIONAL_END.contains(&open.name.as_str()) {
            diag(open.name_range, Severity::Error, format!("<{}> is never closed", open.name), "unclosed-tag");
        }
    }

    if let Some(html) = html_tag
        && !seen_doctype
    {
        diag(html.start.saturating_sub(1)..html.end, Severity::Info, "Missing <!DOCTYPE html>".into(), "doctype");
    }
    if let Some(head) = head_tag
        && !has_title
    {
        diag(head, Severity::Info, "<head> has no <title>".into(), "title");
    }

    for (start, end) in scripts {
        out.extend(js::lint(&src[start..end], start, js::Context::InlineScript, js::Dialect::JavaScript));
    }

    out
}

fn scan_while(bytes: &[u8], mut i: usize, f: impl Fn(u8) -> bool) -> usize {
    while i < bytes.len() && f(bytes[i]) {
        i += 1;
    }
    i
}

fn find_ci(haystack: &str, needle: &str) -> Option<usize> {
    let h = haystack.as_bytes();
    let n = needle.as_bytes();
    if n.len() > h.len() {
        return None;
    }
    (0..=h.len() - n.len()).find(|&i| h[i..i + n.len()].eq_ignore_ascii_case(n))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(src: &str) -> Vec<String> {
        lint(src).into_iter().map(|d| d.rule.into_owned()).collect()
    }

    #[test]
    fn structure() {
        assert!(rules("<div><span></div>").contains(&"unclosed-tag".to_string()));
        assert!(rules("<div></span></div>").contains(&"stray-closing-tag".to_string()));
        assert!(rules("<br></br>").contains(&"void-closing-tag".to_string()));
        assert!(rules("<div id=a></div><p id=\"a\"></p>").contains(&"duplicate-id".to_string()));
        assert!(rules("<div class=a class=b></div>").contains(&"duplicate-attr".to_string()));
        assert!(rules("<img src=x.png>").contains(&"img-alt".to_string()));
        assert!(rules("<ul><li>a<li>b</ul>").is_empty());
        assert!(rules("<script>if (a = 1) {}</script>").contains(&"no-cond-assign".to_string()));
        assert!(rules("<p>a < b</p>").is_empty());
    }
}
