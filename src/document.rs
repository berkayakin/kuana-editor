//! Text storage, undo history and file I/O. Offsets are UTF-8 byte offsets
//! throughout so they line up with tree-sitter and oxc spans.

use std::{
    borrow::Cow,
    ops::Range,
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex, PoisonError,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};

use ropey::Rope;
use serde::{Deserialize, Serialize};
use tree_sitter::InputEdit;

use crate::{
    encoding::{self, TextEncoding},
    lint::Diagnostic,
    syntax::{Language, Syntax, input_edit},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selection {
    pub anchor: usize,
    pub head: usize,
}

impl Selection {
    pub fn cursor(at: usize) -> Self {
        Self {
            anchor: at,
            head: at,
        }
    }

    pub fn range(&self) -> Range<usize> {
        self.anchor.min(self.head)..self.anchor.max(self.head)
    }

    pub fn is_empty(&self) -> bool {
        self.anchor == self.head
    }

    pub fn reversed(&self) -> bool {
        self.head < self.anchor
    }
}

/// A line breakpoint. A `condition` makes it stop only when that expression
/// is true; a `log_message` makes it a logpoint, which logs (with
/// `{expression}`s filled in) instead of stopping.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Breakpoint {
    /// 0-based.
    pub row: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub condition: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_message: Option<String>,
}

impl Breakpoint {
    pub fn at(row: usize) -> Self {
        Self { row, ..Self::default() }
    }
}

struct EditRecord {
    /// Position at which the edit was applied (in the coordinates of the
    /// buffer at application time).
    start: usize,
    old: String,
    new: String,
}

struct Transaction {
    /// Names the text as of after this transaction (a new id whenever it
    /// grows), so the undo history's position tells whether the text is the
    /// one saved (`Document::saved_at`).
    id: u64,
    edits: Vec<EditRecord>,
    before: Vec<Selection>,
    after: Vec<Selection>,
    at: Instant,
    kind: EditKind,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EditKind {
    Typing,
    Deleting,
    Other,
}

/// Line edits kept for views catching up (soft wrap); past this many they
/// rebuild from scratch.
const LINE_EDIT_LOG: usize = 10_000;

/// How many versions' patches are kept for other editors of a document to
/// catch up with (each editor catches up when it's notified, so a few).
const PATCH_LOG: usize = 512;

/// How many texts sent to language servers are kept (see `lsp_versions`):
/// a server answers for a version some edits back, seldom hundreds.
const MAX_LSP_VERSIONS: usize = 256;

static NEXT_DOCUMENT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// How long an edit may parse on the UI thread; a longer parse finishes in
/// the background (the editor shows the old tree, edited, meanwhile).
const PARSE_BUDGET: Duration = Duration::from_millis(1);
/// The same when a file opens.
const OPEN_PARSE_BUDGET: Duration = Duration::from_millis(10);

/// Above this, a file opens as plain text without language features.
const LARGE_FILE_SIZE: u64 = 20_000_000;
/// Above this, it doesn't open at all (it would take that much memory twice over).
const MAX_FILE_SIZE: u64 = 500_000_000;

/// What [`Document::open`] fails with for a file that looks binary
/// (`encoding::looks_binary`).
#[derive(Debug)]
pub struct BinaryFile;

impl BinaryFile {
    /// Whether opening failed because the file looks binary.
    pub fn is(err: &std::io::Error) -> bool {
        err.get_ref().is_some_and(|e| e.is::<BinaryFile>())
    }
}

impl std::fmt::Display for BinaryFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("it looks like a binary file")
    }
}

impl std::error::Error for BinaryFile {}

pub struct Document {
    /// Unique per document: an editor showing it notices when it's replaced
    /// as a whole (an unloaded tab read its file).
    id: u64,
    /// Only a stand-in for a file not read yet (`Document::unloaded`).
    stand_in: bool,
    rope: Rope,
    pub path: Option<PathBuf>,
    pub language: Language,
    syntax: Syntax,
    version: u64,
    /// The undo history's position (`history_position`) when the text was
    /// last saved or read; None when no position is the file's (an unsaved
    /// buffer, a new encoding). Undoing back to it, the text is saved again.
    saved_at: Option<u64>,
    /// The last `Transaction::id` given.
    last_transaction: u64,
    undo_stack: Vec<Transaction>,
    redo_stack: Vec<Transaction>,
    /// Merged view of built-in and external diagnostics, sorted by position.
    pub diagnostics: Vec<Diagnostic>,
    /// Counts the times `diagnostics` was built again (not moved by edits,
    /// which change `version`), for what's drawn from it.
    diagnostics_generation: u64,
    /// How many of `diagnostics` there are of each severity (a tab's error
    /// dot, the status bar's counts), so drawing doesn't count them.
    severities: [usize; 4],
    builtin_diagnostics: Vec<Diagnostic>,
    external_diagnostics: Vec<Diagnostic>,
    /// Each language server's diagnostics (by server id), apart: a publish
    /// replaces only its own server's, which the others' keep following
    /// edits meanwhile.
    lsp_diagnostics: Vec<(String, Vec<Diagnostic>)>,
    /// The text at the versions language servers were sent last (oldest
    /// first), so what they computed for one of them (diagnostics, edits)
    /// can be read in that text and moved to today's.
    lsp_versions: std::collections::VecDeque<(u64, Rope)>,
    pub uses_crlf: bool,
    /// The file's encoding, used again when saving.
    pub encoding: TextEncoding,
    /// Big enough that highlighting, linting and language servers are off.
    pub large: bool,
    /// Edits since the language server last heard about this document.
    lsp_changes: LspChanges,
    /// Line breakpoints with the offsets their lines start at (sorted), so
    /// they stay on their lines as text is edited above them.
    breakpoints: Vec<(usize, Breakpoint)>,
    /// Where the line a debug session is paused on starts.
    debug_line: Option<usize>,
    /// What each edit did to lines, tagged with the version it led to, so
    /// views can update just those lines (see `line_edits_since`).
    line_edits: Vec<(u64, LineEdit)>,
    /// Each version's edits since `patches_base`, for the other editors of
    /// this document to move their carets.
    patches: std::collections::VecDeque<(u64, Patch)>,
    patches_base: u64,
    /// The version `line_edits` start from.
    line_edits_base: u64,
    /// How many lines are longer than `wrap::LONG_LINE` (they always break
    /// into rows, so the editor needs a wrap map).
    long_lines: usize,
    /// The last parse ran out of time: the trees are older ones, edited,
    /// until a background parse (`parse_job`) finishes.
    parse_behind: bool,
    /// Edits made while behind, with the version they led to, to replay on
    /// a background parse of an older version.
    parse_edits: Vec<(u64, InputEdit)>,
    /// The version the trees were last fully parsed for (None: not yet).
    parsed_version: Option<u64>,
    /// How long an edit may parse here (`PARSE_BUDGET`; tests change it).
    parse_budget: Duration,
    /// Changes whenever the trees do (an edit, a parse finishing), for
    /// caches of what's derived from them.
    tree_version: u64,
    /// The language server's inlay hints (sorted); they follow edits until
    /// the next answer replaces them.
    inlay_hints: Vec<InlayHint>,
    /// Counts the hints' replacements (not hints following edits), so soft
    /// wrap knows when to look for lines whose hints changed.
    inlay_generation: u64,
    /// The chunks of lines (the editor's `HINT_CHUNK`s) whose hints were
    /// asked for as of a `hint_stamp`: the others, and all of them after an
    /// edit or a server's word, are due (`hints_fresh`).
    hint_chunks: ((u64, u64), Vec<usize>),
    /// How many times a server said its hints changed.
    hint_refreshes: u64,
    /// Language servers' code lenses, by line (shown above it).
    code_lenses: Vec<CodeLens>,
    /// The `lens_stamp` they were read at.
    lenses_read: Option<(u64, u64)>,
    /// How many times a server said its lenses changed.
    lens_refreshes: u64,
    /// The last lens id given out (`CodeLens::id`).
    last_lens: u64,
    /// Counts replacements and resolved titles (lenses following edits
    /// change `version`).
    code_lens_generation: u64,
    /// Modification time of the file when it was last read or written, to
    /// notice changes made by other programs.
    disk_mtime: Option<SystemTime>,
    /// The saves taken (`save_job`) and being written, shared with them.
    writes: Arc<Writes>,
    /// The last save taken (its ticket), and the newest taken back in.
    tickets: (u64, u64),
    /// What the last save taken writes: the version, encoding and line ends.
    last_save: Option<(u64, TextEncoding, bool)>,
}

impl Document {
    pub fn empty() -> Self {
        Self::from_text(String::new(), None)
    }

    /// A stand-in for a file not read yet (a restored tab, see
    /// `Editor::new_unloaded`): no text and no parse; dirty when the session
    /// kept unsaved edits for it.
    pub fn unloaded(path: Option<PathBuf>, dirty: bool) -> Self {
        let mut doc = Self::empty();
        doc.stand_in = true;
        doc.path = path;
        if dirty {
            doc.mark_unsaved();
        }
        doc
    }

    /// Reads a file as text. One that looks binary fails with [`BinaryFile`]:
    /// as text it shows as garbage, and a save would damage it.
    pub fn open(path: &Path) -> std::io::Result<Self> {
        Self::read(path, false)
    }

    /// Reads a file as text even when it looks binary (Open Anyway).
    pub fn open_anyway(path: &Path) -> std::io::Result<Self> {
        Self::read(path, true)
    }

