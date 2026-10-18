//! The editor's side of the language server: keeping the server's copy of
//! the document current, completion, hover documentation, signature help,
//! and the requests behind Go to Declaration, Find Usages, Rename and
//! Show Context Actions (whose results the workspace presents).

use std::{
    cmp::Reverse,
    collections::{HashMap, HashSet},
    ops::Range,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use gpui::{App, 
    AnyElement, Context, FontWeight, HighlightStyle, MouseButton, Pixels, Point, SharedString, StyledText, Task,
    WeakEntity, Window, div, font, prelude::*, px,
};
use nucleo_matcher::{
    Config, Matcher, Utf32Str,
    pattern::{CaseMatching, Normalization, Pattern},
};
use serde_json::{Value, json};

use super::{Editor, EditorEvent, EditorMode};
use crate::document::{CodeLens, Document, InlayHint};
use crate::{
    assets::Icon,
    document::EditKind,
    lint::Severity,
    lsp::{
        LspStore, Server,
        rpc::RpcError,
        protocol::{
            self, CodeAction, CompletionItem, Encoding, HoverBlock, Location, Position, TextEdit, offset_to_position,
            offsets_to_range, path_to_uri, position_to_offset,
        },
    },
    theme::Theme,
    ui::{HoverFill, icon_el, tooltip_view},
};

/// Requests the workspace carries out, since they reach beyond this editor.
pub enum EditorLspEvent {
    /// Jump to one location, or let the user pick among several; `panel`:
    /// list them in the Find panel instead (Find Usages).
    Navigate { title: SharedString, locations: Vec<Location>, encoding: Encoding, panel: bool },
    /// Ask for a new name, then rename through the server.
    Rename { server: Arc<Server>, path: PathBuf, position: Position, current: String },
    /// Offer code actions (each with the server that offered it) in a menu
    /// at this window position.
    CodeActions { actions: Vec<(Arc<Server>, CodeAction)>, position: Point<Pixels> },
    /// Something to tell the user ("No usages found").
    Message(SharedString),
    /// Run a server's command (a code lens's): `workspace/executeCommand`.
    Command { server: Arc<Server>, command: Value },
    /// Show a call or type hierarchy of these items (what `prepare…`
    /// returned) in the Find panel.
    Hierarchy { kind: crate::hierarchy::Kind, server: Arc<Server>, items: Vec<Value> },
}

impl gpui::EventEmitter<EditorLspEvent> for Editor {}

/// How `locations_request` shows what the server found.
#[derive(Clone, Copy, PartialEq)]
enum Show {
    /// Go there, or pick among several.
    Pick,
    /// As `Pick`, unless the request was made on the answer (a declaration):
    /// then its usages, to pick from.
    OrUsages,
    /// In the Find panel.
    Panel,
}

const MENU_ROWS: usize = 10;
const MENU_ROW_HEIGHT: f32 = 22.;
/// How many matches the menu keeps, the best: a server may send thousands of
/// items, and only the best few are ever looked at.
const MAX_MATCHES: usize = 1000;
/// The selected completion is resolved this long after it's selected (not
/// for every item passed on the way).
const RESOLVE_PAUSE: Duration = Duration::from_millis(100);
/// How long an accepted completion's resolve may hold back what follows it
/// (see `accept_completion`); later, the edits it would bring are given up.
const RESOLVE_WAIT: Duration = Duration::from_secs(1);
/// Inlay hints are asked for by chunks of this many lines, those in and
/// around the view: a big file's hints all at once cost the server (and
/// Kuana) for lines nobody looks at.
const HINT_CHUNK: usize = 100;
/// After an edit, hints are asked for once typing pauses this long.
const EDIT_PAUSE: Duration = Duration::from_millis(350);
/// After an edit, lenses are asked for once typing pauses this long.
const LENS_EDIT_PAUSE: Duration = Duration::from_millis(400);
/// After scrolling to lines without hints, they're asked for this soon.
const SCROLL_PAUSE: Duration = Duration::from_millis(50);
/// Lenses that came into view get their titles this soon: scrolling
/// through doesn't resolve every lens it passes.
const LENS_PAUSE: Duration = Duration::from_millis(100);

pub(super) struct CompletionMenu {
    items: Arc<Vec<CompletionItem>>,
    /// Indices into `items` that match what's typed, best first. Empty only
    /// while the first ranking for what's typed now is under way.
    matches: Vec<usize>,
    /// What's typed that `matches` are for. When the text has more (or
    /// less), a ranking for it is under way (`refilter_completion`).
    ranked_for: String,
    selected: usize,
    /// First visible row.
    scroll: usize,
    /// Where the word being completed starts.
    start: usize,
    /// The servers that answered, and for each item the one it came from.
    servers: Vec<Arc<Server>>,
    sources: Vec<usize>,
    /// The server's list depends on the prefix; ask again as it grows.
    incomplete: bool,
    /// Items resolved while the menu was open (the selected one, after a
    /// pause): the item, the document version the server answered for, and
    /// the edits it brings besides its text (an auto-import), in that text.
    resolved: Vec<(usize, u64, Vec<TextEdit>)>,
}

/// A `completionItem/resolve` request under way for the menu's selected item.
struct Resolving {
    /// The list and the item.
    items: Arc<Vec<CompletionItem>>,
    item: usize,
    /// Waits for the answer and puts it in the menu's `resolved`.
    _task: Task<()>,
}

pub(super) struct HoverPopover {
    /// Moving within this range keeps the popover.
    range: std::ops::Range<usize>,
    diagnostics: Vec<(Severity, String)>,
    blocks: Vec<HoverBlock>,
    /// Editor-local position of the popover's bottom-left (it opens upward).
    anchor: Point<Pixels>,
}

pub(super) struct SignatureHelp {
    label: String,
    active: Option<std::ops::Range<usize>>,
    row: usize,
}

/// LSP state an editor carries; part of `Editor`.
#[derive(Default)]
pub(super) struct EditorLsp {
    pub(super) store: Option<WeakEntity<LspStore>>,
    pub(super) completion: Option<CompletionMenu>,
    completion_task: Option<Task<()>>,
    /// Stops converting and ranking a list that a newer request replaced.
    completion_cancel: Arc<AtomicBool>,
    /// Ranks the menu again for what's typed, off the UI thread.
    rank_task: Option<Task<()>>,
    /// Stops a ranking that a newer one replaced.
    rank_cancel: Arc<AtomicBool>,
    /// Resolves the menu's selected item a moment after it's selected
    /// (`resolve_selected_soon`).
    resolve_task: Option<Task<()>>,
    resolving: Option<Resolving>,
    pub(super) hover: Option<HoverPopover>,
    hover_task: Option<Task<()>>,
    /// The word a pending hover request is for.
    hover_pending: Option<std::ops::Range<usize>>,
    /// Closes a shown hover shortly after the pointer leaves its word, unless
    /// the pointer reaches the popover first.
    hover_hide: Option<Task<()>>,
    /// The word under the pointer while ⌘/Ctrl is held, when the server knows
    /// its declaration (where ⌘/Ctrl-click goes): drawn as a link.
    pub(super) link: Option<std::ops::Range<usize>>,
    /// The word a declaration request is out for, and the request.
    link_pending: Option<std::ops::Range<usize>>,
    link_task: Option<Task<()>>,
    /// Words with a declaration, for a document version: going back over one
    /// doesn't ask again.
    link_found: (u64, Vec<std::ops::Range<usize>>),
    /// The word under the pointer the server had no declaration for. Only
    /// until the pointer leaves it or ⌘/Ctrl comes up: a server still loading
    /// the project answers nothing (rust-analyzer: `[]`, `null`, "content
    /// modified"), and the next hover should ask again.
    link_none: Option<std::ops::Range<usize>>,
    pub(super) signature: Option<SignatureHelp>,
    signature_task: Option<Task<()>>,
    /// The server reports diagnostics, so the built-in syntax check is redundant.
    pub(super) active: bool,
    /// Other uses of the symbol at the caret (`textDocument/documentHighlight`),
    /// for the document version they were computed on.
    pub(super) symbol_highlights: (u64, Vec<std::ops::Range<usize>>),
    symbol_highlight_task: Option<Task<()>>,
    /// Asks for the inlay hints the view needs: the `hint_stamp` it was
    /// scheduled at, and the task.
    inlay_task: Option<((u64, u64), Task<()>)>,
    /// Asks for the code lenses: the `lens_stamp` it was scheduled at, and the task.
    lens_task: Option<((u64, u64), Task<()>)>,
    /// Gives the lenses in view that have no title yet theirs.
    lens_resolve: Option<Task<()>>,
    /// The buffer lines shown last frame (None until the editor is drawn).
    view: Option<Range<usize>>,
}

/// A server's name as a diagnostic source (sources are `&'static str`):
/// each name is leaked once, and servers are few.
fn diagnostic_source(server: &str) -> &'static str {
    static NAMES: std::sync::Mutex<Vec<&'static str>> = std::sync::Mutex::new(Vec::new());
    if server.is_empty() {
        return "lsp";
    }
    let mut names = NAMES.lock().unwrap();
    if let Some(name) = names.iter().find(|n| **n == server) {
        return name;
    }
    let name: &'static str = Box::leak(server.to_string().into_boxed_str());
    names.push(name);
    name
}

pub(super) fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

/// How a completion item ranks against what's typed (lower first).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Rank<'a> {
    /// The filter text starts with what's typed: an exact match first, then
    /// the server's ranking, which knows what fits here.
    Prefix { exact: Reverse<bool>, sort_text: &'a str, score: Reverse<u32> },
    /// Any other fuzzy match: the best match first.
    Fuzzy { score: Reverse<u32>, sort_text: &'a str },
}

impl Rank<'_> {
    /// Items alike in this are equally good matches, among which the
    /// server's preselected item is chosen.
    fn tier(&self) -> (u8, u32) {
        match *self {
            Rank::Prefix { exact, .. } => (u8::from(!exact.0), 0),
            Rank::Fuzzy { score, .. } => (2, score.0),
        }
    }
}

/// The items matching what's typed, best first (the best `MAX_MATCHES`),
/// and which of them to select; None when `cancel` was set meanwhile (a
/// newer ranking replaced this one). Ties go by label, as in VS Code:
/// servers rank many items alike (rust-analyzer gives most of a type's
/// methods one `sortText`) and list them in no useful order. The selection
/// is the first item the server preselects among the best matches, else the
/// first. Runs off the UI thread: lists can be long.
fn rank_completions(items: &[CompletionItem], typed: &str, cancel: &AtomicBool) -> Option<(Vec<usize>, usize)> {
    let pattern = Pattern::parse(typed, CaseMatching::Smart, Normalization::Smart);
    let mut matcher = Matcher::new(Config::DEFAULT);
    let mut buf = Vec::new();
    let mut ranked: Vec<(Rank, &str, usize)> = Vec::new();
    for (i, item) in items.iter().enumerate() {
        if i % 512 == 0 && cancel.load(Ordering::Relaxed) {
            return None;
        }
        let score = match typed.is_empty() {
            true => 0,
            false => match pattern.score(Utf32Str::new(&item.filter_text, &mut buf), &mut matcher) {
                Some(score) => score,
                None => continue,
            },
        };
        let sort_text = item.sort_text.as_str();
        let rank = match starts_with_typed(&item.filter_text, typed) {
            Some(exact) => Rank::Prefix { exact: Reverse(exact), sort_text, score: Reverse(score) },
            None => Rank::Fuzzy { score: Reverse(score), sort_text },
        };
        ranked.push((rank, item.sort_label.as_str(), i));
    }
    // Only the best are kept: they go first (in no order), then just they are
    // sorted. The server's order last.
    if ranked.len() > MAX_MATCHES {
        ranked.select_nth_unstable(MAX_MATCHES - 1);
        ranked.truncate(MAX_MATCHES);
    }
    ranked.sort_unstable();
    let best = ranked.first().map(|(rank, ..)| rank.tier());
    let selected = ranked
        .iter()
        .take_while(|(rank, ..)| Some(rank.tier()) == best)
        .position(|&(.., i)| items[i].preselect)
        .unwrap_or(0);
    Some((ranked.into_iter().map(|(.., i)| i).collect(), selected))
}

/// Whether an item's other edits (auto-imports) may come only from
/// `completionItem/resolve`: it has none, isn't the user's snippet, and its
/// server resolves items.
fn needs_resolve(item: &CompletionItem, server: &Server) -> bool {
    item.additional_edits.is_empty() && item.raw.get("kuanaUserSnippet").is_none() && server.resolves("completionProvider")
}

/// Where `range` of the text of `version` is now, if what it holds wasn't
/// edited since (only moved); None when it was, or that text is too old.
fn range_since(doc: &Document, version: u64, range: &Range<usize>) -> Option<Range<usize>> {
    let moved = doc.offset_since(version, range.start)?..doc.offset_since(version, range.end)?;
    (moved.len() == range.len()).then_some(moved)
}

/// A fresh flag in `slot`, setting the one it replaces: whatever works
/// with that one stops.
fn renew(slot: &mut Arc<AtomicBool>) -> Arc<AtomicBool> {
    slot.store(true, Ordering::Relaxed);
    *slot = Arc::new(AtomicBool::new(false));
    slot.clone()
}

/// A request for the inlay hints of neighboring chunks of lines.
struct HintRun {
    chunks: Range<usize>,
    /// Where their lines are: the hints at these offsets are theirs.
    bytes: Range<usize>,
    range: protocol::LspRange,
}

/// A server's inlay hints (`textDocument/inlayHint`) in `rope`, the ones in `bytes`.
fn read_inlay_hints(answer: &Value, rope: &ropey::Rope, encoding: Encoding, bytes: &Range<usize>) -> Vec<InlayHint> {
    let hints = answer.as_array().into_iter().flatten();
    hints
        .filter_map(|h| {
            let position = protocol::Position::from_json(h.get("position")?)?;
            // A string, or parts with a `value` each.
            let label = match h.get("label")? {
                Value::String(s) => s.trim().to_string(),
                Value::Array(parts) => parts.iter().filter_map(|p| p.get("value")?.as_str()).collect::<String>().trim().to_string(),
                _ => return None,
            };
            let offset = position_to_offset(rope, position, encoding);
            let flag = |key: &str| h.get(key).and_then(Value::as_bool).unwrap_or(false);
            (!label.is_empty() && bytes.contains(&offset)).then(|| InlayHint { offset, label, pad_left: flag("paddingLeft"), pad_right: flag("paddingRight") })
        })
        .collect()
}

/// A code lens's title and command, once it has them (a lens with an empty
/// title shows nothing, and is dropped).
fn lens_command(lens: &Value) -> Option<(String, Value)> {
    let command = lens.get("command").filter(|c| !c.is_null())?;
    let title = command.get("title")?.as_str()?.trim().to_string();
    (!title.is_empty()).then(|| (title, command.clone()))
}

/// Sends `method` to each server with its params, all at once; the answers
/// in the servers' order.
async fn ask_all(requests: Vec<(Arc<Server>, Value)>, method: &'static str) -> Vec<(Arc<Server>, Result<Value, RpcError>)> {
    futures::future::join_all(requests.into_iter().map(|(server, params)| async move {
        let result = server.request(method, params).await;
        (server, result)
    }))
    .await
}

/// Several servers' locations as one list, in the first's encoding and
/// without repeats (servers often agree on a definition). A location from a
/// server counting columns otherwise is converted through its file's text
/// (`text_of`), and kept as it is when that can't be read.
fn merge_locations(answers: Vec<(Encoding, Vec<Location>)>, mut text_of: impl FnMut(&Path) -> Option<ropey::Rope>) -> (Encoding, Vec<Location>) {
    let encoding = answers.iter().find(|(_, found)| !found.is_empty()).map_or(Encoding::Utf16, |(e, _)| *e);
    let mut texts: HashMap<PathBuf, Option<ropey::Rope>> = HashMap::new();
    let mut seen = HashSet::new();
    let mut merged = Vec::new();
    for (from, found) in answers {
        for mut location in found {
            if from != encoding
                && let Some(rope) = texts.entry(location.path.clone()).or_insert_with(|| text_of(&location.path))
            {
                location.range = offsets_to_range(rope, protocol::range_to_offsets(rope, location.range, from), encoding);
            }
            let (start, end) = (location.range.start, location.range.end);
            if seen.insert((location.path.clone(), start.line, start.character, end.line, end.character)) {
                merged.push(location);
            }
        }
    }
    (encoding, merged)
}

/// Whether `text` starts with `typed` ignoring case, and if so, whether
/// that's all of it.
fn starts_with_typed(text: &str, typed: &str) -> Option<bool> {
    let mut rest = text.chars().flat_map(char::to_lowercase);
    typed.chars().flat_map(char::to_lowercase).all(|c| rest.next() == Some(c)).then(|| !typed.is_empty() && rest.next().is_none())
}

