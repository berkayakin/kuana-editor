//! Dictation into a text field, as Claude's apps take a prompt: an agent
//! session's composer (its mic button, or Dictate) and any other editor
//! through the Dictate action. The words come from the system's own speech
//! recognition where it has one (`kuana_os::dictation`: macOS, Windows), else
//! from a transcription service the AI settings reach (Linux, or the
//! `dictation.engine` setting). What's heard so far shows at the caret as
//! text being composed (`Editor::dictate`: underlined and dimmed), replaced
//! as it changes, and stays as typed text once final; all of it is one undo
//! step. Typing meanwhile ends the dictation there, keeping its words;
//! Escape (`cancel`) takes them back out. One dictation at a time, app-wide.

use std::ops::Range;
use std::path::PathBuf;

use futures::StreamExt as _;
use gpui::{App, AppContext as _, Context, Entity, EventEmitter, Global, Subscription, Task, WeakEntity};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::editor::Editor;
use crate::settings::Settings;

gpui::actions!(dictation, [Dictate]);
use kuana_os::dictation::{self as os, Event};

/// The `dictation` setting.
#[derive(Deserialize, JsonSchema, Default, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct DictationSettings {
    /// The language spoken, a BCP 47 tag (`en-US`, `tr-TR`); the system's
    /// unless set.
    pub language: Option<String>,
    /// Where speech becomes text: `system` (the computer's own recognition,
    /// macOS and Windows) or `service` (a transcription service; Linux's only).
    pub engine: Option<Engine>,
    /// The service: an AI provider that transcribes (`openai`, `groq`, or
    /// `custom` for a Whisper server), its key and URL from the `ai` settings.
    pub provider: Option<String>,
    /// The service's model (`gpt-4o-mini-transcribe` for OpenAI…).
    pub model: Option<String>,
}

#[derive(Deserialize, JsonSchema, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum Engine {
    System,
    Service,
}

/// Languages offered for dictation (Settings → AI → Dictation): a BCP 47
/// tag and the language's own name. Others can be set in settings.json.
pub const LANGUAGES: &[(&str, &str)] = &[
    ("en-US", "English (US)"),
    ("en-GB", "English (UK)"),
    ("de-DE", "Deutsch"),
    ("es-ES", "Español"),
    ("fr-FR", "Français"),
    ("it-IT", "Italiano"),
    ("pt-BR", "Português (Brasil)"),
    ("nl-NL", "Nederlands"),
    ("pl-PL", "Polski"),
    ("tr-TR", "Türkçe"),
    ("ru-RU", "Русский"),
    ("uk-UA", "Українська"),
    ("cs-CZ", "Čeština"),
    ("sv-SE", "Svenska"),
    ("da-DK", "Dansk"),
    ("nb-NO", "Norsk"),
    ("ja-JP", "日本語"),
    ("ko-KR", "한국어"),
    ("zh-CN", "中文（简体）"),
    ("hi-IN", "हिन्दी"),
];

impl DictationSettings {
    /// The engine in effect: the system's where it has one, unless set.
    pub fn engine(&self) -> Engine {
        self.engine.unwrap_or(if os::supported() { Engine::System } else { Engine::Service })
    }
}

/// What the store tells: a dictation couldn't start or go on.
pub enum DictationEvent {
    Failed(String),
}

/// The one dictation going on (`store`), observed by what shows it (the
/// composer's mic button and its level).
pub struct DictationStore {
    active: Option<Active>,
    /// The last one's hints, which a service takes as its prompt.
    hints: Vec<String>,
}

struct Active {
    editor: WeakEntity<Editor>,
    /// Where its words are in the editor now.
    range: Range<usize>,
    /// The document's version after its last edit: another one means the
    /// user typed meanwhile.
    version: u64,
    /// The undo history where it began and after its last edit: its steps
    /// become one, typing after them stays apart.
    undo_mark: usize,
    undo_end: usize,
    /// It began by taking out a selection (so taking its words back out
    /// still leaves a change).
    replaced: bool,
    /// The microphone's level, 0 to 1.
    level: f32,
    /// A space goes before the words (they follow a word).
    spaced: bool,
    /// They carry on a sentence: their first word isn't capitalized.
    carries_on: bool,
    source: Source,
    /// Stop was asked: the last words are on their way.
    stopping: bool,
    _events: Task<()>,
    _watch: Subscription,
}