    fn read(path: &Path, binary: bool) -> std::io::Result<Self> {
        use std::io::Read;
        let size = std::fs::metadata(path)?.len();
        if size > MAX_FILE_SIZE {
            return Err(std::io::Error::other(format!("{} MB is too large to open", size / 1_000_000)));
        }
        // The start first: a binary file isn't read further.
        let mut file = std::fs::File::open(path)?;
        let mut bytes = Vec::new();
        (&mut file).take(8000).read_to_end(&mut bytes)?;
        if !binary && encoding::looks_binary(&bytes) {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, BinaryFile));
        }
        file.read_to_end(&mut bytes)?;
        let (text, encoding) = encoding::decode(&bytes, None);
        // A large file is shown as plain text: no parsing, linting or language
        // server. Decided before building the syntax tree, which would parse it.
        let mut doc = Self::with_text(text, Some(path.to_path_buf()), size > LARGE_FILE_SIZE, Some(OPEN_PARSE_BUDGET));
        doc.encoding = encoding;
        doc.disk_mtime = mtime(path);
        Ok(doc)
    }

    /// Whether the file at `path` reads as `text` (in `encoding`, else as
    /// `open` would read it; line endings as LF), like a document of it.
    pub fn file_has(path: &Path, text: &str, encoding: Option<TextEncoding>) -> bool {
        let Ok(bytes) = std::fs::read(path) else { return false };
        let read = match encoding {
            Some(encoding) => encoding::decode_as(&bytes, encoding),
            None => encoding::decode(&bytes, None).0,
        };
        encoding::to_lf(read) == text
    }

    pub(crate) fn from_text(text: String, path: Option<PathBuf>) -> Self {
        Self::with_text(text, path, false, None)
    }

    /// `budget`: parse for at most this long here, the rest in the background.
    fn with_text(text: String, path: Option<PathBuf>, large: bool, budget: Option<Duration>) -> Self {
        // Normalize to LF internally; remember the style for saving.
        let uses_crlf = text.contains("\r\n");
        let text = encoding::to_lf(text);
        let rope = Rope::from_str(&text);
        let language = match path.as_deref() {
            Some(path) if !large => Language::from_path(path),
            _ => Language::PlainText,
        };
        let (syntax, parsed) = match budget {
            Some(budget) => Syntax::new_within(language, &rope, budget),
            None => (Syntax::new(language, &rope), true),
        };
        let long_lines = long_lines(&rope, 0..usize::MAX);
        Self {
            id: NEXT_DOCUMENT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            stand_in: false,
            rope,
            path,
            language,
            syntax,
            version: 0,
            saved_at: Some(0),
            last_transaction: 0,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            diagnostics: Vec::new(),
            diagnostics_generation: 0,
            severities: [0; 4],
            builtin_diagnostics: Vec::new(),
            external_diagnostics: Vec::new(),
            lsp_diagnostics: Vec::new(),
            lsp_versions: std::collections::VecDeque::new(),
            uses_crlf,
            encoding: TextEncoding::UTF8,
            large,
            lsp_changes: LspChanges::default(),
            breakpoints: Vec::new(),
            debug_line: None,
            line_edits: Vec::new(),
            line_edits_base: 0,
            patches: std::collections::VecDeque::new(),
            patches_base: 0,
            long_lines,
            parse_behind: !parsed,
            parse_edits: Vec::new(),
            parsed_version: parsed.then_some(0),
            parse_budget: PARSE_BUDGET,
            tree_version: 0,
            inlay_hints: Vec::new(),
            inlay_generation: 1,
            hint_chunks: ((0, 0), Vec::new()),
            hint_refreshes: 0,
            code_lenses: Vec::new(),
            lenses_read: None,
            lens_refreshes: 0,
            last_lens: 0,
            code_lens_generation: 0,
            disk_mtime: None,
            writes: Arc::default(),
            tickets: (0, 0),
            last_save: None,
        }
    }

    pub fn rope(&self) -> &Rope {
        &self.rope
    }

    pub fn syntax(&self) -> &Syntax {
        &self.syntax
    }

    pub fn len(&self) -> usize {
        self.rope.len_bytes()
    }

    pub fn version(&self) -> u64 {
        self.version
    }

    pub fn is_dirty(&self) -> bool {
        self.saved_at != Some(self.history_position())
    }

    /// Where the undo history is: the id of the transaction on top (0 for
    /// none). Edits, undo and redo move it, and the same text has the same.
    fn history_position(&self) -> u64 {
        self.undo_stack.last().map_or(0, |t| t.id)
    }

    fn next_transaction(&mut self) -> u64 {
        self.last_transaction += 1;
        self.last_transaction
    }

    pub fn file_name(&self) -> String {
        self.path
            .as_deref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Untitled".into())
    }

    pub fn set_path(&mut self, path: PathBuf) {
        let language = Language::from_path(&path);
        self.path = Some(path);
        if language != self.language {
            self.set_language(language);
        }
    }

    /// Parses again as `language` (an extension's grammar can arrive later).
    /// Large files stay plain text.
    pub fn set_language(&mut self, language: Language) {
        if self.large {
            return;
        }
        self.language = language;
        self.set_syntax(Syntax::new(language, &self.rope), true);
        self.set_builtin_diagnostics(Vec::new());
        self.set_external_diagnostics(Vec::new());
    }

    /// A job that reads the file again and compares it with the text as it
    /// is now, to run in the background (window focus, Git operations and
    /// the watcher ask every open file); give its result to `apply_reload`.
    /// None without a file. The job finds nothing (`Ok(None)`) when the
    /// file's modification time is the one last read or written.
    pub fn reload_job(&self) -> Option<impl FnOnce() -> std::io::Result<Option<Reload>> + Send + 'static> {
        let path = self.path.clone()?;
        let (id, version, encoding, known, rope) = (self.id, self.version, self.encoding, self.disk_mtime, self.rope.clone());
        Some(move || {
            // Before reading: a write after it is noticed next time.
            let mtime = mtime(&path);
            if known.is_some() && mtime == known {
                return Ok(None);
            }
            let bytes = std::fs::read(&path)?;
            let (text, encoding) = encoding::decode(&bytes, Some(encoding));
            let text = encoding::to_lf(text);
            let changes = if rope == text.as_str() { Changes::default() } else { Changes::between(&String::from(&rope), &text) };
            Ok(Some(Reload { id, version, known, encoding, mtime, changes }))
        })
    }

    /// Takes in a reload read for the text as it is (else it's dropped:
    /// unsaved edits, or the text changed or was saved while the file was
    /// read). The lines that changed are one edit, undone like any other,
    /// and the text counts as saved after it. True when the text changed.
    pub fn apply_reload(&mut self, reload: Reload, selections: &[Selection]) -> bool {
        // A save being written may be what the reload read, half done.
        if (reload.id, reload.version, reload.known) != (self.id, self.version, self.disk_mtime) || self.is_dirty() || self.writes.unsettled() {
            return false;
        }
        self.encoding = reload.encoding;
        self.disk_mtime = reload.mtime;
        if reload.changes.edits.is_empty() {
            return false;
        }
        self.take_changes(reload.changes, selections);
        self.saved_at = Some(self.history_position());
        true
    }

    /// Makes a reload's changes one undoable edit: change by change (the
    /// trees reparse just around them) unless there are so many that the
    /// trees' edits would take long (each edit edits every embedded
    /// language's tree too); then one edit from the first change to the
    /// last, which what follows the text and soft wrap still see as the
    /// changes they are.
    fn take_changes(&mut self, changes: Changes, selections: &[Selection]) {
        /// Edits times trees at most for a reload taken change by change.
        const ONE_BY_ONE: usize = 2_000;
        let Changes { edits, span } = changes;
        let (Some(first), Some(last)) = (edits.first(), edits.last()) else { return };
        if edits.len() * self.syntax.tree_count().max(1) <= ONE_BY_ONE {
            self.edit(edits, selections, EditKind::Other);
            return;
        }
        let range = first.0.start..last.0.end;
        let before = self.rope.clone();
        let old = self.slice(range.clone());
        // Logged as `edit` logs them: the last first, each in the text before it.
        let logged: Vec<LineEdit> = edits.iter().rev().map(|(range, text)| LineEdit::of(&before, range.clone(), text)).collect();
        self.replace_logging(range.clone(), &span, LineEdit::of(&before, range.clone(), &span), &logged);
        let patch = Patch::new(edits.iter().map(|(range, text)| (range.clone(), text.len())).collect());
        self.shift_tracked(&patch, &before, &ExactLines::new());
        self.version += 1;
        self.log_patch(patch);
        self.reparse();
        self.redo_stack.clear();
        let record = EditRecord { start: range.start, old, new: span };
        let id = self.next_transaction();
        self.undo_stack.push(Transaction { id, edits: vec![record], before: selections.to_vec(), after: Vec::new(), at: Instant::now(), kind: EditKind::Other });
    }

    /// Reads the file again as `encoding` (when it was guessed wrong).
    /// Unsaved edits are lost; callers ask first. Nothing to undo after it.
    pub fn reload_with_encoding(&mut self, encoding: TextEncoding) -> std::io::Result<()> {
        let path = self.path.clone().ok_or_else(|| std::io::Error::other("document has no path"))?;
        let bytes = std::fs::read(&path)?;
        self.encoding = encoding;
        self.disk_mtime = mtime(&path);
        let text = encoding::to_lf(encoding::decode_as(&bytes, encoding));
        if self.rope != text.as_str() {
            self.take_changes(Changes::between(&String::from(&self.rope), &text), &[]);
            self.undo_stack.clear();
            self.redo_stack.clear();
        }
        self.saved_at = Some(self.history_position());
        Ok(())
    }

    /// Saves in another encoding from now on (the next save converts).
    pub fn convert_to(&mut self, encoding: TextEncoding) {
        if self.encoding != encoding {
            self.encoding = encoding;
            self.mark_unsaved();
        }
    }

    /// Saves with Windows (CRLF) or Unix (LF) line endings from now on.
    pub fn set_crlf(&mut self, crlf: bool) {
        if self.uses_crlf != crlf {
            self.uses_crlf = crlf;
            self.mark_unsaved();
        }
    }

    /// A change that isn't an edit (encoding, line endings) still needs a save.
    fn mark_unsaved(&mut self) {
        self.saved_at = None;
    }

    /// Saves here and now.
    #[cfg(test)]
    pub fn save(&mut self) -> std::io::Result<()> {
        let job = self.save_job()?;
        self.saved(job.write())
    }

    /// What a save writes, taken now: the text as it is (a rope clone), to
    /// write anywhere (`SaveJob::write`, off the UI thread), then take back
    /// in with `saved`. Until then the document is unsaved, as before.
    pub fn save_job(&mut self) -> std::io::Result<SaveJob> {
        let path = self.path.clone().ok_or_else(|| std::io::Error::other("document has no path"))?;
        self.tickets.0 += 1;
        self.last_save = Some((self.version, self.encoding, self.uses_crlf));
        Ok(SaveJob {
            id: self.id,
            ticket: self.tickets.0,
            position: self.history_position(),
            path,
            rope: self.rope.clone(),
            encoding: self.encoding,
            crlf: self.uses_crlf,
            settles: Settles::new(&self.writes),
            in_flight: InFlight::new(),
        })
    }

    /// Takes a save's result back in: written, the document counts as saved
    /// as of the text it wrote (so unsaved again if edited since, or if
    /// its encoding or line ends changed) and knows the file's new
    /// modification time. A save that a newer one overtook changes nothing.
    pub fn saved(&mut self, written: Written) -> std::io::Result<()> {
        let mtime = match written.outcome {
            Outcome::Written(mtime) => mtime,
            Outcome::Overtaken => return Ok(()),
            Outcome::Failed(err) => return Err(err),
        };
        if written.id == self.id && written.ticket > self.tickets.1 {
            self.tickets.1 = written.ticket;
            self.disk_mtime = mtime;
            if (written.encoding, written.crlf) == (self.encoding, self.uses_crlf) {
                self.saved_at = Some(written.position);
            }
        }
        Ok(())
    }

    /// Whether the text as it is now is being saved (written in the
    /// background, not taken back in yet): closing waits for it.
    pub fn is_being_saved(&self) -> bool {
        self.writes.unsettled() && self.last_save == Some((self.version, self.encoding, self.uses_crlf))
    }

    /// Whether the file was modified or removed by another program since it
    /// was last read or written. Auto-save skips such files so it never
    /// overwrites someone else's changes or recreates a deleted file. While
    /// a save is being written, the change is that save.
    pub fn changed_on_disk(&self) -> bool {
        let Some(path) = &self.path else { return false };
        self.disk_mtime.is_some() && !self.writes.unsettled() && mtime(path) != self.disk_mtime
    }

    // ---- Coordinate helpers -------------------------------------------------

    pub fn line_count(&self) -> usize {
        self.rope.len_lines()
    }

    pub fn line_start(&self, row: usize) -> usize {
        self.rope.line_to_byte(row.min(self.rope.len_lines()))
    }

    /// End of the row's content, excluding the newline.
    pub fn line_end(&self, row: usize) -> usize {
        if row + 1 >= self.rope.len_lines() {
            self.rope.len_bytes()
        } else {
            self.rope.line_to_byte(row + 1) - 1
        }
    }

    pub fn row_of(&self, offset: usize) -> usize {
        self.rope.byte_to_line(offset.min(self.rope.len_bytes()))
    }

    /// Text of a row without the trailing newline.
    pub fn line_text(&self, row: usize) -> String {
        let start = self.line_start(row);
        let end = self.line_end(row);
        self.slice(start..end)
    }

    pub fn slice(&self, range: Range<usize>) -> String {
        let s = self.rope.byte_to_char(range.start.min(self.len()));
        let e = self.rope.byte_to_char(range.end.min(self.len()));
        self.rope.slice(s..e).to_string()
    }

    pub fn char_at(&self, offset: usize) -> Option<char> {
        if offset >= self.len() {
            return None;
        }
        Some(self.rope.char(self.rope.byte_to_char(offset)))
    }

    pub fn char_before(&self, offset: usize) -> Option<char> {
        if offset == 0 {
            return None;
        }
        let ci = self.rope.byte_to_char(offset.min(self.len()));
        (ci > 0).then(|| self.rope.char(ci - 1))
    }

    pub fn next_char_boundary(&self, offset: usize) -> usize {
        match self.char_at(offset) {
            Some(c) => offset + c.len_utf8(),
            None => self.len(),
        }
    }

    pub fn prev_char_boundary(&self, offset: usize) -> usize {
        match self.char_before(offset) {
            Some(c) => offset - c.len_utf8(),
            None => 0,
        }
    }

    /// Snaps an arbitrary byte offset to a char boundary at or before it.
    pub fn clip(&self, offset: usize) -> usize {
        let offset = offset.min(self.len());
        self.rope.char_to_byte(self.rope.byte_to_char(offset))
    }

    // ---- Editing --------------------------------------------------------------

    /// Applies edits given in current-buffer coordinates. Overlapping edits
    /// are merged into one (their texts joined in order), as two carets'
    /// deletions can overlap (Backspace deleting a bracket pair next to
    /// another caret). Returns, for each edit (in input order), the range its
    /// new text occupies afterwards, including edits that changed nothing.
    pub fn edit(
        &mut self,
        edits: Vec<(Range<usize>, String)>,
        selections_before: &[Selection],
        kind: EditKind,
    ) -> Vec<Range<usize>> {
        let count = edits.len();
        let mut edits: Vec<(usize, Range<usize>, String)> = edits
            .into_iter()
            .enumerate()
            .map(|(i, (range, text))| {
                let start = self.clip(range.start);
                (i, start..self.clip(range.end.max(start)), text)
            })
            .collect();
        edits.sort_by_key(|(_, range, _)| (range.start, range.end));

        // Group overlapping edits; each member remembers where its text starts
        // inside the group's text.
        struct Group {
            range: Range<usize>,
            text: String,
            members: Vec<(usize, Range<usize>)>,
        }
        let mut groups: Vec<Group> = Vec::with_capacity(count);
        for (i, range, text) in edits {
            match groups.last_mut() {
                Some(g) if range.start < g.range.end => {
                    g.range.end = g.range.end.max(range.end);
                    g.members.push((i, g.text.len()..g.text.len() + text.len()));
                    g.text.push_str(&text);
                }
                _ => groups.push(Group { members: vec![(i, 0..text.len())], range, text }),
            }
        }

        // Apply from last to first so earlier ranges remain valid.
        let before = self.rope.clone();
        let mut records = Vec::with_capacity(groups.len());
        for g in groups.iter().rev() {
            if g.range.is_empty() && g.text.is_empty() {
                continue;
            }
            let old = self.slice(g.range.clone());
            self.raw_replace(g.range.clone(), &g.text);
            records.push(EditRecord { start: g.range.start, old, new: g.text.clone() });
        }
        // Tracked positions move once for all of them.
        let patch = Patch::new(
            groups.iter().filter(|g| !(g.range.is_empty() && g.text.is_empty())).map(|g| (g.range.clone(), g.text.len())).collect(),
        );
        self.shift_tracked(&patch, &before, &ExactLines::new());
        let mut patch = Some(patch);

        // Compute post-edit ranges by accumulating deltas front to back.
        let mut result = vec![0..0; count];
        let mut delta: isize = 0;
        for g in &groups {
            let start = (g.range.start as isize + delta) as usize;
            for (i, within) in &g.members {
                result[*i] = start + within.start..start + within.end;
            }
            delta += g.text.len() as isize - g.range.len() as isize;
        }

        if !records.is_empty() {
            self.version += 1;
            self.log_patch(patch.take().expect("set above"));
            self.reparse();
            self.redo_stack.clear();
            let now = Instant::now();
            let merge = self.undo_stack.last().is_some_and(|t| {
                kind != EditKind::Other
                    && t.kind == kind
                    && now.duration_since(t.at) < Duration::from_millis(800)
            });
            let id = self.next_transaction();
            if merge {
                let t = self.undo_stack.last_mut().unwrap();
                t.id = id;
                t.edits.extend(records);
                t.at = now;
            } else {
                self.undo_stack.push(Transaction {
                    id,
                    edits: records,
                    before: selections_before.to_vec(),
                    after: Vec::new(),
                    at: now,
                    kind,
                });
            }
        }
        result
    }

    /// Where the undo history is now, for `merge_undo_since`.
    pub fn undo_mark(&self) -> usize {
        self.undo_stack.len()
    }

    /// Makes everything done since `mark` one undo step (a Vim change: the
    /// operator and the text typed after it).
    pub fn merge_undo_since(&mut self, mark: usize) {
        if self.undo_stack.len() <= mark + 1 {
            return;
        }
        let mut merged = self.undo_stack.split_off(mark);
        let mut first = merged.remove(0);
        for t in merged {
            first.id = t.id;
            first.edits.extend(t.edits);
            first.after = t.after;
            first.at = t.at;
        }
        first.kind = EditKind::Other;
        self.undo_stack.push(first);
    }

    /// Records the selections after the most recent transaction, for redo.
    pub fn set_selections_after(&mut self, selections: &[Selection]) {
        if let Some(t) = self.undo_stack.last_mut() {
            t.after = selections.to_vec();
        }
    }

    /// Breaks typing coalescing so the next edit starts a new undo step.
    pub fn finalize_transaction(&mut self) {
        if let Some(t) = self.undo_stack.last_mut() {
            t.kind = EditKind::Other;
        }
    }

    /// Undoes the last transaction as one batch of edits (see
    /// `make_at_once`): thousands of carets' typing is as many edits as
    /// carets, and what follows the text moves once.
    pub fn undo(&mut self) -> Option<Vec<Selection>> {
        let t = self.undo_stack.pop()?;
        // Its records undone, the last first.
        let undone: Vec<Replaced> = t.edits.iter().rev().map(|r| Replaced { start: r.start, old: &r.new, new: &r.old }).collect();
        self.make_at_once(&undone);
        let selections = t.before.clone();
        self.redo_stack.push(t);
        Some(selections)
    }

    pub fn redo(&mut self) -> Option<Vec<Selection>> {
        let t = self.redo_stack.pop()?;
        let redone: Vec<Replaced> = t.edits.iter().map(|r| Replaced { start: r.start, old: &r.old, new: &r.new }).collect();
        self.make_at_once(&redone);
        let selections = t.after.clone();
        self.undo_stack.push(t);
        Some(selections)
    }

    /// Makes edits (made one after another, each in the text of its time)
    /// as one version: composed (see `compose`), the rope and trees take
    /// touching ones as one replacement (a word typed letter by letter),
    /// and what follows the text moves once, as through the edits one by
    /// one. Except at line starts where edits touch or merge, there which
    /// went first decides: those are moved edit by edit.
    fn make_at_once(&mut self, edits: &[Replaced<'_>]) {
        let composed = compose(edits.iter().copied());
        let exact = self.exact_line_starts(&composed, edits);
        // Typed and deleted again: nothing to make.
        let composed: Vec<Composed> = composed.into_iter().filter(|e| !(e.old.is_empty() && e.new_text.is_empty())).collect();
        let before = self.rope.clone();
        // From the last, so the ranges before stay valid.
        let mut end = composed.len();
        while end > 0 {
            let mut start = end - 1;
            while start > 0 && composed[start - 1].old.end == composed[start].old.start {
                start -= 1;
            }
            let range = composed[start].old.start..composed[end - 1].old.end;
            match &composed[start..end] {
                [edit] => self.raw_replace(range, &edit.new_text),
                touching => self.raw_replace(range, &touching.iter().map(|e| &*e.new_text).collect::<String>()),
            }
            end = start;
        }
        let patch = Patch::new(composed.iter().map(|e| (e.old.clone(), e.new_text.len())).collect());
        self.shift_tracked(&patch, &before, &exact);
        self.version += 1;
        // One patch for the version, however many edits: other editors
        // catch up through a log of limited length.
        self.log_patch(patch);
        self.reparse();
    }

    /// The tracked line starts (breakpoints, the debug line, code lenses)
    /// that `composed` edits reach where they touch each other or merged,
    /// moved through `edits` one by one (see `line_starts_one_by_one`).
    fn exact_line_starts(&self, composed: &[Composed], edits: &[Replaced]) -> ExactLines {
        let mut starts: Vec<usize> = self.breakpoints.iter().map(|(start, _)| *start).chain(self.debug_line).chain(self.code_lenses.iter().map(|l| l.line_start)).collect();
        if starts.is_empty() {
            return ExactLines::new();
        }
        starts.sort_unstable();
        starts.dedup();
        let mut wanted = Vec::new();
        let mut first = 0;
        while first < composed.len() {
            let mut end = first + 1;
            while end < composed.len() && composed[end - 1].old.end == composed[end].old.start {
                end += 1;
            }
            if end - first > 1 || composed[first].merged {
                let (from, to) = (composed[first].old.start, composed[end - 1].old.end);
                let at = starts.partition_point(|&s| s < from);
                wanted.extend(starts[at..].iter().take_while(|&&s| s <= to));
            }
            first = end;
        }
        if wanted.is_empty() {
            return ExactLines::new();
        }
        let moved = line_starts_one_by_one(&self.rope, edits, &wanted);
        wanted.into_iter().zip(moved).collect()
    }

    fn raw_replace(&mut self, range: Range<usize>, text: &str) {
        let lines = LineEdit::of(&self.rope, range.clone(), text);
        self.replace_logging(range, text, lines, &[lines]);
    }

    /// `raw_replace`, whose `lines` are logged as the edits in it (one
    /// replacement can stand for several changes, each logged).
    fn replace_logging(&mut self, range: Range<usize>, text: &str, lines: LineEdit, logged: &[LineEdit]) {
        self.lsp_changes.record(&self.rope, range.clone(), text);
        // Every caller bumps the version once its edits are done.
        if self.line_edits.len() + logged.len() > LINE_EDIT_LOG {
            // From after this transaction: part of it would mislead.
            self.line_edits.clear();
            self.line_edits_base = self.version + 1;
        } else {
            self.line_edits.extend(logged.iter().map(|lines| (self.version + 1, *lines)));
        }
        let row = lines.row;
        self.long_lines -= long_lines(&self.rope, row..row + lines.old_rows);
        let edit = input_edit(&self.rope, range.clone(), text);
        if self.parse_behind {
            self.parse_edits.push((self.version + 1, edit));
        }
        let s = self.rope.byte_to_char(range.start);
        let e = self.rope.byte_to_char(range.end);
        self.rope.remove(s..e);
        self.rope.insert(s, text);
        self.syntax.edit(&edit);
        self.long_lines += long_lines(&self.rope, row..row + lines.new_rows);
    }

    /// Parses after an edit, within `PARSE_BUDGET`.
    fn reparse(&mut self) {
        let parsed = self.syntax.reparse_within(&self.rope, Some(self.parse_budget));
        self.after_parse(parsed);
    }

    fn set_syntax(&mut self, syntax: Syntax, parsed: bool) {
        self.syntax = syntax;
        self.after_parse(parsed);
    }

    fn after_parse(&mut self, parsed: bool) {
        self.tree_version += 1;
        self.parse_behind = !parsed;
        if parsed {
            self.parse_edits.clear();
            self.parsed_version = Some(self.version);
        }
    }

    /// When a parse ran out of time: the version it's for and a full parse
    /// to run in the background; give the result to `finish_parse`.
    pub fn parse_job(&self) -> Option<(u64, impl FnOnce() -> Syntax + Send + 'static)> {
        self.parse_behind.then(|| (self.version, self.syntax.parse_job(&self.rope)))
    }

    /// Takes a background parse of `version`, catching it up with the edits
    /// made since. True when it's still behind (edits came meanwhile).
    pub fn finish_parse(&mut self, version: u64, mut syntax: Syntax) -> bool {
        if self.parsed_version.is_some_and(|parsed| version <= parsed) || version > self.version || syntax.language() != self.language {
            return self.parse_behind;
        }
        for (_, edit) in self.parse_edits.iter().filter(|(v, _)| *v > version) {
            syntax.edit(edit);
        }
        self.parse_edits.retain(|(v, _)| *v > version);
        self.syntax = syntax;
        self.tree_version += 1;
        self.parsed_version = Some(version);
        self.parse_behind = version != self.version;
        self.parse_behind
    }

    /// The file's own syntax tree for the text as it is, to read off the UI
    /// thread (see `Syntax::tree_job`).
    pub fn tree_job(&self) -> impl FnOnce() -> Option<tree_sitter::Tree> + Send + use<> {
        self.syntax.tree_job(&self.rope, !self.parse_behind)
    }

    /// Changes whenever the syntax trees do.
    pub fn tree_version(&self) -> u64 {
        self.tree_version
    }

    /// Whether some line is longer than `wrap::LONG_LINE`.
    pub fn has_long_lines(&self) -> bool {
        self.long_lines > 0
    }

    /// The line edits that lead from `version` to the current text, oldest
    /// first, each in the lines of the text before it; None when they
    /// aren't kept (too many, or the whole text was replaced).
    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn is_stand_in(&self) -> bool {
        self.stand_in
    }

    fn log_patch(&mut self, patch: Patch) {
        self.log_patches(vec![patch]);
    }

    /// The edits that made the current version, in the order they applied.
    fn log_patches(&mut self, patches: Vec<Patch>) {
        for patch in patches {
            self.patches.push_back((self.version, patch));
        }
        while self.patches.len() > PATCH_LOG {
            let (version, _) = self.patches.pop_front().expect("not empty");
            // Versions up to this one can't be caught up with any more.
            self.patches_base = version;
            while self.patches.front().is_some_and(|(v, _)| *v == version) {
                self.patches.pop_front();
            }
        }
    }

    /// The edits since `version`, in order, to move positions from then to
    /// now; None when that's too long ago (or `version` isn't this text's).
    pub fn patches_since(&self, version: u64) -> Option<impl Iterator<Item = &Patch>> {
        if version < self.patches_base || version > self.version {
            return None;
        }
        Some(self.patches.iter().filter(move |(v, _)| *v > version).map(|(_, patch)| patch))
    }

    pub fn line_edits_since(&self, version: u64) -> Option<impl Iterator<Item = &LineEdit>> {
        if version < self.line_edits_base || version > self.version {
            return None;
        }
        let first = self.line_edits.partition_point(|(v, _)| *v <= version);
        Some(self.line_edits[first..].iter().map(|(_, edit)| edit))
    }

    /// Moves what follows the text (breakpoints, the debug line, inlay
    /// hints, code lenses, diagnostics) across a transaction's
    /// edits, once for all of them. Runs after the rope changed; `old` is
    /// the text before.
    fn shift_tracked(&mut self, patch: &Patch, old: &Rope, exact: &ExactLines) {
        if patch.edits.is_empty() {
            return;
        }
        #[cfg(test)]
        {
            let tracked = self.breakpoints.len() + self.inlay_hints.len() + self.code_lenses.len() + self.diagnostics.len();
            WORK.with(|work| work.set(work.get() + 1 + tracked));
        }
        self.shift_breakpoints(patch, old, exact);
        self.debug_line = self.debug_line.and_then(|offset| self.line_moved(offset, patch, old, exact));
        self.shift_inlay_hints(patch);
        self.code_lenses = std::mem::take(&mut self.code_lenses)
            .into_iter()
            .filter_map(|lens| Some(CodeLens { line_start: self.line_moved(lens.line_start, patch, old, exact)?, ..lens }))
            .collect();
        // The merged list too: shifting keeps its order, so it needn't be
        // built and sorted again.
        for list in [&mut self.builtin_diagnostics, &mut self.external_diagnostics, &mut self.diagnostics] {
            shift_diagnostics(list, patch);
        }
        for (_, list) in &mut self.lsp_diagnostics {
            shift_diagnostics(list, patch);
        }
        // Edits drop the problems they touch.
        self.severities = count_severities(&self.diagnostics);
    }

    pub fn code_lenses(&self) -> &[CodeLens] {
        &self.code_lenses
    }

    /// Replaces the code lenses (read in today's text, asked for at
    /// `stamp`: see `lens_stamp`), giving each its id.
    pub fn set_code_lenses(&mut self, mut lenses: Vec<CodeLens>, stamp: (u64, u64)) {
        lenses.sort_by_key(|l| l.line_start);
        for lens in &mut lenses {
            self.last_lens += 1;
            lens.id = self.last_lens;
        }
        self.code_lenses = lenses;
        self.lenses_read = Some(stamp);
        self.code_lens_generation += 1;
    }

    /// Changes when the lenses are replaced or one gets its title (see
    /// `code_lens_generation`).
    pub fn code_lens_generation(&self) -> u64 {
        self.code_lens_generation
    }

    /// What the code lenses are read for: the version and how many times
    /// servers said they changed. Lenses asked for at one stamp are taken
    /// only while it holds.
    pub fn lens_stamp(&self) -> (u64, u64) {
        (self.version, self.lens_refreshes)
    }

    /// Whether the code lenses were read in today's text, since a server
    /// last said they changed.
    pub fn lenses_current(&self) -> bool {
        self.lenses_read == Some(self.lens_stamp())
    }

    /// A server says its lenses changed: they're due again.
    pub fn lenses_changed(&mut self) {
        self.lens_refreshes += 1;
    }

    /// Lens `id`'s title and command came (`codeLens/resolve`); None: it
    /// has none, and goes.
    pub fn resolve_code_lens(&mut self, id: u64, resolved: Option<(String, serde_json::Value)>) {
        match resolved {
            Some((title, command)) => {
                if let Some(lens) = self.code_lenses.iter_mut().find(|l| l.id == id) {
                    (lens.title, lens.command, lens.unresolved) = (title.into(), Arc::new(command), None);
                }
            }
            None => self.code_lenses.retain(|l| l.id != id),
        }
        // Rows drawn from the lenses (cached by this) show the title now.
        self.code_lens_generation += 1;
    }

    pub fn inlay_hints(&self) -> &[InlayHint] {
        &self.inlay_hints
    }

    #[cfg(test)]
    pub fn set_inlay_hints(&mut self, mut hints: Vec<InlayHint>) {
        hints.sort_by_key(|h| h.offset);
        self.inlay_hints = hints;
        self.inlay_generation += 1;
    }

    /// What inlay hints are asked for at: the version and how many times
    /// servers said they changed. An answer to a request made at one stamp
    /// is taken only while it holds.
    pub fn hint_stamp(&self) -> (u64, u64) {
        (self.version, self.hint_refreshes)
    }

    /// Whether the hints of lines chunk `chunk` were asked for in today's
    /// text, since a server last said they changed.
    pub fn hints_fresh(&self, chunk: usize) -> bool {
        self.hint_chunks.0 == self.hint_stamp() && self.hint_chunks.1.contains(&chunk)
    }

    /// Whether no edit came since hints were last asked for.
    pub fn hints_current(&self) -> bool {
        self.hint_chunks.0.0 == self.version
    }

    /// `chunks` had their hints asked for now (`hint_stamp`), which are
    /// `hints` in `range` (bytes); None: they couldn't be had, and the ones
    /// there stay (following edits) until the next answer.
    pub fn set_chunk_hints(&mut self, range: Range<usize>, chunks: Range<usize>, hints: Option<Vec<InlayHint>>) {
        if self.hint_chunks.0 != self.hint_stamp() {
            self.hint_chunks = (self.hint_stamp(), Vec::new());
        }
        for chunk in chunks {
            if !self.hint_chunks.1.contains(&chunk) {
                self.hint_chunks.1.push(chunk);
            }
        }
        let Some(mut hints) = hints else { return };
        hints.retain(|h| range.contains(&h.offset));
        hints.sort_by_key(|h| h.offset);
        let first = self.inlay_hints.partition_point(|h| h.offset < range.start);
        let last = self.inlay_hints.partition_point(|h| h.offset < range.end);
        if self.inlay_hints[first..last] != hints[..] {
            self.inlay_hints.splice(first..last, hints);
            self.inlay_generation += 1;
        }
    }

    /// A server says its hints changed: every chunk is due again.
    pub fn hints_changed(&mut self) {
        self.hint_refreshes += 1;
    }

    /// Changes when the hints are replaced (from 1; see `inlay_generation`).
    pub fn inlay_generation(&self) -> u64 {
        self.inlay_generation
    }

    /// Hints after an edit move with the text; ones inside replaced text go.
    /// A hint where text is inserted stays after it (a type after a name
    /// being typed).
    fn shift_inlay_hints(&mut self, patch: &Patch) {
        self.inlay_hints.retain_mut(|h| {
            let (mut delta, near) = patch.around(h.offset..h.offset);
            for (range, new_len) in near {
                if h.offset > range.start && h.offset < range.end {
                    return false;
                }
                if h.offset >= range.end {
                    delta += Patch::change(range, *new_len);
                }
            }
            h.offset = moved(h.offset, delta);
            true
        });
    }

    pub fn set_builtin_diagnostics(&mut self, diagnostics: Vec<Diagnostic>) {
        self.builtin_diagnostics = diagnostics;
        self.merge_diagnostics();
    }

    pub fn set_external_diagnostics(&mut self, diagnostics: Vec<Diagnostic>) {
        self.external_diagnostics = diagnostics;
        self.merge_diagnostics();
    }

    /// Diagnostics from one unnamed language server (tests).
    #[cfg(test)]
    pub fn set_lsp_diagnostics(&mut self, diagnostics: Vec<Diagnostic>) {
        self.set_server_diagnostics("", diagnostics);
    }

    /// Replaces one language server's diagnostics (already in today's text).
    pub fn set_server_diagnostics(&mut self, server: &str, diagnostics: Vec<Diagnostic>) {
        match self.lsp_diagnostics.iter_mut().find(|(id, _)| id == server) {
            Some((_, list)) => *list = diagnostics,
            None => self.lsp_diagnostics.push((server.to_string(), diagnostics)),
        }
        self.merge_diagnostics();
    }

    /// Drops a server's diagnostics (it stopped or restarted).
    pub fn clear_server_diagnostics(&mut self, server: &str) {
        let before = self.lsp_diagnostics.len();
        self.lsp_diagnostics.retain(|(id, _)| id != server);
        if self.lsp_diagnostics.len() != before {
            self.merge_diagnostics();
        }
    }

    /// Notes the text as language servers are told it is now (the version
    /// sent with `didOpen` / `didChange`).
    pub fn note_lsp_version(&mut self) {
        if self.lsp_versions.back().is_some_and(|(v, _)| *v == self.version) {
            return;
        }
        // Past what the patch log can map from, an old text is no use.
        while self.lsp_versions.front().is_some_and(|(v, _)| *v < self.patches_base) || self.lsp_versions.len() >= MAX_LSP_VERSIONS {
            self.lsp_versions.pop_front();
        }
        self.lsp_versions.push_back((self.version, self.rope.clone()));
    }

    /// The text language servers were sent as `version`; None when it's too
    /// old (or never was).
    pub fn text_at_lsp_version(&self, version: u64) -> Option<&Rope> {
        if version == self.version {
            return Some(&self.rope);
        }
        self.lsp_versions.iter().find(|(v, _)| *v == version).map(|(_, rope)| rope)
    }

    /// Moves diagnostics found in the text of `version` to today's text, as
    /// they would have followed the edits since: the ones those edits touched
    /// go. None when the edits since aren't kept any more.
    pub fn diagnostics_since(&self, version: u64, mut diagnostics: Vec<Diagnostic>) -> Option<Vec<Diagnostic>> {
        for patch in self.patches_since(version)? {
            shift_diagnostics(&mut diagnostics, patch);
        }
        Some(diagnostics)
    }

    /// An offset in the text of `version`, in today's text (see `Patch::map`).
    pub fn offset_since(&self, version: u64, offset: usize) -> Option<usize> {
        Some(self.patches_since(version)?.fold(offset, |offset, patch| patch.map(offset)))
    }

    /// The breakpoints, by row.
    pub fn breakpoints(&self) -> Vec<Breakpoint> {
        self.breakpoints.iter().map(|(_, b)| b.clone()).collect()
    }

    pub fn breakpoint(&self, row: usize) -> Option<&Breakpoint> {
        // Sorted by line start, so by row: drawing asks for every line on screen.
        let ix = self.breakpoints.partition_point(|(_, b)| b.row < row);
        self.breakpoints.get(ix).map(|(_, b)| b).filter(|b| b.row == row)
    }

    pub fn set_breakpoints(&mut self, breakpoints: &[Breakpoint]) {
        let lines = self.line_count();
        let mut placed: Vec<(usize, Breakpoint)> =
            breakpoints.iter().filter(|b| b.row < lines).map(|b| (self.line_start(b.row), b.clone())).collect();
        placed.sort_by_key(|(start, _)| *start);
        placed.dedup_by_key(|(start, _)| *start);
        self.breakpoints = placed;
    }

    /// Puts `breakpoint` on its row, replacing the one there.
    pub fn set_breakpoint(&mut self, breakpoint: Breakpoint) {
        if breakpoint.row >= self.line_count() {
            return;
        }
        let start = self.line_start(breakpoint.row);
        match self.breakpoints.binary_search_by_key(&start, |(s, _)| *s) {
            Ok(ix) => self.breakpoints[ix].1 = breakpoint,
            Err(ix) => self.breakpoints.insert(ix, (start, breakpoint)),
        }
    }

    /// Adds or removes the breakpoint on `row`; true when one is there now.
    pub fn toggle_breakpoint(&mut self, row: usize) -> bool {
        let start = self.line_start(row);
        match self.breakpoints.binary_search_by_key(&start, |(s, _)| *s) {
            Ok(ix) => {
                self.breakpoints.remove(ix);
                false
            }
            Err(ix) => {
                self.breakpoints.insert(ix, (start, Breakpoint::at(row)));
                true
            }
        }
    }

    /// Breakpoints follow their line's text: an edit before a line moves
    /// it, one that removes the line (see `shift_line`) removes it. (Runs
    /// after the rope changed, so lines can be looked up again.)
    fn shift_breakpoints(&mut self, patch: &Patch, old: &Rope, exact: &ExactLines) {
        if self.breakpoints.is_empty() {
            return;
        }
        let mut moved = Vec::with_capacity(self.breakpoints.len());
        for (offset, breakpoint) in std::mem::take(&mut self.breakpoints) {
            if let Some(start) = self.line_moved(offset, patch, old, exact) {
                moved.push((start, Breakpoint { row: self.row_of(start), ..breakpoint }));
            }
        }
        moved.dedup_by_key(|(start, _)| *start);
        self.breakpoints = moved;
    }

    /// Where the line starting at `offset` starts after `patch` (see
    /// `shift_line`), unless it was worked out edit by edit (`exact`).
    fn line_moved(&self, offset: usize, patch: &Patch, old: &Rope, exact: &ExactLines) -> Option<usize> {
        match exact.get(&offset) {
            Some(moved) => *moved,
            None => shift_line(&self.rope, offset, patch, old),
        }
    }

    /// The row a debug session is paused on, following edits.
    pub fn debug_row(&self) -> Option<usize> {
        self.debug_line.map(|offset| self.row_of(offset))
    }

    /// Highlights `row` as where a debug session is paused (None: running).
    pub fn set_debug_row(&mut self, row: Option<usize>) {
        self.debug_line = row.filter(|&r| r < self.line_count()).map(|r| self.line_start(r));
    }

    /// Edits not yet sent to the language server, oldest first; each is
    /// relative to the text after the previous one, as LSP expects.
    pub fn take_lsp_changes(&mut self) -> LspChanges {
        std::mem::take(&mut self.lsp_changes)
    }

    fn merge_diagnostics(&mut self) {
        self.diagnostics = self
            .builtin_diagnostics
            .iter()
            .chain(&self.external_diagnostics)
            .chain(self.lsp_diagnostics.iter().flat_map(|(_, list)| list))
            .cloned()
            .collect();
        self.diagnostics
            .sort_by_key(|d| (d.range.start, d.severity));
        self.diagnostics_generation += 1;
        self.severities = count_severities(&self.diagnostics);
    }

    /// Changes when the diagnostics are built again; with `version`, it
    /// tells when they may have changed.
    pub fn diagnostics_generation(&self) -> u64 {
        self.diagnostics_generation
    }

    /// How many diagnostics have `severity`, without looking at them all.
    pub fn severity_count(&self, severity: crate::lint::Severity) -> usize {
        self.severities[severity as usize]
    }

    pub fn has_errors(&self) -> bool {
        self.severity_count(crate::lint::Severity::Error) > 0
    }
}