impl Editor {
    pub fn set_lsp(&mut self, store: WeakEntity<LspStore>) {
        self.lsp.store = Some(store);
    }

    /// A server opened this editor's document: its hints and lenses can be
    /// asked for.
    pub fn lsp_opened(&mut self, cx: &mut Context<Self>) {
        self.schedule_inlay_hints(cx);
        self.schedule_code_lenses(cx);
    }

    /// The document's servers that are up, the main one first.
    pub(super) fn lsp_servers(&self, cx: &gpui::App) -> Vec<Arc<Server>> {
        let (Some(path), Some(store)) = (self.doc.read(cx).path.as_deref(), self.lsp.store.as_ref().and_then(|s| s.upgrade())) else {
            return Vec::new();
        };
        store.read(cx).servers_for(path)
    }

    /// The main server.
    pub(super) fn lsp_server(&self, cx: &gpui::App) -> Option<Arc<Server>> {
        self.lsp_servers(cx).into_iter().next()
    }

    /// The first server that has `capability` (`hoverProvider`…).
    pub(super) fn lsp_server_with(&self, capability: &str, cx: &gpui::App) -> Option<Arc<Server>> {
        self.lsp_servers(cx).into_iter().find(|s| s.capability(capability))
    }

    /// Every server that has `capability`, the main one first, each with its
    /// request for `offset` (columns in its own encoding). A file often has
    /// several (TypeScript plus Tailwind or ESLint), and the others know
    /// things the main one doesn't, so these requests go to all of them.
    fn position_requests(&self, capability: &str, offset: usize, cx: &App) -> Vec<(Arc<Server>, Value)> {
        self.lsp_servers(cx)
            .into_iter()
            .filter(|s| s.capability(capability))
            .filter_map(|server| {
                let params = self.position_params(offset, &server, cx)?;
                Some((server, params))
            })
            .collect()
    }

    /// Records an edit: tells the server and the workspace, and drops
    /// popovers that no longer apply.
    pub(super) fn edited(&mut self, cx: &mut Context<Self>) {
        self.schedule_parse(cx);
        if let (Some(store), Some(path)) = (self.lsp.store.as_ref().and_then(|s| s.upgrade()), self.doc.read(cx).path.clone()) {
            let changes = self.doc.as_mut(cx).take_lsp_changes();
            if !changes.is_empty() {
                // What servers answer for this version is read in this text.
                self.doc.as_mut(cx).note_lsp_version();
                // Servers that wait to be asked for diagnostics are asked
                // (once this edit's effects settle: the store may be busy).
                let (store, path) = (store.downgrade(), path.clone());
                cx.defer(move |cx| {
                    store.update(cx, |s, cx| s.pull_soon(&path, cx)).ok();
                });
            }
            store.read(cx).did_change(&path, self.doc.read(cx).version(), changes, self.doc.read(cx).rope());
        }
        self.lsp.hover = None;
        self.lsp.hover_task = None;
        self.lsp.hover_pending = None;
        self.link_word(None, cx);
        cx.emit(EditorEvent::Edited);
    }

    pub(super) fn lsp_did_save(&self, cx: &gpui::App) {
        if let (Some(store), Some(path)) = (self.lsp.store.as_ref().and_then(|s| s.upgrade()), self.doc.read(cx).path.as_deref()) {
            store.read(cx).did_save(path, self.doc.read(cx).rope());
        }
    }

    /// One server's diagnostics for the document, as it published them.
    /// They're read in the text of the version they were computed for (the
    /// server may be some edits behind) and moved to today's text; one for
    /// a version no longer known is dropped (a newer one follows). The other servers' diagnostics stay as they are.
    pub fn set_server_diagnostics(&mut self, source: crate::lsp::LspDiagnostics, cx: &mut Context<Self>) {
        let name = diagnostic_source(&source.server);
        let doc = self.doc.read(cx);
        let diagnostics = match source.params.get("version").and_then(Value::as_u64) {
            Some(version) => doc
                .text_at_lsp_version(version)
                .map(|text| protocol::diagnostics(&source.params, text, source.encoding, name))
                .and_then(|found| doc.diagnostics_since(version, found)),
            None => Some(protocol::diagnostics(&source.params, doc.rope(), source.encoding, name)),
        };
        let Some(mut diagnostics) = diagnostics else { return };
        diagnostics.sort_by_key(|d| d.range.start);
        self.doc.as_mut(cx).set_server_diagnostics(&source.id, diagnostics);
        // Hints and lenses aren't asked for again here: a server whose
        // change elsewhere changes them says so (`workspace/inlayHint/refresh`).
        if !self.lsp.active {
            self.lsp.active = true;
            // Re-run the built-in linter without its now-duplicate syntax check.
            self.linted_version = None;
            self.schedule_lint(std::time::Duration::ZERO, cx);
        }
        cx.notify();
    }

    fn text_document(&self, cx: &App) -> Option<Value> {
        Some(json!({ "uri": path_to_uri(self.doc.read(cx).path.as_deref()?) }))
    }

    fn position_params(&self, offset: usize, server: &Server, cx: &App) -> Option<Value> {
        Some(json!({
            "textDocument": self.text_document(cx)?,
            "position": offset_to_position(self.doc.read(cx).rope(), offset, server.encoding).to_json(),
        }))
    }

    // ---- Completion ------------------------------------------------------------------

    /// After typing: open, narrow or close completion, and update signature help.
    pub(super) fn after_typing(&mut self, typed: &str, cx: &mut Context<Self>) {
        if self.selections.len() != 1 || self.mode != EditorMode::Full {
            return;
        }
        let servers = self.lsp_servers(cx);
        let Some(last) = typed.chars().last() else { return };
        let head = self.primary_selection().head;
        // Any completing server's trigger (`.` for TypeScript, `"` for Tailwind).
        let trigger = servers
            .iter()
            .filter(|s| s.capability("completionProvider"))
            .flat_map(|s| s.completion_triggers())
            .find(|t| head >= t.len() && self.doc.read(cx).slice(head - t.len()..head) == *t);
        if servers.iter().any(|s| s.capability("completionProvider")) {
            match (&self.lsp.completion, trigger) {
                (_, Some(trigger)) => self.request_completion(Some(trigger), cx),
                (Some(menu), None) if is_word_char(last) && menu.incomplete => self.request_completion(None, cx),
                (Some(_), None) => {}
                (None, None) if is_word_char(last) => self.request_completion(None, cx),
                (None, None) => {}
            }
        }
        if let Some(server) = servers.iter().find(|s| s.capability("signatureHelpProvider"))
            && (self.lsp.signature.is_some() || server.signature_triggers().iter().any(|t| typed.ends_with(t.as_str())))
        {
            self.request_signature_help(cx);
        }
    }

    pub(super) fn show_completions(&mut self, _: &super::ShowCompletions, _: &mut Window, cx: &mut Context<Self>) {
        if self.mode == EditorMode::Full {
            self.request_completion(None, cx);
        }
    }

    /// Asks every completing server (for a trigger character, the ones it
    /// triggers) and shows their items together.
    fn request_completion(&mut self, trigger: Option<String>, cx: &mut Context<Self>) {
        let head = self.primary_selection().head;
        let start = self.word_start(head, cx);
        let requests: Vec<(Arc<Server>, Value)> = self
            .lsp_servers(cx)
            .into_iter()
            .filter(|s| s.capability("completionProvider"))
            .filter(|s| trigger.as_ref().is_none_or(|t| s.completion_triggers().contains(t)))
            .filter_map(|server| {
                let mut params = self.position_params(head, &server, cx)?;
                params["context"] = match &trigger {
                    Some(t) => json!({ "triggerKind": 2, "triggerCharacter": t }),
                    None => json!({ "triggerKind": 1 }),
                };
                Some((server, params))
            })
            .collect();
        if requests.is_empty() {
            return;
        }
        let language = self.doc.read(cx).language;
        let cancel = renew(&mut self.lsp.completion_cancel);
        self.lsp.completion_task = Some(cx.spawn(async move |this, cx| {
            let servers: Vec<Arc<Server>> = requests.iter().map(|(s, _)| s.clone()).collect();
            let answers = futures::future::join_all(requests.into_iter().map(|(server, params)| async move {
                server.request("textDocument/completion", params).await
            }))
            .await;
            // Ranked for what's typed by now (the caret left the word: nothing to show).
            let Ok(Some(typed)) = this.update(cx, |e, cx| e.typed_from(start, cx)) else { return };
            // Lists can be long (rust-analyzer's run to thousands, sent again on
            // every key while incomplete): they're read and ranked off the UI thread.
            let made = cx.background_executor().spawn(async move {
                let (mut items, mut sources, mut incomplete) = (Vec::new(), Vec::new(), false);
                for (ix, answer) in answers.into_iter().enumerate() {
                    let Ok(value) = answer else { continue };
                    incomplete |= value.get("isIncomplete").and_then(Value::as_bool).unwrap_or(false);
                    let found = protocol::completion_items(value);
                    sources.extend(std::iter::repeat_n(ix, found.len()));
                    items.extend(found);
                }
                let snippets = super::snippet::user_snippet_items(language);
                sources.extend(std::iter::repeat_n(0, snippets.len()));
                items.extend(snippets);
                let (matches, selected) = rank_completions(&items, &typed, &cancel)?;
                Some((Arc::new(items), sources, incomplete, typed, matches, selected))
            });
            let Some((items, sources, incomplete, typed, matches, selected)) = made.await else { return };
            this.update(cx, |e, cx| {
                // Nothing matches what's typed: no menu (unless typing went on meanwhile).
                if matches.is_empty() && e.typed_from(start, cx).as_ref() == Some(&typed) {
                    e.close_completion(cx);
                    return;
                }
                e.stop_ranking();
                e.stop_resolving();
                e.lsp.completion = Some(CompletionMenu {
                    items,
                    scroll: selected.saturating_sub(MENU_ROWS - 1),
                    matches,
                    ranked_for: typed,
                    selected,
                    start,
                    servers,
                    sources,
                    incomplete,
                    resolved: Vec::new(),
                });
                // Typed on (or moved away) while the list was read.
                e.refilter_completion(cx);
                e.resolve_selected_soon(cx);
                cx.notify();
            })
            .ok();
        }));
    }

    /// What's typed of the word from `start` to the caret; None when the
    /// caret isn't in that word any more (several carets, another line,
    /// something else typed).
    fn typed_from(&self, start: usize, cx: &App) -> Option<String> {
        let head = self.selections.get(self.primary).map_or(0, |s| s.head);
        let doc = self.doc.read(cx);
        if self.selections.len() != 1 || head < start || doc.row_of(head) != doc.row_of(start) {
            return None;
        }
        Some(doc.slice(start..head)).filter(|typed| typed.chars().all(is_word_char))
    }

    /// Start of the identifier the caret is in or after.
    fn word_start(&self, offset: usize, cx: &App) -> usize {
        let mut start = offset;
        while let Some(c) = self.doc.read(cx).char_before(start) {
            if !is_word_char(c) {
                break;
            }
            start -= c.len_utf8();
        }
        start
    }

    /// Re-matches the menu against what's typed, off the UI thread (the
    /// matches stay as they were meanwhile); closes it right away when the
    /// caret left the word, or once nothing matches.
    pub(super) fn refilter_completion(&mut self, cx: &mut Context<Self>) {
        let Some(menu) = self.lsp.completion.as_ref() else { return };
        let Some(typed) = self.typed_from(menu.start, cx) else {
            self.close_completion(cx);
            return;
        };
        if typed == menu.ranked_for {
            // Back to what's shown: a ranking under way is for something else.
            self.stop_ranking();
            return;
        }
        let items = menu.items.clone();
        let cancel = renew(&mut self.lsp.rank_cancel);
        let ranked = items.clone();
        let ranking = cx.background_executor().spawn(async move { rank_completions(&ranked, &typed, &cancel).map(|ranked| (typed, ranked)) });
        self.lsp.rank_task = Some(cx.spawn(async move |this, cx| {
            let Some((typed, (matches, selected))) = ranking.await else { return };
            this.update(cx, |e, cx| e.show_ranking(&items, typed, matches, selected, cx)).ok();
        }));
    }

    /// A ranking of `items` for `typed` is done: the menu shows it, unless
    /// it lists other items by now, or something else is typed (a newer
    /// ranking is on its way).
    fn show_ranking(&mut self, items: &Arc<Vec<CompletionItem>>, typed: String, matches: Vec<usize>, selected: usize, cx: &mut Context<Self>) {
        let Some(start) = self.lsp.completion.as_ref().filter(|m| Arc::ptr_eq(&m.items, items)).map(|m| m.start) else { return };
        if self.typed_from(start, cx).as_ref() != Some(&typed) {
            return;
        }
        if matches.is_empty() {
            self.close_completion(cx);
            return;
        }
        let Some(menu) = self.lsp.completion.as_mut() else { return };
        menu.matches = matches;
        menu.ranked_for = typed;
        menu.selected = selected;
        menu.scroll = selected.saturating_sub(MENU_ROWS - 1);
        self.resolve_selected_soon(cx);
        cx.notify();
    }

    /// Ranks here and now when the menu's matches aren't for what's typed:
    /// a key that picks from them came before the ranking was done.
    fn finish_ranking(&mut self, cx: &mut Context<Self>) {
        let Some(menu) = self.lsp.completion.as_ref() else { return };
        let Some(typed) = self.typed_from(menu.start, cx).filter(|typed| *typed != menu.ranked_for) else { return };
        let items = menu.items.clone();
        self.stop_ranking();
        if let Some((matches, selected)) = rank_completions(&items, &typed, &AtomicBool::new(false)) {
            self.show_ranking(&items, typed, matches, selected, cx);
        }
    }

    fn stop_ranking(&mut self) {
        renew(&mut self.lsp.rank_cancel);
        self.lsp.rank_task = None;
    }

    fn close_completion(&mut self, cx: &mut Context<Self>) {
        self.stop_ranking();
        self.stop_resolving();
        if self.lsp.completion.take().is_some() {
            cx.notify();
        }
    }

    /// The menu's list goes (or another replaces it): resolving its
    /// selection stops.
    fn stop_resolving(&mut self) {
        self.lsp.resolve_task = None;
        self.lsp.resolving = None;
    }