enum Source {
    System(os::Dictation),
    /// Recorded for the service, sent when stopped.
    #[cfg(unix)]
    Recording(Option<os::record::Recording>),
    /// A file sent to the service (tests, `KUANA_DICTATION_FILE`).
    File(PathBuf),
    /// Gone to the service; its answer comes as the final words.
    Sent(#[allow(dead_code)] Task<()>),
}

impl EventEmitter<DictationEvent> for DictationStore {}

struct GlobalStore(Entity<DictationStore>);

impl Global for GlobalStore {}

/// Makes the store (from `main`).
pub fn init(cx: &mut App) {
    let store = cx.new(|_| DictationStore { active: None, hints: Vec::new() });
    cx.set_global(GlobalStore(store));
}

pub fn store(cx: &App) -> Option<Entity<DictationStore>> {
    cx.try_global::<GlobalStore>().map(|g| g.0.clone())
}

/// Starts dictating into `editor` at its caret, or stops a dictation going
/// into it. `hints`: words to expect (the project's, its files').
pub fn toggle(editor: &Entity<Editor>, hints: Vec<String>, cx: &mut App) {
    let Some(store) = store(cx) else { return };
    store.update(cx, |s, cx| {
        if s.is_into(editor) {
            s.stop(cx);
        } else {
            s.start(editor, hints, cx);
        }
    });
}

/// Ends a dictation into `editor` taking its words back out (Escape);
/// whether there was one.
pub fn cancel_into(editor: &Entity<Editor>, cx: &mut App) -> bool {
    let Some(store) = store(cx) else { return false };
    store.update(cx, |s, cx| {
        let into = s.is_into(editor);
        if into {
            s.cancel(cx);
        }
        into
    })
}

impl DictationStore {
    pub fn is_into(&self, editor: &Entity<Editor>) -> bool {
        self.active.as_ref().is_some_and(|a| a.editor == editor.downgrade())
    }

    /// The level of the microphone while one listens (None otherwise).
    pub fn level(&self) -> Option<f32> {
        self.active.as_ref().map(|a| a.level)
    }

    /// Whether the words are being worked out (stopped, waiting for the
    /// last of them, or for the service).
    pub fn finishing(&self) -> bool {
        self.active.as_ref().is_some_and(|a| a.stopping)
    }

    fn start(&mut self, editor: &Entity<Editor>, hints: Vec<String>, cx: &mut Context<Self>) {
        // Another one ends first, keeping its words.
        self.finish(cx);
        let settings = Settings::get(cx).dictation.clone().unwrap_or_default();
        let (tx, mut rx) = futures::channel::mpsc::unbounded::<Event>();
        // Tests listen to a file in place of the microphone.
        let file = std::env::var_os("KUANA_DICTATION_FILE").map(PathBuf::from);
        let source = match settings.engine() {
            Engine::System => {
                let sink = tx.clone();
                match os::start(os::Options { language: settings.language.clone(), hints: hints.clone(), file: file.clone() }, move |event| {
                    let _ = sink.unbounded_send(event);
                }) {
                    Ok(dictation) => Source::System(dictation),
                    Err(why) => {
                        cx.emit(DictationEvent::Failed(why));
                        return;
                    }
                }
            }
            Engine::Service => match file {
                Some(file) => Source::File(file),
                None => match record(tx.clone()) {
                    Ok(source) => source,
                    Err(why) => {
                        cx.emit(DictationEvent::Failed(why));
                        return;
                    }
                },
            },
        };
        let events = cx.spawn(async move |this, cx| {
            while let Some(event) = rx.next().await {
                if this.update(cx, |s, cx| s.event(event, cx)).is_err() {
                    break;
                }
            }
        });
        self.begin(editor, source, events, hints, cx);
    }