fn count_severities(diagnostics: &[Diagnostic]) -> [usize; 4] {
    let mut counts = [0; 4];
    for d in diagnostics {
        counts[d.severity as usize] += 1;
    }
    counts
}

/// Lines `row..row + old_rows` became `new_rows` lines.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LineEdit {
    pub row: usize,
    pub old_rows: usize,
    pub new_rows: usize,
}

impl LineEdit {
    /// What replacing `range` of `rope` with `text` does to its lines.
    fn of(rope: &Rope, range: Range<usize>, text: &str) -> Self {
        let row = rope.byte_to_line(range.start);
        LineEdit { row, old_rows: rope.byte_to_line(range.end) - row + 1, new_rows: 1 + text.bytes().filter(|&b| b == b'\n').count() }
    }
}

/// A type or parameter name from the language server, shown in the text
/// before `offset`.
#[derive(Clone, Debug, PartialEq)]
pub struct InlayHint {
    pub offset: usize,
    pub label: String,
    pub pad_left: bool,
    pub pad_right: bool,
}

/// A language server's code lens ("3 references", "▶ Run"): shown above its
/// line, which it keeps across edits (it goes with a line that does). Cheap
/// to copy: what it shows and runs is shared.
#[derive(Clone, Debug, PartialEq)]
pub struct CodeLens {
    /// Tells it apart while its title is asked for (`set_code_lenses` gives it).
    pub id: u64,
    /// The start of its line.
    pub line_start: usize,
    /// Empty until it's resolved.
    pub title: Arc<str>,
    /// The LSP `Command` it runs (null until it's resolved).
    pub command: Arc<serde_json::Value>,
    /// The id of the server that sent it.
    pub server: Arc<str>,
    /// The lens as the server sent it, without its command: it's resolved
    /// (`codeLens/resolve`, which gives it its title) once it comes into
    /// view. Its row is there meanwhile, so lines don't move when it's done.
    pub unresolved: Option<Arc<serde_json::Value>>,
}