    /// Resolves the selected item a moment after it's selected, so the
    /// edits it brings besides its text (auto-imports often come only on
    /// `completionItem/resolve`) are there when it's accepted, and go in
    /// with it.
    fn resolve_selected_soon(&mut self, cx: &mut Context<Self>) {
        let Some(menu) = self.lsp.completion.as_ref() else { return };
        let Some(&i) = menu.matches.get(menu.selected) else { return };
        let Some(server) = menu.servers.get(menu.sources[i]).cloned() else { return };
        let under_way = self.lsp.resolving.as_ref().is_some_and(|r| Arc::ptr_eq(&r.items, &menu.items) && r.item == i);
        // Nothing to ask (or asked already): what was to be asked for an
        // item selected before isn't.
        if under_way || !needs_resolve(&menu.items[i], &server) || menu.resolved.iter().any(|(ix, ..)| *ix == i) {
            self.lsp.resolve_task = None;
            return;
        }
        let items = menu.items.clone();
        self.lsp.resolve_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(RESOLVE_PAUSE).await;
            this.update(cx, |e, cx| e.send_resolve(items, i, &server, cx)).ok();
        }));
    }

    /// Asks `server` to resolve item `i` of `items`, the menu's selection:
    /// the answer, for the text of now, goes into the menu's `resolved`.
    fn send_resolve(&mut self, items: Arc<Vec<CompletionItem>>, i: usize, server: &Server, cx: &mut Context<Self>) {
        self.lsp.resolve_task = None;
        let request = server.client.request("completionItem/resolve", items[i].raw.clone());
        let version = self.doc.read(cx).version();
        let list = items.clone();
        let task = cx.spawn(async move |this, cx| {
            let resolved = request.await;
            this.update(cx, |e, cx| {
                e.lsp.resolving = None;
                // Edited meanwhile: the answer may be for the new text (rust-analyzer
                // starts over when a change comes in), so it isn't kept.
                let menu = e.lsp.completion.as_mut().filter(|m| Arc::ptr_eq(&m.items, &list));
                if let (Ok(resolved), Some(menu), true) = (resolved, menu, e.doc.read(cx).version() == version) {
                    menu.resolved.push((i, version, protocol::text_edits(resolved.get("additionalTextEdits"))));
                }
            })
            .ok();
        });
        self.lsp.resolving = Some(Resolving { items, item: i, _task: task });
    }

    /// Up/Down/Enter/Tab while the menu is open. Returns whether it took the key.
    pub(super) fn completion_key(&mut self, key: CompletionKey, cx: &mut Context<Self>) -> bool {
        self.finish_ranking(cx);
        let Some(menu) = self.lsp.completion.as_mut() else { return false };
        if menu.matches.is_empty() {
            return false;
        }
        match key {
            CompletionKey::Move(delta) => {
                let len = menu.matches.len() as isize;
                menu.selected = ((menu.selected as isize + delta).rem_euclid(len)) as usize;
                if menu.selected < menu.scroll {
                    menu.scroll = menu.selected;
                } else if menu.selected >= menu.scroll + MENU_ROWS {
                    menu.scroll = menu.selected + 1 - MENU_ROWS;
                }
                self.resolve_selected_soon(cx);
                cx.notify();
            }
            CompletionKey::Accept { replace } => {
                let ix = menu.selected;
                self.accept_completion(ix, replace, cx);
            }
        }
        true
    }

    fn accept_completion(&mut self, ix: usize, replace: bool, cx: &mut Context<Self>) {
        let Some(menu) = self.lsp.completion.take() else { return };
        self.stop_ranking();
        self.stop_resolving();
        cx.notify();
        let Some(&i) = menu.matches.get(ix) else { return };
        let item = &menu.items[i];
        // Positioned in the encoding of, and resolved by, the server it came from.
        let Some(server) = menu.servers.get(menu.sources[i]).cloned() else { return };
        let encoding = server.encoding;
        // Its other edits (an auto-import) in today's text: sent with it, or
        // found while it was selected; they go in with it. Else they're asked
        // for now, and the server answers before it hears of anything sent
        // after (`request_first`): rust-analyzer resolves an item only in the
        // text it was made for, and drops the work when a change comes in.
        // The text goes in now, so keys typed next follow it; the edits once
        // the server answers.
        let resolved = menu.resolved.iter().find(|(ix, ..)| *ix == i);
        let (extra, to_come) = if !needs_resolve(item, &server) {
            (protocol::resolve_edits(self.doc.read(cx).rope(), &item.additional_edits, encoding), None)
        } else if let Some(edits) = resolved.and_then(|(_, version, edits)| self.edits_since(edits, encoding, *version, cx)) {
            (edits, None)
        } else {
            let version = self.doc.read(cx).version();
            (Vec::new(), Some((version, server.client.request_first("completionItem/resolve", item.raw.clone(), RESOLVE_WAIT))))
        };
        let mark = self.doc.read(cx).undo_mark();
        let inserted = self.apply_completion(item, menu.start, encoding, replace, extra, cx);
        let Some((version, answer)) = to_come else { return };
        let after = (self.doc.read(cx).version(), inserted);
        // A task of its own: typing on, which asks for completion again, doesn't cancel it.
        cx.spawn(async move |this, cx| {
            let Ok(resolved) = answer.await else { return };
            let edits = protocol::text_edits(resolved.get("additionalTextEdits"));
            if !edits.is_empty() {
                this.update(cx, |e, cx| e.add_resolved_edits(&edits, encoding, version, after, mark, cx)).ok();
            }
        })
        .detach();
    }

    /// A server's `edits` for the text of `version`, where they go in
    /// today's text: one whose text was edited since is left out. None when
    /// that text isn't known any more.
    fn edits_since(&self, edits: &[TextEdit], encoding: Encoding, version: u64, cx: &App) -> Option<Vec<(Range<usize>, String)>> {
        let doc = self.doc.read(cx);
        let text = doc.text_at_lsp_version(version)?;
        let edits = protocol::resolve_edits(text, edits, encoding);
        Some(edits.into_iter().filter_map(|(range, new_text)| Some((range_since(doc, version, &range)?, new_text))).collect())
    }

    /// An accepted completion's other edits, which came after its text went
    /// in (`after`: the version and where its text was then), for the text
    /// of `version`: moved through the edits made since. None go in once
    /// the completion was undone (the history is back at `mark`); one whose
    /// text was edited, or that overlaps the completion (typed into or not:
    /// a snippet's stop), is left out. With nothing done after the
    /// completion, one undo takes them with it.
    fn add_resolved_edits(
        &mut self,
        edits: &[TextEdit],
        encoding: Encoding,
        version: u64,
        (after, inserted): (u64, Range<usize>),
        mark: usize,
        cx: &mut Context<Self>,
    ) {
        let doc = self.doc.read(cx);
        if doc.undo_mark() <= mark {
            return;
        }
        let Some(completion) = doc.offset_since(after, inserted.start).zip(doc.offset_since(after, inserted.end)).map(|(start, end)| start..end) else {
            return;
        };
        let Some(moved) = self.edits_since(edits, encoding, version, cx) else { return };
        let moved: Vec<(Range<usize>, String)> =
            moved.into_iter().filter(|(range, _)| range.end <= completion.start || range.start >= completion.end).collect();
        if moved.is_empty() {
            return;
        }
        let untouched = self.doc.read(cx).version() == after;
        self.edit_and_remap(moved, EditKind::Other, cx);
        if untouched {
            self.doc.as_mut(cx).merge_undo_since(mark);
        }
    }

    /// Inserts a completion at the caret, with `extra` edits (in today's
    /// text) in the same step, and returns where its text went. Enter
    /// inserts; Tab also replaces the rest of the word after the caret (as
    /// in JetBrains).
    fn apply_completion(
        &mut self,
        item: &CompletionItem,
        start: usize,
        encoding: Encoding,
        replace: bool,
        extra: Vec<(Range<usize>, String)>,
        cx: &mut Context<Self>,
    ) -> Range<usize> {
        let head = self.primary_selection().head;
        let rope = self.doc.read(cx).rope().clone();
        let from = item.edit.as_ref().map(|e| position_to_offset(&rope, e.range.start, encoding)).unwrap_or(start).min(head);
        let mut to = head;
        if replace {
            while let Some(c) = self.doc.read(cx).char_at(to).filter(|c| is_word_char(*c)) {
                to += c.len_utf8();
            }
        }
        let (text, stops) = match item.is_snippet {
            true => protocol::expand_snippet(&self.indented_snippet(item.raw_text(), from, cx)),
            false => (item.raw_text().to_string(), Vec::new()),
        };
        let mut edits = vec![(from..to, text.clone())];
        // An extra edit overlapping the main one is a server bug; skip it.
        edits.extend(extra.into_iter().filter(|(range, _)| range.end <= from || range.start >= to));
        // Where the inserted text starts once earlier extra edits apply.
        let shift: isize = edits[1..]
            .iter()
            .filter(|(r, _)| r.end <= from)
            .map(|(r, t)| t.len() as isize - r.len() as isize)
            .sum();
        let new_start = (from as isize + shift) as usize;
        self.edit_and_remap(edits, EditKind::Other, cx);
        let has_stops = stops.len() > 1;
        self.select_snippet(new_start, text.len(), stops, cx);
        self.autoscroll = true;
        // Accepting a function often opens its argument list.
        if text.ends_with('(') || has_stops {
            self.request_signature_help(cx);
        }
        new_start..new_start + text.len()
    }

    // ---- Signature help ----------------------------------------------------------------

    fn request_signature_help(&mut self, cx: &mut Context<Self>) {
        let head = self.primary_selection().head;
        let row = self.doc.read(cx).row_of(head);
        let requests = self.position_requests("signatureHelpProvider", head, cx);
        if requests.is_empty() {
            return;
        }
        self.lsp.signature_task = Some(cx.spawn(async move |this, cx| {
            let answers = ask_all(requests, "textDocument/signatureHelp").await;
            this.update(cx, |e, cx| {
                // One signature shows: the first server's that has one.
                e.lsp.signature = answers
                    .iter()
                    .find_map(|(_, result)| protocol::signature(result.as_ref().ok()?))
                    .map(|(label, active)| SignatureHelp { label, active, row });
                cx.notify();
            })
            .ok();
        }));
    }

    /// Closes popovers that no longer fit where the caret is.
    pub(super) fn lsp_selection_changed(&mut self, cx: &mut Context<Self>) {
        if self.lsp.completion.is_some() {
            self.refilter_completion(cx);
        }
        let head = self.selections.get(self.primary).map_or(0, |s| s.head);
        if self.lsp.signature.as_ref().is_some_and(|s| s.row != self.doc.read(cx).row_of(head)) {
            self.lsp.signature = None;
            self.lsp.signature_task = None;
        }
    }

    /// The editor lost the keyboard: completion and signature help go (a
    /// request still out for them too, so its answer doesn't reopen them);
    /// hover follows the pointer, so it stays.
    pub(super) fn focus_lost(&mut self, cx: &mut Context<Self>) {
        self.lsp.completion_task = None;
        renew(&mut self.lsp.completion_cancel);
        self.close_completion(cx);
        self.lsp.signature_task = None;
        if self.lsp.signature.take().is_some() {
            cx.notify();
        }
    }

    /// Escape: closes completion, signature help or hover. Returns whether
    /// anything was open.
    pub(super) fn dismiss_lsp_popovers(&mut self, cx: &mut Context<Self>) -> bool {
        self.stop_ranking();
        self.stop_resolving();
        let open = self.lsp.completion.take().is_some() | self.lsp.signature.take().is_some() | self.lsp.hover.take().is_some();
        if open {
            cx.notify();
        }
        open
    }

    // ---- Hover -------------------------------------------------------------------------

    /// Mouse moved over the editor (no button held): show documentation and
    /// problems for the word under it after a short pause.
    pub(super) fn hover_at(&mut self, position: Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(layout) = self.layout.clone() else { return };
        let over_text = self.mode == EditorMode::Full
            && layout.bounds.contains(&position)
            && position.x >= layout.bounds.left() + layout.gutter_width
            && position.x < layout.bounds.right() - px(super::SCROLLBAR_WIDTH);
        if !over_text {
            self.cancel_pending_hover();
            self.hide_hover_soon(window, cx);
            return;
        }
        let offset = self.offset_for_position(position, window, cx);
        let range = super::word_range(self.doc.read(cx), offset);
        let range = if range.is_empty() { offset..self.doc.read(cx).next_char_boundary(offset) } else { range };
        if self.lsp.hover.as_ref().is_some_and(|h| h.range == range) {
            // Back on the word: keep its popover.
            self.lsp.hover_hide = None;
            return;
        }
        if self.lsp.hover_pending.as_ref() == Some(&range) {
            return;
        }
        // On the way to the popover the pointer crosses other words, so the
        // shown one stays a moment longer.
        self.hide_hover_soon(window, cx);
        self.lsp.hover_pending = Some(range.clone());
        self.lsp.hover_task = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(std::time::Duration::from_millis(450)).await;
            this.update_in(cx, |e, window, cx| e.show_hover(range, window, cx)).ok();
        }));
    }

    /// The pointer left the editor (onto tabs, the title bar, a menu): a
    /// hover that was about to open would describe code it's no longer over,
    /// and a shown one goes away.
    pub(super) fn pointer_left(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.cancel_pending_hover();
        self.hide_hover_soon(window, cx);
        self.link_word(None, cx);
    }

    fn cancel_pending_hover(&mut self) {
        self.lsp.hover_task = None;
        self.lsp.hover_pending = None;
    }

    fn hide_hover_soon(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.lsp.hover.is_none() || self.lsp.hover_hide.is_some() {
            return;
        }
        self.lsp.hover_hide = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(std::time::Duration::from_millis(350)).await;
            this.update(cx, |e, cx| {
                e.lsp.hover_hide = None;
                e.lsp.hover = None;
                cx.notify();
            })
            .ok();
        }));
    }

    pub(super) fn quick_documentation(&mut self, _: &super::QuickDocumentation, window: &mut Window, cx: &mut Context<Self>) {
        let head = self.primary_selection().head;
        let range = super::word_range(self.doc.read(cx), head);
        self.show_hover(range, window, cx);
    }

    /// Shows problems at `range` right away, then the server's documentation.
    fn show_hover(&mut self, range: std::ops::Range<usize>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(anchor) = self.local_position(range.start, window, cx) else { return };
        let diagnostics: Vec<(Severity, String)> = self
            .doc
            .read(cx)
            .diagnostics
            .iter()
            .filter(|d| d.range.start <= range.end && d.range.end >= range.start)
            .map(|d| (d.severity, d.message.to_string()))
            .collect();
        if !diagnostics.is_empty() {
            self.lsp.hover = Some(HoverPopover { range: range.clone(), diagnostics: diagnostics.clone(), blocks: Vec::new(), anchor });
            cx.notify();
        }
        let requests = self.position_requests("hoverProvider", range.start, cx);
        if requests.is_empty() {
            self.lsp.hover_pending = None;
            return;
        }
        self.lsp.hover_task = Some(cx.spawn(async move |this, cx| {
            let answers = ask_all(requests, "textDocument/hover").await;
            this.update(cx, |e, cx| {
                e.lsp.hover_pending = None;
                // Every server's documentation, the main one's first.
                let blocks: Vec<HoverBlock> = answers.iter().filter_map(|(_, r)| r.as_ref().ok()).flat_map(protocol::hover_blocks).collect();
                if blocks.is_empty() && diagnostics.is_empty() {
                    return;
                }
                e.lsp.hover_hide = None;
                e.lsp.hover = Some(HoverPopover { range, diagnostics, blocks, anchor });
                cx.notify();
            })
            .ok();
        }));
    }

    /// Editor-local pixel position of the top-left of `offset`'s character.
    fn local_position(&mut self, offset: usize, window: &mut Window, cx: &mut Context<Self>) -> Option<Point<Pixels>> {
        self.sync_wrap(window, cx);
        let layout = self.layout.clone()?;
        let x = self.x_for_offset(offset, window, cx);
        let drow = self.display_row_of(offset, cx);
        Some(gpui::point(
            layout.text_left - layout.bounds.left() + x - self.scroll.x,
            layout.line_height * drow as f32 - self.scroll.y,
        ))
    }

    // ---- ⌘/Ctrl-hover links --------------------------------------------------------------

    /// The pointer moved, or ⌘/Ctrl went down or up (`held`): the word under
    /// the pointer is a link while ⌘/Ctrl-click would go somewhere (like VS
    /// Code, only once the server knows its declaration).
    pub(super) fn update_link(&mut self, position: Point<Pixels>, held: bool, window: &mut Window, cx: &mut Context<Self>) {
        let word = if held && self.drag.is_none() { self.word_under(position, window, cx) } else { None };
        self.link_word(word, cx);
    }

    /// ⌘/Ctrl went down or up. The workspace tells every shown editor, since
    /// only the focused one hears it: the pointer may rest on another.
    pub fn secondary_modifier_changed(&mut self, held: bool, window: &mut Window, cx: &mut Context<Self>) {
        if held || self.lsp.link.is_some() || self.lsp.link_pending.is_some() {
            self.update_link(window.mouse_position(), held, window, cx);
        }
    }

    /// Shows `word` as a link if the server has a declaration for it (a found
    /// one is remembered for the document version); `None` hides the link.
    pub(super) fn link_word(&mut self, word: Option<std::ops::Range<usize>>, cx: &mut Context<Self>) {
        if word.is_some() && [&self.lsp.link, &self.lsp.link_pending, &self.lsp.link_none].contains(&&word) {
            return;
        }
        // Another word: what was asked about the last one no longer matters.
        self.lsp.link_task = None;
        self.lsp.link_pending = None;
        self.lsp.link_none = None;
        if self.lsp.link.take().is_some() {
            cx.notify();
        }
        let Some(word) = word else { return };
        let version = self.doc.read(cx).version();
        if self.lsp.link_found.0 != version {
            self.lsp.link_found = (version, Vec::new());
        }
        if self.lsp.link_found.1.contains(&word) {
            self.lsp.link = Some(word);
            cx.notify();
            return;
        }
        let requests = self.position_requests("definitionProvider", word.start, cx);
        if requests.is_empty() {
            return;
        }
        self.lsp.link_pending = Some(word.clone());
        self.lsp.link_task = Some(cx.spawn(async move |this, cx| {
            let answers = ask_all(requests, "textDocument/definition").await;
            let found = answers.iter().any(|(_, r)| r.as_ref().is_ok_and(|v| !protocol::locations(v).is_empty()));
            this.update(cx, |e, cx| {
                e.lsp.link_pending = None;
                if !found {
                    e.lsp.link_none = Some(word);
                } else if e.lsp.link_found.0 == version {
                    e.lsp.link_found.1.push(word.clone());
                    e.lsp.link = Some(word);
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    /// The word whose text is under `position`: not past the end of its line,
    /// on rows between lines (problems, lenses), the gutter or pinned lines.
    fn word_under(&mut self, position: Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) -> Option<std::ops::Range<usize>> {
        let layout = self.layout.clone()?;
        let over_text = self.mode == EditorMode::Full
            && layout.bounds.contains(&position)
            && position.x >= layout.bounds.left() + layout.gutter_width
            && position.x < layout.bounds.right() - px(super::SCROLLBAR_WIDTH);
        if !over_text || self.sticky_at(position).is_some() {
            return None;
        }
        self.sync_wrap(window, cx);
        let word = super::word_range(self.doc.read(cx), self.offset_for_position(position, window, cx));
        let drow = ((position.y - layout.bounds.top() + self.scroll.y) / layout.line_height).floor() as usize;
        if word.is_empty() || self.display_row_of(word.start, cx) != drow || self.display_row_of(word.end, cx) != drow {
            return None;
        }
        let left = layout.text_left - self.scroll.x;
        let start = left + self.x_for_offset(word.start, window, cx);
        let end = left + self.x_for_offset(word.end, window, cx);
        (start <= position.x && position.x <= end).then_some(word)
    }

    // ---- What's in view ------------------------------------------------------------------

    /// The buffer lines shown this frame. The inlay hints and lens titles
    /// they and the lines around them need are asked for when they're
    /// missing or out of date.
    pub(super) fn lsp_view(&mut self, lines: Range<usize>, cx: &mut Context<Self>) {
        self.lsp.view = Some(lines);
        self.want_inlay_hints(cx);
        self.want_code_lenses(cx);
    }

    /// The lines whose hints and lens titles are wanted: the ones shown and
    /// as many again above and below, so scrolling a little finds them
    /// there. Before the editor is drawn, the file's first lines.
    fn wanted_lines(&self, cx: &App) -> Range<usize> {
        let count = self.doc.read(cx).line_count();
        match &self.lsp.view {
            Some(view) => {
                let margin = view.len().max(1);
                view.start.saturating_sub(margin).min(count)..(view.end + margin).min(count)
            }
            None => 0..HINT_CHUNK.min(count),
        }
    }

    // ---- Inlay hints ---------------------------------------------------------------------

    fn inlay_hints_on(&self, cx: &App) -> bool {
        self.mode == EditorMode::Full && !self.doc.read(cx).large && self.file_settings.inlay_hints
    }

    /// After an edit (or a reload, or hints turned on): asks for the hints
    /// in view once typing pauses (the ones there follow edits meanwhile).
    pub(super) fn schedule_inlay_hints(&mut self, cx: &mut Context<Self>) {
        if !self.inlay_hints_on(cx) {
            // Off here: another editor of the file may show the document's
            // hints, so they stay (this one just doesn't draw them).
            self.lsp.inlay_task = None;
            return;
        }
        self.fetch_inlay_hints_after(EDIT_PAUSE, cx);
    }

    /// Each frame: asks for the hints in view that aren't for today's text,
    /// soon after scrolling, once typing pauses after an edit (another
    /// editor's of the file, or a server saying they changed).
    fn want_inlay_hints(&mut self, cx: &mut Context<Self>) {
        if !self.inlay_hints_on(cx) {
            return;
        }
        let stamp = self.doc.read(cx).hint_stamp();
        let pending = self.lsp.inlay_task.as_ref().map(|(stamp, _)| *stamp);
        if pending == Some(stamp) || self.due_hint_chunks(cx).is_empty() || self.lsp_server_with("inlayHintProvider", cx).is_none() {
            return;
        }
        // Edited since the last answer, or since one was scheduled (typing
        // goes on): the pause starts again.
        let edited = pending.is_some_and(|(version, _)| version != stamp.0) || !self.doc.read(cx).hints_current();
        self.fetch_inlay_hints_after(if edited { EDIT_PAUSE } else { SCROLL_PAUSE }, cx);
    }

    /// The chunks of `HINT_CHUNK` lines in view whose hints weren't asked
    /// for in today's text.
    fn due_hint_chunks(&self, cx: &App) -> Vec<usize> {
        let lines = self.wanted_lines(cx);
        let doc = self.doc.read(cx);
        (lines.start / HINT_CHUNK..lines.end.div_ceil(HINT_CHUNK)).filter(|&chunk| !doc.hints_fresh(chunk)).collect()
    }

    /// After `delay`, asks for the hints of the chunks in view that are due,
    /// a request per run of neighboring chunks.
    fn fetch_inlay_hints_after(&mut self, delay: Duration, cx: &mut Context<Self>) {
        let stamp = self.doc.read(cx).hint_stamp();
        let task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            let asked = this.update(cx, |e, cx| {
                let server = e.lsp_server_with("inlayHintProvider", cx)?;
                let document = e.text_document(cx)?;
                let runs = e.hint_runs(&e.due_hint_chunks(cx), server.encoding, cx);
                (!runs.is_empty()).then(|| (server, document, runs, e.doc.read(cx).hint_stamp()))
            });
            let Ok(Some((server, document, runs, stamp))) = asked else {
                this.update(cx, |e, _| e.lsp.inlay_task = None).ok();
                return;
            };
            let answers = futures::future::join_all(runs.iter().map(|run| {
                server.request("textDocument/inlayHint", json!({ "textDocument": document, "range": run.range.to_json() }))
            }))
            .await;
            this.update(cx, |e, cx| {
                e.lsp.inlay_task = None;
                // Edited meanwhile (in another editor of the file): the next
                // frame asks again. The server said they changed meanwhile:
                // they're taken, and stay due.
                if e.doc.read(cx).version() == stamp.0 {
                    let fresh = e.doc.read(cx).hint_stamp() == stamp;
                    for (run, answer) in runs.into_iter().zip(answers) {
                        // No answer counts as asked too: asking again every
                        // frame would flood a server that refuses (it says
                        // when to ask again, `workspace/inlayHint/refresh`).
                        let hints = answer.ok().map(|value| read_inlay_hints(&value, e.doc.read(cx).rope(), server.encoding, &run.bytes));
                        e.doc.as_mut(cx).set_chunk_hints(run.bytes, if fresh { run.chunks } else { 0..0 }, hints);
                    }
                }
                cx.notify();
            })
            .ok();
        });
        self.lsp.inlay_task = Some((stamp, task));
    }

    /// The requests for `chunks` (sorted): one per run of neighbors.
    fn hint_runs(&self, chunks: &[usize], encoding: Encoding, cx: &App) -> Vec<HintRun> {
        let doc = self.doc.read(cx);
        let mut runs: Vec<Range<usize>> = Vec::new();
        for &chunk in chunks {
            match runs.last_mut() {
                Some(run) if run.end == chunk => run.end += 1,
                _ => runs.push(chunk..chunk + 1),
            }
        }
        runs.into_iter()
            .map(|chunks| {
                let start = doc.line_start(chunks.start * HINT_CHUNK);
                let to_end = chunks.end * HINT_CHUNK >= doc.line_count();
                let end = if to_end { doc.len() } else { doc.line_start(chunks.end * HINT_CHUNK) };
                // A hint at the very end of the file is the last chunk's.
                HintRun { chunks, bytes: start..end + usize::from(to_end), range: offsets_to_range(doc.rope(), start..end, encoding) }
            })
            .collect()
    }

    // ---- Code lens ------------------------------------------------------------------------

    fn code_lenses_on(&self, cx: &App) -> bool {
        self.mode == EditorMode::Full && !self.doc.read(cx).large && self.file_settings.code_lens
    }

    /// After edits settle (or a reload, or lenses turned on), asks every
    /// server that has them for the file's code lenses, shown above lines.
    /// One sent without its command gets it, and its title, once it's in
    /// view (`want_lens_titles`); the ones in view now get theirs before
    /// they show, so their titles don't blink.
    pub(super) fn schedule_code_lenses(&mut self, cx: &mut Context<Self>) {
        if !self.code_lenses_on(cx) {
            self.lsp.lens_task = None;
            self.lsp.lens_resolve = None;
            if !self.doc.read(cx).code_lenses().is_empty() {
                let stamp = self.doc.read(cx).lens_stamp();
                self.doc.as_mut(cx).set_code_lenses(Vec::new(), stamp);
                cx.notify();
            }
            return;
        }
        let stamp = self.doc.read(cx).lens_stamp();
        let task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(LENS_EDIT_PAUSE).await;
            let asked = this.update(cx, |e, cx| {
                let servers: Vec<Arc<Server>> = e.lsp_servers(cx).into_iter().filter(|s| s.capability("codeLensProvider")).collect();
                if servers.is_empty() {
                    return None;
                }
                Some((servers, json!({ "textDocument": e.text_document(cx)? }), e.doc.read(cx).lens_stamp()))
            });
            let Ok(Some((servers, params, stamp))) = asked else {
                this.update(cx, |e, _| e.lsp.lens_task = None).ok();
                return;
            };
            let mut found: Vec<(Arc<Server>, Vec<Value>)> = Vec::new();
            for server in servers {
                if let Ok(Value::Array(lenses)) = server.request("textDocument/codeLens", params.clone()).await {
                    found.push((server, lenses));
                }
            }
            // Read in the text they're for: edited meanwhile, the next frame
            // asks again.
            let read = this.update(cx, |e, cx| {
                (e.doc.read(cx).version() == stamp.0).then(|| {
                    let lenses = e.read_code_lenses(found, cx);
                    let in_view = e.lenses_to_resolve(&lenses, cx);
                    (lenses, in_view)
                })
            });
            let Ok(Some((mut lenses, in_view))) = read else {
                this.update(cx, |e, cx| {
                    e.lsp.lens_task = None;
                    cx.notify();
                })
                .ok();
                return;
            };
            let answers = futures::future::join_all(in_view.iter().map(|(_, server, lens)| server.request("codeLens/resolve", lens.clone()))).await;
            this.update(cx, |e, cx| {
                e.lsp.lens_task = None;
                // The server said they changed meanwhile: they're taken, and stay due.
                if e.doc.read(cx).version() == stamp.0 {
                    let mut gone = Vec::new();
                    for ((ix, ..), answer) in in_view.into_iter().zip(answers) {
                        match answer.ok().as_ref().and_then(lens_command) {
                            Some((title, command)) => (lenses[ix].title, lenses[ix].command, lenses[ix].unresolved) = (title.into(), Arc::new(command), None),
                            None => gone.push(ix),
                        }
                    }
                    let lenses = lenses.into_iter().enumerate().filter(|(ix, _)| !gone.contains(ix)).map(|(_, lens)| lens).collect();
                    // Titles being asked for are for the lenses these replace.
                    e.lsp.lens_resolve = None;
                    e.doc.as_mut(cx).set_code_lenses(lenses, stamp);
                }
                cx.notify();
            })
            .ok();
        });
        self.lsp.lens_task = Some((stamp, task));
    }

    /// Each frame: asks for the lenses when they aren't for today's text (a
    /// server said they changed, or another editor of the file edited it),
    /// else for the titles of the ones in view that have none yet.
    fn want_code_lenses(&mut self, cx: &mut Context<Self>) {
        if !self.code_lenses_on(cx) {
            return;
        }
        let pending = self.lsp.lens_task.as_ref().map(|(stamp, _)| *stamp);
        if self.doc.read(cx).lenses_current() {
            if pending.is_none() {
                self.want_lens_titles(cx);
            }
            return;
        }
        // A server says they changed again (rust-analyzer does after every
        // edit anywhere): the pause starts again.
        if pending != Some(self.doc.read(cx).lens_stamp()) && self.lsp_servers(cx).iter().any(|s| s.capability("codeLensProvider")) {
            self.schedule_code_lenses(cx);
        }
    }

    /// The lenses in view without a title get theirs (`codeLens/resolve`)
    /// a moment after they came into view, a batch at a time.
    fn want_lens_titles(&mut self, cx: &mut Context<Self>) {
        if self.lsp.lens_resolve.is_some() || self.lenses_to_resolve(self.doc.read(cx).code_lenses(), cx).is_empty() {
            return;
        }
        self.lsp.lens_resolve = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(LENS_PAUSE).await;
            let Ok(asked) = this.update(cx, |e, cx| {
                let lenses = e.doc.read(cx).code_lenses();
                e.lenses_to_resolve(lenses, cx).into_iter().map(|(ix, server, lens)| (lenses[ix].id, server, lens)).collect::<Vec<_>>()
            }) else {
                return;
            };
            let answers = futures::future::join_all(asked.iter().map(|(_, server, lens)| server.request("codeLens/resolve", lens.clone()))).await;
            this.update(cx, |e, cx| {
                e.lsp.lens_resolve = None;
                let mut doc = e.doc.as_mut(cx);
                for ((id, ..), answer) in asked.into_iter().zip(answers) {
                    // One that has no command, or no answer, goes.
                    doc.resolve_code_lens(id, answer.ok().as_ref().and_then(lens_command));
                }
                drop(doc);
                cx.notify();
            })
            .ok();
        }));
    }

    /// Servers' code lenses (`textDocument/codeLens`) in today's text,
    /// sorted: ones with a command as they are, others to resolve (when
    /// their server does; else they're dropped).
    fn read_code_lenses(&self, found: Vec<(Arc<Server>, Vec<Value>)>, cx: &App) -> Vec<CodeLens> {
        let doc = self.doc.read(cx);
        let mut out = Vec::new();
        for (server, lenses) in found {
            let resolves = server.resolves("codeLensProvider");
            for lens in lenses {
                let Some(start) = lens.pointer("/range/start").and_then(protocol::Position::from_json) else { continue };
                let line_start = doc.line_start(doc.row_of(position_to_offset(doc.rope(), start, server.encoding)));
                let (title, command, unresolved) = match lens_command(&lens) {
                    Some((title, command)) => (title, command, None),
                    None if resolves && lens.get("command").is_none_or(Value::is_null) => (String::new(), Value::Null, Some(lens)),
                    None => continue,
                };
                out.push(CodeLens {
                    id: 0,
                    line_start,
                    title: title.into(),
                    command: Arc::new(command),
                    server: server.id.as_str().into(),
                    unresolved: unresolved.map(Arc::new),
                });
            }
        }
        out.sort_by_key(|l| l.line_start);
        out
    }

    /// The lenses among `lenses` (sorted) on the lines in view that have no
    /// title yet: their index, their server and what to send it.
    fn lenses_to_resolve(&self, lenses: &[CodeLens], cx: &App) -> Vec<(usize, Arc<Server>, Value)> {
        let doc = self.doc.read(cx);
        let lines = self.wanted_lines(cx);
        let start = doc.line_start(lines.start);
        let end = if lines.end < doc.line_count() { doc.line_start(lines.end) } else { doc.len() + 1 };
        let in_view = lenses.partition_point(|l| l.line_start < start)..lenses.partition_point(|l| l.line_start < end);
        let wanted: Vec<usize> = in_view.filter(|&ix| lenses[ix].unresolved.is_some()).collect();
        if wanted.is_empty() {
            return Vec::new();
        }
        let servers = self.lsp_servers(cx);
        wanted
            .into_iter()
            .filter_map(|ix| {
                let server = servers.iter().find(|s| *s.id == *lenses[ix].server)?.clone();
                Some((ix, server, Value::clone(lenses[ix].unresolved.as_deref()?)))
            })
            .collect()
    }

    /// Carries out a clicked code lens. References and locations open the
    /// usages list; rust-analyzer's Run and Debug run Kuana's target for the
    /// line; anything else goes to the server if it runs that command.
    pub(super) fn run_code_lens(&mut self, lens: CodeLens, cx: &mut Context<Self>) {
        let name = lens.command["command"].as_str().unwrap_or_default().to_string();
        let args = lens.command.get("arguments").cloned().unwrap_or(Value::Null);
        let server = self.lsp_servers(cx).into_iter().find(|s| *s.id == *lens.server);
        let row = self.doc.read(cx).row_of(lens.line_start);
        let navigate = |locations: Vec<protocol::Location>, server: Option<Arc<Server>>, cx: &mut Context<Self>| match server {
            Some(server) if !locations.is_empty() => {
                cx.emit(EditorLspEvent::Navigate { title: lens.title.clone().into(), locations, encoding: server.encoding, panel: false })
            }
            _ => cx.emit(EditorLspEvent::Message("Nothing found".into())),
        };
        match name.as_str() {
            "editor.action.showReferences" | "rust-analyzer.showReferences" => navigate(protocol::locations(&args[2]), server, cx),
            "rust-analyzer.gotoLocation" => navigate(protocol::locations(&args[0]), server, cx),
            "rust-analyzer.runSingle" | "rust-analyzer.debugSingle" => {
                let debug = name.ends_with("debugSingle");
                // What the lens describes, else Kuana's own target on that line.
                let target = crate::run::from_rust_analyzer(&args[0]).filter(|t| !debug || t.debug.is_some()).or_else(|| {
                    let targets = self.run_targets(cx);
                    targets
                        .iter()
                        .filter(|t| t.row == Some(row) || t.rows.contains(&row))
                        .filter(|t| !debug || t.debug.is_some())
                        .min_by_key(|t| t.rows.len())
                        .cloned()
                });
                match target {
                    Some(target) => cx.emit(super::EditorRunEvent { target, debug, menu_at: None }),
                    None => cx.emit(EditorLspEvent::Message(if debug { "Nothing to debug here" } else { "Nothing to run here" }.into())),
                }
            }
            _ => match server.filter(|s| s.executes(&name)) {
                Some(server) => cx.emit(EditorLspEvent::Command { server, command: (*lens.command).clone() }),
                None => cx.emit(EditorLspEvent::Message(format!("Kuana can't run “{}”", lens.title).into())),
            },
        }
    }

    // ---- Symbol highlights ---------------------------------------------------------------

    /// After the caret settles, asks for the other uses of the symbol under it.
    pub(super) fn schedule_symbol_highlights(&mut self, cx: &mut Context<Self>) {
        if self.mode != EditorMode::Full {
            return;
        }
        let head = self.primary_selection().head;
        // Leaving the highlighted symbol drops them now rather than after the request.
        let (version, ranges) = &self.lsp.symbol_highlights;
        if *version != self.doc.read(cx).version() || !ranges.iter().any(|r| r.start <= head && head <= r.end) {
            self.lsp.symbol_highlights.1.clear();
        }
        self.lsp.symbol_highlight_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(std::time::Duration::from_millis(200)).await;
            let Ok((requests, version)) = this.update(cx, |e, cx| {
                (e.position_requests("documentHighlightProvider", e.primary_selection().head, cx), e.doc.read(cx).version())
            }) else {
                return;
            };
            if requests.is_empty() {
                return;
            }
            let answers = ask_all(requests, "textDocument/documentHighlight").await;
            this.update(cx, |e, cx| {
                if e.doc.read(cx).version() != version {
                    return;
                }
                let rope = e.doc.read(cx).rope();
                // Every server's, each read in its own encoding.
                let mut ranges: Vec<std::ops::Range<usize>> = answers
                    .iter()
                    .filter_map(|(server, result)| Some((server, result.as_ref().ok()?.as_array()?)))
                    .flat_map(|(server, highlights)| {
                        highlights
                            .iter()
                            .filter_map(|h| protocol::LspRange::from_json(h.get("range")?))
                            .map(|r| protocol::range_to_offsets(rope, r, server.encoding))
                    })
                    .collect();
                ranges.sort_by_key(|r| (r.start, r.end));
                ranges.dedup();
                // A lone match is just the word itself: nothing worth pointing out.
                if ranges.len() < 2 {
                    ranges.clear();
                }
                e.lsp.symbol_highlights = (version, ranges);
                cx.notify();
            })
            .ok();
        }));
    }

    /// The highlights that still match the text.
    pub(super) fn current_symbol_highlights(&self, cx: &App) -> &[std::ops::Range<usize>] {
        let (version, ranges) = &self.lsp.symbol_highlights;
        if *version == self.doc.read(cx).version() { ranges } else { &[] }
    }

    // ---- Navigation, rename and code actions ------------------------------------------------

    pub(super) fn go_to_declaration(&mut self, _: &super::GoToDeclaration, _: &mut Window, cx: &mut Context<Self>) {
        let head = self.primary_selection().head;
        self.navigate_from(head, cx);
    }

    /// Go to Declaration from an offset (⌘/Ctrl-click passes the click's). On
    /// the declaration itself it lists the usages instead, like JetBrains' Go to
    /// Declaration or Usages (and VS Code's ⌘-click on a definition).
    pub(super) fn navigate_from(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.locations_request("textDocument/definition", offset, "Declaration", "No declaration found", Value::Null, Show::OrUsages, cx);
    }

    pub(super) fn go_to_implementation(&mut self, _: &super::GoToImplementation, _: &mut Window, cx: &mut Context<Self>) {
        let head = self.primary_selection().head;
        let name = self.doc.read(cx).slice(super::word_range(self.doc.read(cx), head));
        let title = format!("Implementations of {name}");
        self.locations_request("textDocument/implementation", head, &title, "No implementations found", Value::Null, Show::Pick, cx);
    }

    pub(super) fn go_to_type_definition(&mut self, _: &super::GoToTypeDefinition, _: &mut Window, cx: &mut Context<Self>) {
        let head = self.primary_selection().head;
        self.locations_request("textDocument/typeDefinition", head, "Type Definition", "No type definition found", Value::Null, Show::Pick, cx);
    }

    pub(super) fn call_hierarchy(&mut self, _: &super::CallHierarchy, _: &mut Window, cx: &mut Context<Self>) {
        self.hierarchy(crate::hierarchy::Kind::Call, cx);
    }

    pub(super) fn type_hierarchy(&mut self, _: &super::TypeHierarchy, _: &mut Window, cx: &mut Context<Self>) {
        self.hierarchy(crate::hierarchy::Kind::Type, cx);
    }

    /// Asks for the hierarchy items at the caret; the workspace shows them.
    fn hierarchy(&mut self, kind: crate::hierarchy::Kind, cx: &mut Context<Self>) {
        let Some(server) = self.lsp_server_with(kind.capability(), cx) else {
            let message = match self.lsp_server(cx) {
                Some(server) => format!("{} has no {}", server.name, kind.name().to_lowercase()),
                None => "No language server for this file".into(),
            };
            cx.emit(EditorLspEvent::Message(message.into()));
            return;
        };
        let head = self.primary_selection().head;
        let Some(params) = self.position_params(head, &server, cx) else { return };
        cx.spawn(async move |this, cx| {
            let result = server.request(kind.prepare(), params).await;
            this.update(cx, |_, cx| match result {
                Ok(Value::Array(items)) if !items.is_empty() => cx.emit(EditorLspEvent::Hierarchy { kind, server, items }),
                Ok(_) => cx.emit(EditorLspEvent::Message(format!("No {} here", kind.name().to_lowercase()).into())),
                Err(err) if err.is_cancelled() => {}
                Err(err) => cx.emit(EditorLspEvent::Message(err.message.into())),
            })
            .ok();
        })
        .detach();
    }

    pub(super) fn find_usages(&mut self, _: &super::FindUsages, _: &mut Window, cx: &mut Context<Self>) {
        let head = self.primary_selection().head;
        self.find_usages_at(head, true, cx);
    }

    /// `panel`: in the Find panel (Find Usages), else to pick from (Go to
    /// Declaration on a declaration, as JetBrains' popup).
    fn find_usages_at(&mut self, offset: usize, panel: bool, cx: &mut Context<Self>) {
        let name = self.doc.read(cx).slice(super::word_range(self.doc.read(cx), offset));
        let context = json!({ "includeDeclaration": true });
        let show = if panel { Show::Panel } else { Show::Pick };
        self.locations_request("textDocument/references", offset, &format!("Usages of {name}"), "No usages found", context, show, cx);
    }

    #[allow(clippy::too_many_arguments)]
    fn locations_request(
        &mut self,
        method: &'static str,
        offset: usize,
        title: &str,
        none: &'static str,
        context: Value,
        show: Show,
        cx: &mut Context<Self>,
    ) {
        // `textDocument/definition` → `definitionProvider`.
        let capability = format!("{}Provider", method.trim_start_matches("textDocument/"));
        let mut requests = self.position_requests(&capability, offset, cx);
        // No server says it can: the main one is asked anyway.
        if requests.is_empty()
            && let Some(server) = self.lsp_server(cx)
            && let Some(params) = self.position_params(offset, &server, cx)
        {
            requests.push((server, params));
        }
        if requests.is_empty() {
            cx.emit(EditorLspEvent::Message("No language server for this file".into()));
            return;
        }
        if !context.is_null() {
            for (_, params) in &mut requests {
                params["context"] = context.clone();
            }
        }
        let title: SharedString = title.to_string().into();
        let (path, rope) = (self.doc.read(cx).path.clone(), self.doc.read(cx).rope().clone());
        cx.spawn(async move |this, cx| {
            let answers = ask_all(requests, method).await;
            let mut found = Vec::new();
            let mut error = None;
            for (server, result) in answers {
                match result {
                    Ok(value) => found.push((server.encoding, protocol::locations(&value))),
                    // The server dropped it (the text changed meanwhile): not an error.
                    Err(err) if err.is_cancelled() => {}
                    Err(err) => _ = error.get_or_insert(err),
                }
            }
            // Only when no server answered is a failure worth telling.
            if found.is_empty() {
                if let Some(err) = error {
                    this.update(cx, |_, cx| cx.emit(EditorLspEvent::Message(err.message.into()))).ok();
                }
                return;
            }
            // A server counting columns otherwise may point into other files:
            // they're read off the UI thread.
            let (encoding, locations) = cx
                .background_executor()
                .spawn(async move {
                    merge_locations(found, |p| match path.as_deref() == Some(p) {
                        true => Some(rope.clone()),
                        false => std::fs::read_to_string(p).ok().map(|text| ropey::Rope::from_str(&text)),
                    })
                })
                .await;
            this.update(cx, |this, cx| {
                if show == Show::OrUsages && this.is_declaration_at(offset, &locations, encoding, cx) {
                    this.find_usages_at(offset, false, cx);
                } else if locations.is_empty() {
                    cx.emit(EditorLspEvent::Message(none.into()));
                } else {
                    cx.emit(EditorLspEvent::Navigate { title, locations, encoding, panel: show == Show::Panel });
                }
            })
            .ok();
        })
        .detach();
    }

    /// Whether `locations` are just the name at `offset`. A server that answers
    /// with the whole item (from its first keyword) isn't recognized, and Go to
    /// Declaration jumps there.
    fn is_declaration_at(&self, offset: usize, locations: &[Location], encoding: Encoding, cx: &App) -> bool {
        let doc = self.doc.read(cx);
        let Some(path) = doc.path.as_deref() else { return false };
        // By its real path: servers spell paths their own way (on Windows
        // `c:\…` for Kuana's `\\?\C:\…`).
        let here = |p: &Path| p == path || crate::document_store::key(p) == crate::document_store::key(path);
        let word = super::word_range(doc, offset);
        !locations.is_empty()
            && locations.iter().all(|l| here(&l.path) && word.contains(&protocol::range_to_offsets(doc.rope(), l.range, encoding).start))
    }

    pub(super) fn rename_symbol(&mut self, _: &super::RenameSymbol, _: &mut Window, cx: &mut Context<Self>) {
        let Some(server) = self.lsp_server_with("renameProvider", cx) else {
            cx.emit(EditorLspEvent::Message("Renaming needs a language server".into()));
            return;
        };
        let head = self.primary_selection().head;
        let word = super::word_range(self.doc.read(cx), head);
        let (Some(path), false) = (self.doc.read(cx).path.clone(), word.is_empty()) else { return };
        let position = offset_to_position(self.doc.read(cx).rope(), word.start, server.encoding);
        let current = self.doc.read(cx).slice(word);
        cx.emit(EditorLspEvent::Rename { server, path, position, current });
    }

    pub(super) fn show_context_actions(&mut self, _: &super::ShowContextActions, window: &mut Window, cx: &mut Context<Self>) {
        let servers: Vec<Arc<Server>> = self.lsp_servers(cx).into_iter().filter(|s| s.capability("codeActionProvider")).collect();
        if servers.is_empty() {
            cx.emit(EditorLspEvent::Message("No actions available here".into()));
            return;
        }
        let range = self.primary_selection().range();
        let rope = self.doc.read(cx).rope().clone();
        let Some(document) = self.text_document(cx) else { return };
        let (store, path) = (self.lsp.store.as_ref().and_then(|s| s.upgrade()), self.doc.read(cx).path.clone());
        // Each server is asked for fixes for its own problems under the caret,
        // as it published them (kept by the store, for every editor of the file).
        let requests: Vec<(Arc<Server>, Value)> = servers
            .into_iter()
            .map(|server| {
                let published = match (&store, &path) {
                    (Some(store), Some(path)) => store.read(cx).published(path, &server.id),
                    _ => Vec::new(),
                };
                let diagnostics: Vec<Value> = published
                    .iter()
                    .filter(|d| {
                        protocol::LspRange::from_json(d.get("range").unwrap_or(&Value::Null))
                            .map(|r| protocol::range_to_offsets(&rope, r, server.encoding))
                            .is_some_and(|r| r.start <= range.end && r.end >= range.start)
                    })
                    .cloned()
                    .collect();
                let params = json!({
                    "textDocument": document,
                    "range": offsets_to_range(&rope, range.clone(), server.encoding).to_json(),
                    "context": { "diagnostics": diagnostics, "triggerKind": 1 },
                });
                (server, params)
            })
            .collect();
        let Some(local) = self.local_position(range.start, window, cx) else { return };
        let Some(layout) = self.layout.clone() else { return };
        let position = layout.bounds.origin + local + gpui::point(px(0.), layout.line_height);
        cx.spawn(async move |this, cx| {
            let answers = futures::future::join_all(requests.into_iter().map(|(server, params)| async move {
                let result = server.request("textDocument/codeAction", params).await;
                (server, result)
            }))
            .await;
            let actions: Vec<(Arc<Server>, CodeAction)> = answers
                .into_iter()
                .flat_map(|(server, result)| {
                    let found = result.map(|v| protocol::code_actions(&v)).unwrap_or_default();
                    found.into_iter().map(move |action| (server.clone(), action))
                })
                .collect();
            this.update(cx, |_, cx| {
                if actions.is_empty() {
                    cx.emit(EditorLspEvent::Message("No actions available here".into()));
                } else {
                    cx.emit(EditorLspEvent::CodeActions { actions, position });
                }
            })
            .ok();
        })
        .detach();
    }

    // ---- Popovers ----------------------------------------------------------------------

    pub(super) fn render_lsp_popovers(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut out = Vec::new();
        let Some(layout) = self.layout.clone() else { return out };
        let theme = Theme::get(cx).clone();
        let mono = font(theme.mono_font.clone());
        let panel = || {
            div()
                .absolute()
                .bg(theme.elevated)
                .border_1()
                .border_color(gpui::Hsla::from(theme.border).alpha(0.85))
                .rounded(px(10.))
                .shadow(crate::ui::soft_shadow(&theme, 2))
                .text_size(px(theme.ui_font_size - 0.5))
                .text_color(theme.text)
        };

        if let Some((label, active)) = self.lsp.signature.as_ref().map(|s| (s.label.clone(), s.active.clone())) {
            let head = self.primary_selection().head;
            if let Some(at) = self.local_position(head, window, cx) {
                let highlights = active.map(|r| {
                    (r, HighlightStyle { color: Some(theme.accent.into()), font_weight: Some(FontWeight::BOLD), ..Default::default() })
                });
                let top = (at.y - px(30.)).max(px(0.));
                out.push(
                    panel()
                        .left(at.x.max(px(0.)))
                        .top(top)
                        .px_2()
                        .py_1()
                        .font(mono.clone())
                        .child(StyledText::new(label).with_highlights(highlights))
                        .into_any_element(),
                );
            }
        }

        // A menu still ranking its first matches isn't drawn yet.
        if let Some((start, rows)) = self.lsp.completion.as_ref().map(|m| (m.start, m.matches.len().min(MENU_ROWS))).filter(|(_, rows)| *rows > 0) {
            let at = self.local_position(start, window, cx);
            if let (Some(at), Some(menu)) = (at, self.lsp.completion.as_ref()) {
                let height = px(rows as f32 * MENU_ROW_HEIGHT + 8.);
                let below = at.y + layout.line_height;
                let top = if below + height > layout.bounds.size.height && at.y > height { at.y - height } else { below };
                let items = menu.items.clone();
                let rows_el: Vec<AnyElement> = menu
                    .matches
                    .iter()
                    .enumerate()
                    .skip(menu.scroll)
                    .take(MENU_ROWS)
                    .map(|(ix, &item_ix)| {
                        let item = &items[item_ix];
                        let selected = ix == menu.selected;
                        div()
                            .id(("completion", ix))
                            .relative()
                            .flex()
                            .items_center()
                            .gap_2()
                            .h(px(MENU_ROW_HEIGHT))
                            .px_2()
                            .rounded(px(6.))
                            .cursor_pointer()
                            .when(selected, |el| el.bg(theme.accent_soft))
                            .on_mouse_down(MouseButton::Left, cx.listener(move |e, _, _, cx| {
                                cx.stop_propagation();
                                e.accept_completion(ix, false, cx);
                            }))
                            .child(div().flex_shrink_0().font(mono.clone()).child(item.label.clone()))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_color(theme.text_dim)
                                    .child(item.detail.clone().unwrap_or_default()),
                            )
                            .child(div().flex_shrink_0().text_size(px(11.)).text_color(theme.text_dim).child(item.kind.unwrap_or("")))
                            .into_any_element()
                    })
                    .collect();
                out.push(
                    panel()
                        .left(at.x - px(8.))
                        .top(top)
                        .w(px(440.))
                        .p_1()
                        .on_mouse_move(|_, _, cx| cx.stop_propagation())
                        .children(rows_el)
                        .into_any_element(),
                );
            }
        }

        if let Some(hover) = self.lsp.hover.as_ref() {
            let mut body = div().flex().flex_col().gap_2();
            for (severity, message) in &hover.diagnostics {
                let color = super::severity_color(&theme, *severity);
                body = body.child(div().text_color(color).child(message.clone()));
            }
            for block in &hover.blocks {
                body = body.child(if block.code {
                    div()
                        .font(mono.clone())
                        .text_size(px(theme.editor_font_size - 1.))
                        .px_2()
                        .py_1()
                        .rounded(px(4.))
                        .bg(theme.background)
                        .child(block.text.clone())
                } else {
                    div().text_color(theme.text_muted).child(block.text.clone())
                });
            }
            // Above the word when it fits, else on the side with more room,
            // never taller than that side so it's always fully visible.
            const MAX_WIDTH: f32 = 560.;
            const MAX_HEIGHT: f32 = 320.;
            let gap = px(2.);
            let space_above = hover.anchor.y - gap - px(6.);
            let space_below = layout.bounds.size.height - (hover.anchor.y + layout.line_height) - gap - px(6.);
            let below = space_above < px(MAX_HEIGHT) && space_below > space_above;
            let max_h = if below { space_below } else { space_above }.min(px(MAX_HEIGHT)).max(px(48.));
            let right_edge = layout.bounds.size.width - px(super::SCROLLBAR_WIDTH + 4.);
            let left = hover.anchor.x.min(right_edge - px(MAX_WIDTH)).max(px(0.));
            // Its text, for the Copy button (the popover's text isn't selectable).
            let text: String = hover
                .diagnostics
                .iter()
                .map(|(_, m)| m.clone())
                .chain(hover.blocks.iter().map(|b| b.text.clone()))
                .collect::<Vec<_>>()
                .join("\n\n");
            let copy = div()
                .id("hover-copy")
                .absolute()
                .top(px(4.))
                .right(px(4.))
                .size(px(22.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(5.))
                .border_1()
                .border_color(theme.border)
                .cursor_pointer()
                .invisible()
                .group_hover("hover-popover", |s| s.visible())
                .hover_fill(theme.elevated, theme.hover)
                .tooltip(|_, cx| tooltip_view("Copy", cx))
                .on_click(move |_, _, cx| cx.write_to_clipboard(gpui::ClipboardItem::new_string(text.clone())))
                .child(icon_el(Icon::Copy, px(13.), theme.text_muted.into()));
            let el = panel()
                .group("hover-popover")
                .left(left)
                .max_w(px(MAX_WIDTH))
                // Clicks stay in the popover (they'd move the caret and close it).
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
                // The wheel scrolls the popover, not the editor under it.
                .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
                // Reaching the popover keeps it open (and stops another word's
                // popover from replacing it).
                .on_mouse_move(cx.listener(|e, _, _, cx| {
                    cx.stop_propagation();
                    e.lsp.hover_hide = None;
                    e.cancel_pending_hover();
                }))
                .child(div().id("hover").max_h(max_h).overflow_y_scroll().p_2().child(body))
                .child(copy);
            let el = if below {
                el.top(hover.anchor.y + layout.line_height + gap)
            } else {
                el.bottom(layout.bounds.size.height - hover.anchor.y + gap)
            };
            out.push(el.into_any_element());
        }
        out
    }
}

