//! Popups for jumping around: Search Everywhere (double Shift), Go to File,
//! Recent Files, Recent Projects, Find Action, File Structure and Go to Line.

use std::{
    ops::Range,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use gpui::{
    Action, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, HighlightStyle,
    Hsla, IntoElement, Render, ScrollStrategy, SharedString, StyledText, Subscription, Task,
    UniformListScrollHandle, Window, div, prelude::*, px, uniform_list,
};
use nucleo_matcher::{
    Config, Matcher, Utf32Str,
    pattern::{CaseMatching, Normalization, Pattern},
};

gpui::actions!(
    picker,
    [
        /// ⌫ / Delete in a picker's field: removes the selected item where a
        /// picker can (Show Bookmarks) and nothing is typed; edits the field otherwise.
        RemoveSelected,
    ]
);

use crate::{
    assets::Icon,
    clone_dialog::display_path,
    lsp::protocol::{Encoding, Location},
    run::RunTarget,
    syntax::Symbol,
    editor::{Cancel, Editor, EditorEvent, MoveDown, MoveUp, Newline, PageDown, PageUp, Submit},
    theme::Theme,
    ui::{HoverFill, binding_label, icon_el, input_box, kbd, modal_panel},
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PickerMode {
    Everywhere,
    Files,
    Recent,
    /// Recently opened folders; `recent` holds them instead of files.
    Projects,
    Actions,
    /// The active file's symbols; see [`Picker::set_symbols`].
    Structure,
    /// Places in the project (usages, declarations); see [`Picker::set_locations`].
    Locations,
    /// Run configurations; see [`Picker::set_run_targets`].
    Run,
    /// The same list, to debug instead (only targets that can be).
    Debug,
    /// Running processes to debug, as targets with `DebugSpec::Attach`.
    Attach,
    /// Symbols across the project from the language servers; the workspace
    /// answers [`PickerEvent::SymbolQuery`] with [`Picker::set_locations`].
    Symbols,
    /// The project's bookmarks; see [`Picker::set_bookmarks`].
    Bookmarks,
    /// A list an extension's code asks the user to pick from; see
    /// [`Picker::set_choices`].
    Choices,
}

/// One row of a [`PickerMode::Choices`] list.
pub struct Choice {
    /// What [`PickerEvent::Chosen`] carries.
    pub id: String,
    pub label: SharedString,
    pub detail: Option<SharedString>,
    /// An asset path for `svg()`.
    pub icon: Option<SharedString>,
    /// A file whose type's (full-color) icon shows instead of `icon`.
    pub file: Option<PathBuf>,
}

/// One place in a [`PickerMode::Locations`] list.
pub struct LocationRow {
    pub location: Location,
    /// The file, relative to the project.
    pub file: String,
    /// 1-based.
    pub line: usize,
    /// The line's text, trimmed (Symbols: the symbol's name).
    pub preview: String,
    pub encoding: Encoding,
}

/// One row of a [`PickerMode::Bookmarks`] list.
pub struct BookmarkRow {
    pub path: PathBuf,
    /// 0-based.
    pub row: usize,
    pub mnemonic: Option<char>,
    pub description: Option<String>,
    /// The file, relative to the project.
    pub file: String,
    /// The line's text, trimmed.
    pub preview: String,
}

pub struct ActionEntry {
    pub name: &'static str,
    pub action: Box<dyn Action>,
}

pub enum PickerEvent {
    OpenFile(PathBuf),
    OpenProject { path: PathBuf, new_window: bool },
    RunAction(Box<dyn Action>),
    /// Jump to an offset in the active editor.
    GoTo(usize),
    OpenLocation { location: Location, encoding: Encoding },
    /// Symbols mode: the query changed.
    SymbolQuery(String),
    Run(RunTarget),
    Debug(RunTarget),
    /// A [`PickerMode::Choices`] row's id.
    Chosen(String),
    /// Bookmarks: go to this one (0-based row).
    OpenBookmark { path: PathBuf, row: usize },
    /// Bookmarks: ⌫ with nothing typed removes the selected one.
    RemoveBookmark { path: PathBuf, row: usize },
    /// A removable Choices list: ⌫ with nothing typed on this row's id.
    RemoveChoice(String),
    Dismissed,
}

#[derive(Clone)]
enum Item {
    Header(SharedString),
    File {
        path: PathBuf,
        name: SharedString,
        dir: SharedString,
        name_hl: Vec<Range<usize>>,
        dir_hl: Vec<Range<usize>>,
    },
    Project {
        path: PathBuf,
        name: SharedString,
        dir: SharedString,
        name_hl: Vec<Range<usize>>,
        dir_hl: Vec<Range<usize>>,
    },
    Action {
        ix: usize,
        hl: Vec<Range<usize>>,
    },
    Symbol {
        ix: usize,
        hl: Vec<Range<usize>>,
        /// Indent by nesting; flat while filtering.
        nested: bool,
    },
    Location {
        ix: usize,
    },
    Run {
        ix: usize,
        hl: Vec<Range<usize>>,
    },
    Choice {
        ix: usize,
        hl: Vec<Range<usize>>,
    },
    Bookmark {
        ix: usize,
    },
}

pub struct Picker {
    mode: PickerMode,
    query: Entity<Editor>,
    root: Option<PathBuf>,
    files: Arc<Vec<PathBuf>>,
    recent: Vec<PathBuf>,
    actions: Arc<Vec<ActionEntry>>,
    shortcuts: Arc<Vec<Option<String>>>,
    /// Symbols with their 1-based line numbers.
    symbols: Arc<Vec<(Symbol, usize)>>,
    /// The symbol around the caret, preselected like JetBrains.
    current_symbol: Option<usize>,
    locations: Arc<Vec<LocationRow>>,
    run_targets: Arc<Vec<RunTarget>>,
    choices: Arc<Vec<Choice>>,
    /// Choices: ⌫ with nothing typed asks to remove the selected row.
    removable: bool,
    bookmarks: Arc<Vec<BookmarkRow>>,
    /// Overrides the mode's title (Locations: "Usages of foo").
    title: Option<SharedString>,
    /// The title's icon, in place of the mode's.
    title_icon: Option<Icon>,
    items: Arc<Vec<Item>>,
    selected: usize,
    scroll: UniformListScrollHandle,
    matcher: Matcher,
    /// The best files for a query (score, index in `files`), found in the
    /// background (see `search_files`), and the query and file list they're for.
    file_matches: Arc<Vec<(u32, usize)>>,
    file_matches_for: Option<(String, usize)>,
    file_search: Option<(Arc<AtomicBool>, Task<()>)>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<PickerEvent> for Picker {}

const MAX_RESULTS: usize = 80;

/// Actions listed under Recent Projects, for folders not in the list yet.
const PROJECT_ACTIONS: [&str; 3] = ["Open Folder…", "Open Folder in This Window…", "New Window"];

impl Picker {
    pub fn new(
        mode: PickerMode,
        root: Option<PathBuf>,
        files: Arc<Vec<PathBuf>>,
        recent: Vec<PathBuf>,
        actions: Arc<Vec<ActionEntry>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let placeholder = match mode {
            PickerMode::Everywhere => "Search files and actions",
            PickerMode::Files => "Go to file",
            PickerMode::Recent => "Search recent files",
            PickerMode::Projects => "Search recent projects",
            PickerMode::Actions => "Search actions",
            PickerMode::Structure => "Search symbols",
            PickerMode::Locations => "Filter",
            PickerMode::Run => "Run configuration",
            PickerMode::Debug => "Debug configuration",
            PickerMode::Attach => "Search processes by name, command or PID",
            PickerMode::Symbols => "Search symbols in the project",
            PickerMode::Choices => "Filter",
            PickerMode::Bookmarks => "Filter bookmarks",
        };
        let query = cx.new(|cx| Editor::single_line(placeholder, cx));
        window.focus(&query.focus_handle(cx));
        let shortcuts = Arc::new(actions.iter().map(|a| binding_label(a.action.as_ref(), window)).collect());
        let subscription = cx.subscribe(&query, |this, query, _: &EditorEvent, cx| {
            if this.mode == PickerMode::Symbols {
                // The current results stay put until the server answers the
                // new query, so the list doesn't jump on every key.
                cx.emit(PickerEvent::SymbolQuery(query.read(cx).text(cx)));
                return;
            }
            this.update_items(cx)
        });
        let mut this = Self {
            mode,
            query,
            root,
            files,
            recent,
            actions,
            shortcuts,
            symbols: Arc::new(Vec::new()),
            current_symbol: None,
            locations: Arc::new(Vec::new()),

            run_targets: Arc::new(Vec::new()),
            choices: Arc::new(Vec::new()),
            removable: false,
            bookmarks: Arc::new(Vec::new()),
            title: None,
            title_icon: None,
            items: Arc::new(Vec::new()),
            selected: 0,
            scroll: UniformListScrollHandle::new(),
            matcher: Matcher::new({
                let mut config = Config::DEFAULT;
                config.set_match_paths();
                config
            }),
            file_matches: Arc::new(Vec::new()),
            file_matches_for: None,
            file_search: None,
            _subscriptions: vec![subscription],
        };
        this.update_items(cx);
        this
    }

    pub fn mode(&self) -> PickerMode {
        self.mode
    }

    /// Called when the background file scan finishes.
    pub fn set_files(&mut self, files: Arc<Vec<PathBuf>>, cx: &mut Context<Self>) {
        self.files = files;
        self.update_items(cx);
    }

    /// Fills File Structure; `cursor` picks the symbol to preselect.
    pub fn set_symbols(&mut self, symbols: Vec<(Symbol, usize)>, cursor: usize, cx: &mut Context<Self>) {
        self.current_symbol = symbols.iter().rposition(|(s, _)| s.offset <= cursor);
        self.symbols = Arc::new(symbols);
        self.update_items(cx);
    }

    /// Fills a Run picker.
    pub fn set_run_targets(&mut self, targets: Vec<RunTarget>, cx: &mut Context<Self>) {
        self.run_targets = Arc::new(targets);
        self.update_items(cx);
    }

    /// Replaces the mode's title (a Files picker that picks a file to compare).
    pub fn set_title(&mut self, title: SharedString, cx: &mut Context<Self>) {
        self.title = Some(title);
        cx.notify();
    }

    /// Replaces the mode's title icon (a Choices list Kuana itself shows).
    pub fn set_title_icon(&mut self, icon: Icon, cx: &mut Context<Self>) {
        self.title_icon = Some(icon);
        cx.notify();
    }

    /// Fills a Choices picker: its title, what the empty field says, the rows.
    pub fn set_choices(&mut self, title: SharedString, placeholder: &str, choices: Vec<Choice>, cx: &mut Context<Self>) {
        self.title = Some(title);
        if !placeholder.is_empty() {
            self.query.update(cx, |q, cx| q.set_placeholder(placeholder.to_string(), cx));
        }
        self.choices = Arc::new(choices);
        self.update_items(cx);
    }

    /// Lets ⌫ remove a Choices row (`PickerEvent::RemoveChoice`).
    pub fn set_removable(&mut self, cx: &mut Context<Self>) {
        self.removable = true;
        cx.notify();
    }

    /// Refills a Choices picker after a removal, keeping the selection where it was.
    pub fn refill_choices(&mut self, choices: Vec<Choice>, cx: &mut Context<Self>) {
        let selected = self.selected;
        self.choices = Arc::new(choices);
        self.update_items(cx);
        self.selected = selected.min(self.items.len().saturating_sub(1));
        self.move_selection(0, cx);
    }

    /// Fills (or refills, after a removal) a Bookmarks picker, keeping the
    /// selection where it was.
    pub fn set_bookmarks(&mut self, rows: Vec<BookmarkRow>, cx: &mut Context<Self>) {
        let selected = self.selected;
        self.bookmarks = Arc::new(rows);
        self.update_items(cx);
        self.selected = selected.min(self.items.len().saturating_sub(1));
        self.move_selection(0, cx);
    }

    /// Bookmarks: ⌫ or Delete with nothing typed removes the selected one;
    /// otherwise the key goes on to the field (its next binding).
    fn on_remove_selected(&mut self, _: &RemoveSelected, _: &mut Window, cx: &mut Context<Self>) {
        if self.mode == PickerMode::Bookmarks
            && self.query.read(cx).text(cx).is_empty()
            && let Some(Item::Bookmark { ix }) = self.items.get(self.selected)
        {
            let row = &self.bookmarks[*ix];
            cx.emit(PickerEvent::RemoveBookmark { path: row.path.clone(), row: row.row });
            return;
        }
        if self.removable
            && self.query.read(cx).text(cx).is_empty()
            && let Some(Item::Choice { ix, .. }) = self.items.get(self.selected)
        {
            cx.emit(PickerEvent::RemoveChoice(self.choices[*ix].id.clone()));
            return;
        }
        cx.propagate();
    }

    /// Fills a Locations picker.
    pub fn set_locations(&mut self, title: Option<SharedString>, rows: Vec<LocationRow>, cx: &mut Context<Self>) {
        if title.is_some() {
            self.title = title;
        }
        self.locations = Arc::new(rows);
        self.update_items(cx);
    }

    fn relative(&self, path: &Path) -> String {
        self.root
            .as_deref()
            .and_then(|r| path.strip_prefix(r).ok())
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned()
    }

    fn file_item(&mut self, path: &Path, pattern: Option<&Pattern>) -> Option<(u32, Item)> {
        let rel = self.relative(path);
        let (score, name, dir, name_hl, dir_hl) = self.match_path(&rel, pattern)?;
        // Prefer matches in the file name itself.
        let bonus = if !name_hl.is_empty() { 20 } else { 0 };
        Some((score + bonus, Item::File { path: path.to_path_buf(), name, dir, name_hl, dir_hl }))
    }

    /// A recent project, matched on its folder name and `~`-relative location.
    fn project_item(&mut self, path: &Path, pattern: Option<&Pattern>) -> Option<(u32, Item)> {
        let (score, name, dir, name_hl, dir_hl) = self.match_path(&display_path(path), pattern)?;
        let bonus = if !name_hl.is_empty() { 20 } else { 0 };
        Some((score + bonus, Item::Project { path: path.to_path_buf(), name, dir, name_hl, dir_hl }))
    }

    /// Fuzzy-matches a path, split into its last component and the rest
    /// with the matched byte ranges of each.
    #[allow(clippy::type_complexity)]
    fn match_path(
        &mut self,
        rel: &str,
        pattern: Option<&Pattern>,
    ) -> Option<(u32, SharedString, SharedString, Vec<Range<usize>>, Vec<Range<usize>>)> {
        let (score, indices) = match pattern {
            Some(p) => {
                let mut buf = Vec::new();
                let mut indices = Vec::new();
                let score = p.indices(Utf32Str::new(rel, &mut buf), &mut self.matcher, &mut indices)?;
                indices.sort_unstable();
                indices.dedup();
                (score, indices)
            }
            None => (0, Vec::new()),
        };
        let split = rel.rfind(['/', '\\']).map(|i| i + 1).unwrap_or(0);
        let name = rel[split..].to_string();
        let dir = rel[..split].trim_end_matches(['/', '\\']).to_string();
        let split_chars = rel[..split].chars().count() as u32;
        let to_ranges = |s: &str, ix: &mut dyn Iterator<Item = u32>| -> Vec<Range<usize>> {
            let offsets: Vec<usize> = s.char_indices().map(|(b, _)| b).collect();
            ix.filter_map(|c| {
                let b = *offsets.get(c as usize)?;
                Some(b..b + s[b..].chars().next()?.len_utf8())
            })
            .collect()
        };
        let name_hl = to_ranges(&name, &mut indices.iter().filter(|&&i| i >= split_chars).map(|i| i - split_chars));
        let dir_hl = to_ranges(&dir, &mut indices.iter().copied().filter(|&i| i < split_chars));
        Some((score, name.into(), dir.into(), name_hl, dir_hl))
    }

    fn action_item(&mut self, ix: usize, pattern: Option<&Pattern>) -> Option<(u32, Item)> {
        let name = self.actions[ix].name;
        let Some(p) = pattern else {
            return Some((0, Item::Action { ix, hl: Vec::new() }));
        };
        let mut buf = Vec::new();
        let mut indices = Vec::new();
        let score = p.indices(Utf32Str::new(name, &mut buf), &mut self.matcher, &mut indices)?;
        let hl = indices
            .into_iter()
            .filter_map(|c| name.char_indices().nth(c as usize).map(|(b, ch)| b..b + ch.len_utf8()))
            .collect();
        Some((score, Item::Action { ix, hl }))
    }

    fn symbol_item(&mut self, ix: usize, pattern: Option<&Pattern>) -> Option<(u32, Item)> {
        let symbols = self.symbols.clone();
        let name = symbols[ix].0.name.as_str();
        let Some(p) = pattern else {
            return Some((0, Item::Symbol { ix, hl: Vec::new(), nested: true }));
        };
        let mut buf = Vec::new();
        let mut indices = Vec::new();
        let score = p.indices(Utf32Str::new(name, &mut buf), &mut self.matcher, &mut indices)?;
        let hl = indices
            .into_iter()
            .filter_map(|c| name.char_indices().nth(c as usize).map(|(b, ch)| b..b + ch.len_utf8()))
            .collect();
        Some((score, Item::Symbol { ix, hl, nested: false }))
    }

    fn update_items(&mut self, cx: &mut Context<Self>) {
        let text = self.query.read(cx).text(cx);
        let pattern = (!text.trim().is_empty()).then(|| Pattern::parse(&text, CaseMatching::Smart, Normalization::Smart));
        let mut items = Vec::new();

        let wants_files = matches!(self.mode, PickerMode::Everywhere | PickerMode::Files | PickerMode::Recent);
        if wants_files {
            let mut scored: Vec<(u32, Item)> = Vec::new();
            let recent = self.recent.clone();
            if self.mode == PickerMode::Recent || pattern.is_none() {
                // Recent files first, in most-recently-used order.
                for p in &recent {
                    if let Some(item) = self.file_item(p, pattern.as_ref()) {
                        scored.push(item);
                    }
                }
            }
            let limit = if self.mode == PickerMode::Everywhere { 40 } else { MAX_RESULTS };
            if self.mode != PickerMode::Recent {
                let files = self.files.clone();
                match &pattern {
                    // Scored in the background; until that's done, the last
                    // results that still match.
                    Some(p) => {
                        let wanted = (text.clone(), Arc::as_ptr(&files) as usize);
                        if self.file_matches_for.as_ref() != Some(&wanted) {
                            self.search_files(wanted, limit, cx);
                        }
                        scored.clear();
                        let matches = self.file_matches.clone();
                        scored.extend(matches.iter().filter_map(|&(_, ix)| self.file_item(files.get(ix)?, Some(p))));
                    }
                    None => {
                        let rest: Vec<&PathBuf> = files.iter().filter(|p| !recent.contains(p)).take(limit).collect();
                        scored.extend(rest.into_iter().filter_map(|p| self.file_item(p, None)));
                    }
                }
            }
            if self.mode == PickerMode::Everywhere && !scored.is_empty() {
                items.push(Item::Header("Files".into()));
            }
            items.extend(scored.into_iter().take(limit).map(|(_, i)| i));
        }

        if self.mode == PickerMode::Projects {
            let mut scored: Vec<(u32, Item)> = Vec::new();
            for p in self.recent.clone() {
                // The window's own project is already open.
                if self.root.as_deref() == Some(p.as_path()) {
                    continue;
                }
                if let Some(item) = self.project_item(&p, pattern.as_ref()) {
                    scored.push(item);
                }
            }
            if pattern.is_some() {
                // Stable, so recency breaks ties.
                scored.sort_by(|a, b| b.0.cmp(&a.0));
            }
            items.extend(scored.into_iter().map(|(_, i)| i));
            let actions = self.actions.clone();
            let open: Vec<Item> = PROJECT_ACTIONS
                .iter()
                .filter_map(|name| actions.iter().position(|a| a.name == *name))
                .filter_map(|ix| self.action_item(ix, pattern.as_ref()))
                .map(|(_, i)| i)
                .collect();
            if !open.is_empty() && !items.is_empty() {
                items.push(Item::Header("Open".into()));
            }
            items.extend(open);
        }

        if matches!(self.mode, PickerMode::Everywhere | PickerMode::Actions) {
            let mut scored: Vec<(u32, Item)> =
                (0..self.actions.len()).filter_map(|ix| self.action_item(ix, pattern.as_ref())).collect();
            if pattern.is_some() {
                scored.sort_by(|a, b| b.0.cmp(&a.0));
            }
            let limit = if self.mode == PickerMode::Everywhere {
                if pattern.is_some() { 12 } else { 6 }
            } else {
                MAX_RESULTS
            };
            if self.mode == PickerMode::Everywhere && !scored.is_empty() {
                items.push(Item::Header("Actions".into()));
            }
            items.extend(scored.into_iter().take(limit).map(|(_, i)| i));
        }

        if matches!(self.mode, PickerMode::Run | PickerMode::Debug | PickerMode::Attach) {
            let targets = self.run_targets.clone();
            let mut scored: Vec<(u32, Item)> = Vec::new();
            for (ix, target) in targets.iter().enumerate() {
                match &pattern {
                    Some(p) => {
                        let mut buf = Vec::new();
                        let mut indices = Vec::new();
                        let Some(score) = p.indices(Utf32Str::new(&target.name, &mut buf), &mut self.matcher, &mut indices) else {
                            // A process is also found by text in its command line
                            // (which script), after the ones whose name matches.
                            if self.mode == PickerMode::Attach && target.command.to_lowercase().contains(&text.trim().to_lowercase()) {
                                scored.push((0, Item::Run { ix, hl: Vec::new() }));
                            }
                            continue;
                        };
                        let hl = indices
                            .into_iter()
                            .filter_map(|c| target.name.char_indices().nth(c as usize).map(|(b, ch)| b..b + ch.len_utf8()))
                            .collect();
                        scored.push((score, Item::Run { ix, hl }));
                    }
                    None => scored.push((0, Item::Run { ix, hl: Vec::new() })),
                }
            }
            if pattern.is_some() {
                scored.sort_by(|a, b| b.0.cmp(&a.0));
            }
            items.extend(scored.into_iter().map(|(_, i)| i));
        }

        if self.mode == PickerMode::Choices {
            let choices = self.choices.clone();
            let mut scored: Vec<(u32, Item)> = Vec::new();
            for (ix, choice) in choices.iter().enumerate() {
                let Some(p) = &pattern else {
                    scored.push((0, Item::Choice { ix, hl: Vec::new() }));
                    continue;
                };
                let mut buf = Vec::new();
                let mut indices = Vec::new();
                if let Some(score) = p.indices(Utf32Str::new(&choice.label, &mut buf), &mut self.matcher, &mut indices) {
                    let hl = indices.into_iter().filter_map(|c| choice.label.char_indices().nth(c as usize).map(|(b, ch)| b..b + ch.len_utf8())).collect();
                    scored.push((score + 1, Item::Choice { ix, hl }));
                } else if choice.detail.as_ref().is_some_and(|d| d.to_lowercase().contains(&text.trim().to_lowercase())) {
                    // Found by its detail too, after the ones whose label matches.
                    scored.push((0, Item::Choice { ix, hl: Vec::new() }));
                }
            }
            if pattern.is_some() {
                scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
            }
            items.extend(scored.into_iter().map(|(_, i)| i));
        }

        if matches!(self.mode, PickerMode::Locations | PickerMode::Symbols) {
            let locations = self.locations.clone();
            let mut scored: Vec<(u32, Item)> = Vec::new();
            for (ix, row) in locations.iter().enumerate() {
                let score = match &pattern {
                    Some(p) => {
                        let text = format!("{} {}", row.file, row.preview);
                        let mut buf = Vec::new();
                        match p.score(Utf32Str::new(&text, &mut buf), &mut self.matcher) {
                            Some(score) => score,
                            // The server already matched these; just rank them.
                            None if self.mode == PickerMode::Symbols => 0,
                            None => continue,
                        }
                    }
                    None => 0,
                };
                scored.push((score, Item::Location { ix }));
            }
            if pattern.is_some() {
                scored.sort_by(|a, b| b.0.cmp(&a.0));
            }
            items.extend(scored.into_iter().map(|(_, i)| i));
        }

        if self.mode == PickerMode::Bookmarks {
            let bookmarks = self.bookmarks.clone();
            let mut scored: Vec<(u32, Item)> = Vec::new();
            for (ix, row) in bookmarks.iter().enumerate() {
                let score = match &pattern {
                    Some(p) => {
                        let text = format!(
                            "{} {} {} {}",
                            row.mnemonic.map(String::from).unwrap_or_default(),
                            row.description.as_deref().unwrap_or(""),
                            row.file,
                            row.preview
                        );
                        let mut buf = Vec::new();
                        match p.score(Utf32Str::new(&text, &mut buf), &mut self.matcher) {
                            Some(score) => score,
                            None => continue,
                        }
                    }
                    None => 0,
                };
                scored.push((score, Item::Bookmark { ix }));
            }
            if pattern.is_some() {
                scored.sort_by_key(|a| std::cmp::Reverse(a.0));
            }
            items.extend(scored.into_iter().map(|(_, i)| i));
        }

        if self.mode == PickerMode::Structure {
            let mut scored: Vec<(u32, Item)> =
                (0..self.symbols.len()).filter_map(|ix| self.symbol_item(ix, pattern.as_ref())).collect();
            if pattern.is_some() {
                // Stable, so document order breaks ties.
                scored.sort_by(|a, b| b.0.cmp(&a.0));
            }
            items.extend(scored.into_iter().map(|(_, i)| i));
        }

        self.items = Arc::new(items);
        // Like JetBrains, Recent Files preselects the previous file and File
        // Structure the symbol at the caret.
        self.selected = match self.mode {
            PickerMode::Recent if pattern.is_none() && self.items.len() > 1 => 1,
            PickerMode::Structure if pattern.is_none() => self.current_symbol.unwrap_or(0),
            _ => 0,
        };
        self.move_selection(0, cx);
    }

    /// Scores every project file against the query in the background and
    /// keeps the best `limit`; a newer query cancels it.
    fn search_files(&mut self, wanted: (String, usize), limit: usize, cx: &mut Context<Self>) {
        if let Some((cancel, _)) = self.file_search.take() {
            cancel.store(true, Ordering::Relaxed);
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let (files, root, query, stop) = (self.files.clone(), self.root.clone(), wanted.0.clone(), cancel.clone());
        let task = cx.spawn(async move |this, cx| {
            let found = cx.background_executor().spawn(async move { best_files(&files, root.as_deref(), &query, limit, &stop) }).await;
            this.update(cx, |picker, cx| {
                let Some(found) = found else { return };
                if let Some((_, task)) = picker.file_search.take() {
                    // This task is running; let it finish instead of dropping it.
                    task.detach();
                }
                picker.file_matches = Arc::new(found);
                picker.file_matches_for = Some(wanted);
                picker.update_items(cx);
            })
            .ok();
        });
        self.file_search = Some((cancel, task));
    }

    fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        let selectable: Vec<usize> = (0..self.items.len())
            .filter(|&i| !matches!(self.items[i], Item::Header(_)))
            .collect();
        if let Some(last) = selectable.len().checked_sub(1) {
            let pos = selectable.iter().position(|&i| i == self.selected).unwrap_or(0) as isize;
            self.selected = selectable[(pos + delta).clamp(0, last as isize) as usize];
            self.scroll.scroll_to_item(self.selected, ScrollStrategy::Center);
        }
        cx.notify();
    }

    /// `secondary` (⌘/Ctrl held) opens a project in a new window.
    fn confirm(&mut self, ix: usize, secondary: bool, cx: &mut Context<Self>) {
        match self.items.get(ix) {
            Some(Item::File { path, .. }) => cx.emit(PickerEvent::OpenFile(path.clone())),
            Some(Item::Project { path, .. }) => {
                cx.emit(PickerEvent::OpenProject { path: path.clone(), new_window: secondary })
            }
            Some(Item::Action { ix, .. }) => cx.emit(PickerEvent::RunAction(self.actions[*ix].action.boxed_clone())),
            Some(Item::Symbol { ix, .. }) => cx.emit(PickerEvent::GoTo(self.symbols[*ix].0.offset)),
            Some(Item::Run { ix, .. }) => {
                let target = self.run_targets[*ix].clone();
                cx.emit(if self.mode == PickerMode::Run { PickerEvent::Run(target) } else { PickerEvent::Debug(target) })
            }
            Some(Item::Choice { ix, .. }) => cx.emit(PickerEvent::Chosen(self.choices[*ix].id.clone())),
            Some(Item::Bookmark { ix }) => {
                let row = &self.bookmarks[*ix];
                cx.emit(PickerEvent::OpenBookmark { path: row.path.clone(), row: row.row })
            }
            Some(Item::Location { ix }) => cx.emit(PickerEvent::OpenLocation {
                location: self.locations[*ix].location.clone(),
                encoding: self.locations[*ix].encoding,
            }),
            _ => {}
        }
    }

    fn on_up(&mut self, _: &MoveUp, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selection(-1, cx);
    }

    fn on_down(&mut self, _: &MoveDown, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selection(1, cx);
    }

    fn on_page_up(&mut self, _: &PageUp, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selection(-10, cx);
    }

    fn on_page_down(&mut self, _: &PageDown, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selection(10, cx);
    }

    fn on_enter(&mut self, _: &Newline, _: &mut Window, cx: &mut Context<Self>) {
        self.confirm(self.selected, false, cx);
    }

    fn on_submit(&mut self, _: &Submit, _: &mut Window, cx: &mut Context<Self>) {
        self.confirm(self.selected, true, cx);
    }

    fn on_escape(&mut self, _: &Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(PickerEvent::Dismissed);
    }
}

impl Focusable for Picker {
    fn focus_handle(&self, cx: &gpui::App) -> FocusHandle {
        self.query.focus_handle(cx)
    }
}

/// The `limit` best matches of `query` among `files` (score, index), best
/// first, ties in file order; a match in the file name scores higher. None
/// when cancelled.
fn best_files(files: &[PathBuf], root: Option<&Path>, query: &str, limit: usize, cancel: &AtomicBool) -> Option<Vec<(u32, usize)>> {
    let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
    let mut matcher = Matcher::new({
        let mut config = Config::DEFAULT;
        config.set_match_paths();
        config
    });
    let (mut buf, mut indices) = (Vec::new(), Vec::new());
    let mut found: Vec<(u32, usize)> = Vec::new();
    for (ix, path) in files.iter().enumerate() {
        if ix % 2048 == 0 && cancel.load(Ordering::Relaxed) {
            return None;
        }
        let rel = root.and_then(|r| path.strip_prefix(r).ok()).unwrap_or(path).to_string_lossy();
        indices.clear();
        let Some(score) = pattern.indices(Utf32Str::new(&rel, &mut buf), &mut matcher, &mut indices) else { continue };
        let name_start = rel.rfind(['/', '\\']).map_or(0, |i| rel[..i + 1].chars().count()) as u32;
        let bonus = if indices.iter().any(|&i| i >= name_start) { 20 } else { 0 };
        found.push((score + bonus, ix));
    }
    let order = |a: &(u32, usize), b: &(u32, usize)| b.0.cmp(&a.0).then(a.1.cmp(&b.1));
    if found.len() > limit {
        found.select_nth_unstable_by(limit, order);
        found.truncate(limit);
    }
    found.sort_by(order);
    Some(found)
}

fn highlighted(text: SharedString, ranges: &[Range<usize>], color: Hsla) -> StyledText {
    StyledText::new(text).with_highlights(ranges.iter().map(|r| {
        (
            r.clone(),
            HighlightStyle {
                color: Some(color),
                font_weight: Some(FontWeight::BOLD),
                ..Default::default()
            },
        )
    }))
}

impl Render for Picker {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let (title, icon, hint) = match self.mode {
            PickerMode::Everywhere => ("Search Everywhere", Icon::Search, if cfg!(target_os = "macos") { "Double ⇧" } else { "Double Shift" }),
            PickerMode::Files => ("Files", Icon::File, ""),
            PickerMode::Recent => ("Recent Files", Icon::History, ""),
            PickerMode::Projects => (
                "Recent Projects",
                Icon::FolderOpen,
                if cfg!(target_os = "macos") { "⌘↩ opens in a new window" } else { "Ctrl+Enter opens in a new window" },
            ),
            PickerMode::Actions => ("Actions", Icon::Command, ""),
            PickerMode::Structure => ("File Structure", Icon::Braces, ""),
            PickerMode::Locations => ("", Icon::Search, ""),
            PickerMode::Run => ("Run", Icon::Play, ""),
            PickerMode::Debug => ("Debug", Icon::Bug, ""),
            PickerMode::Attach => ("Attach to Process", Icon::Bug, ""),
            PickerMode::Symbols => ("Symbols", Icon::Braces, ""),
            PickerMode::Choices => ("", Icon::Puzzle, ""),
            PickerMode::Bookmarks => ("Bookmarks", Icon::Bookmark, ""),
        };
        let icon = self.title_icon.unwrap_or(icon);
        let title: SharedString = match &self.title {
            Some(t) => t.clone(),
            None => title.into(),
        };
        let hint = match self.mode {
            PickerMode::Locations => format!("{} found", self.locations.len()),
            PickerMode::Symbols if !self.locations.is_empty() => format!("{} found", self.locations.len()),
            PickerMode::Bookmarks if self.bookmarks.is_empty() => "None yet".to_string(),
            PickerMode::Choices if self.removable && self.choices.is_empty() => "None yet".to_string(),
            PickerMode::Choices if self.removable => "⌫ removes".to_string(),
            PickerMode::Bookmarks => "⌫ removes".to_string(),
            _ => hint.to_string(),
        };
        let items = self.items.clone();
        let actions = self.actions.clone();
        let symbols = self.symbols.clone();
        let locations = self.locations.clone();
        let run_targets = self.run_targets.clone();
        let choices = self.choices.clone();
        let bookmarks = self.bookmarks.clone();
        let shortcuts = self.shortcuts.clone();
        let selected = self.selected;
        let mode = self.mode;
        let count = items.len();

        let list = uniform_list("picker-items", count, {
            cx.processor(move |_this, range: Range<usize>, _window, cx| {
                let theme = Theme::get(cx).clone();
                range
                    .map(|ix| {
                        let row = div().id(ix).relative().w_full().flex().items_center().gap_2().h(px(30.)).px_2p5().rounded(px(8.));
                        let row = match &items[ix] {
                            Item::Header(label) => row
                                .pt_1()
                                .text_size(px(11.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(theme.text_dim)
                                .child(label.to_uppercase())
                                .into_any_element(),
                            item => {
                                let is_selected = ix == selected;
                                let row = row
                                    .cursor_pointer()
                                    .when(is_selected, |el| el.bg(theme.accent_soft))
                                    .row_hover(theme.hover)
                                    .on_click(cx.listener(move |this, ev: &gpui::ClickEvent, _, cx| {
                                        this.confirm(ix, ev.modifiers().secondary(), cx)
                                    }));
                                let row = match item {
                                    Item::File { path, name, dir, name_hl, dir_hl } => {
                                        row.child(crate::ui::file_icon(path, px(16.), &theme))
                                            .child(
                                                div()
                                                    .flex_shrink_0()
                                                    .text_color(theme.text)
                                                    .child(highlighted(name.clone(), name_hl, theme.accent.into())),
                                            )
                                            .child(
                                                div()
                                                    .min_w_0()
                                                    .truncate()
                                                    .text_size(px(12.))
                                                    .text_color(theme.text_dim)
                                                    .child(highlighted(dir.clone(), dir_hl, theme.text_muted.into())),
                                            )
                                    }
                                    Item::Project { name, dir, name_hl, dir_hl, .. } => row
                                        .child(crate::ui::folder_icon(false, px(16.), &theme))
                                        .child(
                                            div()
                                                .flex_shrink_0()
                                                .text_color(theme.text)
                                                .child(highlighted(name.clone(), name_hl, theme.accent.into())),
                                        )
                                        .child(
                                            div()
                                                .min_w_0()
                                                .truncate()
                                                .text_size(px(12.))
                                                .text_color(theme.text_dim)
                                                .child(highlighted(dir.clone(), dir_hl, theme.text_muted.into())),
                                        ),
                                    Item::Action { ix, hl } => row
                                        .child(icon_el(Icon::Command, px(14.), theme.text_muted.into()))
                                        .child(
                                            div()
                                                .flex_1()
                                                .text_color(theme.text)
                                                .child(highlighted(actions[*ix].name.into(), hl, theme.accent.into())),
                                        )
                                        .when_some(shortcuts[*ix].clone(), |el, s| el.child(kbd(s, cx))),
                                    Item::Symbol { ix, hl, nested } => {
                                        let (symbol, line) = &symbols[*ix];
                                        let indent = if *nested { symbol.depth.min(8) as f32 * 16. } else { 0. };
                                        row.child(div().w(px(indent)).flex_shrink_0())
                                            .child(
                                                div()
                                                    .min_w_0()
                                                    .truncate()
                                                    .text_color(theme.text)
                                                    .child(highlighted(symbol.name.clone().into(), hl, theme.accent.into())),
                                            )
                                            .child(div().flex_shrink_0().text_size(px(12.)).text_color(theme.text_dim).child(symbol.kind))
                                            .child(div().flex_1())
                                            .child(
                                                div()
                                                    .flex_shrink_0()
                                                    .text_size(px(12.))
                                                    .text_color(theme.text_dim)
                                                    .child(format!(":{line}")),
                                            )
                                    }
                                    Item::Run { ix, hl } => {
                                        let target = &run_targets[*ix];
                                        let icon = if mode == PickerMode::Run { Icon::Play } else { Icon::Bug };
                                        row.child(icon_el(icon, px(13.), theme.success.into()))
                                            .child(
                                                div()
                                                    .flex_shrink_0()
                                                    .text_color(theme.text)
                                                    .child(highlighted(target.name.clone().into(), hl, theme.accent.into())),
                                            )
                                            .when(target.command != target.name, |row| {
                                                row.child(
                                                    div()
                                                        .min_w_0()
                                                        .truncate()
                                                        .text_size(px(12.))
                                                        .text_color(theme.text_dim)
                                                        .child(target.command.clone()),
                                                )
                                            })
                                    }
                                    Item::Choice { ix, hl } => {
                                        let choice = &choices[*ix];
                                        row.when_some(choice.file.as_ref(), |row, file| row.child(crate::ui::file_icon(file, px(14.), &theme)))
                                            .when_some(choice.icon.clone().filter(|_| choice.file.is_none()), |row, icon| {
                                                row.child(gpui::svg().path(icon).size(px(14.)).flex_shrink_0().text_color(theme.text_muted))
                                            })
                                        .child(div().flex_shrink_0().text_color(theme.text).child(highlighted(choice.label.clone(), hl, theme.accent.into())))
                                        .when_some(choice.detail.clone(), |row, detail| {
                                            row.child(div().min_w_0().truncate().text_size(px(12.)).text_color(theme.text_dim).child(detail))
                                        })
                                    }
                                    Item::Location { ix } => {
                                        let row_data = &locations[*ix];
                                        let icon = if mode == PickerMode::Symbols { Icon::Braces } else { Icon::FileCode };
                                        row.child(icon_el(icon, px(14.), theme.text_muted.into()))
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .min_w_0()
                                                    .truncate()
                                                    .text_color(theme.text)
                                                    .child(row_data.preview.clone()),
                                            )
                                            .child(
                                                // Long paths give way to the name, not the other way round.
                                                div()
                                                    .min_w_0()
                                                    .max_w(gpui::relative(0.5))
                                                    .truncate()
                                                    .text_size(px(12.))
                                                    .text_color(theme.text_dim)
                                                    .child(format!("{}:{}", row_data.file, row_data.line)),
                                            )
                                    }
                                    Item::Bookmark { ix } => {
                                        let data = &bookmarks[*ix];
                                        let marker = match data.mnemonic {
                                            Some(m) => div()
                                                .size(px(16.))
                                                .flex_shrink_0()
                                                .flex()
                                                .items_center()
                                                .justify_center()
                                                .rounded(px(4.))
                                                .bg(theme.accent_soft)
                                                .text_size(px(11.))
                                                .font_weight(FontWeight::BOLD)
                                                .text_color(theme.accent)
                                                .child(m.to_string())
                                                .into_any_element(),
                                            None => div()
                                                .size(px(16.))
                                                .flex_shrink_0()
                                                .flex()
                                                .items_center()
                                                .justify_center()
                                                .child(icon_el(Icon::Bookmark, px(14.), theme.text_muted.into()))
                                                .into_any_element(),
                                        };
                                        row.child(marker)
                                            .when_some(data.description.clone(), |row, description| {
                                                row.child(div().flex_shrink_0().max_w(gpui::relative(0.4)).truncate().text_color(theme.text).child(description))
                                            })
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .min_w_0()
                                                    .truncate()
                                                    .text_color(if data.description.is_some() { theme.text_dim } else { theme.text })
                                                    .child(data.preview.clone()),
                                            )
                                            .child(
                                                div()
                                                    .min_w_0()
                                                    .max_w(gpui::relative(0.5))
                                                    .truncate()
                                                    .text_size(px(12.))
                                                    .text_color(theme.text_dim)
                                                    .child(format!("{}:{}", data.file, data.row + 1)),
                                            )
                                    }
                                    Item::Header(_) => unreachable!(),
                                };
                                row.into_any_element()
                            }
                        };
                        div().w_full().px_1().child(row)
                    })
                    .collect()
            })
        })
        .track_scroll(self.scroll.clone())
        .h(px((count.clamp(1, 12) * 30) as f32))
        .w_full();
        // Padding goes on a wrapper: GPUI's scroll-to-item counts a list's own
        // padding twice, so a list that fits its rows would scroll by 8px.
        let list = div().py_1().child(div().relative().child(list).child(crate::ui::list_scrollbar("picker-scrollbar", &self.scroll, cx)));

        modal_panel(cx)
            .key_context("Picker")
            .on_action(cx.listener(Self::on_remove_selected))
            .on_action(cx.listener(Self::on_up))
            .on_action(cx.listener(Self::on_down))
            .on_action(cx.listener(Self::on_page_up))
            .on_action(cx.listener(Self::on_page_down))
            .on_action(cx.listener(Self::on_enter))
            .on_action(cx.listener(Self::on_submit))
            .on_action(cx.listener(Self::on_escape))
            .w(px(620.))
            .max_w(window.viewport_size().width - px(48.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px_3p5()
                    .pt_3()
                    .pb_2()
                    .text_size(px(12.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1p5()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme.text_muted)
                            .child(icon_el(icon, px(13.), theme.text_muted.into()))
                            .child(title),
                    )
                    .child(div().text_color(theme.text_dim).child(hint)),
            )
            .child(div().px_2p5().pb_2().child(input_box(&self.query, Some(Icon::Search), window, cx)))
            .child(crate::ui::island(&theme).p_0().mx(px(crate::ui::GAP)).mb(px(crate::ui::GAP)).child(if count == 0 {
                div()
                    .h(px(38.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(theme.text_dim)
                    .child("Nothing found")
                    .into_any_element()
            } else {
                list.into_any_element()
            }))
    }
}

// ---- Go to Line ----------------------------------------------------------------

pub enum GoToLineEvent {
    Confirm { line: usize, column: usize },
    Dismissed,
}

pub struct GoToLine {
    input: Entity<Editor>,
    line_count: usize,
    current: (usize, usize),
}

impl EventEmitter<GoToLineEvent> for GoToLine {}

impl GoToLine {
    pub fn new(line_count: usize, current: (usize, usize), window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| Editor::single_line(format!("{}:{}", current.0, current.1), cx));
        window.focus(&input.focus_handle(cx));
        cx.subscribe(&input, |_, _, _: &EditorEvent, cx| cx.notify()).detach();
        Self { input, line_count, current }
    }

    fn parse(&self, cx: &gpui::App) -> Option<(usize, usize)> {
        let text = self.input.read(cx).text(cx);
        let text = text.trim();
        if text.is_empty() {
            return None;
        }
        let mut parts = text.splitn(2, [':', ',']);
        let line = parts.next()?.trim().parse::<usize>().ok()?;
        let column = match parts.next() {
            Some(c) if !c.trim().is_empty() => c.trim().parse::<usize>().ok()?,
            _ => 1,
        };
        Some((line.clamp(1, self.line_count.max(1)), column.max(1)))
    }

    fn on_enter(&mut self, _: &Newline, _: &mut Window, cx: &mut Context<Self>) {
        if let Some((line, column)) = self.parse(cx) {
            cx.emit(GoToLineEvent::Confirm { line, column });
        }
    }

    fn on_escape(&mut self, _: &Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(GoToLineEvent::Dismissed);
    }
}

impl Focusable for GoToLine {
    fn focus_handle(&self, cx: &gpui::App) -> FocusHandle {
        self.input.focus_handle(cx)
    }
}

impl Render for GoToLine {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let valid = self.input.read(cx).doc.read(cx).len() == 0 || self.parse(cx).is_some();
        modal_panel(cx)
            .key_context("GoToLine")
            .on_action(cx.listener(Self::on_enter))
            .on_action(cx.listener(Self::on_escape))
            .w(px(380.))
            .p_3()
            .gap_2()
            .child(div().text_size(px(12.)).font_weight(FontWeight::SEMIBOLD).text_color(theme.text_muted).child("Go to Line:Column"))
            .child(input_box(&self.input, Some(Icon::Hash), window, cx))
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(if valid { theme.text_dim } else { theme.error })
                    .child(if valid {
                        format!(
                            "Current position {}:{} · {} lines",
                            self.current.0, self.current.1, self.line_count
                        )
                    } else {
                        "Enter a line number, optionally followed by :column".into()
                    }),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn best_files_ranks_names_first_and_keeps_the_top() {
        let root = PathBuf::from("/p");
        let mut files: Vec<PathBuf> = (0..500).map(|i| root.join(format!("src/mod{i}/lib.rs"))).collect();
        files.push(root.join("docs/editor/notes.md"));
        files.push(root.join("src/editor.rs"));
        let found = best_files(&files, Some(&root), "editor", 10, &AtomicBool::new(false)).unwrap();
        // Both match; the one whose file name matches comes first.
        assert_eq!(found.iter().map(|&(_, ix)| ix).collect::<Vec<_>>(), [501, 500]);

        // A query everything matches equally: the first ones, in order.
        let found = best_files(&files, Some(&root), "lib", 3, &AtomicBool::new(false)).unwrap();
        assert_eq!(found.iter().map(|&(_, ix)| ix).collect::<Vec<_>>(), [0, 1, 2]);
        assert!(best_files(&files, Some(&root), "lib", 3, &AtomicBool::new(true)).is_none(), "cancelled");
    }
}