    /// Takes the dictation's place in the editor (its caret, its selection
    /// to replace) and follows the editor for typing.
    fn begin(&mut self, editor: &Entity<Editor>, source: Source, events: Task<()>, hints: Vec<String>, cx: &mut Context<Self>) {
        let (range, version, undo_mark, spaced, carries_on) = {
            let e = editor.read(cx);
            let range = e.primary_selection().range();
            let doc = e.doc.read(cx);
            // The text before the caret (its last couple of hundred characters).
            let rope = doc.rope();
            let end = rope.byte_to_char(range.start.min(rope.len_bytes()));
            let before = rope.slice(end.saturating_sub(200)..end).to_string();
            // Apart from a word the caret's right after; lowercase when they
            // go on with a sentence typed before them.
            let spaced = before.chars().next_back().is_some_and(|c| !c.is_whitespace());
            let carries_on = before.trim_end().chars().next_back().is_some_and(|c| !".!?:\n".contains(c));
            (range, doc.version(), doc.undo_mark(), spaced, carries_on)
        };
        let watch = cx.observe(editor, |s, editor, cx| s.editor_changed(&editor, cx));
        self.active = Some(Active {
            editor: editor.downgrade(),
            range: range.clone(),
            version,
            undo_mark,
            undo_end: undo_mark,
            replaced: !range.is_empty(),
            level: 0.,
            spaced,
            carries_on,
            source,
            stopping: false,
            _events: events,
            _watch: watch,
        });
        // Anything selected goes, as typing would take it.
        if !range.is_empty() {
            self.show_words("", false, cx);
        }
        self.hints = hints;
        cx.notify();
    }

    /// Stops listening: the last words come, final.
    pub fn stop(&mut self, cx: &mut Context<Self>) {
        let Some(active) = self.active.as_mut() else { return };
        if active.stopping {
            return;
        }
        active.stopping = true;
        let send: Option<Result<Vec<u8>, String>> = match &mut active.source {
            Source::System(dictation) => {
                dictation.stop();
                None
            }
            #[cfg(unix)]
            Source::Recording(recording) => recording.take().map(|r| Ok(r.finish())),
            Source::File(file) => Some(std::fs::read(&*file).map_err(|e| format!("Couldn't read {}: {e}", file.display()))),
            Source::Sent(_) => None,
        };
        if let Some(audio) = send {
            self.send_to_service(audio, cx);
        }
        cx.notify();
    }

    /// Ends it, taking its words back out.
    pub fn cancel(&mut self, cx: &mut Context<Self>) {
        let Some(active) = self.active.as_mut() else { return };
        if let Source::System(dictation) = &active.source {
            dictation.cancel();
        }
        self.show_words("", true, cx);
        self.finish(cx);
    }

    /// The recording, transcribed by the service in the background.
    fn send_to_service(&mut self, audio: Result<Vec<u8>, String>, cx: &mut Context<Self>) {
        let settings = Settings::get(cx).dictation.clone().unwrap_or_default();
        let ai = Settings::get(cx).ai.clone().unwrap_or_default();
        let provider = settings.provider.clone().unwrap_or_else(|| "openai".into());
        let model = settings.model.clone().unwrap_or_else(|| crate::ai::transcribe::default_model(&provider).to_string());
        let url = ai.providers.get(&provider).and_then(|p| p.url.clone());
        let language = settings.language.clone();
        let prompt = self.hints.join(", ");
        let job = cx.background_executor().spawn(async move {
            let wav = audio?;
            let endpoint = crate::ai::providers::Endpoint::of(&provider, url.as_deref())?;
            crate::ai::transcribe::transcribe(&endpoint, &model, wav, language.as_deref(), &prompt)
        });
        let task = cx.spawn(async move |this, cx| {
            let heard = job.await;
            this.update(cx, |s, cx| match heard {
                Ok(text) => {
                    s.event(Event::Words { text, done: true }, cx);
                    s.event(Event::Ended(None), cx);
                }
                Err(why) => s.event(Event::Ended(Some(why)), cx),
            })
            .ok();
        });
        if let Some(active) = self.active.as_mut() {
            active.source = Source::Sent(task);
        }
    }

    fn event(&mut self, event: Event, cx: &mut Context<Self>) {
        match event {
            Event::Words { text, done } => {
                self.show_words(&text, done, cx);
                if done {
                    self.finish(cx);
                }
            }
            Event::Level(level) => {
                if let Some(active) = self.active.as_mut() {
                    active.level = level;
                    cx.notify();
                }
            }
            Event::Ended(why) => {
                self.finish(cx);
                if let Some(why) = why {
                    cx.emit(DictationEvent::Failed(why));
                }
            }
        }
    }

