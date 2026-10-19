//! Markdown preview: the document parsed with pulldown-cmark into a few
//! block kinds, drawn with GPUI in a pane item of its own (`MarkdownPreview`,
//! in `view.rs`). Code blocks are highlighted with the editor's own grammars;
//! local images show; links open in the browser. HTML isn't rendered as such:
//! the tags READMEs use (emphasis, links, images, line breaks, centered
//! blocks, headings, tables, details) map onto the same blocks and styles,
//! comments are hidden, and other tags are dropped with their text kept.
//! Code blocks have Copy, and shell ones Run (in a Run tab, see `PreviewEvent`).

use std::{
    ops::Range,
    path::{Path, PathBuf},
    rc::Rc,
};

use gpui::{
    AnyElement, App, FontStyle, FontWeight, HighlightStyle, InteractiveText, IntoElement, SharedString, StrikethroughStyle,
    StyledText, UnderlineStyle, Window, div, font, img, prelude::*, px,
};
use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ropey::Rope;

use crate::{
    assets::Icon,
    syntax::{Highlight, Language, Syntax},
    theme::Theme,
    ui::{HoverFill, icon_el, tooltip_view},
};

mod view;
pub use view::{MarkdownPreview, PreviewEvent};

/// Text with inline styling: byte ranges of `text` and how they look.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Inline {
    pub text: String,
    pub styles: Vec<(Range<usize>, Style)>,
    /// Links and where they go.
    pub links: Vec<(Range<usize>, String)>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Style {
    Bold,
    Italic,
    Code,
    Strike,
    Link,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Block {
    Heading(u8, Inline),
    Paragraph(Inline),
    /// A paragraph that's just an image; `path` is the local file it shows,
    /// `width` an HTML image's width in pixels.
    Image { src: String, alt: String, path: Option<PathBuf>, width: Option<f32> },
    /// A fenced block, highlighted when its language is one Kuana knows.
    /// `command` is what Run runs: a shell block's commands (see `shell_command`).
    Code { lang: String, text: String, highlights: Vec<(Range<usize>, Highlight)>, command: Option<String> },
    Quote(Vec<Block>),
    /// `start`: the first number of an ordered list.
    List { start: Option<u64>, items: Vec<Item> },
    Table { aligns: Vec<Alignment>, rows: Vec<Vec<Inline>> },
    Rule,
    /// HTML's `align="center"` (and `<center>`).
    Center(Vec<Block>),
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Item {
    /// A task list item: done or not.
    pub task: Option<bool>,
    pub blocks: Vec<Block>,
}

/// The document's blocks. Code is highlighted and images found here, once per
/// version, not per frame; `base` is the document's folder (for relative images).
pub fn parse(markdown: &str, base: Option<&Path>) -> Vec<Block> {
    let options = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let mut builder = Builder { base: base.map(Path::to_path_buf), ..Builder::default() };
    for event in Parser::new_ext(markdown, options) {
        builder.event(event);
    }
    builder.finish()
}

/// Turns the event stream into blocks. Containers (quotes, lists, items)
/// are a stack; text collects into the innermost open inline.
#[derive(Default)]
struct Builder {
    /// Open containers and the blocks gathered inside each; the bottom is the document.
    stack: Vec<(Container, Vec<Block>)>,
    inline: Option<Inline>,
    /// Inline styles opened and not closed yet: where they started.
    open: Vec<(Style, usize, Option<String>)>,
    code: Option<(String, String)>,
    image: Option<(String, String)>,
    table: Option<(Vec<Alignment>, Vec<Vec<Inline>>)>,
    heading: Option<u8>,
    base: Option<PathBuf>,
    /// An HTML block's text, handled whole when it ends (a tag can span lines).
    html: Option<String>,
}

enum Container {
    Document,
    Quote,
    List(Option<u64>, Vec<Item>),
    Item(Option<bool>),
    /// An HTML block element (`<p>`, `<div>`, `<h2>`…); `center`: its contents are centered.
    Html { center: bool },
}

impl Builder {
    fn blocks(&mut self) -> &mut Vec<Block> {
        if self.stack.is_empty() {
            self.stack.push((Container::Document, Vec::new()));
        }
        &mut self.stack.last_mut().unwrap().1
    }

    fn text(&mut self, text: &str) {
        if let Some((_, code)) = &mut self.code {
            code.push_str(text);
        } else if let Some((_, alt)) = &mut self.image {
            alt.push_str(text);
        } else {
            self.inline.get_or_insert_with(Inline::default).text.push_str(text);
        }
    }

    /// Ends the current paragraph-like text (also for "tight" list items, whose text has no paragraph).
    fn flush_inline(&mut self) {
        // Styles don't cross paragraphs; one still open is unbalanced HTML.
        self.open.clear();
        if let Some(mut inline) = self.inline.take().filter(|i| !i.text.trim().is_empty()) {
            // HTML's line breaks before a closing tag leave a space.
            let len = inline.text.trim_end().len();
            inline.text.truncate(len);
            for range in inline.styles.iter_mut().map(|(r, _)| r).chain(inline.links.iter_mut().map(|(r, _)| r)) {
                range.end = range.end.min(len);
            }
            self.blocks().push(Block::Paragraph(inline));
        }
    }

    fn event(&mut self, event: Event) {
        match event {
            Event::Start(Tag::HtmlBlock) => self.html = Some(String::new()),
            Event::End(TagEnd::HtmlBlock) => {
                if let Some(html) = self.html.take() {
                    self.html(&html);
                }
            }
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(t) => self.text(&t),
            Event::Code(t) => {
                let inline = self.inline.get_or_insert_with(Inline::default);
                let start = inline.text.len();
                inline.text.push_str(&t);
                inline.styles.push((start..inline.text.len(), Style::Code));
            }
            Event::SoftBreak => self.text(" "),
            Event::HardBreak => self.text("\n"),
            Event::Rule => {
                self.flush_inline();
                self.blocks().push(Block::Rule);
            }
            Event::TaskListMarker(done) => {
                if let Some((Container::Item(task), _)) = self.stack.last_mut() {
                    *task = Some(done);
                }
            }
            Event::Html(t) | Event::InlineHtml(t) => match &mut self.html {
                Some(block) => block.push_str(&t),
                None => self.html(&t),
            },
            _ => {}
        }
    }

    fn start(&mut self, tag: Tag) {
        let at = self.inline.as_ref().map_or(0, |i| i.text.len());
        match tag {
            Tag::Paragraph => self.flush_inline(),
            Tag::Heading { level, .. } => {
                self.flush_inline();
                self.heading = Some(heading_level(level));
            }
            Tag::BlockQuote(_) => {
                self.flush_inline();
                self.stack.push((Container::Quote, Vec::new()));
            }
            Tag::CodeBlock(kind) => {
                self.flush_inline();
                let lang = match kind {
                    CodeBlockKind::Fenced(info) => info.split_whitespace().next().unwrap_or("").to_string(),
                    CodeBlockKind::Indented => String::new(),
                };
                self.code = Some((lang, String::new()));
            }
            Tag::List(start) => {
                self.flush_inline();
                self.stack.push((Container::List(start, Vec::new()), Vec::new()));
            }
            Tag::Item => {
                self.flush_inline();
                self.stack.push((Container::Item(None), Vec::new()));
            }
            Tag::Table(aligns) => {
                self.flush_inline();
                self.table = Some((aligns, Vec::new()));
            }
            Tag::TableHead | Tag::TableRow => {
                if let Some((_, rows)) = &mut self.table {
                    rows.push(Vec::new());
                }
            }
            Tag::TableCell => self.inline = Some(Inline::default()),
            Tag::Emphasis => self.open.push((Style::Italic, at, None)),
            Tag::Strong => self.open.push((Style::Bold, at, None)),
            Tag::Strikethrough => self.open.push((Style::Strike, at, None)),
            Tag::Link { dest_url, .. } => self.open.push((Style::Link, at, Some(dest_url.to_string()))),
            Tag::Image { dest_url, .. } => self.image = Some((dest_url.to_string(), String::new())),
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        // An HTML element left open inside a quote or list ends with it.
        if matches!(tag, TagEnd::Item | TagEnd::List(_) | TagEnd::BlockQuote(_)) {
            while matches!(self.stack.last(), Some((Container::Html { .. }, _))) {
                self.close_html_block();
            }
        }
        match tag {
            TagEnd::Paragraph => self.flush_inline(),
            TagEnd::Heading(_) => {
                let inline = self.inline.take().unwrap_or_default();
                let level = self.heading.take().unwrap_or(1);
                self.blocks().push(Block::Heading(level, inline));
            }
            TagEnd::Table => {
                if let Some((aligns, rows)) = self.table.take() {
                    self.blocks().push(Block::Table { aligns, rows });
                }
            }
            TagEnd::CodeBlock => {
                if let Some((lang, text)) = self.code.take() {
                    let text = text.trim_end_matches('\n').to_string();
                    let highlights = code_highlights(&lang, &text);
                    let command = shell_command(&lang, &text);
                    self.blocks().push(Block::Code { lang, text, highlights, command });
                }
            }
            TagEnd::BlockQuote(_) => {
                self.flush_inline();
                if let Some((_, blocks)) = self.stack.pop() {
                    self.blocks().push(Block::Quote(blocks));
                }
            }
            TagEnd::Item => {
                self.flush_inline();
                if let Some((Container::Item(task), blocks)) = self.stack.pop()
                    && let Some((Container::List(_, items), _)) = self.stack.last_mut()
                {
                    items.push(Item { task, blocks });
                }
            }
            TagEnd::List(_) => {
                if let Some((Container::List(start, items), _)) = self.stack.pop() {
                    self.blocks().push(Block::List { start, items });
                }
            }
            TagEnd::TableCell => {
                let cell = self.inline.take().unwrap_or_default();
                if let Some((_, rows)) = &mut self.table
                    && let Some(row) = rows.last_mut()
                {
                    row.push(cell);
                }
            }
            TagEnd::Image => {
                if let Some((src, alt)) = self.image.take() {
                    // On its own it's a picture; inside text, its description.
                    let alone = self.inline.as_ref().is_none_or(|i| i.text.trim().is_empty());
                    if alone && self.table.is_none() {
                        self.inline = None;
                        let path = self.local_image(&src);
                        self.blocks().push(Block::Image { src, alt, path, width: None });
                    } else {
                        self.text(&alt);
                    }
                }
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough | TagEnd::Link => self.close_style(None),
            _ => {}
        }
    }

    /// Ends the innermost open style (`Some`: only if it's that one).
    fn close_style(&mut self, only: Option<Style>) {
        if only.is_some_and(|style| self.open.last().is_none_or(|(open, ..)| *open != style)) {
            return;
        }
        if let Some((style, start, url)) = self.open.pop() {
            let inline = self.inline.get_or_insert_with(Inline::default);
            let range = start..inline.text.len();
            if let Some(url) = url {
                inline.links.push((range.clone(), url));
            }
            inline.styles.push((range, style));
        }
    }

    fn local_image(&self, src: &str) -> Option<PathBuf> {
        self.base.as_ref().map(|b| b.join(src)).filter(|p| !src.contains("://") && p.is_file())
    }

    /// HTML, from a block or an inline tag: its tags as the Markdown they
    /// stand for, its text as text.
    fn html(&mut self, html: &str) {
        if self.code.is_some() {
            return;
        }
        for token in html_tokens(html) {
            match token {
                HtmlToken::Text(text) => {
                    // Whitespace runs collapse to a space, as in HTML; none starts a line.
                    let mut collapsed = String::with_capacity(text.len());
                    for c in text.chars() {
                        if !c.is_whitespace() {
                            collapsed.push(c);
                        } else if !collapsed.ends_with(' ') {
                            collapsed.push(' ');
                        }
                    }
                    let at_start = self.inline.as_ref().is_none_or(|i| i.text.is_empty() || i.text.ends_with(['\n', ' ']));
                    let text = decode_entities(if at_start { collapsed.trim_start() } else { &collapsed });
                    if !text.is_empty() {
                        self.text(&text);
                    }
                }
                HtmlToken::Tag(tag) => self.html_tag(&tag),
            }
        }
    }

    fn html_tag(&mut self, tag: &HtmlTag) {
        let at = self.inline.as_ref().map_or(0, |i| i.text.len());
        let style = match tag.name.as_str() {
            "b" | "strong" => Some(Style::Bold),
            "i" | "em" => Some(Style::Italic),
            "s" | "del" | "strike" => Some(Style::Strike),
            "code" | "kbd" | "tt" => Some(Style::Code),
            "a" => Some(Style::Link),
            _ => None,
        };
        if let Some(style) = style {
            if tag.closing {
                self.close_style(Some(style));
            } else {
                // A link without an href (an anchor) is plain text.
                let url = tag.attr("href").map(str::to_string);
                if style != Style::Link || url.is_some() {
                    self.open.push((style, at, url));
                }
            }
            return;
        }
        let heading = tag.name.strip_prefix('h').and_then(|n| n.parse::<u8>().ok()).filter(|n| (1..=6).contains(n));
        match tag.name.as_str() {
            "br" => self.text("\n"),
            "hr" => self.event(Event::Rule),
            "img" => {
                let src = tag.attr("src").unwrap_or_default().to_string();
                let alt = tag.attr("alt").unwrap_or_default().to_string();
                let path = self.local_image(&src);
                let alone = self.inline.as_ref().is_none_or(|i| i.text.trim().is_empty());
                // A picture of its own; a remote one (badges) or one inside text, its description.
                if path.is_some() && alone && self.table.is_none() {
                    self.inline = None;
                    let width = tag.attr("width").and_then(|w| w.trim_end_matches("px").parse::<f32>().ok());
                    self.blocks().push(Block::Image { src, alt, path, width });
                } else if !alt.is_empty() {
                    self.text(&alt);
                }
            }
            "p" | "div" | "center" | "details" | "section" | "header" | "footer" => {
                self.flush_inline();
                if tag.closing {
                    self.close_html_block();
                } else {
                    let center = tag.name == "center" || tag.attr("align").is_some_and(|a| a.eq_ignore_ascii_case("center"));
                    self.stack.push((Container::Html { center }, Vec::new()));
                }
            }
            "summary" => {
                // A details block shows open; its summary is a bold line.
                if tag.closing {
                    self.close_style(Some(Style::Bold));
                    self.flush_inline();
                } else {
                    self.flush_inline();
                    self.open.push((Style::Bold, 0, None));
                }
            }
            _ if heading.is_some() => {
                if tag.closing {
                    if self.heading.is_some() {
                        self.end(TagEnd::Heading(HeadingLevel::H1));
                    }
                    self.close_html_block();
                } else {
                    self.flush_inline();
                    let center = tag.attr("align").is_some_and(|a| a.eq_ignore_ascii_case("center"));
                    self.stack.push((Container::Html { center }, Vec::new()));
                    self.heading = heading;
                }
            }
            "table" if tag.closing => self.end(TagEnd::Table),
            "table" => {
                self.flush_inline();
                self.table = Some((Vec::new(), Vec::new()));
            }
            "tr" if !tag.closing => self.start(Tag::TableRow),
            "td" | "th" if tag.closing => self.end(TagEnd::TableCell),
            "td" | "th" => self.start(Tag::TableCell),
            "li" if !tag.closing => {
                self.flush_inline();
                self.text("• ");
            }
            "li" => self.flush_inline(),
            _ => {}
        }
    }

    /// Closes the innermost HTML block element: centered contents as a
    /// `Center` block, others in place.
    fn close_html_block(&mut self) {
        self.flush_inline();
        if matches!(self.stack.last(), Some((Container::Html { .. }, _)))
            && let Some((Container::Html { center }, blocks)) = self.stack.pop()
        {
            if center {
                self.blocks().push(Block::Center(blocks));
            } else {
                self.blocks().extend(blocks);
            }
        }
    }

    fn finish(mut self) -> Vec<Block> {
        self.flush_inline();
        // Anything left open (malformed input) folds into the document.
        while self.stack.len() > 1 {
            let (_, blocks) = self.stack.pop().unwrap();
            self.blocks().extend(blocks);
        }
        self.stack.pop().map(|(_, blocks)| blocks).unwrap_or_default()
    }
}

enum HtmlToken {
    Text(String),
    Tag(HtmlTag),
}

struct HtmlTag {
    /// Lowercase.
    name: String,
    closing: bool,
    attrs: Vec<(String, String)>,
}

impl HtmlTag {
    fn attr(&self, name: &str) -> Option<&str> {
        self.attrs.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }
}

/// Splits HTML into text and tags; comments, doctypes and stray `<` are dropped
/// or kept as text the way a browser would.
fn html_tokens(html: &str) -> Vec<HtmlToken> {
    let mut tokens = Vec::new();
    let mut rest = html;
    while !rest.is_empty() {
        let Some(lt) = rest.find('<') else {
            tokens.push(HtmlToken::Text(rest.to_string()));
            break;
        };
        if lt > 0 {
            tokens.push(HtmlToken::Text(rest[..lt].to_string()));
        }
        rest = &rest[lt..];
        if let Some(comment) = rest.strip_prefix("<!--") {
            rest = comment.find("-->").map_or("", |end| &comment[end + 3..]);
            continue;
        }
        // The tag ends at the first `>` outside quotes.
        let mut quote = None;
        let end = rest.char_indices().skip(1).find(|&(_, c)| match quote {
            Some(q) if c == q => {
                quote = None;
                false
            }
            Some(_) => false,
            None if c == '"' || c == '\'' => {
                quote = Some(c);
                false
            }
            None => c == '>',
        });
        match end.and_then(|(end, _)| parse_tag(&rest[1..end]).map(|tag| (end, tag))) {
            Some((end, tag)) => {
                tokens.push(HtmlToken::Tag(tag));
                rest = &rest[end + 1..];
            }
            // Not a tag ("a < b"): text.
            None => {
                tokens.push(HtmlToken::Text("<".into()));
                rest = &rest[1..];
            }
        }
    }
    tokens
}

/// The inside of `<…>`: `/name`, `name attr="v" attr2=v bare /`, `!doctype`.
fn parse_tag(inner: &str) -> Option<HtmlTag> {
    let (closing, inner) = match inner.strip_prefix('/') {
        Some(rest) => (true, rest),
        None => (false, inner),
    };
    let inner = inner.trim_end_matches('/');
    let name_end = inner.find(|c: char| !c.is_ascii_alphanumeric() && c != '-').unwrap_or(inner.len());
    let name = inner[..name_end].to_ascii_lowercase();
    if name.is_empty() || !name.starts_with(|c: char| c.is_ascii_alphabetic()) {
        // `<!doctype …>`, `<?xml …>` aren't shown; anything else isn't a tag.
        return inner.starts_with(['!', '?']).then(|| HtmlTag { name: String::new(), closing, attrs: Vec::new() });
    }
    let mut attrs = Vec::new();
    let mut rest = inner[name_end..].trim_start();
    while !rest.is_empty() {
        let key_end = rest.find(|c: char| c.is_whitespace() || c == '=').unwrap_or(rest.len());
        let key = rest[..key_end].to_ascii_lowercase();
        rest = rest[key_end..].trim_start();
        let value = if let Some(after) = rest.strip_prefix('=') {
            let after = after.trim_start();
            let (value, next) = match after.chars().next() {
                Some(q @ ('"' | '\'')) => {
                    let body = &after[1..];
                    let close = body.find(q).unwrap_or(body.len());
                    (&body[..close], body.get(close + 1..).unwrap_or(""))
                }
                _ => {
                    let end = after.find(char::is_whitespace).unwrap_or(after.len());
                    (&after[..end], &after[end..])
                }
            };
            rest = next.trim_start();
            decode_entities(value)
        } else {
            String::new()
        };
        if !key.is_empty() {
            attrs.push((key, value));
        } else {
            break;
        }
    }
    Some(HtmlTag { name, closing, attrs })
}

/// The entities HTML in Markdown uses.
fn decode_entities(text: &str) -> String {
    if !text.contains('&') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        let decoded = rest.find(';').filter(|&semi| semi <= 10).and_then(|semi| {
            let name = &rest[1..semi];
            let c = match name {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                "nbsp" => Some('\u{a0}'),
                "copy" => Some('©'),
                "mdash" => Some('—'),
                "ndash" => Some('–'),
                _ => match name.strip_prefix('#') {
                    Some(hex) if hex.starts_with(['x', 'X']) => u32::from_str_radix(&hex[1..], 16).ok().and_then(char::from_u32),
                    Some(dec) => dec.parse().ok().and_then(char::from_u32),
                    None => None,
                },
            };
            c.map(|c| (c, semi))
        });
        match decoded {
            Some((c, semi)) => {
                out.push(c);
                rest = &rest[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn heading_level(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

// ---- Drawing -------------------------------------------------------------------------

/// How to draw.
pub struct Draw {
    /// 1.0 is the UI font size.
    pub zoom: f32,
    /// What a code block's buttons do; without it they don't show.
    pub on_code: Option<Rc<dyn Fn(CodeAction, &mut Window, &mut App)>>,
    /// The code block whose Copy button shows a check (by its `ix`).
    pub copied: Option<usize>,
}

/// A code block's button was clicked.
pub enum CodeAction {
    /// The block (by its `ix`, for `Draw::copied`) and its text.
    Copy { ix: usize, text: String },
    /// Its `command`.
    Run(String),
}

pub fn render(blocks: &[Block], d: &Draw, cx: &App) -> AnyElement {
    let theme = Theme::get(cx);
    let size = (theme.ui_font_size + 1.) * d.zoom;
    div()
        .flex()
        .flex_col()
        .gap(px(12. * d.zoom))
        .text_size(px(size))
        .line_height(px(size * 1.6))
        .text_color(theme.text)
        .children(blocks.iter().enumerate().map(|(ix, b)| render_block(b, ix, d, cx)))
        .into_any_element()
}

fn render_block(block: &Block, ix: usize, d: &Draw, cx: &App) -> AnyElement {
    let theme = Theme::get(cx);
    match block {
        Block::Heading(level, inline) => {
            let size = match level {
                1 => 26.,
                2 => 21.,
                3 => 18.,
                4 => 16.,
                _ => 14.5,
            } * d.zoom;
            div()
                .pt(px(if *level <= 2 { 8. } else { 4. }))
                .text_size(px(size))
                .line_height(px(size * 1.3))
                .font_weight(FontWeight::SEMIBOLD)
                .when(*level <= 2, |el| el.pb_1p5().border_b_1().border_color(theme.border_subtle))
                .child(render_inline(inline, ("md-heading", ix), cx))
                .into_any_element()
        }
        Block::Paragraph(inline) => render_inline(inline, ("md-p", ix), cx),
        Block::Image { src, alt, path, width } => {
            match path {
                Some(path) => img(path.clone())
                    .max_w_full()
                    .when_some(*width, |el, w| el.w(px(w * d.zoom)))
                    .rounded(px(6.))
                    .into_any_element(),
                None => div().text_color(theme.text_dim).child(format!("[{}]", if alt.is_empty() { src } else { alt })).into_any_element(),
            }
        }
        Block::Code { text, highlights, command, .. } => {
            let group = SharedString::from(format!("md-code-{ix}"));
            let line = (theme.editor_font_size - 1.) * d.zoom * 1.45;
            // The buttons sit centered on the first line, so a one-line
            // block is at least as tall as they are with a margin around them
            // (its line centered too).
            let pad_y = if d.on_code.is_some() { ((CODE_BAR + 2. * CODE_BAR_MARGIN - line) / 2.).max(8.) } else { 8. };
            let bar_top = pad_y + (line - CODE_BAR) / 2.;
            div()
                .group(group.clone())
                .relative()
                .px_3()
                .py(px(pad_y))
                .rounded(px(8.))
                .bg(theme.panel)
                .border_1()
                .border_color(theme.border_subtle)
                .font(font(theme.mono_font.clone()))
                .text_size(px((theme.editor_font_size - 1.) * d.zoom))
                .line_height(px(line))
                .child(highlighted_code(text, highlights, cx))
                .when_some(d.on_code.clone(), |el, on_code| {
                    el.child(code_buttons(ix, text, command.as_deref(), group, on_code, d.copied == Some(ix), cx).top(px(bar_top)).right(px(bar_top)))
                })
                .into_any_element()
        }
        Block::Quote(blocks) => div()
            .pl_3()
            .border_l_2()
            .border_color(theme.border)
            .text_color(theme.text_muted)
            .flex()
            .flex_col()
            .gap_2()
            .children(blocks.iter().enumerate().map(|(i, b)| render_block(b, ix * 1000 + i, d, cx)))
            .into_any_element(),
        Block::List { start, items } => div()
            .flex()
            .flex_col()
            .gap_1()
            .children(items.iter().enumerate().map(|(i, item)| {
                let marker: AnyElement = match (item.task, start) {
                    (Some(done), _) => div()
                        .mt(px(4.))
                        .size(px(14.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(3.))
                        .border_1()
                        .border_color(if done { theme.accent } else { theme.border })
                        .when(done, |el| el.bg(theme.accent).text_color(theme.background).text_size(px(10.)).child("✓"))
                        .into_any_element(),
                    (None, Some(n)) => div().text_color(theme.text_muted).child(format!("{}.", n + i as u64)).into_any_element(),
                    (None, None) => div().text_color(theme.text_muted).child("•").into_any_element(),
                };
                div()
                    .flex()
                    .gap_2()
                    .child(div().min_w(px(18.)).flex().justify_end().child(marker))
                    .child(
                        div()
                            // Auto basis: a zero one (`flex_1`) can wrap the item a letter per line.
                            .flex_auto()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .children(item.blocks.iter().enumerate().map(|(j, b)| render_block(b, (ix * 100 + i) * 100 + j, d, cx))),
                    )
            }))
            .into_any_element(),
        Block::Table { aligns, rows } => div()
            .flex()
            .flex_col()
            .rounded(px(6.))
            .border_1()
            .border_color(theme.border)
            .overflow_hidden()
            .children(rows.iter().enumerate().map(|(r, row)| {
                div()
                    .flex()
                    .when(r > 0, |el| el.border_t_1().border_color(theme.border_subtle))
                    .when(r == 0, |el| el.bg(theme.panel).font_weight(FontWeight::SEMIBOLD))
                    .children(row.iter().enumerate().map(|(c, cell)| {
                        let cell_el = div().flex_1().min_w_0().px_2().py_1().child(render_inline(cell, ("md-cell", (ix * 100 + r) * 100 + c), cx));
                        match aligns.get(c) {
                            Some(Alignment::Center) => cell_el.flex().justify_center(),
                            Some(Alignment::Right) => cell_el.flex().justify_end(),
                            _ => cell_el,
                        }
                    }))
            }))
            .into_any_element(),
        Block::Rule => div().h(px(1.)).my_1().bg(theme.border).into_any_element(),
        // Text keeps the full width (a heading's rule spans it) and centers its
        // lines; images center as boxes.
        Block::Center(blocks) => div()
            .flex()
            .flex_col()
            .text_center()
            .gap(px(12. * d.zoom))
            .children(blocks.iter().enumerate().map(|(i, b)| {
                let el = render_block(b, ix * 1000 + i, d, cx);
                match b {
                    Block::Image { .. } => div().flex().justify_center().child(el),
                    _ => div().child(el),
                }
            }))
            .into_any_element(),
    }
}

/// The height of a code block's buttons (with their frame), and the least
/// room around them.
const CODE_BAR: f32 = 24.;
const CODE_BAR_MARGIN: f32 = 5.;

/// Copy and (for a shell block) Run at a code block's top right (the caller
/// places them), shown while it's hovered, or while Copy shows that it worked.
fn code_buttons(
    ix: usize,
    text: &str,
    command: Option<&str>,
    group: SharedString,
    on_code: Rc<dyn Fn(CodeAction, &mut Window, &mut App)>,
    copied: bool,
    cx: &App,
) -> gpui::Div {
    let theme = Theme::get(cx);
    let button = |id: &'static str, icon: Icon, tip: &'static str| {
        div()
            .id((id, ix))
            // The frame's border is the rest of `CODE_BAR`.
            .size(px(CODE_BAR - 2.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(5.))
            .cursor_pointer()
            .hover_bg(theme.hover)
            .tooltip(move |_, cx| tooltip_view(tip, cx))
            .child(icon_el(icon, px(13.), theme.text_muted.into()))
    };
    let (text, run) = (text.to_string(), on_code.clone());
    div()
        .absolute()
        .flex()
        .gap(px(1.))
        .rounded(px(6.))
        .border_1()
        .border_color(theme.border)
        // Over the code's first line, which it covers.
        .bg(theme.elevated)
        .when(!copied, |el| el.invisible().group_hover(group, |s| s.visible()))
        .when_some(command.map(str::to_string), |el, command| {
            el.child(
                button("md-code-run", Icon::Play, "Run in Terminal")
                    .on_click(move |_, window, cx| run(CodeAction::Run(command.clone()), window, cx)),
            )
        })
        .child(
            button("md-code-copy", if copied { Icon::Check } else { Icon::Copy }, if copied { "Copied" } else { "Copy" })
                .on_click(move |_, window, cx| on_code(CodeAction::Copy { ix, text: text.clone() }, window, cx)),
        )
}

/// What Run runs for a code block: a shell block's text, or only its commands
/// when it shows them after `$ ` prompts with their output between (a
/// command's `\` continuation lines kept); none for other languages, or
/// nothing but comments.
fn shell_command(lang: &str, text: &str) -> Option<String> {
    let lang = lang.to_lowercase();
    let shell = matches!(lang.as_str(), "sh" | "bash" | "shell" | "zsh" | "console" | "shell-session" | "shellsession" | "terminal")
        // Run tabs on Windows run PowerShell (`terminal::powershell_run_script`).
        || (cfg!(windows) && matches!(lang.as_str(), "powershell" | "pwsh" | "ps1"));
    if !shell {
        return None;
    }
    let command = if text.lines().any(|l| l.trim_start().starts_with("$ ")) {
        let mut lines = Vec::new();
        let mut continued = false;
        for line in text.lines() {
            if let Some(command) = line.trim_start().strip_prefix("$ ") {
                lines.push(command);
            } else if !continued {
                continue;
            } else {
                lines.push(line);
            }
            continued = line.trim_end().ends_with('\\');
        }
        lines.join("\n")
    } else {
        text.to_string()
    };
    command.lines().any(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#')).then(|| command.trim().to_string())
}

/// Styled text; links open in the browser when clicked.
fn render_inline(inline: &Inline, id: (&'static str, usize), cx: &App) -> AnyElement {
    let theme = Theme::get(cx);
    let highlights: Vec<(Range<usize>, HighlightStyle)> = inline
        .styles
        .iter()
        .filter(|(r, _)| r.start < r.end)
        .map(|(range, style)| {
            let mut h = HighlightStyle::default();
            match style {
                Style::Bold => h.font_weight = Some(FontWeight::BOLD),
                Style::Italic => h.font_style = Some(FontStyle::Italic),
                Style::Strike => h.strikethrough = Some(StrikethroughStyle { thickness: px(1.), color: None }),
                Style::Code => {
                    h.background_color = Some(theme.hover.into());
                    h.color = Some(theme.syntax.string.into());
                }
                Style::Link => {
                    h.color = Some(theme.accent.into());
                    h.underline = Some(UnderlineStyle { thickness: px(1.), color: None, wavy: false });
                }
            }
            (range.clone(), h)
        })
        .collect();
    let text = StyledText::new(SharedString::from(inline.text.clone())).with_highlights(highlights);
    if inline.links.is_empty() {
        return div().child(text).into_any_element();
    }
    let links = inline.links.clone();
    let ranges: Vec<Range<usize>> = links.iter().map(|(r, _)| r.clone()).collect();
    InteractiveText::new(id, text)
        .on_click(ranges, move |ix, _, cx| {
            if let Some((_, url)) = links.get(ix)
                && (url.starts_with("http://") || url.starts_with("https://") || url.starts_with("mailto:"))
            {
                cx.open_url(url);
            }
        })
        .into_any_element()
}

/// The highlights of a fenced block, when its language is one Kuana knows.
fn code_highlights(lang: &str, text: &str) -> Vec<(Range<usize>, Highlight)> {
    let extension = match lang.to_lowercase().as_str() {
        "rust" => "rs".to_string(),
        "javascript" | "jsx" => "js".into(),
        "typescript" => "ts".into(),
        "python" => "py".into(),
        "bash" | "shell" | "zsh" | "console" => "sh".into(),
        "markdown" => "md".into(),
        "golang" => "go".into(),
        other => other.to_string(),
    };
    let language = Language::from_path(&PathBuf::from(format!("code.{extension}")));
    if language == Language::PlainText || lang.is_empty() {
        return Vec::new();
    }
    let rope = Rope::from_str(text);
    Syntax::new(language, &rope).highlights(&rope, 0..text.len())
}

fn highlighted_code(text: &str, highlights: &[(Range<usize>, Highlight)], cx: &App) -> AnyElement {
    let theme = Theme::get(cx);
    let highlights = highlights
        .iter()
        .map(|(range, kind)| (range.clone(), HighlightStyle { color: Some(theme.highlight_color(*kind)), ..Default::default() }));
    div()
        .whitespace_nowrap()
        .overflow_hidden()
        .child(StyledText::new(SharedString::from(text.to_string())).with_highlights(highlights))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_markdown() {
        let md = "# Title\n\nSome **bold** and `code` with a [link](https://x.y).\n\n- one\n- [x] done\n\n1. first\n2. second\n\n> quoted\n\n```rust\nfn main() {}\n```\n\n| a | b |\n|---|--:|\n| 1 | 2 |\n\n---\n\n![logo](img.png)\n";
        let dir = std::env::temp_dir().join(format!("kuana-md-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("img.png"), b"").unwrap();
        let blocks = parse(md, Some(&dir));
        assert!(matches!(&blocks[0], Block::Heading(1, i) if i.text == "Title"));
        let Block::Paragraph(p) = &blocks[1] else { panic!("{:?}", blocks[1]) };
        assert_eq!(p.text, "Some bold and code with a link.");
        assert!(p.styles.contains(&(5..9, Style::Bold)));
        assert!(p.styles.contains(&(14..18, Style::Code)));
        assert_eq!(p.links, vec![(26..30, "https://x.y".to_string())]);
        let Block::List { start: None, items } = &blocks[2] else { panic!("{:?}", blocks[2]) };
        assert_eq!((items[0].task, items[1].task), (None, Some(true)));
        assert!(matches!(&items[1].blocks[0], Block::Paragraph(i) if i.text == "done"));
        assert!(matches!(&blocks[3], Block::List { start: Some(1), items } if items.len() == 2));
        assert!(matches!(&blocks[4], Block::Quote(inner) if matches!(&inner[0], Block::Paragraph(i) if i.text == "quoted")));
        let Block::Code { lang, text, highlights, command } = &blocks[5] else { panic!("{:?}", blocks[5]) };
        assert_eq!(*command, None);
        assert_eq!((lang.as_str(), text.as_str()), ("rust", "fn main() {}"));
        assert!(highlights.iter().any(|(r, h)| *r == (0..2) && h.base() == Highlight::Keyword), "{highlights:?}");
        let Block::Table { aligns, rows } = &blocks[6] else { panic!("{:?}", blocks[6]) };
        assert_eq!((aligns[1], rows.len(), rows[1][1].text.as_str()), (Alignment::Right, 2, "2"));
        assert_eq!(blocks[7], Block::Rule);
        assert_eq!(blocks[8], Block::Image { src: "img.png".into(), alt: "logo".into(), path: Some(dir.join("img.png")), width: None });
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn renders_the_html_readmes_use() {
        let dir = std::env::temp_dir().join(format!("kuana-md-html-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("logo.png"), b"").unwrap();
        let md = concat!(
            "<!-- hidden -->\n",
            "<p align=\"center\">\n  <img src=\"logo.png\" alt=\"Kuana\" width=\"120\">\n</p>\n\n",
            "<h1 align=\"center\">Kuana &amp; co</h1>\n\n",
            "<p align=\"center\">\n  <a href=\"https://ci\"><img src=\"https://badge.svg\" alt=\"CI\"></a>\n  A <b>fast</b>\n  editor\n</p>\n\n",
            "Press <kbd>⌘K</kbd> then<br>commit, a < b.\n\n",
            "<details>\n<summary>More</summary>\n\nHidden **text**.\n\n</details>\n\n",
            "<table><tr><th>a</th><th>b</th></tr><tr><td>1</td><td><i>2</i></td></tr></table>\n",
        );
        let blocks = parse(md, Some(&dir));
        let png = Some(dir.join("logo.png"));
        assert_eq!(blocks[0], Block::Center(vec![Block::Image { src: "logo.png".into(), alt: "Kuana".into(), path: png, width: Some(120.) }]));
        assert!(matches!(&blocks[1], Block::Center(b) if matches!(&b[..], [Block::Heading(1, i)] if i.text == "Kuana & co")), "{:?}", blocks[1]);
        let Block::Center(inner) = &blocks[2] else { panic!("{:?}", blocks[2]) };
        let [Block::Paragraph(p)] = &inner[..] else { panic!("{inner:?}") };
        assert_eq!(p.text, "CI A fast editor");
        assert_eq!(p.links, vec![(0..2, "https://ci".to_string())]);
        assert!(p.styles.contains(&(5..9, Style::Bold)));
        let Block::Paragraph(p) = &blocks[3] else { panic!("{:?}", blocks[3]) };
        assert_eq!(p.text, "Press ⌘K then\ncommit, a < b.");
        assert!(p.styles.contains(&(6..10, Style::Code)));
        assert!(matches!(&blocks[4], Block::Paragraph(i) if i.text == "More" && i.styles == vec![(0..4, Style::Bold)]), "{:?}", blocks[4]);
        assert!(matches!(&blocks[5], Block::Paragraph(i) if i.text == "Hidden text."));
        let Block::Table { rows, .. } = &blocks[6] else { panic!("{:?}", blocks[6]) };
        assert_eq!(rows.iter().map(|r| r.iter().map(|c| c.text.as_str()).collect::<Vec<_>>()).collect::<Vec<_>>(), vec![vec!["a", "b"], vec!["1", "2"]]);
        assert_eq!(blocks.len(), 7, "{blocks:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn runs_shell_blocks_commands() {
        assert_eq!(shell_command("bash", "cargo build\ncargo test"), Some("cargo build\ncargo test".into()));
        assert_eq!(shell_command("rust", "fn main() {}"), None);
        assert_eq!(shell_command("", "make"), None);
        assert_eq!(shell_command("sh", "# just a comment\n"), None);
        // Prompts: only the commands, continuations included, not the output.
        let console = "$ npm install \\\n    --save-dev x\nadded 1 package\n$ npm test\n> ok";
        assert_eq!(shell_command("console", console), Some("npm install \\\n    --save-dev x\nnpm test".into()));
    }
}