pub(super) enum CompletionKey {
    Move(isize),
    Accept { replace: bool },
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use gpui::{AppContext, Entity, TestAppContext};

    use super::*;
    use crate::{
        document::Document,
        lsp::{LspStatus, fake_server},
    };

    /// Runs the app until `done` holds; servers answer from other threads.
    fn wait_for(cx: &mut TestAppContext, what: &str, mut done: impl FnMut(&mut TestAppContext) -> bool) {
        let started = Instant::now();
        loop {
            cx.run_until_parked();
            if done(cx) {
                return;
            }
            assert!(started.elapsed() < Duration::from_secs(10), "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn completions_starting_with_what_is_typed_keep_the_servers_ranking() {
        let item = |label: &str, sort: &str| CompletionItem::from_json(json!({ "label": label, "sortText": sort })).unwrap();
        // rust-analyzer on `self.tasks.cl` (a Vec): most methods share one
        // sortText, in no useful order; trait methods rank lower.
        let mut items = vec![
            item("select_nth_unstable", "7fffffff"),
            item("clone_from_slice", "7fffffff"),
            item("copy_from_slice", "0"),
            item("clone", "80000004"),
            item("clear", "7fffffff"),
            item("split_off", "7ffffff1"),
        ];
        let rank = |items: &[CompletionItem], typed: &str| {
            let (matches, selected) = rank_completions(items, typed, &AtomicBool::new(false)).unwrap();
            (matches.iter().map(|&i| items[i].label.clone()).collect::<Vec<_>>(), selected)
        };
        // Fuzzy matches follow, however the server ranks them.
        assert_eq!(rank(&items, "cl").0, ["clear", "clone_from_slice", "clone", "copy_from_slice", "select_nth_unstable"]);
        // An exact match goes first.
        assert_eq!(rank(&items, "clone").0, ["clone", "clone_from_slice"]);
        // Nothing typed: the server's ranking, then labels.
        assert_eq!(rank(&items, "").0[..4], ["copy_from_slice", "split_off", "clear", "clone_from_slice"]);
        // The server's preselected item is chosen among matches as good as
        // the first, but not over an exact match or a prefix match.
        items[1].preselect = true;
        assert_eq!(rank(&items, "cl").1, 1);
        assert_eq!(rank(&items, "clone").1, 0);
        items[1].preselect = false;
        items[2].preselect = true;
        assert_eq!(rank(&items, "cl").1, 0);
    }

    #[test]
    fn long_lists_keep_their_best_matches() {
        let items: Vec<CompletionItem> =
            (0..MAX_MATCHES * 3).map(|i| CompletionItem::from_json(json!({ "label": format!("item{i}"), "sortText": format!("{:05}", MAX_MATCHES * 3 - i) })).unwrap()).collect();
        let (matches, _) = rank_completions(&items, "item", &AtomicBool::new(false)).unwrap();
        // The best by the server's ranking, in order.
        let expected: Vec<usize> = (0..MAX_MATCHES).map(|n| MAX_MATCHES * 3 - 1 - n).collect();
        assert_eq!(matches, expected);
        // A ranking that a newer one replaced stops.
        assert!(rank_completions(&items, "item", &AtomicBool::new(true)).is_none());
    }

    #[gpui::test]
    fn code_lenses_come_resolved_sit_above_their_line_and_run(cx: &mut TestAppContext) {
        if crate::lsp::servers::which("python3").is_none() {
            return;
        }
        cx.executor().allow_parking();
        crate::trust::set_trust_all(true);
        let dir = std::env::temp_dir().join(format!("kuana-lsp-lens-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.rs");
        std::fs::write(&path, "fn a() {}\nfn b() {}\n").unwrap();
        let store = cx.new(|_| {
            let mut store = LspStore::new();
            store.test_specs = Some(vec![fake_server("lens", "")]);
            store
        });
        let editor: Entity<Editor> = cx.new(|cx| Editor::new(Document::open(&path).unwrap(), cx));
        store.update(cx, |s, cx| s.register(&editor, cx));
        wait_for(cx, "the server", |cx| editor.read_with(cx, |e, cx| !e.doc.read(cx).diagnostics.is_empty()));
        // Asked after a pause in editing; resolved for its title.
        editor.update(cx, |e, cx| e.schedule_code_lenses(cx));
        wait_for(cx, "the lens", |cx| {
            cx.executor().advance_clock(std::time::Duration::from_millis(500));
            editor.read_with(cx, |e, cx| !e.doc.read(cx).code_lenses().is_empty())
        });
        let lens = editor.read_with(cx, |e, cx| e.doc.read(cx).code_lenses()[0].clone());
        assert_eq!((&*lens.title, lens.line_start), ("lens lens", 0));
        editor.update(cx, |e, cx| assert_eq!(*e.block_rows(cx), [(0, 1)]));

        // Clicked: a references lens opens the usages list.
        let events = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let seen = events.clone();
        let _subscription = cx.update(|cx| {
            cx.subscribe(&editor, move |_, event: &EditorLspEvent, _| {
                if let EditorLspEvent::Navigate { title, locations, .. } = event {
                    seen.borrow_mut().push((title.to_string(), locations.len()));
                }
            })
        });
        editor.update(cx, |e, cx| e.run_code_lens(lens, cx));
        assert_eq!(*events.borrow(), [("lens lens".to_string(), 1)]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[gpui::test]
    fn inlay_hints_are_asked_again_when_the_file_changes_on_disk(cx: &mut TestAppContext) {
        if crate::lsp::servers::which("python3").is_none() {
            return;
        }
        cx.executor().allow_parking();
        crate::trust::set_trust_all(true);
        let dir = std::env::temp_dir().join(format!("kuana-lsp-hints-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.rs");
        std::fs::write(&path, "let index = 1;\n").unwrap();
        let store = cx.new(|_| {
            let mut store = LspStore::new();
            store.test_specs = Some(vec![fake_server("hints", "")]);
            store
        });
        let editor: Entity<Editor> = cx.new(|cx| Editor::new(Document::open(&path).unwrap(), cx));
        store.update(cx, |s, cx| s.register(&editor, cx));
        let hints = |cx: &mut TestAppContext| editor.read_with(cx, |e, cx| e.doc.read(cx).inlay_hints().iter().map(|h| h.offset).collect::<Vec<_>>());
        // The server's hint goes at the end of the first line.
        wait_for(cx, "the hint", |cx| {
            cx.executor().advance_clock(Duration::from_millis(500));
            hints(cx) == [14]
        });

        // A merge adds a line above: the hint moves with its line until the
        // server answers for the new text.
        std::fs::write(&path, "// merged\nlet index = 1;\n").unwrap();
        let later = std::time::SystemTime::now() + Duration::from_secs(5);
        std::fs::File::options().write(true).open(&path).unwrap().set_modified(later).unwrap();
        editor.update(cx, |e, cx| e.reload_if_changed(cx));
        // Read in the background.
        cx.run_until_parked();
        assert_eq!(hints(cx), [24]);
        wait_for(cx, "the new hint", |cx| {
            cx.executor().advance_clock(Duration::from_millis(500));
            hints(cx) == [9]
        });
        // The file is saved as it is now: a server that checks on save checks again.
        wait_for(cx, "the server's check", |cx| {
            editor.read_with(cx, |e, cx| e.doc.read(cx).diagnostics.iter().any(|d| &*d.message == "hints checked // merged"))
        });
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Hints are asked for by chunks of lines around the view, never for the
    /// whole file: scrolling asks for the ones it comes to, an edit or the
    /// server saying they changed for the ones in view again.
    #[gpui::test]
    fn inlay_hints_come_for_the_lines_in_view(cx: &mut TestAppContext) {
        if crate::lsp::servers::which("python3").is_none() {
            return;
        }
        cx.executor().allow_parking();
        crate::trust::set_trust_all(true);
        let dir = std::env::temp_dir().join(format!("kuana-lsp-hint-view-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.rs");
        std::fs::write(&path, (0..1000).map(|n| format!("line {n}\n")).collect::<String>()).unwrap();
        let store = cx.new(|_| {
            let mut store = LspStore::new();
            store.test_specs = Some(vec![fake_server("hints", "")]);
            store
        });
        let editor: Entity<Editor> = cx.new(|cx| Editor::new(Document::open(&path).unwrap(), cx));
        store.update(cx, |s, cx| s.register(&editor, cx));
        // The rows with a hint, and the hints' labels (which count the server's answers).
        let hints = |cx: &mut TestAppContext| {
            editor.read_with(cx, |e, cx| {
                let doc = e.doc.read(cx);
                doc.inlay_hints().iter().map(|h| (doc.row_of(h.offset), h.label.clone())).collect::<Vec<_>>()
            })
        };
        let rows = |cx: &mut TestAppContext| hints(cx).into_iter().map(|(row, _)| row).collect::<Vec<_>>();
        // Not drawn yet: the first lines'.
        wait_for(cx, "the first hints", |cx| {
            cx.executor().advance_clock(Duration::from_millis(500));
            rows(cx) == [0]
        });
        // Scrolled to line 500: the chunks around it, and only those.
        editor.update(cx, |e, cx| e.lsp_view(500..550, cx));
        wait_for(cx, "the hints in view", |cx| {
            cx.executor().advance_clock(Duration::from_millis(100));
            rows(cx) == [0, 400, 500]
        });
        // An edit: they follow it, then the ones in view are asked for again.
        editor.update(cx, |e, cx| e.edit_and_remap(vec![(0..0, "// top\n".into())], EditKind::Other, cx));
        assert_eq!(rows(cx), [1, 401, 501]);
        wait_for(cx, "the hints in view again", |cx| {
            cx.executor().advance_clock(Duration::from_millis(500));
            rows(cx) == [1, 400, 500]
        });
        // Saved, the server checks the file and says its hints changed: the
        // ones in view are asked for again (by the next frame).
        let before = hints(cx);
        editor.update(cx, |e, cx| e.lsp_did_save(cx));
        wait_for(cx, "the hints asked for again", |cx| {
            editor.update(cx, |e, cx| e.lsp_view(500..550, cx));
            cx.executor().advance_clock(Duration::from_millis(100));
            hints(cx)[1..] != before[1..]
        });
        assert_eq!(rows(cx), [1, 400, 500]);
        assert_eq!(hints(cx)[0], before[0], "out of view, not asked for");
        store.update(cx, |s, _| s.shutdown_all());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Drawing the editor tells it the lines in view, each frame: the hints
    /// asked for are the ones around what it shows (here, scrolled to line 500).
    #[gpui::test]
    fn a_drawn_editor_asks_for_the_hints_it_shows(cx: &mut TestAppContext) {
        if crate::lsp::servers::which("python3").is_none() {
            return;
        }
        cx.executor().allow_parking();
        crate::trust::set_trust_all(true);
        cx.update(|cx| cx.set_global(crate::theme::Theme::blank_for_tests()));
        let dir = std::env::temp_dir().join(format!("kuana-lsp-drawn-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.rs");
        std::fs::write(&path, (0..1000).map(|n| format!("line {n}\n")).collect::<String>()).unwrap();
        let store = cx.new(|_| {
            let mut store = LspStore::new();
            store.test_specs = Some(vec![fake_server("hints", "")]);
            store
        });
        let editor: Entity<Editor> = cx.new(|cx| Editor::new(Document::open(&path).unwrap(), cx));
        store.update(cx, |s, cx| s.register(&editor, cx));
        let cx = cx.add_empty_window();
        let frame = |cx: &mut gpui::VisualTestContext| {
            cx.draw(gpui::point(px(0.), px(0.)), gpui::size(px(800.), px(600.)), |_, _| gpui::AnyView::from(editor.clone()));
        };
        let rows = |cx: &mut TestAppContext| {
            editor.read_with(cx, |e, cx| {
                let doc = e.doc.read(cx);
                doc.inlay_hints().iter().map(|h| doc.row_of(h.offset)).collect::<Vec<_>>()
            })
        };
        // The first frame scrolls to the caret; then the view moves on.
        frame(cx);
        editor.update(cx, |e, cx| e.scroll.y = e.line_height(cx) * 500.);
        let started = Instant::now();
        loop {
            frame(cx);
            cx.executor().advance_clock(Duration::from_millis(100));
            cx.run_until_parked();
            if rows(cx).contains(&500) {
                break;
            }
            assert!(started.elapsed() < Duration::from_secs(10), "timed out waiting for the hints in view: {:?}", rows(cx));
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(rows(cx).iter().all(|row| (400..600).contains(row)), "{:?}", rows(cx));
        store.update(cx, |s, _| s.shutdown_all());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The file's lenses are asked for all at once, but given their titles
    /// (`codeLens/resolve`, a search each for some servers) only in view;
    /// the others keep their rows meanwhile.
    #[gpui::test]
    fn code_lens_titles_come_for_the_lines_in_view(cx: &mut TestAppContext) {
        if crate::lsp::servers::which("python3").is_none() {
            return;
        }
        cx.executor().allow_parking();
        crate::trust::set_trust_all(true);
        let dir = std::env::temp_dir().join(format!("kuana-lsp-lens-view-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.rs");
        std::fs::write(&path, (0..1000).map(|n| format!("line {n}\n")).collect::<String>()).unwrap();
        let store = cx.new(|_| {
            let mut store = LspStore::new();
            store.test_specs = Some(vec![fake_server("lenses", "")]);
            store
        });
        let editor: Entity<Editor> = cx.new(|cx| Editor::new(Document::open(&path).unwrap(), cx));
        store.update(cx, |s, cx| s.register(&editor, cx));
        let lenses = |cx: &mut TestAppContext| editor.read_with(cx, |e, cx| e.doc.read(cx).code_lenses().len());
        let titled = |cx: &mut TestAppContext| {
            editor.read_with(cx, |e, cx| {
                let doc = e.doc.read(cx);
                doc.code_lenses().iter().filter(|l| !l.title.is_empty()).map(|l| doc.row_of(l.line_start)).collect::<Vec<_>>()
            })
        };
        // Not drawn yet: the first lines' lens has its title, the others their rows.
        wait_for(cx, "the lenses", |cx| {
            cx.executor().advance_clock(Duration::from_millis(500));
            lenses(cx) == 10
        });
        assert_eq!(titled(cx), [0]);
        assert_eq!(editor.update(cx, |e, cx| e.block_rows(cx).len()), 10);
        // Scrolled to line 500: its lens gets its title.
        editor.update(cx, |e, cx| e.lsp_view(500..550, cx));
        wait_for(cx, "the title in view", |cx| {
            cx.executor().advance_clock(Duration::from_millis(200));
            titled(cx) == [0, 500]
        });
        store.update(cx, |s, _| s.shutdown_all());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[gpui::test]
    fn go_to_declaration_on_the_declaration_lists_its_usages(cx: &mut TestAppContext) {
        if crate::lsp::servers::which("python3").is_none() {
            return;
        }
        cx.executor().allow_parking();
        crate::trust::set_trust_all(true);
        let dir = std::env::temp_dir().join(format!("kuana-lsp-decl-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.rs");
        std::fs::write(&path, "fn a() {}\nfn b() { a() }\n").unwrap();
        let store = cx.new(|_| {
            let mut store = LspStore::new();
            store.test_specs = Some(vec![fake_server("decl", "")]);
            store
        });
        let editor: Entity<Editor> = cx.new(|cx| Editor::new(Document::open(&path).unwrap(), cx));
        store.update(cx, |s, cx| s.register(&editor, cx));
        wait_for(cx, "the server", |cx| editor.read_with(cx, |e, cx| !e.doc.read(cx).diagnostics.is_empty()));
        let events = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let seen = events.clone();
        let _subscription = cx.update(|cx| {
            cx.subscribe(&editor, move |_, event: &EditorLspEvent, _| {
                if let EditorLspEvent::Navigate { title, locations, .. } = event {
                    seen.borrow_mut().push((title.to_string(), locations.len()));
                }
            })
        });
        // On a usage: the declaration.
        editor.update(cx, |e, cx| e.navigate_from(19, cx));
        wait_for(cx, "the declaration", |_| events.borrow().len() == 1);
        // On the declaration's name: its usages.
        editor.update(cx, |e, cx| e.navigate_from(3, cx));
        wait_for(cx, "the usages", |_| events.borrow().len() == 2);
        assert_eq!(*events.borrow(), [("Declaration".to_string(), 1), ("Usages of a".to_string(), 2)]);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Kuana opens files by their real path; a server may name them its own
    /// way (Windows: `c:\…` for Kuana's `\\?\C:\…`; macOS: `/var` for `/private/var`).
    #[gpui::test]
    fn a_server_naming_the_file_its_own_way_still_means_this_file(cx: &mut TestAppContext) {
        if crate::lsp::servers::which("python3").is_none() {
            return;
        }
        cx.executor().allow_parking();
        crate::trust::set_trust_all(true);
        let dir = std::env::temp_dir().join(format!("kuana-lsp-respell-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.rs"), "fn a() {}\nfn b() { a() }\n").unwrap();
        let path = dir.join("a.rs").canonicalize().unwrap();
        let store = cx.new(|_| {
            let mut store = LspStore::new();
            store.test_specs = Some(vec![fake_server("respell", "")]);
            store
        });
        let editor: Entity<Editor> = cx.new(|cx| Editor::new(Document::open(&path).unwrap(), cx));
        store.update(cx, |s, cx| s.register(&editor, cx));
        // Its diagnostics reach it.
        wait_for(cx, "the server's diagnostics", |cx| editor.read_with(cx, |e, cx| !e.doc.read(cx).diagnostics.is_empty()));
        let events = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let seen = events.clone();
        let _subscription = cx.update(|cx| {
            cx.subscribe(&editor, move |_, event: &EditorLspEvent, _| {
                if let EditorLspEvent::Navigate { title, .. } = event {
                    seen.borrow_mut().push(title.to_string());
                }
            })
        });
        // ⌘/Ctrl-click on the declaration's name: its usages, not a jump to itself.
        editor.update(cx, |e, cx| e.navigate_from(3, cx));
        wait_for(cx, "an answer", |_| !events.borrow().is_empty());
        assert_eq!(*events.borrow(), ["Usages of a"]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[gpui::test]
    fn cmd_hover_links_only_words_with_a_declaration(cx: &mut TestAppContext) {
        if crate::lsp::servers::which("python3").is_none() {
            return;
        }
        cx.executor().allow_parking();
        crate::trust::set_trust_all(true);
        let dir = std::env::temp_dir().join(format!("kuana-lsp-link-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.rs");
        // The fake server knows declarations on the first two lines only.
        std::fs::write(&path, "fn a() {}\nfn b() { a() }\nlet x = 1;\n").unwrap();
        let store = cx.new(|_| {
            let mut store = LspStore::new();
            store.test_specs = Some(vec![fake_server("link", "")]);
            store
        });
        let editor: Entity<Editor> = cx.new(|cx| Editor::new(Document::open(&path).unwrap(), cx));
        store.update(cx, |s, cx| s.register(&editor, cx));
        wait_for(cx, "the server", |cx| editor.read_with(cx, |e, cx| !e.doc.read(cx).diagnostics.is_empty()));
        let link = |cx: &mut TestAppContext| editor.read_with(cx, |e, _| e.lsp.link.clone());
        let answered = |cx: &mut TestAppContext| editor.read_with(cx, |e, _| e.lsp.link_pending.is_none());

        // A usage of `a`: a link once the server answers.
        editor.update(cx, |e, cx| e.link_word(Some(19..20), cx));
        assert_eq!(link(cx), None);
        wait_for(cx, "the link", |cx| link(cx) == Some(19..20));
        // `x` has no declaration: no link, and moving over it doesn't ask again.
        editor.update(cx, |e, cx| e.link_word(Some(29..30), cx));
        wait_for(cx, "the answer", answered);
        assert_eq!(link(cx), None);
        editor.update(cx, |e, cx| e.link_word(Some(29..30), cx));
        assert!(answered(cx));
        // Back on `a`: known already, a link right away.
        editor.update(cx, |e, cx| e.link_word(Some(19..20), cx));
        assert_eq!(link(cx), Some(19..20));
        // Back on `x`: asked again (a server still loading answers nothing).
        editor.update(cx, |e, cx| e.link_word(Some(29..30), cx));
        assert!(!answered(cx));
        wait_for(cx, "the answer", answered);
        assert_eq!(link(cx), None);
        // ⌘/Ctrl up, or an edit: gone.
        editor.update(cx, |e, cx| e.link_word(None, cx));
        assert_eq!(link(cx), None);
        editor.update(cx, |e, cx| e.link_word(Some(19..20), cx));
        wait_for(cx, "the link", |cx| link(cx) == Some(19..20));
        editor.update(cx, |e, cx| e.edit_and_remap(vec![(0..0, " ".into())], EditKind::Other, cx));
        assert_eq!(link(cx), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Accepting a completion puts its text in at once. What resolving it
    /// brings (an import, and here an edit at the end of its line) comes
    /// later, after keys typed meanwhile, and goes where the server meant it
    /// without touching them. The server, like rust-analyzer, answers only
    /// for the text the item was made for: it's asked before the text goes in.
    #[gpui::test]
    fn keys_typed_while_a_completion_resolves_stay(cx: &mut TestAppContext) {
        if crate::lsp::servers::which("python3").is_none() {
            return;
        }
        cx.executor().allow_parking();
        crate::trust::set_trust_all(true);
        let dir = std::env::temp_dir().join(format!("kuana-lsp-resolve-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.rs");
        std::fs::write(&path, "fn main() {\n    fo; // tail\n}\n").unwrap();
        let store = cx.new(|_| {
            let mut store = LspStore::new();
            store.test_specs = Some(vec![fake_server("resolve", "")]);
            store
        });
        let editor: Entity<Editor> = cx.new(|cx| Editor::new(Document::open(&path).unwrap(), cx));
        store.update(cx, |s, cx| s.register(&editor, cx));
        wait_for(cx, "the server", |cx| editor.read_with(cx, |e, cx| !e.doc.read(cx).diagnostics.is_empty()));
        let text = |cx: &mut TestAppContext| editor.read_with(cx, |e, cx| e.doc.read(cx).rope().to_string());
        let complete = |at: usize, cx: &mut TestAppContext| {
            editor.update(cx, |e, cx| {
                e.select_range(at..at, cx);
                e.request_completion(None, cx);
            });
            wait_for(cx, "completion", |cx| editor.read_with(cx, |e, _| e.lsp.completion.as_ref().is_some_and(|m| !m.matches.is_empty())));
        };

        // After `fo`: accepted, then `(` and a word key (which asks for
        // completion again) typed before the server resolves the item.
        complete(18, cx);
        editor.update(cx, |e, cx| {
            assert!(e.completion_key(CompletionKey::Accept { replace: false }, cx));
            e.insert("(", cx);
            e.insert("x", cx);
        });
        assert_eq!(text(cx), "fn main() {\n    foo(x); // tail\n}\n");
        wait_for(cx, "the import", |cx| text(cx).starts_with("use "));
        assert_eq!(text(cx), "use foo;\nfn main() {\n    foo(x); // tail // end\n}\n");
        editor.read_with(cx, |e, _| assert_eq!(e.primary_selection().head, "use foo;\nfn main() {\n    foo(x".len()));

        // Nothing typed meanwhile: one undo takes the completion and its edits.
        let end = text(cx).len();
        editor.update(cx, |e, cx| e.edit_and_remap(vec![(end..end, "fo".into())], EditKind::Other, cx));
        complete(end + 2, cx);
        editor.update(cx, |e, cx| assert!(e.completion_key(CompletionKey::Accept { replace: false }, cx)));
        wait_for(cx, "the second import", |cx| text(cx).starts_with("use foo;\nuse foo;\n"));
        assert_eq!(text(cx), "use foo;\nuse foo;\nfn main() {\n    foo(x); // tail // end\n}\nfoo // end");
        editor.update(cx, |e, cx| e.undo_step(cx));
        assert_eq!(text(cx), "use foo;\nfn main() {\n    foo(x); // tail // end\n}\nfo");
        store.update(cx, |s, _| s.shutdown_all());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The selected item is resolved a moment after it's selected: what it
    /// brings besides its text then goes in with it, in one step, also after
    /// typing on (moved from the text the server answered for). Accepted
    /// while that request is under way, it's asked again, and the server
    /// answers before it hears of the completion or keys typed after it.
    #[gpui::test]
    fn a_resolved_selection_goes_in_with_its_imports(cx: &mut TestAppContext) {
        if crate::lsp::servers::which("python3").is_none() {
            return;
        }
        cx.executor().allow_parking();
        crate::trust::set_trust_all(true);
        let dir = std::env::temp_dir().join(format!("kuana-lsp-resolved-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.rs");
        std::fs::write(&path, "fn main() {\n    f; // tail\n}\n").unwrap();
        let store = cx.new(|_| {
            let mut store = LspStore::new();
            store.test_specs = Some(vec![fake_server("resolve", "")]);
            store
        });
        let editor: Entity<Editor> = cx.new(|cx| Editor::new(Document::open(&path).unwrap(), cx));
        store.update(cx, |s, cx| s.register(&editor, cx));
        wait_for(cx, "the server", |cx| editor.read_with(cx, |e, cx| !e.doc.read(cx).diagnostics.is_empty()));
        let text = |cx: &mut TestAppContext| editor.read_with(cx, |e, cx| e.doc.read(cx).rope().to_string());
        let complete = |at: usize, cx: &mut TestAppContext| {
            editor.update(cx, |e, cx| {
                e.select_range(at..at, cx);
                e.request_completion(None, cx);
            });
            wait_for(cx, "completion", |cx| editor.read_with(cx, |e, _| e.lsp.completion.as_ref().is_some_and(|m| !m.matches.is_empty())));
        };

        // After `f`: once the pause is over the selection is resolved (the
        // server takes a while); typed on, then accepted.
        complete(17, cx);
        wait_for(cx, "the selection resolved", |cx| {
            cx.executor().advance_clock(RESOLVE_PAUSE);
            editor.read_with(cx, |e, _| e.lsp.completion.as_ref().is_some_and(|m| !m.resolved.is_empty()))
        });
        editor.update(cx, |e, cx| {
            e.insert("o", cx);
            assert!(e.completion_key(CompletionKey::Accept { replace: false }, cx));
        });
        assert_eq!(text(cx), "use foo;\nfn main() {\n    foo; // tail // end\n}\n");
        // One undo takes all of it; what was typed stays.
        editor.update(cx, |e, cx| e.undo_step(cx));
        assert_eq!(text(cx), "fn main() {\n    fo; // tail\n}\n");

        // Accepted while that request is under way, and a key typed at once.
        complete(18, cx);
        cx.executor().advance_clock(RESOLVE_PAUSE);
        cx.run_until_parked();
        assert!(editor.read_with(cx, |e, _| e.lsp.resolving.is_some()));
        editor.update(cx, |e, cx| {
            assert!(e.completion_key(CompletionKey::Accept { replace: false }, cx));
            e.insert("(", cx);
        });
        assert_eq!(text(cx), "fn main() {\n    foo(); // tail\n}\n");
        wait_for(cx, "the import", |cx| text(cx).starts_with("use "));
        assert_eq!(text(cx), "use foo;\nfn main() {\n    foo(); // tail // end\n}\n");
        store.update(cx, |s, _| s.shutdown_all());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// rust-analyzer gives an item's import only on `completionItem/resolve`,
    /// and only while the text is the version it made the item for. Accepted
    /// with a key typed at once, `HashSet` gets `use std::collections::HashSet`;
    /// resolved while selected, `HashMap` gets its import as it goes in.
    #[gpui::test]
    #[ignore = "needs rust-analyzer and the standard library's sources (rust-src); run with --ignored"]
    fn rust_analyzer_imports_what_it_completes(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        crate::trust::set_trust_all(true);
        let dir = std::env::temp_dir().join(format!("kuana-ra-imports-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        let dir = dir.canonicalize().unwrap();
        std::fs::write(dir.join("Cargo.toml"), "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n").unwrap();
        let path = dir.join("src/main.rs");
        std::fs::write(&path, "fn main() {\n    let set: HashSe\n}\n").unwrap();
        let store = cx.new(|_| {
            let mut store = LspStore::new();
            store.set_root(Some(dir.clone()));
            store
        });
        let editor: Entity<Editor> = cx.new(|cx| Editor::new(Document::open(&path).unwrap(), cx));
        store.update(cx, |s, cx| s.register(&editor, cx));
        let text = |cx: &mut TestAppContext| editor.read_with(cx, |e, cx| e.doc.read(cx).rope().to_string());
        let wait = |cx: &mut TestAppContext, what: &str, seconds: u64, done: &dyn Fn(&mut TestAppContext) -> bool| {
            let started = Instant::now();
            while !done(cx) {
                assert!(started.elapsed() < Duration::from_secs(seconds), "timed out waiting for {what}: {:?}", text(cx));
                std::thread::sleep(Duration::from_millis(20));
                cx.run_until_parked();
            }
        };
        // The menu's index of the first item `label` starts, once the menu has one.
        let offered = |cx: &mut TestAppContext, label: &str| {
            editor.read_with(cx, |e, _| {
                let menu = e.lsp.completion.as_ref()?;
                menu.matches.iter().position(|&i| menu.items[i].label.starts_with(label))
            })
        };
        // Asked until offered: rust-analyzer loads the project and the standard library first.
        let complete = |cx: &mut TestAppContext, at: usize, label: &str| {
            let started = Instant::now();
            loop {
                editor.update(cx, |e, cx| {
                    e.select_range(at..at, cx);
                    e.request_completion(None, cx);
                });
                let asked = Instant::now();
                while asked.elapsed() < Duration::from_secs(2) {
                    std::thread::sleep(Duration::from_millis(20));
                    cx.run_until_parked();
                    if let Some(ix) = offered(cx, label) {
                        return ix;
                    }
                }
                assert!(started.elapsed() < Duration::from_secs(180), "rust-analyzer never offered {label}");
            }
        };

        // Accepted, and a key typed at once (inside `HashSet<>`).
        let ix = complete(cx, "fn main() {\n    let set: HashSe".len(), "HashSet");
        editor.update(cx, |e, cx| {
            e.accept_completion(ix, false, cx);
            e.insert("u8", cx);
        });
        wait(cx, "HashSet's import", 30, &|cx| text(cx).contains("use std::collections::HashSet;"));
        assert!(text(cx).contains("let set: HashSet<u8>"), "{}", text(cx));

        // Resolved while selected, then accepted: in with it.
        let end = text(cx).rfind('}').unwrap();
        editor.update(cx, |e, cx| e.edit_and_remap(vec![(end..end, "    let map: HashMa\n".into())], EditKind::Other, cx));
        let ix = complete(cx, end + "    let map: HashMa".len(), "HashMap");
        editor.update(cx, |e, cx| {
            e.lsp.completion.as_mut().unwrap().selected = ix;
            e.resolve_selected_soon(cx);
        });
        cx.executor().advance_clock(RESOLVE_PAUSE);
        wait(cx, "HashMap resolved", 30, &|cx| editor.read_with(cx, |e, _| e.lsp.completion.as_ref().is_some_and(|m| !m.resolved.is_empty())));
        editor.update(cx, |e, cx| e.accept_completion(ix, false, cx));
        let imports = text(cx).lines().filter(|l| l.starts_with("use ") && l.contains("HashMap")).count();
        assert_eq!(imports, 1, "{}", text(cx));
        store.update(cx, |s, _| s.shutdown_all());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[gpui::test]
    fn servers_hear_of_the_files_they_watch(cx: &mut TestAppContext) {
        if crate::lsp::servers::which("python3").is_none() {
            return;
        }
        cx.executor().allow_parking();
        crate::trust::set_trust_all(true);
        let dir = std::env::temp_dir().join(format!("kuana-lsp-watch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.rs");
        std::fs::write(&path, "fn a() {}\n").unwrap();
        let store = cx.new(|_| {
            let mut store = LspStore::new();
            store.test_specs = Some(vec![fake_server("watch", "")]);
            store
        });
        let editor: Entity<Editor> = cx.new(|cx| Editor::new(Document::open(&path).unwrap(), cx));
        store.update(cx, |s, cx| s.register(&editor, cx));
        let messages = |cx: &mut TestAppContext| editor.read_with(cx, |e, cx| e.doc.read(cx).diagnostics.iter().map(|d| d.message.to_string()).collect::<Vec<_>>());
        wait_for(cx, "the server", |cx| messages(cx).iter().any(|m| m == "watch problem"));

        // It registered **/*.toml: it hears of Cargo.toml, not of b.rs.
        let changes = [(dir.join("Cargo.toml"), crate::lsp::FileChange::Changed), (dir.join("b.rs"), crate::lsp::FileChange::Created)];
        wait_for(cx, "the watched change", |cx| {
            store.read_with(cx, |s, _| s.files_changed(&changes));
            messages(cx).iter().any(|m| m.starts_with("watch watched"))
        });
        assert_eq!(messages(cx), ["watch watched 2 Cargo.toml"]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[gpui::test]
    fn a_document_gets_every_server(cx: &mut TestAppContext) {
        if crate::lsp::servers::which("python3").is_none() {
            return;
        }
        cx.executor().allow_parking();
        crate::trust::set_trust_all(true);
        let dir = std::env::temp_dir().join(format!("kuana-lsp-servers-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.ts");
        std::fs::write(&path, "x.y\n").unwrap();

        let store = cx.new(|_| {
            let mut store = LspStore::new();
            store.test_specs = Some(vec![fake_server("main", "."), fake_server("extra", "")]);
            store
        });
        let editor: Entity<Editor> = cx.new(|cx| Editor::new(Document::open(&path).unwrap(), cx));
        store.update(cx, |s, cx| s.register(&editor, cx));

        // Both get the text; their diagnostics are shown together, each
        // server's kept apart for code actions.
        let from_servers = |e: &Editor, cx: &gpui::App| e.doc.read(cx).diagnostics.iter().filter(|d| d.message.ends_with(" problem")).count();
        wait_for(cx, "diagnostics", |cx| {
            let found = editor.read_with(cx, |e, cx| from_servers(e, cx));
            if found < 2 {
                eprintln!("status: {:?}, diagnostics: {found}", store.read_with(cx, |s, _| s.status(&path)));
            }
            found == 2
        });
        editor.read_with(cx, |e, cx| {
            let mut messages: Vec<&str> = e.doc.read(cx).diagnostics.iter().map(|d| &*d.message).filter(|m| m.ends_with(" problem")).collect();
            messages.sort();
            assert_eq!(messages, ["extra problem", "main problem"]);
            let store = store.read(cx);
            assert_eq!((store.published(&path, "fake-main").len(), store.published(&path, "fake-extra").len()), (1, 1));
        });
        store.read_with(cx, |s, _| assert_eq!(s.status(&path), Some(LspStatus::Ready("python3, python3".into()))));

        // Completion asks both and lists their items together.
        editor.update(cx, |e, cx| {
            // Just after the dot: nothing typed yet, so every item shows.
            e.select_range(2..2, cx);
            e.request_completion(None, cx);
        });
        wait_for(cx, "completion", |cx| editor.read_with(cx, |e, _| e.lsp.completion.is_some()));
        let labels = |cx: &mut TestAppContext| {
            editor.read_with(cx, |e, _| {
                let menu = e.lsp.completion.as_ref().unwrap();
                let mut labels: Vec<String> = menu.items.iter().map(|i| i.label.clone()).collect();
                labels.sort();
                labels
            })
        };
        assert_eq!(labels(cx), ["extra-item", "main-item"]);

        // A trigger character asks only the servers it triggers.
        editor.update(cx, |e, cx| {
            e.lsp.completion = None;
            e.request_completion(Some(".".into()), cx);
        });
        wait_for(cx, "triggered completion", |cx| editor.read_with(cx, |e, _| e.lsp.completion.is_some()));
        assert_eq!(labels(cx), ["main-item"]);

        // Both know the same declaration: it's listed once.
        let events = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let seen = events.clone();
        let _subscription = cx.update(|cx| {
            cx.subscribe(&editor, move |_, event: &EditorLspEvent, _| {
                if let EditorLspEvent::Navigate { locations, .. } = event {
                    seen.borrow_mut().push(locations.len());
                }
            })
        });
        editor.update(cx, |e, cx| e.navigate_from(0, cx));
        wait_for(cx, "the declaration", |_| !events.borrow().is_empty());
        assert_eq!(*events.borrow(), [1]);

        store.update(cx, |s, _| s.shutdown_all());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[gpui::test]
    fn a_restarted_server_outlives_its_predecessor(cx: &mut TestAppContext) {
        if crate::lsp::servers::which("python3").is_none() {
            return;
        }
        cx.executor().allow_parking();
        crate::trust::set_trust_all(true);
        let dir = std::env::temp_dir().join(format!("kuana-lsp-restart-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.ts");
        std::fs::write(&path, "x\n").unwrap();
        let store = cx.new(|_| {
            let mut store = LspStore::new();
            store.test_specs = Some(vec![fake_server("restarted", "")]);
            store
        });
        let editor: Entity<Editor> = cx.new(|cx| Editor::new(Document::open(&path).unwrap(), cx));
        store.update(cx, |s, cx| s.register(&editor, cx));
        let ready = |cx: &mut TestAppContext| store.read_with(cx, |s, _| matches!(s.status(&path), Some(LspStatus::Ready(_))));
        wait_for(cx, "the first server", ready);
        store.update(cx, |s, cx| s.restart(&path, cx));
        wait_for(cx, "the second server", ready);
        // The first one exits after the restart; that's not the new one failing.
        std::thread::sleep(Duration::from_millis(600));
        cx.run_until_parked();
        store.read_with(cx, |s, _| assert!(matches!(s.status(&path), Some(LspStatus::Ready(_))), "{:?}", s.status(&path)));
        store.update(cx, |s, _| s.shutdown_all());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[gpui::test]
    fn servers_get_their_settings(cx: &mut TestAppContext) {
        if crate::lsp::servers::which("python3").is_none() {
            return;
        }
        cx.executor().allow_parking();
        crate::trust::set_trust_all(true);
        let dir = std::env::temp_dir().join(format!("kuana-lsp-settings-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.ts");
        std::fs::write(&path, "x\n").unwrap();
        let settings = crate::settings::Settings::parse(
            r#"{ "lsp": { "fake-configured": {
                "initialization_options": { "greeting": "hello" },
                "settings": { "fake.mode": "strict" }
            } } }"#,
        )
        .unwrap();
        cx.update(|cx| cx.set_global(settings));

        let store = cx.new(|_| {
            let mut store = LspStore::new();
            store.test_specs = Some(vec![fake_server("configured", "")]);
            store
        });
        let editor: Entity<Editor> = cx.new(|cx| Editor::new(Document::open(&path).unwrap(), cx));
        store.update(cx, |s, cx| s.register(&editor, cx));
        // The server echoes its initialization options and the setting it asked for.
        wait_for(cx, "diagnostics", |cx| {
            editor.read_with(cx, |e, cx| e.doc.read(cx).diagnostics.iter().any(|d| &*d.message == "configured problem hello strict"))
        });
        store.update(cx, |s, _| s.shutdown_all());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A server answering hover, definitions, signature help and highlights
    /// by its name ("main" or "extra"), each in its own way.
    const ANSWERING_SERVER: &str = r#"
import sys, json
name = sys.argv[1]
def read():
    headers = {}
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            sys.exit(0)
        line = line.strip()
        if not line:
            break
        k, v = line.split(b":", 1)
        headers[k.lower()] = v.strip()
    return json.loads(sys.stdin.buffer.read(int(headers[b"content-length"])))
def send(msg):
    body = json.dumps(msg).encode()
    sys.stdout.buffer.write(b"Content-Length: %d\r\n\r\n" % len(body) + body)
    sys.stdout.buffer.flush()
def line(n):
    return {"start": {"line": n, "character": 0}, "end": {"line": n, "character": 1}}
while True:
    m = read()
    method, id = m.get("method"), m.get("id")
    if method == "exit":
        sys.exit(0)
    if method is None or id is None:
        continue
    uri = ((m.get("params") or {}).get("textDocument") or {}).get("uri")
    result = None
    if method == "initialize":
        caps = {"hoverProvider": True, "definitionProvider": True, "signatureHelpProvider": {}, "documentHighlightProvider": True}
        result = {"capabilities": dict(caps, textDocumentSync=1)}
    elif method == "textDocument/hover":
        result = {"contents": {"kind": "plaintext", "value": name + " docs"}}
    elif method == "textDocument/definition":
        # Both know line 1's; only "extra" knows line 2's.
        result = [{"uri": uri, "range": line(n)} for n in ([0, 1] if name == "extra" else [0])]
    elif method == "textDocument/signatureHelp" and name == "extra":
        result = {"signatures": [{"label": "extra(a)"}]}
    elif method == "textDocument/documentHighlight":
        result = [{"range": line(0)}, {"range": line(1 if name == "main" else 2)}]
    send({"jsonrpc": "2.0", "id": id, "result": result})
"#;

    fn answering_server(name: &str) -> &'static crate::lsp::servers::ServerSpec {
        let leak = |s: String| -> &'static str { Box::leak(s.into_boxed_str()) };
        let args: &'static [&'static str] = Box::leak(vec!["-c", ANSWERING_SERVER, leak(name.to_string())].into_boxed_slice());
        let candidates: &'static [(&'static str, &'static [&'static str])] = Box::leak(vec![("python3", args)].into_boxed_slice());
        let id = leak(format!("answering-{name}"));
        Box::leak(Box::new(crate::lsp::servers::ServerSpec { id, candidates, ..crate::lsp::servers::SPEC }))
    }

    /// Hover, declarations, signature help and highlights ask every server
    /// (the second may know what the first doesn't) and put their answers together.
    #[gpui::test]
    fn position_requests_ask_every_server(cx: &mut TestAppContext) {
        if crate::lsp::servers::which("python3").is_none() {
            return;
        }
        cx.executor().allow_parking();
        crate::trust::set_trust_all(true);
        cx.update(|cx| cx.set_global(crate::theme::Theme::blank_for_tests()));
        let dir = std::env::temp_dir().join(format!("kuana-lsp-every-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.rs");
        std::fs::write(&path, "fn a() {}\nfn b() {}\nfn c() {}\n").unwrap();
        let store = cx.new(|_| {
            let mut store = LspStore::new();
            store.test_specs = Some(vec![answering_server("main"), answering_server("extra")]);
            store
        });
        let editor: Entity<Editor> = cx.new(|cx| Editor::new(Document::open(&path).unwrap(), cx));
        store.update(cx, |s, cx| s.register(&editor, cx));
        wait_for(cx, "both servers", |cx| editor.read_with(cx, |e, cx| e.lsp_servers(cx).len() == 2));

        // Declarations: the main server's and the other's, the shared one once.
        let events = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let seen = events.clone();
        let _subscription = cx.update(|cx| {
            cx.subscribe(&editor, move |_, event: &EditorLspEvent, _| {
                if let EditorLspEvent::Navigate { locations, .. } = event {
                    seen.borrow_mut().push(locations.iter().map(|l| l.range.start.line).collect::<Vec<_>>());
                }
            })
        });
        editor.update(cx, |e, cx| e.navigate_from(3, cx));
        wait_for(cx, "the declarations", |_| !events.borrow().is_empty());
        assert_eq!(*events.borrow(), [vec![0, 1]]);

        // Signature help: the main server has none, the other one does.
        editor.update(cx, |e, cx| {
            e.select_range(4..4, cx);
            e.request_signature_help(cx);
        });
        wait_for(cx, "signature help", |cx| editor.read_with(cx, |e, _| e.lsp.signature.is_some()));
        editor.read_with(cx, |e, _| assert_eq!(e.lsp.signature.as_ref().unwrap().label, "extra(a)"));

        // Highlights: both servers', each once, in order.
        editor.update(cx, |e, cx| {
            e.select_range(0..0, cx);
            e.schedule_symbol_highlights(cx);
        });
        wait_for(cx, "highlights", |cx| {
            cx.executor().advance_clock(Duration::from_millis(250));
            editor.read_with(cx, |e, cx| !e.current_symbol_highlights(cx).is_empty())
        });
        editor.read_with(cx, |e, cx| assert_eq!(e.current_symbol_highlights(cx), [0..1, 10..11, 20..21]));

        // Hover: both servers' documentation, the main one's first.
        let cx = cx.add_empty_window();
        cx.draw(gpui::point(px(0.), px(0.)), gpui::size(px(800.), px(600.)), |_, _| gpui::AnyView::from(editor.clone()));
        cx.update(|window, cx| editor.update(cx, |e, cx| e.show_hover(3..4, window, cx)));
        wait_for(cx, "hover", |cx| editor.read_with(cx, |e, _| e.lsp.hover.is_some()));
        editor.read_with(cx, |e, _| {
            let blocks: Vec<&str> = e.lsp.hover.as_ref().unwrap().blocks.iter().map(|b| b.text.as_str()).collect();
            assert_eq!(blocks, ["main docs", "extra docs"]);
        });
        store.update(cx, |s, _| s.shutdown_all());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn locations_from_servers_counting_otherwise_are_converted_and_merged() {
        use protocol::{LspRange, Position};
        let at = |line, start, end| Location {
            path: PathBuf::from("/p/a.rs"),
            range: LspRange { start: Position { line, character: start }, end: Position { line, character: end } },
        };
        // "ğ" is 2 bytes in UTF-8, 1 unit in UTF-16: `x` is at 2 or 1.
        let rope = ropey::Rope::from_str("ğx\n");
        let answers = vec![
            (Encoding::Utf8, vec![at(0, 2, 3)]),
            // The same place and one more, counted in UTF-16.
            (Encoding::Utf16, vec![at(0, 1, 2), at(1, 0, 0)]),
        ];
        let (encoding, merged) = merge_locations(answers, |_| Some(rope.clone()));
        assert_eq!((encoding, merged), (Encoding::Utf8, vec![at(0, 2, 3), at(1, 0, 0)]));
        // A file that can't be read keeps what the server said.
        let (_, merged) = merge_locations(vec![(Encoding::Utf8, vec![at(0, 0, 1)]), (Encoding::Utf16, vec![at(0, 1, 2)])], |_| None);
        assert_eq!(merged, [at(0, 0, 1), at(0, 1, 2)]);
    }
}