    /// The words heard so far in place of the ones shown before.
    fn show_words(&mut self, text: &str, done: bool, cx: &mut Context<Self>) {
        let Some(active) = self.active.as_mut() else { return };
        let Some(editor) = active.editor.upgrade() else { return };
        let text = joined(text, active.spaced, active.carries_on);
        let range = active.range.clone();
        let (range, version, undo_end) = editor.update(cx, |e, cx| {
            let range = e.dictate(range, &text, done, cx);
            let doc = e.doc.read(cx);
            (range, doc.version(), doc.undo_mark())
        });
        active.range = range;
        active.version = version;
        active.undo_end = undo_end;
    }

    /// Typing (or anything that edits the field) while words come: the
    /// dictation ends there, its words kept.
    fn editor_changed(&mut self, editor: &Entity<Editor>, cx: &mut Context<Self>) {
        let Some(active) = self.active.as_ref() else { return };
        if editor.read(cx).doc.read(cx).version() != active.version {
            if let Source::System(dictation) = &active.source {
                dictation.cancel();
            }
            self.finish(cx);
        }
    }

    /// Over: what it wrote stays as one undo step, apart from anything typed
    /// after it. Nothing written in the end (Cancel took the words back out,
    /// or none were heard) leaves no step, so the text isn't taken for changed.
    fn finish(&mut self, cx: &mut Context<Self>) {
        let Some(active) = self.active.take() else { return };
        if let Some(editor) = active.editor.upgrade() {
            let steps = active.undo_mark..active.undo_end;
            editor.update(cx, |e, cx| {
                e.end_dictation(cx);
                e.doc.update(cx, |doc, _| {
                    // Only while the text is as its last edit left it: an Undo
                    // meanwhile may have brought words back.
                    let wrote_nothing = !active.replaced && active.range.is_empty() && doc.version() == active.version;
                    if wrote_nothing { doc.forget_undo(steps) } else { doc.merge_undo(steps) }
                });
            });
        }
        cx.notify();
    }
}

/// Dictated words after the caret: a space first when they follow a word
/// (not before punctuation), and when they carry on a sentence typed before
/// them, their first word lowercase if the recognizer capitalized it as a
/// start (`Fix` → `fix`; not `I`, `API` or `McDonald`).
fn joined(text: &str, after_word: bool, carries_on: bool) -> String {
    let mut words = text.to_string();
    if carries_on {
        let first: &str = words.split(|c: char| !c.is_alphanumeric() && c != '\'').next().unwrap_or("");
        let mut letters = first.chars();
        let capitalized = letters.next().is_some_and(char::is_uppercase) && letters.clone().count() > 0 && letters.all(char::is_lowercase);
        if capitalized && !first.starts_with("I'") {
            let upper = first.chars().next().map_or(0, char::len_utf8);
            words = format!("{}{}", words[..upper].to_lowercase(), &words[upper..]);
        }
    }
    if after_word && !words.is_empty() && !words.starts_with(|c: char| c.is_whitespace() || ",.;:!?)".contains(c)) {
        words.insert(0, ' ');
    }
    words
}

/// Starts recording for the service (Unix: the system's recorder).
fn record(tx: futures::channel::mpsc::UnboundedSender<Event>) -> Result<Source, String> {
    #[cfg(unix)]
    {
        let recording = os::record::start(
            std::sync::Arc::new(move |event| {
                let _ = tx.unbounded_send(event);
            }),
            None,
        )?;
        Ok(Source::Recording(Some(recording)))
    }
    #[cfg(not(unix))]
    {
        let _ = tx;
        Err("Recording for a transcription service isn't supported here: use the system's speech recognition".into())
    }
}

#[cfg(test)]
mod tests {
    use gpui::{Entity, Focusable as _, TestAppContext};

    use super::*;

    fn setup(cx: &mut TestAppContext, text: &str) -> (Entity<DictationStore>, Entity<Editor>) {
        let store = cx.new(|_| DictationStore { active: None, hints: Vec::new() });
        let editor = cx.new(|cx| {
            let mut e = Editor::multi_line("Ask", cx);
            e.set_text(text, cx);
            let end = text.len();
            e.select_range(end..end, cx);
            e
        });
        // As if it listened: the words come from the test.
        store.update(cx, |s, cx| s.begin(&editor, Source::File("/dev/null".into()), Task::ready(()), Vec::new(), cx));
        cx.run_until_parked();
        (store, editor)
    }

    fn text(editor: &Entity<Editor>, cx: &mut TestAppContext) -> String {
        editor.read_with(cx, |e, cx| e.text(cx))
    }

