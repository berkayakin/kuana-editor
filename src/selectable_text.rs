//! Text the mouse can select, across many pieces: a press starts a selection
//! in one piece, a drag over others extends it (in their order, by `Key`), a
//! double click takes a word and a triple click the piece. The pieces share
//! a [`TextSelection`], which knows each one's text, so the selection can be
//! copied whole. GPUI's `StyledText` draws the text; this element adds the
//! highlight behind it, the I-beam, and links that open on a click.

use std::{
    cell::RefCell,
    collections::BTreeMap,
    ops::Range,
    rc::Rc,
};

use gpui::{
    App, Bounds, CursorStyle, Element, ElementId, GlobalElementId, Hitbox, HitboxBehavior, Hsla, InspectorElementId, IntoElement,
    LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, SharedString, StyledText, Window, fill, point, px,
    size,
};

/// Where a piece sits among the others: the selection runs between two
/// places in this order (a message's index, then its piece's).
pub type Key = (usize, usize);

/// A place in the text: a piece and a byte offset in it.
type Place = (Key, usize);

#[derive(Default)]
struct Inner {
    anchor: Option<Place>,
    head: Option<Place>,
    dragging: bool,
    /// Every piece's text, as last drawn.
    texts: BTreeMap<Key, SharedString>,
}

/// The selection the pieces of one view share.
pub struct TextSelection {
    inner: RefCell<Inner>,
    /// Redraws the view that shows the pieces (a cached view must be told).
    redraw: Box<dyn Fn(&mut App)>,
    /// Opens a link that isn't a web address (a file, say).
    open: Box<dyn Fn(&str, &mut App)>,
}

impl TextSelection {
    /// A selection whose pieces are drawn by a view `redraw` redraws; `open`
    /// opens links that aren't web addresses.
    pub fn with_links(redraw: impl Fn(&mut App) + 'static, open: impl Fn(&str, &mut App) + 'static) -> Rc<TextSelection> {
        Rc::new(TextSelection { inner: RefCell::default(), redraw: Box::new(redraw), open: Box::new(open) })
    }

    /// The ends in order, when something's selected.
    fn span(&self) -> Option<(Place, Place)> {
        let inner = self.inner.borrow();
        let (a, h) = (inner.anchor?, inner.head?);
        let (start, end) = if a <= h { (a, h) } else { (h, a) };
        (start != end).then_some((start, end))
    }

    /// Unselects; whether anything was selected.
    pub fn clear(&self) -> bool {
        let mut inner = self.inner.borrow_mut();
        let had = inner.anchor.is_some();
        inner.anchor = None;
        inner.head = None;
        inner.dragging = false;
        had
    }

    /// The selected text: pieces joined by line breaks, a blank line between
    /// messages.
    pub fn text(&self) -> Option<String> {
        let (start, end) = self.span()?;
        let inner = self.inner.borrow();
        let mut out = String::new();
        let mut last_message = None;
        for (key, text) in inner.texts.range(start.0..=end.0) {
            let Some(range) = selected_in(start, end, *key, text.len()) else { continue };
            let range = snap(text, range);
            if let Some(last) = last_message {
                out.push_str(if last == key.0 { "\n" } else { "\n\n" });
            }
            out.push_str(&text[range]);
            last_message = Some(key.0);
        }
        (!out.is_empty()).then_some(out)
    }

    /// The part of piece `key` (of `len` bytes) that's selected.
    fn range_of(&self, key: Key, len: usize) -> Option<Range<usize>> {
        let (start, end) = self.span()?;
        selected_in(start, end, key, len)
    }

    /// Forgets the pieces of messages from `message` on (the conversation
    /// went back, so new ones may come under those keys).
    pub fn forget_from(&self, message: usize) {
        self.inner.borrow_mut().texts.split_off(&(message, 0));
    }

    fn note_text(&self, key: Key, text: &SharedString) {
        let mut inner = self.inner.borrow_mut();
        if inner.texts.get(&key) != Some(text) {
            inner.texts.insert(key, text.clone());
        }
    }

}

fn selected_in(start: Place, end: Place, key: Key, len: usize) -> Option<Range<usize>> {
    if key < start.0 || key > end.0 {
        return None;
    }
    let from = if key == start.0 { start.1 } else { 0 };
    let to = if key == end.0 { end.1 } else { len };
    (from < to.min(len)).then(|| from..to.min(len))
}