/// One replacement in LSP terms: where it was (as line plus UTF-8 and
/// UTF-16 columns, since servers differ) and the new text.
#[derive(Clone, Debug, PartialEq)]
pub struct LspChange {
    pub start: (u32, u32, u32),
    pub end: (u32, u32, u32),
    pub text: String,
}

#[derive(Default, Debug)]
pub struct LspChanges {
    pub changes: Vec<LspChange>,
    /// Too many changes, or the whole text was replaced: send it all.
    pub full: bool,
}

impl LspChanges {
    /// Past this, sending the whole text is cheaper than the edit list.
    const MAX: usize = 1000;

    fn record(&mut self, rope: &Rope, range: Range<usize>, text: &str) {
        if self.full {
            return;
        }
        if self.changes.len() >= Self::MAX {
            self.mark_full();
            return;
        }
        let point = |offset: usize| {
            let line = rope.byte_to_line(offset);
            let line_start = rope.line_to_byte(line);
            let (c0, c1) = (rope.byte_to_char(line_start), rope.byte_to_char(offset));
            let utf16 = rope.char_to_utf16_cu(c1) - rope.char_to_utf16_cu(c0);
            (line as u32, (offset - line_start) as u32, utf16 as u32)
        };
        self.changes.push(LspChange { start: point(range.start), end: point(range.end), text: text.to_string() });
    }

    fn mark_full(&mut self) {
        self.changes.clear();
        self.full = true;
    }

    pub fn is_empty(&self) -> bool {
        self.changes.is_empty() && !self.full
    }
}

fn mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// What a save writes, taken from a document (`Document::save_job`): its
/// text then, a rope clone, written anywhere with `write`; the result goes
/// back to `Document::saved`.
pub struct SaveJob {
    id: u64,
    ticket: u64,
    /// The undo history's position for the text written.
    position: u64,
    path: PathBuf,
    rope: Rope,
    encoding: TextEncoding,
    crlf: bool,
    settles: Settles,
    in_flight: InFlight,
}

impl SaveJob {
    /// Writes the text: line ends turned into CRLF if the file has them and
    /// encoded chunk by chunk as they're written (see `encoding::write_file`).
    /// A document's saves write one at a time, and one that a newer save
    /// overtook writes nothing, so the file ends with the newest text.
    pub fn write(self) -> Written {
        let SaveJob { id, ticket, position, path, rope, encoding, crlf, settles, in_flight } = self;
        // Local History: what the file was before Kuana first saved it.
        if !crate::local_history::has_history(&path) {
            crate::local_history::record_disk(&path, crate::local_history::Kind::Original);
        }
        let outcome = {
            let mut newest = settles.0.written.lock().unwrap_or_else(PoisonError::into_inner);
            if *newest > ticket {
                Outcome::Overtaken
            } else {
                let line_end = if crlf { "\r\n" } else { "\n" };
                let pieces = rope.chunks().flat_map(|chunk| {
                    chunk.split_inclusive('\n').flat_map(move |part| match part.strip_suffix('\n') {
                        Some(line) => [line, line_end],
                        None => [part, ""],
                    })
                });
                match encoding::write_file(&path, pieces, encoding) {
                    Ok(()) => {
                        *newest = ticket;
                        Outcome::Written(mtime(&path))
                    }
                    Err(err) => Outcome::Failed(err),
                }
            }
        };
        drop(in_flight);
        if matches!(outcome, Outcome::Written(_)) {
            crate::local_history::record_rope(&path, &rope, crate::local_history::Kind::Saved);
        }
        Written { id, ticket, position, encoding, crlf, outcome, _settles: settles }
    }
}

/// A save written (or not), for `Document::saved`.
pub struct Written {
    id: u64,
    ticket: u64,
    position: u64,
    encoding: TextEncoding,
    crlf: bool,
    outcome: Outcome,
    _settles: Settles,
}

enum Outcome {
    /// With the file's modification time after it.
    Written(Option<SystemTime>),
    /// A newer save was written first.
    Overtaken,
    Failed(std::io::Error),
}

/// A document's saves: written one at a time (holding `written`, the
/// newest one written), and how many were taken and aren't back in yet.
#[derive(Default)]
struct Writes {
    written: Mutex<u64>,
    unsettled: AtomicUsize,
}

impl Writes {
    fn unsettled(&self) -> bool {
        self.unsettled.load(Ordering::SeqCst) > 0
    }
}

/// Counts a save as unsettled until its result is taken back in, or
/// dropped on the way.
struct Settles(Arc<Writes>);

impl Settles {
    fn new(writes: &Arc<Writes>) -> Self {
        writes.unsettled.fetch_add(1, Ordering::SeqCst);
        Self(writes.clone())
    }
}

impl Drop for Settles {
    fn drop(&mut self) {
        self.0.unsettled.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Saves of any document being written, which quitting waits for.
static IN_FLIGHT: (Mutex<usize>, Condvar) = (Mutex::new(0), Condvar::new());

/// Counts a save in `IN_FLIGHT` until it's written, or dropped unwritten.
struct InFlight;

impl InFlight {
    fn new() -> Self {
        *IN_FLIGHT.0.lock().unwrap_or_else(PoisonError::into_inner) += 1;
        Self
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        *IN_FLIGHT.0.lock().unwrap_or_else(PoisonError::into_inner) -= 1;
        IN_FLIGHT.1.notify_all();
    }
}

/// Waits for the saves being written (quitting: a file must not be left
/// half written).
pub fn wait_for_saves() {
    let mut count = IN_FLIGHT.0.lock().unwrap_or_else(PoisonError::into_inner);
    while *count > 0 {
        count = IN_FLIGHT.1.wait(count).unwrap_or_else(PoisonError::into_inner);
    }
}

/// How long a reload may diff the old text with the new (in the
/// background); past it the diff is coarser, and fewer things keep their place.
const RELOAD_DIFF_TIMEOUT: Duration = Duration::from_millis(100);

/// The file as read again in the background (`Document::reload_job`), as
/// the changes that turn the document's text into it.
pub struct Reload {
    /// The document and version it was compared with, and the file's
    /// modification time the document knew then.
    id: u64,
    version: u64,
    known: Option<SystemTime>,
    encoding: TextEncoding,
    /// The file's modification time when it was read.
    mtime: Option<SystemTime>,
    changes: Changes,
}

/// What a reload changes: the lines that changed (`changed_ranges`). What
/// follows the text (diagnostics, editors' folds and carets, breakpoints,
/// the debug line, inlay hints, code lenses) moves with them: on lines that
/// stayed it follows them, in changed text it goes.
#[derive(Default)]
struct Changes {
    /// Ranges of the old text (sorted, apart) and what replaces them.
    edits: Vec<(Range<usize>, String)>,
    /// The new text from the first edit's start to the last one's end.
    span: String,
}

impl Changes {
    fn between(old: &str, new: &str) -> Self {
        let ranges = changed_ranges(old, new);
        let span = match (ranges.first(), ranges.last()) {
            (Some((_, first)), Some((_, last))) => new[first.start..last.end].to_string(),
            _ => String::new(),
        };
        Changes { edits: ranges.into_iter().map(|(range, by)| (range, new[by].to_string())).collect(), span }
    }
}

/// What turns `old` into `new`, as the byte ranges of `old` that change
/// with the ranges of `new` that replace them (sorted, apart). Found by
/// lines; within a run of changed lines, lines pair up in order, the
/// shorter side placed along the longer where its lines are most alike
/// (lines added above a changed one, below it, or around a block that got
/// indented), and each pair only changes the bytes between their common
/// start and end. So what's on the unchanged parts of a line stays: its
/// breakpoint, a diagnostic on a name whose value changed, the code after
/// a new indentation.
fn changed_ranges(old: &str, new: &str) -> Vec<(Range<usize>, Range<usize>)> {
    // Line pairs compared at most for placing one run (else only its ends).
    const PLACING: usize = 20_000;
    let (old_lines, new_lines): (Vec<&str>, Vec<&str>) = (old.split_inclusive('\n').collect(), new.split_inclusive('\n').collect());
    // Where each line starts, and the end.
    let starts = |lines: &[&str]| -> Vec<usize> {
        let mut at = vec![0];
        at.extend(lines.iter().scan(0, |end, line| {
            *end += line.len();
            Some(*end)
        }));
        at
    };
    let (old_at, new_at) = (starts(&old_lines), starts(&new_lines));
    let mut changes = Vec::new();
    let mut change = |old_rows: Range<usize>, new_rows: Range<usize>| {
        let (o, n) = (old_at[old_rows.start]..old_at[old_rows.end], new_at[new_rows.start]..new_at[new_rows.end]);
        let (prefix, suffix) = common_ends(&old[o.clone()], &new[n.clone()]);
        if o.len() != prefix + suffix || n.len() != prefix + suffix {
            changes.push((o.start + prefix..o.end - suffix, n.start + prefix..n.end - suffix));
        }
    };
    let diff = similar::TextDiff::configure().timeout(RELOAD_DIFF_TIMEOUT).diff_slices(&old_lines, &new_lines);
    for op in diff.ops() {
        let (tag, o, n) = op.as_tag_tuple();
        if tag == similar::DiffTag::Equal {
            continue;
        }
        let pairs = o.len().min(n.len());
        let rest = o.len().max(n.len()) - pairs;
        // Where the pairs start when they skip `skip` lines of the longer side.
        let from = |skip: usize| if o.len() > n.len() { (o.start + skip, n.start) } else { (o.start, n.start + skip) };
        let alike = |skip: usize| -> usize {
            let (o_from, n_from) = from(skip);
            (0..pairs)
                .map(|i| {
                    let (prefix, suffix) = common_ends(old_lines[o_from + i], new_lines[n_from + i]);
                    prefix + suffix
                })
                .sum()
        };
        let skip = match (pairs, rest) {
            (0, _) | (_, 0) => 0,
            _ if pairs * (rest + 1) <= PLACING => (0..=rest).max_by_key(|&skip| (alike(skip), std::cmp::Reverse(skip))).unwrap_or(0),
            _ => [0, rest].into_iter().max_by_key(|&skip| (alike(skip), std::cmp::Reverse(skip))).unwrap_or(0),
        };
        // The longer side's lines before and after the pairs were added or removed.
        let (o_from, n_from) = from(skip);
        change(o.start..o_from, n.start..n_from);
        for i in 0..pairs {
            change(o_from + i..o_from + i + 1, n_from + i..n_from + i + 1);
        }
        change(o_from + pairs..o.end, n_from + pairs..n.end);
    }
    changes
}

/// How many bytes `a` and `b` share at their start, and then at their end
/// (whole characters).
fn common_ends(a: &str, b: &str) -> (usize, usize) {
    let mut prefix = a.bytes().zip(b.bytes()).take_while(|(x, y)| x == y).count();
    while !a.is_char_boundary(prefix) {
        prefix -= 1;
    }
    let (a, b) = (&a[prefix..], &b[prefix..]);
    let mut suffix = a.bytes().rev().zip(b.bytes().rev()).take_while(|(x, y)| x == y).count();
    // The bytes from there on are the same in both, so a boundary in one is in the other.
    while !a.is_char_boundary(a.len() - suffix) {
        suffix -= 1;
    }
    (prefix, suffix)
}

/// One transaction's edits, for moving tracked positions: the replaced
/// ranges (sorted, not overlapping, in the text before the edits) with the
/// length of their new text.
pub struct Patch {
    edits: Vec<(Range<usize>, usize)>,
    /// `before[i]`: the length change of `edits[..i]`.
    before: Vec<isize>,
}

impl Patch {
    pub fn new(edits: Vec<(Range<usize>, usize)>) -> Self {
        let mut before = Vec::with_capacity(edits.len() + 1);
        let mut total = 0;
        before.push(0);
        for (range, new_len) in &edits {
            total += Patch::change(range, *new_len);
            before.push(total);
        }
        Patch { edits, before }
    }

    pub fn change(range: &Range<usize>, new_len: usize) -> isize {
        new_len as isize - range.len() as isize
    }

    /// Where `offset` is after the edits: moved by the ones before it; inside
    /// a replaced range, the end of the new text; at an insertion, before it.
    pub fn map(&self, offset: usize) -> usize {
        let (mut delta, near) = self.around(offset..offset);
        for (range, new_len) in near {
            if offset <= range.start {
                break;
            } else if offset >= range.end {
                delta += Patch::change(range, *new_len);
            } else {
                return moved(range.start, delta) + new_len;
            }
        }
        moved(offset, delta)
    }

    /// For something spanning `span`: the length change of the edits that
    /// end before it starts (they move it), and the edits touching or
    /// overlapping it, which each list judges by its own rule (a few at most).
    pub fn around(&self, span: Range<usize>) -> (isize, &[(Range<usize>, usize)]) {
        let first = self.edits.partition_point(|(range, _)| range.end < span.start);
        let last = first + self.edits[first..].partition_point(|(range, _)| range.start <= span.end);
        (self.before[first], &self.edits[first..last])
    }
}

#[cfg(test)]
thread_local! {
    /// What composing transactions and moving tracked positions visited, for
    /// tests that check undo's work grows with the edits, not their square.
    static WORK: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// An edit as a transaction's record keeps it, borrowed: in the text of
/// its time, `start..start + old.len()` held `old` and became `new`.
#[derive(Clone, Copy)]
struct Replaced<'a> {
    start: usize,
    old: &'a str,
    new: &'a str,
}

/// One edit of edits taken as a whole (see `compose`): `old`, a range of
/// the text before them holding `old_text`, became `new`, a range of the
/// text after them holding `new_text`.
struct Composed<'a> {
    old: Range<usize>,
    new: Range<usize>,
    old_text: Cow<'a, str>,
    new_text: Cow<'a, str>,
    /// Made of edits that overlapped.
    merged: bool,
}

impl Composed<'_> {
    fn change(&self) -> isize {
        self.new.len() as isize - self.old.len() as isize
    }
}

