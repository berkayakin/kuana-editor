//! Inline Assist: the selection (or the caret's place) rewritten by the
//! model chosen in Settings → AI, as the user asks in a one-line prompt.
//! The rewrite streams into a proposed-change diff tab (`Proposal::Assist`,
//! the agents' Accept / Reject tab) as it's written; Accept puts it in the
//! file's document as one undoable edit, unsaved, if the file still holds
//! the text it was made from. Closing the tab, or Stop, ends the request.
//! The prompts are `ai::tasks`'.

use super::agent_ide::Proposal;
use super::*;
use crate::ai::{StopReason, StreamEvent};
use futures::StreamExt as _;

/// How often the diff tab follows the reply while it streams.
const FOLLOW_EVERY: Duration = Duration::from_millis(150);

/// What a request rewrites, taken when Inline Assist starts.
struct Target {
    path: PathBuf,
    /// The file's text then.
    base: Arc<str>,
    range: std::ops::Range<usize>,
    language: &'static str,
}

impl Workspace {
    pub(super) fn inline_assist(&mut self, _: &InlineAssist, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.active_editor(cx) else { return };
        let target = {
            let e = editor.read(cx);
            let doc = e.doc.read(cx);
            let Some(path) = doc.path.clone().filter(|_| doc.takes_edits()) else {
                self.toast(ToastKind::Info, "Inline Assist works in a saved file", None, cx);
                return;
            };
            Target { path, base: doc.rope().to_string().into(), range: e.primary_selection().range(), language: doc.language.name() }
        };
        if let Err(why) = Settings::get(cx).ai.clone().unwrap_or_default().choice() {
            self.toast(ToastKind::Info, "Choose an AI model for Inline Assist", Some(why), cx);
            cx.defer(|cx| crate::preferences::open(Some(crate::preferences::Page::Ai), cx));
            return;
        }
        self.close_modal(window, cx);
        let (title, hint) = if target.range.is_empty() {
            ("Inline Assist · Insert at the caret", "What should go here? (e.g. a function that parses the date)")
        } else {
            ("Inline Assist · Rewrite the selection", "How should it change? (e.g. handle the error, make it async)")
        };
        let dialog = cx.new(|cx| InputDialog::new(title, Icon::Sparkle, hint, "", None, window, cx));
        let mut target = Some(target);
        let subscription = cx.subscribe_in(&dialog, window, move |ws, _, event: &InputDialogEvent, window, cx| {
            ws.close_modal(window, cx);
            if let (InputDialogEvent::Confirm(instruction), Some(target)) = (event, target.take()) {
                ws.run_inline_assist(target, instruction.clone(), window, cx);
            }
        });
        self.modals.open(Modal::Input(dialog), subscription);
        cx.notify();
    }

    fn run_inline_assist(&mut self, target: Target, instruction: String, window: &mut Window, cx: &mut Context<Self>) {
        let choice = match Settings::get(cx).ai.clone().unwrap_or_default().choice() {
            Ok(choice) => choice,
            Err(why) => {
                self.toast(ToastKind::Error, "Inline Assist couldn't start", Some(why), cx);
                return;
            }
        };
        let rel = self.display_rel(&target.path);
        let schema = self.sql_schema_context(&target.path, cx);
        let request = crate::ai::tasks::inline_assist(&rel, target.language, &target.base, target.range.clone(), &instruction, schema.as_deref());
        let name = target.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let tab_name = format!("Inline Assist · {name}");
        // The tab shows the file unchanged until the first words arrive.
        let proposal = Proposal::Assist { tab_name: tab_name.clone(), base: target.base.clone(), writing: None };
        let Ok(diff) = self.show_proposal(target.path.clone(), tab_name, target.base.to_string(), proposal, window, cx) else {
            self.toast(ToastKind::Error, "Inline Assist couldn't open its tab", None, cx);
            return;
        };
        diff.update(cx, |d, cx| d.set_proposed(target.base.clone(), true, cx));
        let stream = choice.stream(request);
        let id = diff.entity_id();
        let task = cx.spawn(async move |ws, cx| {
            let mut stream = stream;
            let replaced = target.base[target.range.clone()].to_string();
            let with = |code: &str| -> Arc<str> { format!("{}{code}{}", &target.base[..target.range.start], &target.base[target.range.end..]).into() };
            let (mut reply, mut error, mut stop) = (String::new(), None, StopReason::EndTurn);
            let mut shown = std::time::Instant::now();
            while let Some(event) = stream.events.next().await {
                match event {
                    StreamEvent::Text { text, .. } => reply.push_str(&text),
                    StreamEvent::Stop(reason) => stop = reason,
                    StreamEvent::Error(message) => error = Some(message),
                    _ => {}
                }
                if shown.elapsed() >= FOLLOW_EVERY {
                    shown = std::time::Instant::now();
                    let text = with(&crate::ai::tasks::assist_code_so_far(&reply));
                    if diff.update(cx, |d, cx| d.set_proposed(text, true, cx)).is_err() {
                        return;
                    }
                }
            }
            let failure = match (error, &stop) {
                (Some(message), _) => Some(message),
                (None, StopReason::Refusal) => Some("The model declined this request.".to_string()),
                (None, _) if reply.trim().is_empty() => Some("The model sent nothing back.".to_string()),
                _ => None,
            };
            ws.update(cx, |ws, cx| {
                if let Some(Proposal::Assist { writing, .. }) = ws.agents.proposals.get_mut(&id) {
                    *writing = None;
                } else {
                    // Closed meanwhile.
                    return;
                }
                if let Some(message) = failure {
                    diff.update(cx, |d, cx| d.set_proposed(target.base.clone(), false, cx));
                    ws.toast(ToastKind::Error, "Inline Assist failed", Some(message), cx);
                    return;
                }
                let mut text = with(&crate::ai::tasks::assist_code(&reply, &replaced));
                if stop == StopReason::MaxTokens {
                    ws.toast(ToastKind::Info, "Inline Assist's answer was cut off", Some("It reached the length limit; check the end before accepting.".into()), cx);
                    text = with(&crate::ai::tasks::assist_code_so_far(&reply));
                }
                diff.update(cx, |d, cx| d.set_proposed(text, false, cx));
            })
            .ok();
        });
        if let Some(Proposal::Assist { writing, .. }) = self.agents.proposals.get_mut(&id) {
            *writing = Some(task);
        }
    }

    /// Accept in an Inline Assist tab: the rewrite goes into the file's
    /// document as one undoable edit (left unsaved), if the file still
    /// holds the text it was made from.
    pub(super) fn accept_assist(&mut self, path: &Path, base: &Arc<str>, text: &Arc<str>, cx: &mut Context<Self>) {
        let Some(editor) = self.tab_for(path, cx) else {
            self.toast(ToastKind::Error, "Inline Assist's file isn't open any more", None, cx);
            return;
        };
        if *editor.read(cx).doc.read(cx).rope() != **base {
            self.toast(ToastKind::Info, "The file changed since Inline Assist started", Some("Nothing was applied; run Inline Assist again.".into()), cx);
            return;
        }
        editor.update(cx, |e, cx| e.replace_text(text, cx));
    }
}