    #[test]
    fn words_join_what_was_typed_before_them() {
        assert_eq!(joined("the bug", true, true), " the bug");
        assert_eq!(joined("the bug", false, false), "the bug");
        assert_eq!(joined(", then", true, true), ", then");
        assert_eq!(joined("", true, true), "");
        // Carrying on a sentence: the recognizer's capital goes.
        assert_eq!(joined("Fix the bug", true, true), " fix the bug");
        assert_eq!(joined("Fix the bug", true, false), " Fix the bug", "a new sentence keeps it");
        assert_eq!(joined("Fix", false, false), "Fix");
        // Not a word that's always written so.
        assert_eq!(joined("I think so", true, true), " I think so");
        assert_eq!(joined("I'll fix it", true, true), " I'll fix it");
        assert_eq!(joined("API keys", true, true), " API keys");
        assert_eq!(joined("McDonald", true, true), " McDonald");
        assert_eq!(joined("Évidemment", true, true), " évidemment");
    }

    #[gpui::test]
    fn words_replace_each_other_then_undo_as_one(cx: &mut TestAppContext) {
        let (store, editor) = setup(cx, "Fix");
        store.update(cx, |s, cx| s.event(Event::Words { text: "The bug".into(), done: false }, cx));
        assert_eq!(text(&editor, cx), "Fix the bug", "spaced, and lowercase carrying on the sentence");
        assert!(editor.read_with(cx, |e, _| e.composing()), "marked while it may change");
        store.update(cx, |s, cx| s.event(Event::Level(0.6), cx));
        assert_eq!(store.read_with(cx, |s, _| s.level()), Some(0.6));
        store.update(cx, |s, cx| s.event(Event::Words { text: "the bug in cart.py".into(), done: true }, cx));
        assert_eq!(text(&editor, cx), "Fix the bug in cart.py");
        assert!(!editor.read_with(cx, |e, _| e.composing()), "typed text once final");
        assert!(store.read_with(cx, |s, _| s.active.is_none()), "over");
        // One undo step takes all of it out.
        editor.update(cx, |e, cx| e.doc.update(cx, |d, _| d.undo()));
        assert_eq!(text(&editor, cx), "Fix");
    }

    #[gpui::test]
    fn typing_meanwhile_ends_it_keeping_the_words(cx: &mut TestAppContext) {
        let (store, editor) = setup(cx, "Fix");
        store.update(cx, |s, cx| s.event(Event::Words { text: "the".into(), done: false }, cx));
        store.update(cx, |s, cx| s.event(Event::Words { text: "the bug".into(), done: false }, cx));
        editor.update(cx, |e, cx| e.insert_text("!", cx));
        cx.run_until_parked();
        assert!(store.read_with(cx, |s, _| s.active.is_none()), "the user typed: it ends");
        assert_eq!(text(&editor, cx), "Fix the bug!");
        assert!(!editor.read_with(cx, |e, _| e.composing()));
        // Later words (on their way) change nothing.
        store.update(cx, |s, cx| s.event(Event::Words { text: "the bug now".into(), done: true }, cx));
        assert_eq!(text(&editor, cx), "Fix the bug!");
        // What was typed is its own undo step, the dictated words another.
        editor.update(cx, |e, cx| e.doc.update(cx, |d, _| d.undo()));
        assert_eq!(text(&editor, cx), "Fix the bug");
        editor.update(cx, |e, cx| e.doc.update(cx, |d, _| d.undo()));
        assert_eq!(text(&editor, cx), "Fix");
    }

    #[gpui::test]
    fn cancelling_takes_the_words_back_out(cx: &mut TestAppContext) {
        let (store, editor) = setup(cx, "Fix ");
        let history = |cx: &mut TestAppContext| editor.read_with(cx, |e, cx| (e.doc.read(cx).undo_mark(), e.doc.read(cx).is_dirty()));
        let before = history(cx);
        store.update(cx, |s, cx| s.event(Event::Words { text: "the bug".into(), done: false }, cx));
        assert_eq!(text(&editor, cx), "Fix the bug", "no second space after one");
        store.update(cx, |s, cx| s.cancel(cx));
        assert_eq!(text(&editor, cx), "Fix ");
        assert!(store.read_with(cx, |s, _| s.active.is_none()));
        assert_eq!(history(cx), before, "no undo step left, nothing unsaved");
    }