/// Edits made one after another (each in the text of its time: a
/// transaction's records, or them undone from the last) as one set of
/// edits, sorted and apart, to make in one go. Edits merge only where they
/// overlap (typed, then deleted again); letters typed one after another stay
/// apart, so what follows the text moves as through them one by one.
/// Edits that can't touch each other (one `edit`'s, at carets apart) are
/// one step; steps are composed in pairs, then pairs of pairs, each a merge
/// walk: the work grows with the edits times the log of the steps, never
/// with their square.
fn compose<'a>(edits: impl IntoIterator<Item = Replaced<'a>>) -> Vec<Composed<'a>> {
    let mut steps = Vec::new();
    let mut run: Vec<Replaced<'a>> = Vec::new();
    for edit in edits {
        // In the same text as the run's: before the last one (an `edit`'s
        // records, made from the last), or after the text it put there
        // (them undone, from the first).
        let ascending = run.len() > 1 && run[1].start > run[0].start;
        let fits = run.last().is_none_or(|last| {
            let before = edit.start + edit.old.len() < last.start;
            let after = edit.start > last.start + last.new.len();
            match run.len() {
                1 => before || after,
                _ if ascending => after,
                _ => before,
            }
        });
        if !fits {
            steps.push(step_of(std::mem::take(&mut run)));
        }
        run.push(edit);
    }
    if !run.is_empty() {
        steps.push(step_of(run));
    }
    while steps.len() > 1 {
        let mut paired = Vec::with_capacity(steps.len().div_ceil(2));
        let mut steps_left = steps.into_iter();
        while let Some(first) = steps_left.next() {
            paired.push(match steps_left.next() {
                Some(second) => compose_two(first, second),
                None => first,
            });
        }
        steps = paired;
    }
    steps.pop().unwrap_or_default()
}

/// Edits that can't touch each other, made from the last (each ending
/// before the one before it starts, so all in the text before them) or
/// from the first (each starting after the text the one before put
/// there), as composed edits.
fn step_of(mut run: Vec<Replaced<'_>>) -> Vec<Composed<'_>> {
    let ascending = run.len() > 1 && run[1].start > run[0].start;
    if !ascending {
        run.reverse();
    }
    // The length change of the edits before.
    let mut delta = 0;
    run.into_iter()
        .map(|edit| {
            #[cfg(test)]
            WORK.with(|work| work.set(work.get() + 1));
            let (old, new) = if ascending { (moved(edit.start, -delta), edit.start) } else { (edit.start, moved(edit.start, delta)) };
            delta += edit.new.len() as isize - edit.old.len() as isize;
            Composed {
                old: old..old + edit.old.len(),
                new: new..new + edit.new.len(),
                old_text: Cow::Borrowed(edit.old),
                new_text: Cow::Borrowed(edit.new),
                merged: false,
            }
        })
        .collect()
}

/// `first`, then `second` (whose old ranges are in the text `first` made),
/// as one: a merge walk along the text in between, where each edit is
/// either apart from the other side's (and only moves by the length changes
/// before it) or merges with the ones it can't be kept apart from, and
/// with what those can't, in turn.
fn compose_two<'a>(first: Vec<Composed<'a>>, second: Vec<Composed<'a>>) -> Vec<Composed<'a>> {
    // `y`, made later, shares text with `x` (or one is empty strictly
    // inside the other).
    let merges = |x: &Composed, y: &Composed| x.new.end > y.old.start && y.old.end > x.new.start;
    let mut out = Vec::with_capacity(first.len() + second.len());
    let (mut firsts, mut seconds) = (first.into_iter().peekable(), second.into_iter().peekable());
    // The length changes of the edits passed: `firsts`' lead from the text
    // before to the one between, `seconds`' from there to the text after.
    let (mut before, mut after) = (0, 0);
    loop {
        #[cfg(test)]
        WORK.with(|work| work.set(work.get() + 1));
        let (x, y) = match (firsts.peek(), seconds.peek()) {
            (None, None) => break,
            (Some(x), Some(y)) if merges(x, y) => (firsts.next(), seconds.next()),
            // Apart: whichever comes first; at a tie (both empty there),
            // the earlier one.
            (Some(x), Some(y)) if x.new.end <= y.old.start => (firsts.next(), None),
            (Some(_), None) => (firsts.next(), None),
            (_, Some(_)) => (None, seconds.next()),
        };
        let edit = match (x, y) {
            (Some(x), None) => {
                before += x.change();
                Composed { new: moved(x.new.start, after)..moved(x.new.end, after), ..x }
            }
            (None, Some(y)) => {
                after += y.change();
                Composed { old: moved(y.old.start, -before)..moved(y.old.end, -before), ..y }
            }
            (Some(x), Some(y)) => {
                // All that merges, in turn: an edit of either side can only
                // merge with the other side's last one so far.
                let (mut xs, mut ys) = (vec![x], vec![y]);
                loop {
                    #[cfg(test)]
                    WORK.with(|work| work.set(work.get() + 1));
                    if let Some(x) = firsts.next_if(|x| merges(x, &ys[ys.len() - 1])) {
                        xs.push(x);
                    } else if let Some(y) = seconds.next_if(|y| merges(&xs[xs.len() - 1], y)) {
                        ys.push(y);
                    } else {
                        break;
                    }
                }
                let edit = merge(&xs, &ys, before, after);
                before += xs.iter().map(Composed::change).sum::<isize>();
                after += ys.iter().map(Composed::change).sum::<isize>();
                edit
            }
            (None, None) => unreachable!("one side has an edit"),
        };
        out.push(edit);
    }
    out
}

/// Edits of `first` (`xs`) and `second` (`ys`) that merge in turn, as one;
/// `before` and `after` are the length changes of the edits before them.
/// The text between is `xs`' new text where they are, else `ys`' old text
/// (one of them covers every part of it).
fn merge<'a>(xs: &[Composed<'a>], ys: &[Composed<'a>], before: isize, after: isize) -> Composed<'a> {
    let (x0, xn, y0, yn) = (&xs[0], &xs[xs.len() - 1], &ys[0], &ys[ys.len() - 1]);
    let (lo, hi) = (x0.new.start.min(y0.old.start), xn.new.end.max(yn.old.end));
    let before_after = before + xs.iter().map(Composed::change).sum::<isize>();
    let after_after = after + ys.iter().map(Composed::change).sum::<isize>();
    let old_start = if x0.new.start <= y0.old.start { x0.old.start } else { moved(lo, -before) };
    let old_end = if xn.new.end >= yn.old.end { xn.old.end } else { moved(hi, -before_after) };
    let new_start = if y0.old.start <= x0.new.start { y0.new.start } else { moved(lo, after) };
    let new_end = if yn.old.end >= xn.new.end { yn.new.end } else { moved(hi, after_after) };
    // The text before: `xs`' old texts, and between them what `ys` replaced.
    let mut old_text = String::with_capacity(old_end - old_start);
    let mut between = Between { edits: ys, ix: 0, old: true };
    let mut at = lo;
    for x in xs {
        between.read(at..x.new.start, &mut old_text);
        old_text.push_str(&x.old_text);
        at = x.new.end;
    }
    between.read(at..hi, &mut old_text);
    // The text after: `ys`' new texts, and between them what `xs` put there.
    let mut new_text = String::with_capacity(new_end - new_start);
    let mut between = Between { edits: xs, ix: 0, old: false };
    at = lo;
    for y in ys {
        between.read(at..y.old.start, &mut new_text);
        new_text.push_str(&y.new_text);
        at = y.old.end;
    }
    between.read(at..hi, &mut new_text);
    debug_assert_eq!((old_text.len(), new_text.len()), (old_end - old_start, new_end - new_start));
    Composed { old: old_start..old_end, new: new_start..new_end, old_text: Cow::Owned(old_text), new_text: Cow::Owned(new_text), merged: true }
}

/// Reads parts of the text between two composed steps, in order, from the
/// edits (sorted) covering them there: `second`'s old side or `first`'s new.
struct Between<'e, 'a> {
    edits: &'e [Composed<'a>],
    ix: usize,
    old: bool,
}

impl Between<'_, '_> {
    fn read(&mut self, range: Range<usize>, out: &mut String) {
        let mut at = range.start;
        while at < range.end {
            let edit = &self.edits[self.ix];
            let (span, text) = if self.old { (&edit.old, &edit.old_text) } else { (&edit.new, &edit.new_text) };
            if span.end <= at {
                self.ix += 1;
                continue;
            }
            debug_assert!(span.start <= at, "{span:?} covers {at}");
            let end = span.end.min(range.end);
            out.push_str(&text[at - span.start..end - span.start]);
            at = end;
        }
    }
}

/// Line starts moved through edits one at a time, by their old offset;
/// None for a line an edit removed (see `Document::exact_line_starts`).
type ExactLines = std::collections::HashMap<usize, Option<usize>>;

/// Where the line starting at `offset` starts after `patch` (`old` is the
/// text before it, `new` the text after); None when the edits removed the
/// line: replaced text around its start, joined it with the line above,
/// or deleted it whole, line break included.
fn shift_line(new: &Rope, offset: usize, patch: &Patch, old: &Rope) -> Option<usize> {
    let line_start = |at: usize| new.line_to_byte(new.byte_to_line(at.min(new.len_bytes())));
    let (mut delta, near) = patch.around(offset..offset);
    for (range, new_len) in near {
        if range.start < offset && offset < range.end {
            return None;
        }
        if range.start == offset && offset < range.end && *new_len == 0 && old.byte_to_line(range.end) > old.byte_to_line(offset) {
            // Deleted from its start through its line break: the line
            // after takes its place, with its own things.
            return None;
        }
        if range.start < offset && offset == range.end {
            // Text before it went (lines above deleted, or its line
            // break): it's still a line if a line ends where that text did.
            let end = moved(range.start, delta) + new_len;
            if line_start(end) != end {
                return None;
            }
        }
        if offset >= range.end {
            delta += Patch::change(range, *new_len);
        }
    }
    // Typing at the start of a line leaves it where it is; a new line
    // inserted there pushes it down.
    Some(line_start(moved(offset, delta)))
}