/// A range moved onto character boundaries.
fn snap(text: &str, range: Range<usize>) -> Range<usize> {
    let mut start = range.start.min(text.len());
    while !text.is_char_boundary(start) {
        start -= 1;
    }
    let mut end = range.end.min(text.len());
    while !text.is_char_boundary(end) {
        end += 1;
    }
    start..end
}

/// The word around `at` (letters, digits, `_`), or the character there.
fn word_at(text: &str, at: usize) -> Range<usize> {
    let at = snap(text, at..at).start;
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    let start = text[..at].char_indices().rev().take_while(|(_, c)| is_word(*c)).last().map_or(at, |(i, _)| i);
    let end = text[at..].char_indices().find(|(_, c)| !is_word(*c)).map_or(text.len(), |(i, _)| at + i);
    if start == end { at..text[at..].chars().next().map_or(at, |c| at + c.len_utf8()) } else { start..end }
}

/// A piece of selectable text.
pub struct SelectableText {
    key: Key,
    text: StyledText,
    content: SharedString,
    selection: Rc<TextSelection>,
    links: Rc<Vec<(Range<usize>, String)>>,
    color: Hsla,
}

impl SelectableText {
    /// `text` (already styled) as piece `key` of `selection`, highlighted in
    /// `color` where selected; `links` open in the browser on a click.
    pub fn new(key: Key, content: SharedString, text: StyledText, selection: Rc<TextSelection>, links: Vec<(Range<usize>, String)>, color: Hsla) -> Self {
        selection.note_text(key, &content);
        SelectableText { key, text, content, selection, links: Rc::new(links), color }
    }
}

impl IntoElement for SelectableText {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

impl Element for SelectableText {
    type RequestLayoutState = ();
    type PrepaintState = Hitbox;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(&mut self, id: Option<&GlobalElementId>, inspector_id: Option<&InspectorElementId>, window: &mut Window, cx: &mut App) -> (LayoutId, ()) {
        self.text.request_layout(id, inspector_id, window, cx)
    }

    fn prepaint(&mut self, id: Option<&GlobalElementId>, inspector_id: Option<&InspectorElementId>, bounds: Bounds<Pixels>, state: &mut (), window: &mut Window, cx: &mut App) -> Hitbox {
        self.text.prepaint(id, inspector_id, bounds, state, window, cx);
        window.insert_hitbox(bounds, HitboxBehavior::Normal)
    }