    /// Undo in the field while it listens ends it; whatever the words did
    /// stays in the undo history (none of it dropped as "nothing written").
    #[gpui::test]
    fn undoing_meanwhile_keeps_the_history_whole(cx: &mut TestAppContext) {
        let (store, editor) = setup(cx, "Fix");
        store.update(cx, |s, cx| s.event(Event::Words { text: "the bug".into(), done: false }, cx));
        store.update(cx, |s, cx| s.event(Event::Words { text: String::new(), done: false }, cx));
        assert_eq!(text(&editor, cx), "Fix", "heard again as nothing");
        editor.update(cx, |e, cx| {
            e.doc.update(cx, |d, _| d.undo());
            cx.notify();
        });
        cx.run_until_parked();
        assert!(store.read_with(cx, |s, _| s.active.is_none()), "the field changed: it ends");
        assert_eq!(text(&editor, cx), "Fix the bug");
        editor.update(cx, |e, cx| e.doc.update(cx, |d, _| d.undo()));
        assert_eq!(text(&editor, cx), "Fix", "the words' own step is still there");
    }

    #[gpui::test]
    fn cancelling_after_a_selection_leaves_it_one_undo_away(cx: &mut TestAppContext) {
        let (store, editor) = setup(cx, "Fix it");
        store.update(cx, |s, cx| s.cancel(cx));
        editor.update(cx, |e, cx| e.select_range(4..6, cx));
        store.update(cx, |s, cx| s.begin(&editor, Source::File("/dev/null".into()), Task::ready(()), Vec::new(), cx));
        assert_eq!(text(&editor, cx), "Fix ", "the selection goes, as typing would take it");
        store.update(cx, |s, cx| s.event(Event::Words { text: "the bug".into(), done: false }, cx));
        store.update(cx, |s, cx| s.cancel(cx));
        assert_eq!(text(&editor, cx), "Fix ");
        editor.update(cx, |e, cx| e.doc.update(cx, |d, _| d.undo()));
        assert_eq!(text(&editor, cx), "Fix it");
    }

    /// Escape ends a dictation from its start: it listens (or records for a
    /// service) before any words come, and none must come in later.
    #[gpui::test]
    fn escape_ends_it_before_any_words(cx: &mut TestAppContext) {
        cx.update(|cx| {
            cx.set_global(crate::theme::Theme::blank_for_tests());
            cx.bind_keys(crate::keymap::bindings(crate::settings::BaseKeymap::JetBrains));
            init(cx);
        });
        let (editor, cx) = cx.add_window_view(|_, cx| {
            let mut e = Editor::multi_line("Ask", cx);
            e.set_text("Fix", cx);
            e.select_range(3..3, cx);
            e
        });
        cx.update(|window, cx| {
            window.activate_window();
            window.focus(&editor.focus_handle(cx));
        });
        let store = cx.update(|_, cx| store(cx).expect("made by init"));
        store.update(cx, |s, cx| s.begin(&editor, Source::File("/dev/null".into()), Task::ready(()), Vec::new(), cx));
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert!(store.read_with(cx, |s, _| s.active.is_none()), "Escape ended it");
        store.update(cx, |s, cx| s.event(Event::Words { text: "the bug".into(), done: true }, cx));
        assert_eq!(editor.read_with(cx, |e, cx| e.text(cx)), "Fix", "words coming later go nowhere");
    }

    #[gpui::test]
    fn an_error_ends_it_and_says_why(cx: &mut TestAppContext) {
        let (store, editor) = setup(cx, "");
        let said = std::rc::Rc::new(std::cell::RefCell::new(None));
        let heard = said.clone();
        let _sub = cx.update(|cx| cx.subscribe(&store, move |_, event: &DictationEvent, _| {
            let DictationEvent::Failed(why) = event;
            *heard.borrow_mut() = Some(why.clone());
        }));
        store.update(cx, |s, cx| s.event(Event::Words { text: "hello".into(), done: false }, cx));
        store.update(cx, |s, cx| s.event(Event::Ended(Some("No speech was heard.".into())), cx));
        assert_eq!(said.borrow().as_deref(), Some("No speech was heard."));
        assert_eq!(text(&editor, cx), "hello", "what was heard stays");
        assert!(!editor.read_with(cx, |e, _| e.composing()));
    }
}