/// Line starts (sorted, in `before`) moved through `edits` one at a time,
/// as if each were a version of its own: where edits touch or overlap,
/// which went first decides where a line start goes (Enter, then a letter
/// typed in front of the new line break, pushes the line down; the letter
/// first doesn't), which edits taken as a whole can't tell. Each edit
/// finds the line starts it reaches by binary search and moves the ones
/// after it at once, in a Fenwick tree of length changes, so it's the
/// edits times a log, not times the line starts.
fn line_starts_one_by_one(before: &Rope, edits: &[Replaced], starts: &[usize]) -> Vec<Option<usize>> {
    // `changes[i]` sums, Fenwick style, what's been added to line starts
    // `i..`: a start's offset is its first one plus the sum up to it.
    let mut changes = vec![0isize; starts.len() + 1];
    let add_from = |changes: &mut Vec<isize>, from: usize, delta: isize| {
        let mut i = from + 1;
        while i < changes.len() {
            changes[i] += delta;
            i += i & i.wrapping_neg();
        }
    };
    let offset = |changes: &[isize], ix: usize| {
        let (mut sum, mut i) = (0, ix + 1);
        while i > 0 {
            sum += changes[i];
            i -= i & i.wrapping_neg();
        }
        moved(starts[ix], sum)
    };
    let mut gone = vec![false; starts.len()];
    let mut text = before.clone();
    for edit in edits {
        let range = edit.start..edit.start + edit.old.len();
        // The line starts the edit reaches: offsets stay sorted.
        #[cfg(test)]
        WORK.with(|work| work.set(work.get() + 1 + starts.len().ilog2() as usize));
        let (mut lo, mut hi) = (0, starts.len());
        while lo < hi {
            let mid = (lo + hi) / 2;
            if offset(&changes, mid) < range.start {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        let mut end = lo;
        while end < starts.len() && offset(&changes, end) <= range.end {
            end += 1;
        }
        let old = text.clone();
        let (from, to) = (text.byte_to_char(range.start), text.byte_to_char(range.end));
        text.remove(from..to);
        text.insert(from, edit.new);
        let patch = Patch::new(vec![(range.clone(), edit.new.len())]);
        for ix in lo..end {
            #[cfg(test)]
            WORK.with(|work| work.set(work.get() + 1));
            if gone[ix] {
                continue;
            }
            let at = offset(&changes, ix);
            // A removed line keeps a place at the edit, so the order holds.
            let to = shift_line(&text, at, &patch, &old).unwrap_or_else(|| {
                gone[ix] = true;
                range.start
            });
            add_from(&mut changes, ix, to as isize - at as isize);
            add_from(&mut changes, ix + 1, at as isize - to as isize);
        }
        add_from(&mut changes, end, Patch::change(&range, edit.new.len()));
    }
    (0..starts.len()).map(|ix| (!gone[ix]).then(|| offset(&changes, ix))).collect()
}

/// How many of `rows` are longer than `wrap::LONG_LINE`.
fn long_lines(rope: &Rope, rows: Range<usize>) -> usize {
    let rows = rows.start.min(rope.len_lines())..rows.end.min(rope.len_lines());
    // One walk along the lines, not a lookup per row.
    rope.lines_at(rows.start).take(rows.len()).filter(|line| line.len_bytes() > crate::wrap::LONG_LINE).count()
}

fn moved(offset: usize, delta: isize) -> usize {
    (offset as isize + delta).max(0) as usize
}

/// Diagnostics keep roughly in place until the next lint: ones before or
/// after the edits move, ones they touch go.
fn shift_diagnostics(diagnostics: &mut Vec<Diagnostic>, patch: &Patch) {
    diagnostics.retain_mut(|d| {
        let (mut delta, near) = patch.around(d.range.clone());
        for (range, new_len) in near {
            if d.range.end <= range.start {
                // The edit is after it (an empty one stays put at an insertion).
            } else if d.range.start >= range.end {
                delta += Patch::change(range, *new_len);
            } else {
                return false;
            }
        }
        d.range = moved(d.range.start, delta)..moved(d.range.end, delta);
        true
    });
}

#[cfg(test)]
mod tests {
    #[test]
    fn code_lenses_follow_their_lines() {
        let mut doc = Document::from_text("fn a() {}\nfn b() {}\nfn c() {}\n".into(), None);
        let lens = |line_start: usize| CodeLens { id: 0, line_start, title: "1 reference".into(), command: Arc::new(serde_json::Value::Null), server: "s".into(), unresolved: None };
        doc.set_code_lenses(vec![lens(10), lens(20)], doc.lens_stamp());
        // A line above pushes them down; deleting a lens's line drops it.
        doc.edit(vec![(0..0, "// top\n".into())], &[], EditKind::Other);
        assert_eq!(doc.code_lenses().iter().map(|l| l.line_start).collect::<Vec<_>>(), [17, 27]);
        doc.edit(vec![(17..27, String::new())], &[], EditKind::Other);
        assert_eq!(doc.code_lenses().iter().map(|l| l.line_start).collect::<Vec<_>>(), [17]);
    }

    #[test]
    fn hints_come_by_chunk_and_lenses_are_resolved_by_id() {
        let mut doc = Document::from_text("a\nb\nc\nd\n".into(), None);
        let hint = |offset| InlayHint { offset, label: ": T".into(), pad_left: false, pad_right: false };
        let offsets = |doc: &Document| doc.inlay_hints().iter().map(|h| h.offset).collect::<Vec<_>>();
        assert!(!doc.hints_fresh(0));
        // Two chunks' answers, each only within its lines.
        doc.set_chunk_hints(0..4, 0..1, Some(vec![hint(1), hint(5)]));
        doc.set_chunk_hints(4..9, 1..2, Some(vec![hint(5)]));
        assert_eq!(offsets(&doc), [1, 5]);
        assert!(doc.hints_fresh(0) && doc.hints_fresh(1) && !doc.hints_fresh(2));
        // No answer: the hints there stay.
        doc.set_chunk_hints(0..4, 0..1, None);
        assert_eq!(offsets(&doc), [1, 5]);
        // An edit makes every chunk due; the hints follow it meanwhile.
        doc.edit(vec![(0..0, "x".into())], &[], EditKind::Other);
        assert!(!doc.hints_fresh(0) && !doc.hints_current());
        assert_eq!(offsets(&doc), [2, 6]);
        doc.set_chunk_hints(0..5, 0..1, Some(Vec::new()));
        assert_eq!(offsets(&doc), [6]);
        assert!(doc.hints_fresh(0) && !doc.hints_fresh(1) && doc.hints_current());
        // So does a server saying they changed.
        doc.hints_changed();
        assert!(!doc.hints_fresh(0) && doc.hints_current());

        let lens = |line_start| CodeLens {
            id: 0,
            line_start,
            title: "".into(),
            command: Arc::new(serde_json::Value::Null),
            server: "s".into(),
            unresolved: Some(Arc::new(serde_json::json!({ "data": line_start }))),
        };
        assert!(!doc.lenses_current());
        doc.set_code_lenses(vec![lens(5), lens(0)], doc.lens_stamp());
        assert!(doc.lenses_current());
        let ids: Vec<u64> = doc.code_lenses().iter().map(|l| l.id).collect();
        assert!(ids[0] != ids[1]);
        // Due again after a server's word, as after an edit.
        doc.lenses_changed();
        assert!(!doc.lenses_current());
        doc.resolve_code_lens(ids[1], Some(("1 reference".into(), serde_json::json!({ "title": "1 reference" }))));
        doc.resolve_code_lens(ids[0], None);
        let lenses = doc.code_lenses();
        assert_eq!((lenses.len(), &*lenses[0].title, lenses[0].unresolved.is_none()), (1, "1 reference", true));
    }

    use super::*;

    #[test]
    fn one_transaction_moves_things_like_its_edits_one_by_one() {
        let text = "fn a() {\n    one\n}\nfn b() {\n    two\n}\nlast line\n";
        let setup = || {
            let mut doc = Document::from_text(text.into(), None);
            let diagnostic = |range: Range<usize>| Diagnostic {
                range,
                severity: crate::lint::Severity::Warning,
                message: "m".into(),
                rule: "r".into(),
            };
            doc.set_builtin_diagnostics(vec![diagnostic(13..16), diagnostic(31..34), diagnostic(39..39)]);
            doc.set_lsp_diagnostics(vec![diagnostic(3..4), diagnostic(42..46)]);
            let hint = |offset| InlayHint { offset, label: ": T".into(), pad_left: false, pad_right: false };
            doc.set_inlay_hints(vec![hint(4), hint(16), hint(34)]);
            doc.set_breakpoints(&[Breakpoint::at(1), Breakpoint::at(4), Breakpoint::at(6)]);
            doc.set_debug_row(Some(4));
            doc
        };
        let snapshot = |doc: &Document| {
            (
                doc.rope().to_string(),
                doc.diagnostics.iter().map(|d| d.range.clone()).collect::<Vec<_>>(),
                doc.inlay_hints().iter().map(|h| h.offset).collect::<Vec<_>>(),
                doc.breakpoints().iter().map(|b| b.row).collect::<Vec<_>>(),
                doc.debug_row(),
            )
        };
        // Multi-caret edits: a new line at a line start, typing in a
        // diagnostic, deleting a hint's text, an insertion at the end.
        let edit_sets: [Vec<(Range<usize>, &str)>; 3] = [
            vec![(0..0, "// x\n"), (13..13, "z"), (27..27, "\n")],
            vec![(12..16, ""), (31..34, "three"), (46..46, "!")],
            vec![(9..9, "\n\n"), (22..23, ""), (39..39, "x")],
        ];
        for edits in edit_sets {
            let mut together = setup();
            together.edit(edits.iter().map(|(r, t)| (r.clone(), t.to_string())).collect(), &[], EditKind::Other);
            let mut one_by_one = setup();
            for (range, text) in edits.iter().rev() {
                one_by_one.edit(vec![(range.clone(), text.to_string())], &[], EditKind::Other);
            }
            assert_eq!(snapshot(&together), snapshot(&one_by_one), "{edits:?}");
            // The merged list was moved in place, and matches merging again.
            let merged = together.diagnostics.iter().map(|d| d.range.clone()).collect::<Vec<_>>();
            together.merge_diagnostics();
            assert_eq!(merged, together.diagnostics.iter().map(|d| d.range.clone()).collect::<Vec<_>>());
        }
    }

    /// A language server answers for the version it was sent; what it found
    /// is read in that text and moved to today's.
    #[test]
    fn results_for_an_older_version_move_to_todays_text() {
        let problem = |range: Range<usize>| Diagnostic { range, severity: crate::lint::Severity::Error, message: "m".into(), rule: "r".into() };
        let mut doc = Document::from_text("let a = 1;\nlet b = 2;\n".into(), None);
        doc.note_lsp_version();
        let sent = doc.version();
        // The user types above (and over a later spot) before the answer comes.
        doc.edit(vec![(0..0, "// new\n".into())], &[], EditKind::Other);
        doc.note_lsp_version();
        doc.edit(vec![(7 + 15..7 + 16, "x".into())], &[], EditKind::Other);
        doc.note_lsp_version();
        let old_text = doc.text_at_lsp_version(sent).unwrap().to_string();
        assert_eq!(old_text, "let a = 1;\nlet b = 2;\n");
        // `b` (15..16 then) was retyped: its problem goes; `a` moves down.
        let moved = doc.diagnostics_since(sent, vec![problem(4..5), problem(15..16)]).unwrap();
        assert_eq!(moved.iter().map(|d| d.range.clone()).collect::<Vec<_>>(), [11..12]);
        assert_eq!(doc.offset_since(sent, 4), Some(11));
        assert!(doc.text_at_lsp_version(999).is_none());

        // Each server's diagnostics stay apart: one publishing doesn't touch
        // the other's, and a stopped server's go.
        doc.set_server_diagnostics("ts", vec![problem(0..2)]);
        doc.set_server_diagnostics("eslint", vec![problem(3..4)]);
        doc.set_server_diagnostics("ts", vec![problem(5..6)]);
        assert_eq!(doc.diagnostics.iter().map(|d| d.range.clone()).collect::<Vec<_>>(), [3..4, 5..6]);
        doc.clear_server_diagnostics("eslint");
        assert_eq!(doc.diagnostics.iter().map(|d| d.range.clone()).collect::<Vec<_>>(), [5..6]);
    }

    /// A small random generator, the same run after run.
    fn xorshift(mut seed: u64) -> impl FnMut(usize) -> usize {
        move |n: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % n.max(1) as u64) as usize
        }
    }

    /// `text` with sorted, apart `edits` (a range of it, the new text) made.
    fn with_edits(text: &str, edits: impl IntoIterator<Item = (Range<usize>, String)>) -> String {
        let (mut out, mut at) = (String::new(), 0);
        for (range, new) in edits {
            assert!(range.start >= at && range.start <= range.end, "sorted and apart");
            out.push_str(&text[at..range.start]);
            out.push_str(&new);
            at = range.end;
        }
        out + &text[at..]
    }

    /// Random edits of many places at once, some overlapping, made into
    /// one transaction: composed, they turn the text before into the text
    /// after and back, and positions outside them map like the records one
    /// after another (as undo and redo moved them, record by record). Undo
    /// and redo take the whole transaction and log one patch.
    #[test]
    fn transactions_compose_into_edits_that_map_like_their_steps() {
        let mut next = xorshift(0x9e37_79b9_7f4a_7c15);
        let words = ["a", "bc", "\n", "é", "🦀", "", "", "xyz\n"];
        for round in 0..400 {
            let text: String = (0..next(30) + 1).map(|_| words[next(words.len())]).collect();
            let mut doc = Document::from_text(text.clone(), None);
            let (start, mark) = (doc.version(), doc.undo_mark());
            for _ in 0..next(6) + 1 {
                let edits: Vec<(Range<usize>, String)> = (0..next(4) + 1)
                    .map(|_| {
                        let at = doc.clip(next(doc.len() + 1));
                        let end = doc.clip(at + next(4)).max(at);
                        (at..end, words[next(words.len())].to_string())
                    })
                    .collect();
                doc.edit(edits, &[], EditKind::Other);
            }
            doc.merge_undo_since(mark);
            let after = doc.rope().to_string();
            let Some(t) = doc.undo_stack.last() else { continue };
            let composed = compose(t.edits.iter().map(|r| Replaced { start: r.start, old: &r.old, new: &r.new }));
            assert_eq!(with_edits(&text, composed.iter().map(|e| (e.old.clone(), e.new_text.to_string()))), after, "round {round}");
            assert_eq!(with_edits(&after, composed.iter().map(|e| (e.new.clone(), e.old_text.to_string()))), text, "round {round}");
            let patch = Patch::new(composed.iter().map(|e| (e.old.clone(), e.new.len())).collect());
            let records: Vec<Patch> = t.edits.iter().map(|r| Patch::new(vec![(r.start..r.start + r.old.len(), r.new.len())])).collect();
            // Where records overlap they merge, and a position at the edge
            // of the whole may land elsewhere than through them one by one.
            let merged = composed.iter().any(|e| e.merged);
            for offset in (0..=text.len()).filter(|&o| text.is_char_boundary(o)) {
                // Positions the edits didn't touch land in the same place.
                if !composed.iter().any(|e| e.old.start < offset && offset < e.old.end || merged && (e.old.start..=e.old.end).contains(&offset)) {
                    assert_eq!(patch.map(offset), records.iter().fold(offset, |o, p| p.map(o)), "round {round}, offset {offset}");
                }
            }
            assert!(doc.patches_since(start).is_some());

            let version = doc.version();
            doc.undo();
            assert_eq!(doc.rope().to_string(), text, "round {round}");
            assert_eq!(doc.patches_since(version).map(Iterator::count), Some(1), "one patch for the undo");
            doc.redo();
            assert_eq!(doc.rope().to_string(), after);
            doc.undo();
            assert_eq!(doc.rope().to_string(), text);
        }

        let mut doc = Document::from_text("x\n".repeat(700), None);
        let all: Vec<(Range<usize>, String)> = (0..700).map(|i| (i * 2..i * 2 + 1, "yy".into())).collect();
        doc.edit(all, &[], EditKind::Other);
        let before_undo = doc.version();
        doc.undo();
        assert!(doc.patches_since(before_undo).is_some_and(|mut p| p.next().is_some()), "the undo's patch is kept");
        // Line 1 started at 3 ("yy\n" above it), and at 2 again after the undo.
        assert_eq!(doc.offset_since(before_undo, 3), Some(2));
    }

    /// Undo and redo as they were: each record undone (the last first) or
    /// redone in turn, with what follows the text moved by each.
    fn each_record(doc: &mut Document, undo: bool) {
        let t = if undo { doc.undo_stack.pop() } else { doc.redo_stack.pop() }.unwrap();
        let records: Vec<(usize, &str, &str)> = match undo {
            true => t.edits.iter().rev().map(|r| (r.start, r.new.as_str(), r.old.as_str())).collect(),
            false => t.edits.iter().map(|r| (r.start, r.old.as_str(), r.new.as_str())).collect(),
        };
        for (start, old, new) in records {
            let before = doc.rope.clone();
            doc.raw_replace(start..start + old.len(), new);
            doc.shift_tracked(&Patch::new(vec![(start..start + old.len(), new.len())]), &before, &ExactLines::new());
        }
        doc.version += 1;
        doc.reparse();
        if undo { doc.redo_stack.push(t) } else { doc.undo_stack.push(t) }
    }

    /// Many carets typing (Enter too) or deleting, undone and redone at
    /// once: what follows the text lands where undoing or redoing the keys
    /// one by one put it. Mixed (Vim's insert mode: one undo step), where
    /// keys delete what others typed and those edits merge, the text still
    /// comes back and what follows it stays on lines that exist.
    #[test]
    fn undo_moves_things_like_undoing_each_key() {
        let mut next = xorshift(0x2545_f491_4f6c_dd1d);
        for round in 0..500 {
            let lines: Vec<String> = (0..next(8) + 2).map(|i| "ab cd".repeat(next(3)) + &i.to_string()).collect();
            let text = lines.join("\n") + "\n";
            let setup = || {
                let mut doc = Document::from_text(text.clone(), None);
                let rows = doc.line_count();
                doc.set_breakpoints(&(0..rows).filter(|r| r % 2 == 0).map(Breakpoint::at).collect::<Vec<_>>());
                doc.set_debug_row(Some(rows / 2));
                let diagnostic = |range: Range<usize>| Diagnostic { range, severity: crate::lint::Severity::Warning, message: "m".into(), rule: "r".into() };
                doc.set_lsp_diagnostics((0..rows).map(|r| doc.line_start(r)..doc.line_end(r)).map(diagnostic).collect());
                doc.set_inlay_hints((0..rows).map(|r| InlayHint { offset: doc.line_end(r), label: ": T".into(), pad_left: false, pad_right: false }).collect());
                let lens = |line_start| CodeLens { id: 0, line_start, title: "1 reference".into(), command: Arc::new(serde_json::Value::Null), server: "s".into(), unresolved: None };
                doc.set_code_lenses((0..rows).filter(|r| r % 3 == 0).map(|r| lens(doc.line_start(r))).collect(), doc.lens_stamp());
                doc
            };
            let mut carets: Vec<usize> = {
                let doc = setup();
                let mut carets: Vec<usize> = (0..next(4) + 1).map(|_| doc.clip(next(doc.len()))).collect();
                carets.sort();
                carets.dedup();
                carets
            };
            let (mut batch, mut one_by_one) = (setup(), setup());
            let mark = batch.undo_mark();
            // Typing, Backspace, Delete, or all of them.
            let mode = next(4);
            for _ in 0..next(8) + 1 {
                let keys = [&["x", "\n", "yz"][..], &["⌫"], &["⌦"], &["x", "\n", "yz", "⌫", "⌦"]][mode];
                let key = keys[next(keys.len())];
                let edits: Vec<(Range<usize>, String)> = carets
                    .iter()
                    .map(|&at| match key {
                        "⌫" => (batch.prev_char_boundary(at)..at, String::new()),
                        "⌦" => (at..batch.next_char_boundary(at), String::new()),
                        _ => (at..at, key.to_string()),
                    })
                    .collect();
                let kind = if key.len() == 1 { EditKind::Typing } else { EditKind::Deleting };
                let ranges = batch.edit(edits.clone(), &[], kind);
                one_by_one.edit(edits, &[], kind);
                carets = ranges.iter().map(|r| if key == "⌦" { r.start } else { r.end }).collect();
                carets.dedup();
            }
            batch.merge_undo_since(mark);
            one_by_one.merge_undo_since(mark);
            if batch.undo_mark() == mark {
                continue;
            }
            let after = batch.rope().to_string();
            let snapshot = |doc: &Document| {
                (
                    doc.rope().to_string(),
                    doc.diagnostics.iter().map(|d| d.range.clone()).collect::<Vec<_>>(),
                    doc.inlay_hints().iter().map(|h| h.offset).collect::<Vec<_>>(),
                    doc.breakpoints().iter().map(|b| b.row).collect::<Vec<_>>(),
                    doc.code_lenses().iter().map(|l| l.line_start).collect::<Vec<_>>(),
                    doc.debug_row(),
                )
            };
            for (undo, expected) in [(true, &text), (false, &after), (true, &text)] {
                if undo { batch.undo() } else { batch.redo() };
                each_record(&mut one_by_one, undo);
                assert_eq!(&batch.rope().to_string(), expected);
                let (same, expected) = (snapshot(&batch), snapshot(&one_by_one));
                if mode < 3 {
                    assert_eq!(same, expected, "round {round}, mode {mode}, undo {undo}: {text:?}");
                } else {
                    // Problems and hints come again soon anyway; lines are the user's.
                    assert_eq!((same.0, same.3, same.4, same.5), (expected.0, expected.3, expected.4, expected.5), "round {round}, undo {undo}: {text:?}");
                }
            }
        }
    }

    /// Undoing thousands of carets' typing does work in proportion to the
    /// edits and what follows the text: eight times the carets (and lines,
    /// breakpoints, problems, hints) is about eight times the work, where
    /// each key undone in turn, moving everything each time, made it 64.
    /// Typed where lines start, each breakpoint is moved key by key (see
    /// `line_starts_one_by_one`), and that grows by a log at most.
    #[test]
    fn undoing_many_carets_typing_takes_linear_work() {
        let work = |carets: usize, column: usize| {
            let mut doc = Document::from_text("let value = 1;\n".repeat(carets), Some("a.rs".into()));
            let rows = carets;
            doc.set_breakpoints(&(0..rows).map(Breakpoint::at).collect::<Vec<_>>());
            let diagnostic = |range: Range<usize>| Diagnostic { range, severity: crate::lint::Severity::Warning, message: "m".into(), rule: "r".into() };
            doc.set_lsp_diagnostics((0..rows).map(|r| doc.line_start(r) + 4..doc.line_start(r) + 9).map(diagnostic).collect());
            doc.set_inlay_hints((0..rows).map(|r| InlayHint { offset: doc.line_start(r) + 9, label: ": i32".into(), pad_left: false, pad_right: false }).collect());
            let text = doc.rope().to_string();
            // A caret after each `value` (or where each line starts), ten
            // keys typed there.
            let mut carets: Vec<usize> = (0..rows).map(|r| doc.line_start(r) + column).collect();
            let mark = doc.undo_mark();
            for key in "_renamed!!".chars() {
                let edits = carets.iter().map(|&at| (at..at, key.to_string())).collect();
                carets = doc.edit(edits, &[], EditKind::Typing).into_iter().map(|r| r.end).collect();
            }
            doc.merge_undo_since(mark);
            let version = doc.version();
            // Line edits logged from here (the typing filled the log).
            (doc.line_edits_base, doc.line_edits) = (version, Vec::new());
            WORK.with(|work| work.set(0));
            doc.undo();
            let work = WORK.with(|work| work.get());
            assert_eq!(doc.rope().to_string(), text);
            // A word typed at a caret is one replacement for the rope and trees.
            assert_eq!(doc.line_edits_since(version).map(Iterator::count), Some(carets.len()));
            assert_eq!(doc.breakpoints().iter().map(|b| doc.line_start(b.row)).collect::<Vec<_>>(), (0..rows).map(|r| doc.line_start(r)).collect::<Vec<_>>());
            assert_eq!(doc.diagnostics.len(), rows, "the problems apart from the carets stay");
            work
        };
        let (small, large) = (work(500, 9), work(4000, 9));
        assert!(large <= small * 9, "{small} for 500 carets, {large} for 4000");
        let (small, large) = (work(500, 0), work(4000, 0));
        assert!(large <= small * 12, "{small} for 500 carets, {large} for 4000, at line starts");
    }

    #[test]
    fn the_debug_line_follows_edits() {
        let mut doc = Document::from_text("a\nb\nc\n".into(), None);
        doc.set_debug_row(Some(1));
        doc.edit(vec![(0..0, "new\n".into())], &[], EditKind::Other);
        assert_eq!(doc.debug_row(), Some(2));
        doc.undo();
        assert_eq!(doc.debug_row(), Some(1));
    }

    #[test]
    fn breakpoints_follow_their_lines() {
        let mut doc = Document::from_text("a\nb\nc\nd\n".into(), None);
        let rows = |doc: &Document| doc.breakpoints().iter().map(|b| b.row).collect::<Vec<_>>();
        assert!(doc.toggle_breakpoint(2));
        assert_eq!(rows(&doc), [2]);
        // A line added above moves it down (with its condition); typing at its start doesn't.
        doc.set_breakpoint(Breakpoint { condition: Some("x > 1".into()), ..Breakpoint::at(2) });
        doc.edit(vec![(0..0, "new\n".into())], &[], EditKind::Other);
        assert_eq!(doc.breakpoint(3).and_then(|b| b.condition.as_deref()), Some("x > 1"));
        let c = doc.line_start(3);
        doc.edit(vec![(c..c, "x".into())], &[], EditKind::Other);
        assert_eq!(rows(&doc), [3]);
        // Joining it with the line above removes it.
        doc.edit(vec![(c - 1..c, String::new())], &[], EditKind::Other);
        assert!(doc.breakpoints().is_empty());
        doc.set_breakpoints(&[Breakpoint::at(0), Breakpoint::at(9)]);
        assert_eq!(rows(&doc), [0]);
        assert!(!doc.toggle_breakpoint(0));
    }

    /// An editor's folds follow the document's edits (through its patches).
    #[test]
    fn folds_follow_edits() {
        let mut doc = Document::from_text("a {\n  b\n  c\n}\nz\n".into(), None);
        let mut folds = crate::fold::Folds::default();
        folds.fold_rows(&doc, 1, 3);
        assert_eq!(folds.ranges(), [4..13]);
        let edit = |doc: &mut Document, folds: &mut crate::fold::Folds, at: usize, text: &str| {
            let version = doc.version();
            doc.edit(vec![(at..at, text.into())], &[], EditKind::Other);
            folds.follow(doc, version);
        };
        // An edit above moves the fold.
        edit(&mut doc, &mut folds, 0, "x");
        assert_eq!(folds.ranges(), [5..14]);
        // An edit after it leaves it alone; one inside opens it.
        edit(&mut doc, &mut folds, 15, "y");
        assert_eq!(folds.ranges(), [5..14]);
        edit(&mut doc, &mut folds, 8, "!");
        assert!(folds.is_empty());
    }

    #[test]
    fn multi_edit_and_undo() {
        let mut doc = Document::from_text("abc\ndef".into(), None);
        let r = doc.edit(
            vec![(4..4, "X".into()), (0..1, "".into())],
            &[],
            EditKind::Other,
        );
        assert_eq!(doc.rope().to_string(), "bc\nXdef");
        assert_eq!(r, vec![3..4, 0..0]);
        doc.undo();
        assert_eq!(doc.rope().to_string(), "abc\ndef");
        doc.redo();
        assert_eq!(doc.rope().to_string(), "bc\nXdef");
    }

    #[test]
    fn a_parse_that_runs_out_of_time_finishes_in_the_background() {
        let text: String = (0..3000).map(|i| format!("fn f{i}() {{ let x = {i}; }}\n")).collect();
        let mut doc = Document::with_text(text, Some("big.rs".into()), false, Some(Duration::ZERO));
        doc.parse_budget = Duration::ZERO;
        assert!(doc.syntax().tree().is_none(), "opening ran out of time");
        let (opened, job) = doc.parse_job().expect("behind");
        let parsed_on_open = job();

        // An edit while that runs: its own parse runs out of time too.
        let tree_version = doc.tree_version();
        doc.edit(vec![(3..5, "renamed".into())], &[], EditKind::Other);
        assert!(doc.tree_version() > tree_version);
        assert!(doc.finish_parse(opened, parsed_on_open), "still behind the edit");
        let root = doc.syntax().tree().unwrap().root_node();
        assert_eq!(root.child(0).unwrap().byte_range(), 0..27, "the tree took the edit: {}", root.child(0).unwrap().to_sexp());

        let (version, job) = doc.parse_job().expect("behind");
        assert!(!doc.finish_parse(version, job()));
        let fresh = Syntax::new(Language::Rust, doc.rope());
        assert_eq!(doc.syntax().tree().unwrap().root_node().to_sexp(), fresh.tree().unwrap().root_node().to_sexp());
        // An old result arriving late changes nothing.
        assert!(!doc.finish_parse(opened, Syntax::new(Language::Rust, &Rope::from_str("fn old() {}"))));
        assert_eq!(doc.syntax().tree().unwrap().root_node().to_sexp(), fresh.tree().unwrap().root_node().to_sexp());
    }

    /// What the syntax check reads: the document's own tree when it's
    /// parsed for the text, else a parse from it of the text as it is.
    #[test]
    fn the_tree_to_lint_is_the_documents_own() {
        let text: String = (0..300).map(|i| format!("fn f{i}() {{ let x = {i}; }}\n")).collect();
        let mut doc = Document::from_text(text, Some("a.rs".into()));
        let tree = doc.tree_job()().unwrap();
        let first = |tree: &tree_sitter::Tree| tree.root_node().child(0).unwrap().id();
        assert_eq!(first(&tree), first(doc.syntax().tree().unwrap()), "the same tree, not parsed again");
        doc.parse_budget = Duration::ZERO;
        doc.edit(vec![(3..5, "renamed(".into())], &[], EditKind::Other);
        assert!(doc.parse_job().is_some(), "the edit's parse ran out of time");
        let tree = doc.tree_job()().unwrap();
        assert!(tree.root_node().has_error());
        assert_eq!(tree.root_node().to_sexp(), Syntax::new(Language::Rust, doc.rope()).tree().unwrap().root_node().to_sexp());
    }

    #[test]
    fn large_files_are_never_parsed() {
        let text = "{\"a\": 1}\n".repeat(10);
        let doc = Document::with_text(text.clone(), Some("big.json".into()), true, None);
        assert!(doc.large && doc.language == Language::PlainText);
        assert!(doc.syntax().tree().is_none());
        let mut doc = Document::with_text(text, Some("big.json".into()), true, None);
        doc.set_path("renamed.rs".into());
        assert!(doc.language == Language::PlainText && doc.syntax().tree().is_none());
        // A small one is.
        let doc = Document::from_text("{}".into(), Some("small.json".into()));
        assert!(doc.syntax().tree().is_some());
    }

    #[test]
    fn edits_that_change_nothing_still_get_their_range() {
        // Delete with carets at 3 and at the end: the second deletes nothing.
        let mut doc = Document::from_text("abc\ndef".into(), None);
        let r = doc.edit(vec![(3..4, String::new()), (7..7, String::new())], &[], EditKind::Deleting);
        assert_eq!(doc.rope().to_string(), "abcdef");
        assert_eq!(r, vec![3..3, 6..6]);

        // Cut with an empty selection between two non-empty ones.
        let mut doc = Document::from_text("one two three".into(), None);
        let r = doc.edit(
            vec![(8..13, String::new()), (0..3, String::new()), (5..5, String::new())],
            &[],
            EditKind::Other,
        );
        assert_eq!(doc.rope().to_string(), " two ");
        assert_eq!(r, vec![5..5, 0..0, 2..2]);
    }

    #[test]
    fn overlapping_edits_are_merged() {
        // Backspace with carets at 1 and 2 in "()a": the first deletes the
        // pair 0..2, the second 1..2. The "a" must survive.
        let mut doc = Document::from_text("()a".into(), None);
        let r = doc.edit(vec![(0..2, String::new()), (1..2, String::new())], &[], EditKind::Deleting);
        assert_eq!(doc.rope().to_string(), "a");
        assert_eq!(r, vec![0..0, 0..0]);
        doc.undo();
        assert_eq!(doc.rope().to_string(), "()a");

        // Overlapping replacements keep both texts, in order.
        let mut doc = Document::from_text("abcdef".into(), None);
        let r = doc.edit(vec![(2..5, "Y".into()), (1..3, "X".into())], &[], EditKind::Other);
        assert_eq!(doc.rope().to_string(), "aXYf");
        assert_eq!(r, vec![2..3, 1..2]);
    }

    #[test]
    fn notices_changes_made_by_other_programs() {
        let dir = std::env::temp_dir().join(format!("kuana-doc-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.txt");
        std::fs::write(&path, "one").unwrap();
        let mut doc = Document::open(&path).unwrap();
        assert!(!doc.changed_on_disk());

        doc.edit(vec![(3..3, "!".into())], &[], EditKind::Other);
        doc.save().unwrap();
        assert!(!doc.changed_on_disk(), "our own save isn't an outside change");

        let later = std::time::SystemTime::now() + Duration::from_secs(5);
        std::fs::File::options().write(true).open(&path).unwrap().set_modified(later).unwrap();
        assert!(doc.changed_on_disk());
        reload(&mut doc).unwrap();
        assert!(!doc.changed_on_disk());

        // Without a newer modification time the file isn't read again.
        let same = std::fs::metadata(&path).unwrap().modified().unwrap();
        std::fs::write(&path, "other").unwrap();
        std::fs::File::options().write(true).open(&path).unwrap().set_modified(same).unwrap();
        assert!(!reload(&mut doc).unwrap());
        assert_eq!(doc.rope().to_string(), "one!");
        std::fs::File::options().write(true).open(&path).unwrap().set_modified(later + Duration::from_secs(5)).unwrap();
        assert!(reload(&mut doc).unwrap());
        assert_eq!(doc.rope().to_string(), "other");

        // Read while the text changed: dropped, and the file still counts as
        // changed (auto-save leaves it alone).
        std::fs::write(&path, "third").unwrap();
        std::fs::File::options().write(true).open(&path).unwrap().set_modified(later + Duration::from_secs(10)).unwrap();
        let read = doc.reload_job().unwrap()().unwrap().unwrap();
        doc.edit(vec![(0..0, "typed ".into())], &[], EditKind::Typing);
        assert!(!doc.apply_reload(read, &[]));
        assert_eq!(doc.rope().to_string(), "typed other");
        assert!(doc.changed_on_disk());
        // Unsaved edits are never replaced, even read for this version.
        let read = doc.reload_job().unwrap()().unwrap().unwrap();
        assert!(!doc.apply_reload(read, &[]));
        assert_eq!(doc.rope().to_string(), "typed other");
        // Nor what was saved while the file was read.
        std::fs::File::options().write(true).open(&path).unwrap().set_modified(later + Duration::from_secs(15)).unwrap();
        doc.saved_at = Some(doc.history_position());
        let read = doc.reload_job().unwrap()().unwrap().unwrap();
        doc.save().unwrap();
        assert!(!doc.apply_reload(read, &[]));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "typed other");

        std::fs::remove_file(&path).unwrap();
        assert!(doc.changed_on_disk(), "a deleted file counts as changed");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A save taken now and written later (the background): the file gets
    /// the text as it was taken, the document counts as saved as of that
    /// version once it's taken back in, and its own write isn't a change
    /// made by another program. A newer save overtakes an older one.
    #[test]
    fn saves_written_in_the_background() {
        let dir = std::env::temp_dir().join(format!("kuana-doc-saves-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.txt");
        std::fs::write(&path, "one\n").unwrap();
        let mut doc = Document::open(&path).unwrap();
        doc.edit(vec![(3..3, " two".into())], &[], EditKind::Other);
        let job = doc.save_job().unwrap();
        // Typed while it's written: still unsaved after, and the write isn't
        // taken for another program's.
        doc.edit(vec![(0..0, "zero ".into())], &[], EditKind::Typing);
        assert!(doc.is_dirty() && !doc.is_being_saved());
        let written = std::thread::spawn(move || job.write()).join().unwrap();
        assert!(!doc.changed_on_disk(), "its own write, not taken in yet");
        let read = doc.reload_job().unwrap()().unwrap().unwrap();
        assert!(!doc.apply_reload(read, &[]), "not while a save is being written");
        doc.saved(written).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "one two\n");
        assert!(doc.is_dirty() && !doc.changed_on_disk());
        assert!(reload(&mut doc).is_ok_and(|changed| !changed), "the file's time is known");

        // Two saves at once: the newer one's text stays, whichever is written first.
        let older = doc.save_job().unwrap();
        doc.edit(vec![(0..0, "<".into())], &[], EditKind::Other);
        let newer = doc.save_job().unwrap();
        assert!(doc.is_being_saved());
        let newer = newer.write();
        let older = older.write();
        doc.saved(newer).unwrap();
        doc.saved(older).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "<zero one two\n");
        assert!(!doc.is_dirty() && !doc.is_being_saved());

        // Line ends and encoding as the file has them, chunk by chunk; one
        // changed while a save is written still needs a save.
        doc.set_crlf(true);
        doc.convert_to(TextEncoding::from_key("UTF-16LE").unwrap());
        let job = doc.save_job().unwrap();
        doc.set_crlf(false);
        doc.saved(job.write()).unwrap();
        assert!(doc.is_dirty());
        let bytes = std::fs::read(&path).unwrap();
        let units: Vec<u16> = bytes[2..].chunks(2).map(|b| u16::from_le_bytes([b[0], b[1]])).collect();
        assert_eq!((&bytes[..2], String::from_utf16(&units).unwrap()), (&[0xFF, 0xFE][..], "<zero one two\r\n".to_string()));
        let mut big = Document::from_text("line\n".repeat(50_000), Some(dir.join("big.txt")));
        big.set_crlf(true);
        big.save().unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("big.txt")).unwrap(), "line\r\n".repeat(50_000));
        // A character the encoding lacks fails before the file is touched.
        let mut latin = Document::from_text("ok\n".into(), Some(dir.join("latin.txt")));
        latin.save().unwrap();
        latin.convert_to(TextEncoding::from_key("windows-1252").unwrap());
        latin.edit(vec![(0..0, "🦀".into())], &[], EditKind::Other);
        assert!(latin.save().is_err());
        assert_eq!(std::fs::read_to_string(dir.join("latin.txt")).unwrap(), "ok\n");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// What the editor does in the background, here and now: true when
    /// the text changed.
    fn reload(doc: &mut Document) -> std::io::Result<bool> {
        let Some(read) = doc.reload_job().expect("a file")()? else { return Ok(false) };
        Ok(doc.apply_reload(read, &[]))
    }

    /// Takes in `text` as if the file had changed to it.
    fn reload_to(doc: &mut Document, text: &str, selections: &[Selection]) -> bool {
        let changes = Changes::between(&doc.rope().to_string(), text);
        let reload = Reload { id: doc.id(), version: doc.version(), known: doc.disk_mtime, encoding: doc.encoding, mtime: None, changes };
        doc.apply_reload(reload, selections)
    }

    /// Thousands of changed lines are one edit for the trees and undo, but
    /// what follows the text still moves through each change, and soft
    /// wrap measures only the changed lines.
    #[test]
    fn a_reload_of_many_changes_is_one_edit_that_moves_things_like_many() {
        let line = |i: usize, new: bool| format!("let x{i} = {};\n", if new && i.is_multiple_of(2) { i + 1 } else { i });
        let old: String = (0..5000).map(|i| line(i, false)).collect();
        let new: String = (0..5000).map(|i| line(i, true)).collect();
        let mut doc = Document::from_text(old.clone(), Some("a.rs".into()));
        doc.set_breakpoints(&[Breakpoint::at(1), Breakpoint::at(4001)]);
        let name = |doc: &Document, row: usize| doc.line_start(row) + 4..doc.line_start(row) + 6;
        let problem = Diagnostic { range: name(&doc, 3001), severity: crate::lint::Severity::Warning, message: "m".into(), rule: "r".into() };
        doc.set_lsp_diagnostics(vec![problem]);
        let version = doc.version();
        assert!(reload_to(&mut doc, &new, &[]));
        assert_eq!(doc.rope().to_string(), new);
        assert_eq!(doc.undo_stack.last().map(|t| t.edits.len()), Some(1), "one edit");
        assert_eq!(doc.breakpoints().iter().map(|b| b.row).collect::<Vec<_>>(), [1, 4001]);
        assert_eq!(doc.diagnostics.iter().map(|d| d.range.clone()).collect::<Vec<_>>(), [name(&doc, 3001)]);
        let lines: Vec<LineEdit> = doc.line_edits_since(version).unwrap().copied().collect();
        assert!(lines.len() == 2500 && lines.iter().all(|l| l.old_rows == 1 && l.new_rows == 1));
        doc.undo();
        assert_eq!(doc.rope().to_string(), old);
    }

    /// A reload is an edit like any other: undo brings back the text before
    /// it (unsaved then), redo the file's.
    #[test]
    fn a_reload_can_be_undone() {
        let mut doc = Document::from_text("fn a() {\n    one();\n}\n".into(), Some("a.rs".into()));
        doc.edit(vec![(13..16, "two".into())], &[Selection::cursor(13)], EditKind::Other);
        // Saved, and the server told.
        doc.saved_at = Some(doc.history_position());
        doc.take_lsp_changes();
        let caret = [Selection::cursor(16)];
        assert!(reload_to(&mut doc, "use x;\nfn a() {\n    two(1);\n}\n", &caret));
        assert!(!doc.is_dirty());
        assert!(doc.syntax().tree().is_some_and(|t| !t.root_node().has_error()), "parsed again from the edited tree");
        // The edits reach language servers as edits.
        let sent = doc.take_lsp_changes();
        assert!(!sent.full && sent.changes.len() == 2, "{sent:?}");

        assert_eq!(doc.undo(), Some(caret.to_vec()));
        assert_eq!(doc.rope().to_string(), "fn a() {\n    two();\n}\n");
        assert!(doc.is_dirty(), "the file has the reloaded text");
        // The edit before the reload is still there to undo.
        doc.undo();
        assert_eq!(doc.rope().to_string(), "fn a() {\n    one();\n}\n");
        doc.redo();
        doc.redo();
        assert_eq!(doc.rope().to_string(), "use x;\nfn a() {\n    two(1);\n}\n");
        // Typing after a reload is an undo step of its own.
        doc.edit(vec![(0..0, "x".into())], &[], EditKind::Typing);
        doc.undo();
        assert_eq!(doc.rope().to_string(), "use x;\nfn a() {\n    two(1);\n}\n");
    }

    /// Undone back to the text saved, a document is saved again (and the
    /// file may reload it); redone, unsaved again. Typing that joins the
    /// undo step saved last still counts as unsaved.
    #[test]
    fn undoing_back_to_the_saved_text_is_saved() {
        let dir = std::env::temp_dir().join(format!("kuana-doc-undo-saved-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.txt");
        std::fs::write(&path, "one\n").unwrap();
        let mut doc = Document::open(&path).unwrap();
        doc.edit(vec![(3..3, " two".into())], &[], EditKind::Other);
        assert!(doc.is_dirty());
        doc.undo();
        assert!(!doc.is_dirty());
        doc.redo();
        assert!(doc.is_dirty());
        doc.undo();
        // Saved again, so a change on disk reloads it.
        assert!(reload_to(&mut doc, "one\nfrom outside\n", &[]));
        assert!(!doc.is_dirty());

        // Saved mid-word: the next letter joins the word's undo step.
        doc.edit(vec![(0..0, "x".into())], &[], EditKind::Typing);
        doc.save().unwrap();
        assert!(!doc.is_dirty());
        doc.edit(vec![(1..1, "y".into())], &[], EditKind::Typing);
        assert!(doc.is_dirty());
        doc.undo();
        assert_eq!(doc.rope().to_string(), "one\nfrom outside\n");
        assert!(doc.is_dirty(), "the file has the x");
        doc.redo();
        assert!(doc.is_dirty());
        // A new encoding needs a save whatever the undo history says.
        doc.save().unwrap();
        doc.convert_to(TextEncoding::from_key("windows-1251").unwrap());
        doc.edit(vec![(0..0, "z".into())], &[], EditKind::Other);
        doc.undo();
        assert!(doc.is_dirty());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// `old` with `changes` made, checking they're sorted and apart.
    fn apply_changes(old: &str, new: &str, changes: &[(Range<usize>, Range<usize>)]) -> String {
        let (mut text, mut at) = (String::new(), 0);
        for (range, by) in changes {
            assert!(range.start >= at && range.start <= range.end, "sorted and apart: {changes:?}");
            text.push_str(&old[at..range.start]);
            text.push_str(&new[by.clone()]);
            at = range.end;
        }
        text + &old[at..]
    }

    #[test]
    fn changed_ranges_turn_the_old_text_into_the_new() {
        let cases: [(&str, &str, &[(Range<usize>, Range<usize>)]); 11] = [
            // A line added, one removed: whole lines.
            ("a\nc\n", "a\nb\nc\n", &[(2..2, 2..4)]),
            ("a\nb\nc\n", "a\nc\n", &[(2..4, 2..2)]),
            // A changed line, only between what it keeps at its ends.
            ("let x = 1;\n", "let x = 22;\n", &[(8..9, 8..10)]),
            // Every line indented: each keeps its code.
            ("a();\nb();\n", "  a();\n  b();\n", &[(0..0, 0..2), (5..5, 7..9)]),
            // A line added above a changed one: the changed one pairs with its new self.
            ("    foo();\n", "    // c\n    foo(1);\n", &[(0..0, 0..9), (8..8, 17..18)]),
            // Lines wrapped in a block.
            ("foo();\nbar();\n", "if x {\n    foo();\n    bar();\n}\n", &[(0..0, 0..7), (0..0, 7..11), (7..7, 18..22), (14..14, 29..31)]),
            // Whole characters only.
            ("aé\n", "a©\n", &[(1..3, 1..3)]),
            // The last line without a line break, and with one.
            ("a\nb", "a\nb\n", &[(3..3, 3..4)]),
            ("a\nb\n", "", &[(0..4, 0..0)]),
            ("", "a\n", &[(0..0, 0..2)]),
            ("same\n", "same\n", &[]),
        ];
        for (old, new, expected) in cases {
            let changes = changed_ranges(old, new);
            assert_eq!(changes, expected, "{old:?} → {new:?}");
            assert_eq!(apply_changes(old, new, &changes), new);
        }

        // Random edits of random lines.
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut next = |n: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % n as u64) as usize
        };
        let words = ["fn", "x", "    ", "é", "{", "}", "\n", "\n", "\n", "let", "🦀", ""];
        for _ in 0..500 {
            let old: String = (0..next(40)).map(|_| words[next(words.len())]).collect();
            let mut new = old.clone();
            for _ in 0..next(5) {
                let at = new.char_indices().map(|(i, _)| i).nth(next(new.chars().count() + 1)).unwrap_or(new.len());
                let end = new[at..].char_indices().map(|(i, _)| at + i).nth(next(4)).unwrap_or(new.len());
                new.replace_range(at..end, words[next(words.len())]);
            }
            let changes = changed_ranges(&old, &new);
            assert_eq!(apply_changes(&old, &new, &changes), new, "{old:?} → {new:?}: {changes:?}");
        }
    }

    #[test]
    fn a_reload_moves_what_follows_the_text() {
        let old = "fn a() {\n    one();\n    gone();\n}\nfn b() {\n    two();\n}\n";
        // Lines added at the top, `one();` changed, `gone();` removed.
        let new = "use std::fmt;\n\nfn a() {\n    one(1);\n}\nfn b() {\n    two();\n}\n";
        let mut doc = Document::from_text(old.into(), Some("a.rs".into()));
        let diagnostic = |range: Range<usize>| Diagnostic {
            range,
            severity: crate::lint::Severity::Warning,
            message: "m".into(),
            rule: "r".into(),
        };
        // On `one`, on `one`'s `()`, on `gone`, on `two`.
        doc.set_lsp_diagnostics(vec![diagnostic(13..16), diagnostic(16..18), diagnostic(24..28), diagnostic(47..50)]);
        let hint = |offset| InlayHint { offset, label: ": T".into(), pad_left: false, pad_right: false };
        doc.set_inlay_hints(vec![hint(16), hint(28), hint(50)]);
        let lens = |line_start| CodeLens { id: 0, line_start, title: "1 reference".into(), command: Arc::new(serde_json::Value::Null), server: "s".into(), unresolved: None };
        doc.set_code_lenses(vec![lens(0), lens(34)], doc.lens_stamp());
        let condition = Some("x > 1".to_string());
        doc.set_breakpoints(&[Breakpoint::at(1), Breakpoint::at(2), Breakpoint::at(3), Breakpoint { condition: condition.clone(), ..Breakpoint::at(5) }]);
        doc.set_debug_row(Some(5));
        let mut folds = crate::fold::Folds::default();
        folds.fold_rows(&doc, 1, 3);
        folds.fold_rows(&doc, 5, 6);
        doc.take_lsp_changes();
        let version = doc.version();

        assert!(reload_to(&mut doc, new, &[]));
        assert_eq!(doc.rope().to_string(), new);
        // Things on lines that stayed follow them; ones in changed text go.
        let ranges = doc.diagnostics.iter().map(|d| d.range.clone()).collect::<Vec<_>>();
        assert_eq!(ranges, [28..31, 51..54]);
        assert_eq!(doc.inlay_hints().iter().map(|h| h.offset).collect::<Vec<_>>(), [31, 54]);
        assert_eq!(doc.code_lenses().iter().map(|l| doc.row_of(l.line_start)).collect::<Vec<_>>(), [2, 5]);
        // `gone();`'s breakpoint went with it; the changed line kept its own.
        let rows = doc.breakpoints().iter().map(|b| b.row).collect::<Vec<_>>();
        assert_eq!(rows, [3, 4, 6]);
        assert_eq!(doc.breakpoint(6).and_then(|b| b.condition.clone()), condition);
        assert_eq!(doc.debug_row(), Some(6));
        // The fold over changed lines opened; the other moved.
        folds.follow(&doc, version);
        assert_eq!(folds.ranges(), [doc.line_start(6)..doc.line_end(7)]);

        // Other editors' carets follow too: `two` and a caret in `gone();`.
        let patches = doc.patches_since(version).expect("kept").collect::<Vec<_>>();
        let map = |offset: usize| patches.iter().fold(offset, |offset, patch| patch.map(offset));
        assert_eq!((map(47), map(25)), (51, doc.line_start(4)));
        // Soft wrap re-measures only the changed lines: untouched ones keep their rows.
        let mut lines: Vec<Option<usize>> = (0..Rope::from_str(old).len_lines()).map(Some).collect();
        for edit in doc.line_edits_since(version).expect("kept") {
            lines.splice(edit.row..edit.row + edit.old_rows, std::iter::repeat_n(None, edit.new_rows));
        }
        assert_eq!(lines.len(), doc.line_count());
        assert_eq!(lines[5..], [Some(4), Some(5), Some(6), Some(7)]);
        // Saved; the server gets the edits; undo brings back the text before.
        assert!(!doc.is_dirty());
        assert_eq!(doc.take_lsp_changes().changes.len(), 3);
        doc.undo();
        assert_eq!(doc.rope().to_string(), old);
    }

    #[test]
    fn a_reload_that_reindents_every_line_keeps_things_on_their_lines() {
        let mut doc = Document::from_text("a();\nb();\nc();\n".into(), None);
        doc.set_breakpoints(&[Breakpoint::at(0), Breakpoint::at(1), Breakpoint::at(2)]);
        doc.set_lsp_diagnostics(vec![Diagnostic { range: 5..6, severity: crate::lint::Severity::Error, message: "m".into(), rule: "r".into() }]);
        let version = doc.version();
        assert!(reload_to(&mut doc, "    a();\n    b();\n    c();\n", &[]));
        assert_eq!(doc.breakpoints().iter().map(|b| b.row).collect::<Vec<_>>(), [0, 1, 2]);
        assert_eq!(doc.diagnostics.iter().map(|d| d.range.clone()).collect::<Vec<_>>(), [13..14]);
        let patch = doc.patches_since(version).unwrap().next().unwrap();
        assert_eq!(patch.map(6), 14);
    }

    #[test]
    fn deleting_whole_lines_keeps_the_breakpoints_below() {
        let mut doc = Document::from_text("a\nb\nc\n\nd\n".into(), None);
        let rows = |doc: &Document| doc.breakpoints().iter().map(|b| b.row).collect::<Vec<_>>();
        doc.set_breakpoints(&[Breakpoint::at(1), Breakpoint { condition: Some("x".into()), ..Breakpoint::at(2) }, Breakpoint::at(4)]);
        // Delete Line on `b`: its breakpoint goes, the next line's moves up with it.
        doc.edit(vec![(2..4, String::new())], &[], EditKind::Other);
        assert_eq!(rows(&doc), [1, 3]);
        assert_eq!(doc.breakpoint(1).and_then(|b| b.condition.as_deref()), Some("x"));
        // Backspace at the start of `d` removes the empty line above it, not `d`.
        let d = doc.line_start(3);
        doc.edit(vec![(d - 1..d, String::new())], &[], EditKind::Deleting);
        assert_eq!(rows(&doc), [1, 2]);
        doc.undo();
        assert_eq!(rows(&doc), [1, 3]);
    }

    #[test]
    fn tells_whether_a_file_reads_as_some_text() {
        let dir = std::env::temp_dir().join(format!("kuana-doc-has-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.txt");
        std::fs::write(&path, "one\r\ntwo\r\n").unwrap();
        assert!(Document::file_has(&path, "one\ntwo\n", None));
        assert!(!Document::file_has(&path, "one\ntwo", None));
        std::fs::write(&path, [0xEF, 0xF2]).unwrap();
        assert!(Document::file_has(&path, "пт", TextEncoding::from_key("windows-1251")));
        assert!(!Document::file_has(&dir.join("gone.txt"), "", None));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn binary_files_open_as_text_only_when_asked() {
        let dir = std::env::temp_dir().join(format!("kuana-doc-binary-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.zip");
        std::fs::write(&path, b"PK\x03\x04\x00\x00zipped").unwrap();
        let Err(err) = Document::open(&path) else { panic!("opened a binary file as text") };
        assert!(BinaryFile::is(&err));
        assert_eq!(Document::open_anyway(&path).unwrap().len(), 12);
        // UTF-16 text has NULs, but its byte order mark says it's text.
        std::fs::write(&path, b"\xff\xfeh\x00i\x00").unwrap();
        assert_eq!(Document::open(&path).unwrap().rope().to_string(), "hi");
        // As in Git, only the start counts.
        let mut late = vec![b'a'; 9000];
        late.push(0);
        std::fs::write(&path, &late).unwrap();
        assert_eq!(Document::open(&path).unwrap().len(), 9001);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn lines_end_only_at_line_feeds() {
        use crate::lsp::protocol::{Encoding, Position, offset_to_position, position_to_offset};
        let dir = std::env::temp_dir().join(format!("kuana-doc-lines-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.rs");
        // A line separator and a form feed are text inside their lines; a
        // lone CR reads as a line end, as CRLF does.
        std::fs::write(&path, "fn a() {\u{2028}}\nfn b() {\x0c}\nfn c() {}\rfn d() {}\n").unwrap();
        let mut doc = Document::open(&path).unwrap();
        let text = doc.rope().to_string();
        assert_eq!(text, "fn a() {\u{2028}}\nfn b() {\x0c}\nfn c() {}\nfn d() {}\n");
        assert!(!doc.uses_crlf);
        assert_eq!(doc.line_count(), 5);
        assert_eq!(doc.line_text(0), "fn a() {\u{2028}}");
        assert_eq!(doc.line_end(1), text.find("}\nfn c").unwrap() + 1);
        let (b, c, d) = (text.find("fn b").unwrap(), text.find("fn c").unwrap(), text.find("fn d").unwrap());
        assert_eq!((doc.row_of(b), doc.row_of(c), doc.row_of(d)), (1, 2, 3));
        // Git, folds and tree-sitter count the same rows.
        assert_eq!(crate::git::split_lines(&text).len(), 4);
        assert_eq!(text.split('\n').count(), doc.line_count());
        let tree = doc.syntax().tree().unwrap();
        let rows: Vec<usize> = [b, c, d]
            .iter()
            .map(|&at| tree.root_node().descendant_for_byte_range(at, at).unwrap().start_position().row)
            .collect();
        assert_eq!(rows, [1, 2, 3]);
        // Language servers' positions: after the line separator (3 bytes, one
        // UTF-16 unit) and the form feed, and on the lines below them.
        let rope = doc.rope();
        let brace_a = text.find('}').unwrap();
        assert_eq!(offset_to_position(rope, brace_a, Encoding::Utf16), Position { line: 0, character: 9 });
        assert_eq!(offset_to_position(rope, brace_a, Encoding::Utf8), Position { line: 0, character: 11 });
        assert_eq!(position_to_offset(rope, Position { line: 0, character: 9 }, Encoding::Utf16), brace_a);
        let brace_b = text.find("\x0c}").unwrap() + 1;
        assert_eq!(offset_to_position(rope, brace_b, Encoding::Utf16), Position { line: 1, character: 9 });
        for (at, line) in [(b, 1), (c, 2), (d, 3)] {
            assert_eq!(offset_to_position(rope, at, Encoding::Utf16), Position { line, character: 0 });
            assert_eq!(position_to_offset(rope, Position { line, character: 0 }, Encoding::Utf16), at);
        }
        // A save writes the lone CR as LF.
        doc.save().unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
        std::fs::remove_dir_all(&dir).ok();
    }
}