    fn paint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        state: &mut (),
        hitbox: &mut Hitbox,
        window: &mut Window,
        cx: &mut App,
    ) {
        let layout = self.text.layout().clone();
        let index_at = {
            let layout = layout.clone();
            move |position| match layout.index_for_position(position) {
                Ok(ix) | Err(ix) => ix,
            }
        };

        // The pointer: a hand over a link, else the I-beam.
        let over_link = hitbox.is_hovered(window) && {
            let ix = index_at(window.mouse_position());
            self.links.iter().any(|(range, _)| range.contains(&ix))
        };
        window.set_cursor_style(if over_link { CursorStyle::PointingHand } else { CursorStyle::IBeam }, hitbox);

        // The selection, behind the text: a rectangle per row it covers.
        if let Some(range) = self.selection.range_of(self.key, self.content.len()) {
            let range = snap(&self.content, range);
            let line_height = layout.line_height();
            let mut row: Option<(Pixels, Pixels, Pixels)> = None; // y, left, right
            let paint_row = |row: (Pixels, Pixels, Pixels), window: &mut Window| {
                let (y, left, right) = row;
                window.paint_quad(fill(Bounds::new(point(left, y), size((right - left).max(px(3.)), line_height)), self.color));
            };
            let mut indices: Vec<usize> = self.content[range.clone()].char_indices().map(|(i, _)| range.start + i).collect();
            indices.push(range.end);
            for pair in indices.windows(2) {
                let (Some(a), b) = (layout.position_for_index(pair[0]), layout.position_for_index(pair[1])) else { continue };
                // A character's right edge: where the next one starts on the
                // same row, else a little past it (the row's end).
                let right = b.filter(|b| b.y == a.y && b.x >= a.x).map_or(a.x + line_height * 0.3, |b| b.x);
                row = match row {
                    Some((y, left, _)) if y == a.y => Some((y, left, right)),
                    Some(done) => {
                        paint_row(done, window);
                        Some((a.y, a.x, right))
                    }
                    None => Some((a.y, a.x, right)),
                };
            }
            if let Some(last) = row {
                paint_row(last, window);
            }
        }
        self.text.paint(id, inspector_id, bounds, state, &mut (), window, cx);

        // Any press clears the selection first (capture); one on this piece
        // then starts the new one (bubble).
        let selection = self.selection.clone();
        window.on_mouse_event(move |event: &MouseDownEvent, phase, _, cx| {
            if phase.capture() && event.button == MouseButton::Left && selection.clear() {
                (selection.redraw)(cx);
            }
        });
        let (selection, key, content, hit) = (self.selection.clone(), self.key, self.content.clone(), hitbox.clone());
        let at = index_at.clone();
        window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
            if !phase.bubble() || event.button != MouseButton::Left || !hit.is_hovered(window) {
                return;
            }
            let ix = at(event.position);
            let range = match event.click_count {
                1 => ix..ix,
                2 => word_at(&content, ix),
                _ => 0..content.len(),
            };
            let mut inner = selection.inner.borrow_mut();
            inner.anchor = Some((key, range.start));
            inner.head = Some((key, range.end));
            inner.dragging = event.click_count == 1;
            drop(inner);
            (selection.redraw)(cx);
        });
        let (selection, key) = (self.selection.clone(), self.key);
        let at = index_at.clone();
        window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, cx| {
            // Anywhere across this piece's height counts: past a line's end
            // (or its start) the place is that end.
            let across = event.position.y >= bounds.top() && event.position.y <= bounds.bottom();
            if !phase.bubble() || event.pressed_button != Some(MouseButton::Left) || !across {
                return;
            }
            let mut inner = selection.inner.borrow_mut();
            if !inner.dragging {
                return;
            }
            let head = Some((key, at(event.position)));
            if inner.head != head {
                inner.head = head;
                drop(inner);
                (selection.redraw)(cx);
            }
        });
        let (selection, key, links) = (self.selection.clone(), self.key, self.links.clone());
        window.on_mouse_event(move |event: &MouseUpEvent, phase, _, cx| {
            if !phase.bubble() || event.button != MouseButton::Left {
                return;
            }
            let mut inner = selection.inner.borrow_mut();
            if !inner.dragging {
                return;
            }
            inner.dragging = false;
            // A click (no drag) on this piece's link opens it.
            let clicked = inner.anchor.filter(|a| Some(*a) == inner.head && a.0 == key);
            drop(inner);
            if let Some((_, ix)) = clicked
                && bounds.contains(&event.position)
                && let Some((_, url)) = links.iter().find(|(range, _)| range.contains(&ix))
            {
                if url.starts_with("http://") || url.starts_with("https://") || url.starts_with("mailto:") {
                    cx.open_url(url);
                } else {
                    (selection.open)(url, cx);
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selections_span_pieces_in_order() {
        let selection = TextSelection::with_links(|_| {}, |_, _| {});
        for (key, text) in [((0, 0), "hello world"), ((0, 1), "second line"), ((2, 0), "next message")] {
            selection.note_text(key, &SharedString::from(text));
        }
        {
            let mut inner = selection.inner.borrow_mut();
            // Dragged backwards, from the third piece into the first.
            inner.anchor = Some(((2, 0), 4));
            inner.head = Some(((0, 0), 6));
        }
        assert_eq!(selection.text().as_deref(), Some("world\nsecond line\n\nnext"));
        assert_eq!(selection.range_of((0, 1), 11), Some(0..11));
        assert_eq!(selection.range_of((0, 0), 11), Some(6..11));
        assert!(selection.clear() && selection.text().is_none());
    }

    #[test]
    fn words_are_found_around_a_place() {
        assert_eq!(word_at("let some_value = 2;", 6), 4..14);
        assert_eq!(word_at("a = b", 2), 2..3);
        assert_eq!(word_at("héllo wörld", 8), 7..13);
    }
}
