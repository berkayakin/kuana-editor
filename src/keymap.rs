//! Key bindings. A base keymap chosen in settings (`base_keymap`: JetBrains by
//! default, VS Code, Sublime Text or Zed) goes on top
//! of the shortcuts they all share, each in its macOS or Windows/Linux variant.
//! Double Shift (Search Everywhere) isn't a regular binding; the workspace
//! detects it.
//!
//! The user's `keymap.json` is added on top; see [`install`].

use std::{collections::HashMap, path::PathBuf, rc::Rc};

use gpui::{Action, App, KeyBinding, KeyBindingContextPredicate, KeyBindingMetaIndex, NoAction};
use serde::Deserialize;
use serde_json::Value;

use crate::{
    about::About,
    editor::*,
    picker::ActionEntry,
    push_dialog::ConfirmPush,
    terminal::{TerminalClear, TerminalCopy, TerminalFind, TerminalPaste},
    settings::{BaseKeymap, strip_jsonc},
    theme::{self, config_dir},
    workspace::{
        CloseAllTabs, CloseOtherTabs, CloseTab, CloseTerminal, NextPane, PreviousPane, FocusPaneLeft, FocusPaneRight, FocusPaneUp, FocusPaneDown, Rerun, RunConfiguration, StopRun, SplitDown, SplitRight, TogglePinTab, DecreaseFontSize, NewTerminal, ToggleTerminal, FileStructure, Find, IncreaseFontSize, OpenKeymap, OpenSettings, ResetFontSize, RestartLanguageServer, CopilotSignIn, CopilotSignOut, FindAction, FindInFiles, FindNext, FindPrevious, GoToFile, GoToLine, OpenSettingsFile,
        CloseWindow, MinimizeWindow, ZoomWindow, NewFile, NewWindow, NextTab, OpenFile, OpenFolder, OpenFolderInThisWindow,
        PrevTab, Quit, RecentFiles, RecentProjects, RecentTab, RecentTabBack, Replace,
        GitBranches, GitStash, ManageRemotes, GitClone, GitFetch, GitInit, GitPull, GitPush, OpenRemote, OpenThemesFolder, OpenSnippetsFolder, ReloadThemes, ReplaceInFiles,
        Save, SaveAs, SearchEverywhere, SelectTheme, ShowCommit, ShowFileHistory, ToggleCommit, ToggleGitLog, ToggleProblems,
        DebugConfiguration, Resume, PauseProgram, StepOver, StepInto, StepOut, ToggleDebugPanel, AttachToProcess, AddConditionalBreakpoint,
        AddLogpoint, OpenExtensions, ReloadExtensions, ProjectTrust, UndoFileChange, RedoFileChange, OpenProjectSettings,
        ToggleResults, NextResult, PreviousResult, RerunFailedTests, ShowLocalHistory,
        ToggleAutoSave, ToggleSidebar, ToggleSidebarPosition, ToggleClassicLayout, CheckForUpdates, WelcomeSetup, GoToSymbol, SplitTerminal, RenameTerminal, SelectOpenedFile, InstallShellCommand,
    },
};

/// Which base keymap the installed bindings came from.
pub struct Installed(pub crate::settings::BaseKeymap);

impl gpui::Global for Installed {}

/// The built-in bindings for a base keymap: what every keymap shares, then
/// its own layer (later bindings win). On macOS the
/// macOS variant of each keymap, the Windows/Linux one elsewhere.
pub fn bindings(base: BaseKeymap) -> Vec<KeyBinding> {
    let mac = cfg!(target_os = "macos");
    let mut b = common(mac);
    b.extend(match base {
        BaseKeymap::JetBrains => jetbrains(mac),
        BaseKeymap::VSCode => vscode(mac),
        BaseKeymap::SublimeText => sublime_text(mac),
        BaseKeymap::Zed => zed(mac),
    });
    // GPUI ranks a binding without a context as deep as the deepest context,
    // so between the two the later one wins: a keymap's global ⌘T would beat
    // the terminal's own. Context bindings go last so they win where they apply.
    let (global, contextual): (Vec<KeyBinding>, Vec<KeyBinding>) = b.into_iter().partition(|b| b.predicate().is_none());
    global.into_iter().chain(contextual).collect()
}

/// How `base` binds `action` out of the box (its first binding that works in
/// the editor), formatted for display; for showing keymaps side by side.
pub fn shortcut_in(base: BaseKeymap, action: &dyn Action) -> Option<String> {
    let matching: Vec<KeyBinding> = bindings(base)
        .into_iter()
        .filter(|b| {
            b.action().partial_eq(action) && b.predicate().is_none_or(|p| matches!(p.to_string().as_str(), "Editor" | "Workspace"))
        })
        .collect();
    // The platform's own binding comes last (JetBrains' ⌘⇧O after the shared ⌘P);
    // a bare key like F1 is only an alternative.
    let modified = |b: &&KeyBinding| b.keystrokes().first().is_some_and(|k| k.modifiers().modified());
    let binding = matching.iter().rev().find(modified).or(matching.first())?;
    let keys: Vec<String> = binding.keystrokes().iter().map(|k| crate::ui::shortcut_label(&k.to_string())).collect();
    Some(keys.join(" "))
}

/// The keymap chosen in settings (JetBrains unless set).
pub fn current(cx: &App) -> BaseKeymap {
    cx.try_global::<crate::settings::Settings>().and_then(|s| s.base_keymap).unwrap_or_default()
}

/// Shortcuts every editor (and the platform) agrees on, and Kuana's own
/// dialogs. `secondary` is ⌘ on macOS and Ctrl elsewhere.
fn common(mac: bool) -> Vec<KeyBinding> {
    let e = Some("Editor");
    let mut b = vec![
        KeyBinding::new("secondary-n", NewFile, None),
        KeyBinding::new("secondary-o", OpenFile, None),
        KeyBinding::new("secondary-s", Save, None),
        KeyBinding::new("secondary-shift-s", SaveAs, None),
        KeyBinding::new("secondary-q", Quit, None),
        KeyBinding::new("secondary-f", Find, None),
        KeyBinding::new("secondary-shift-f", FindInFiles, None),
        KeyBinding::new("f3", FindNext, None),
        KeyBinding::new("shift-f3", FindPrevious, None),
        // The last used tab, as every editor's Ctrl+Tab switcher starts.
        KeyBinding::new("ctrl-tab", RecentTab, None),
        KeyBinding::new("ctrl-shift-tab", RecentTabBack, None),
        KeyBinding::new("secondary-\\", SplitRight, None),
        KeyBinding::new("ctrl-space", ShowCompletions, e),
        // Dialogs and panels.
        KeyBinding::new("escape", Cancel, Some("PushDialog")),
        KeyBinding::new("escape", Cancel, Some("TrustDialog")),
        KeyBinding::new("escape", Cancel, Some("About")),
        KeyBinding::new("enter", ConfirmPush, Some("PushDialog")),
        KeyBinding::new("secondary-enter", ConfirmPush, Some("PushDialog")),
        KeyBinding::new("up", MoveUp, Some("PushDialog")),
        KeyBinding::new("down", MoveDown, Some("PushDialog")),
        // The diff viewer: F7 / ⇧F7 step through changes (JetBrains and VS Code).
        KeyBinding::new("f7", NextChange, Some("DiffView")),
        KeyBinding::new("shift-f7", PreviousChange, Some("DiffView")),
        KeyBinding::new("ctrl-alt-shift-down", NextChange, Some("DiffView")),
        KeyBinding::new("ctrl-alt-shift-up", PreviousChange, Some("DiffView")),
        KeyBinding::new("up", MoveUp, Some("GitLog")),
        KeyBinding::new("down", MoveDown, Some("GitLog")),
        KeyBinding::new("escape", Cancel, Some("ContextMenu")),
        KeyBinding::new("up", MoveUp, Some("Results")),
        KeyBinding::new("down", MoveDown, Some("Results")),
        KeyBinding::new("left", MoveLeft, Some("Results")),
        KeyBinding::new("right", MoveRight, Some("Results")),
        KeyBinding::new("enter", Newline, Some("Results")),
        KeyBinding::new("escape", Cancel, Some("Results")),
        KeyBinding::new("escape", Cancel, Some("Preferences")),
        KeyBinding::new("up", MoveUp, Some("ContextMenu")),
        KeyBinding::new("down", MoveDown, Some("ContextMenu")),
        KeyBinding::new("enter", Newline, Some("ContextMenu")),
        KeyBinding::new("left", MoveLeft, Some("ContextMenu")),
        KeyBinding::new("right", MoveRight, Some("ContextMenu")),
        // Editor basics
        KeyBinding::new("left", MoveLeft, e),
        KeyBinding::new("right", MoveRight, e),
        KeyBinding::new("up", MoveUp, e),
        KeyBinding::new("down", MoveDown, e),
        KeyBinding::new("shift-left", SelectLeft, e),
        KeyBinding::new("shift-right", SelectRight, e),
        KeyBinding::new("shift-up", SelectUp, e),
        KeyBinding::new("shift-down", SelectDown, e),
        KeyBinding::new("home", MoveLineStart, e),
        KeyBinding::new("end", MoveLineEnd, e),
        KeyBinding::new("shift-home", SelectLineStart, e),
        KeyBinding::new("shift-end", SelectLineEnd, e),
        KeyBinding::new("pageup", PageUp, e),
        KeyBinding::new("pagedown", PageDown, e),
        KeyBinding::new("backspace", Backspace, e),
        KeyBinding::new("shift-backspace", Backspace, e),
        KeyBinding::new("delete", Delete, e),
        KeyBinding::new("enter", Newline, e),
        // New line below in code, "submit" in text fields.
        KeyBinding::new("secondary-enter", Submit, e),
        KeyBinding::new("tab", Indent, e),
        KeyBinding::new("shift-tab", Outdent, e),
        KeyBinding::new("escape", Cancel, e),
        // Copilot's own shortcut for asking for a suggestion, in VS Code and JetBrains.
        KeyBinding::new("alt-\\", ShowEditPrediction, e),
        KeyBinding::new("secondary-a", SelectAll, e),
        KeyBinding::new("secondary-c", Copy, e),
        KeyBinding::new("secondary-x", Cut, e),
        KeyBinding::new("secondary-v", Paste, e),
        KeyBinding::new("secondary-z", Undo, e),
        KeyBinding::new("secondary-z", UndoFileChange, Some("FileTree")),
        KeyBinding::new("secondary-shift-z", RedoFileChange, Some("FileTree")),
        KeyBinding::new("secondary-shift-z", Redo, e),
        KeyBinding::new("secondary-/", ToggleComment, e),
    ];
    if mac {
        b.extend([
            KeyBinding::new("cmd-g", FindNext, None),
            KeyBinding::new("cmd-shift-g", FindPrevious, None),
            KeyBinding::new("cmd-shift-]", NextTab, None),
            KeyBinding::new("cmd-shift-[", PrevTab, None),
            KeyBinding::new("cmd-,", OpenSettings, None),
            KeyBinding::new("cmd-w", CloseTab, None),
            KeyBinding::new("cmd-shift-n", NewWindow, None),
            KeyBinding::new("cmd-shift-w", CloseWindow, None),
            KeyBinding::new("cmd-m", MinimizeWindow, None),
            KeyBinding::new("cmd-t", NewTerminal, Some("Terminal")),
            KeyBinding::new("cmd-w", CloseTerminal, Some("Terminal")),
            KeyBinding::new("cmd-c", TerminalCopy, Some("Terminal")),
            KeyBinding::new("cmd-v", TerminalPaste, Some("Terminal")),
            KeyBinding::new("cmd-k", TerminalClear, Some("Terminal")),
            KeyBinding::new("cmd-f", TerminalFind, Some("Terminal")),
            // macOS text navigation.
            KeyBinding::new("cmd-left", MoveLineStart, e),
            KeyBinding::new("cmd-right", MoveLineEnd, e),
            KeyBinding::new("cmd-shift-left", SelectLineStart, e),
            KeyBinding::new("cmd-shift-right", SelectLineEnd, e),
            KeyBinding::new("cmd-up", MoveDocStart, e),
            KeyBinding::new("cmd-down", MoveDocEnd, e),
            KeyBinding::new("cmd-home", MoveDocStart, e),
            KeyBinding::new("cmd-end", MoveDocEnd, e),
            KeyBinding::new("cmd-shift-up", SelectDocStart, e),
            KeyBinding::new("cmd-shift-down", SelectDocEnd, e),
            KeyBinding::new("alt-left", MoveWordLeft, e),
            KeyBinding::new("alt-right", MoveWordRight, e),
            KeyBinding::new("alt-shift-left", SelectWordLeft, e),
            KeyBinding::new("alt-shift-right", SelectWordRight, e),
            KeyBinding::new("alt-backspace", DeleteWordLeft, e),
            KeyBinding::new("alt-delete", DeleteWordRight, e),
            KeyBinding::new("ctrl-a", MoveLineStart, e),
            KeyBinding::new("ctrl-e", MoveLineEnd, e),
            KeyBinding::new("ctrl-cmd-space", ShowCharacterPalette, e),
        ]);
    } else {
        b.extend([
            KeyBinding::new("ctrl-f4", CloseTab, None),
            KeyBinding::new("ctrl-shift-t", NewTerminal, Some("Terminal")),
            KeyBinding::new("ctrl-shift-w", CloseTerminal, Some("Terminal")),
            KeyBinding::new("ctrl-shift-c", TerminalCopy, Some("Terminal")),
            KeyBinding::new("ctrl-shift-v", TerminalPaste, Some("Terminal")),
            KeyBinding::new("ctrl-shift-f", TerminalFind, Some("Terminal")),
            KeyBinding::new("ctrl-home", MoveDocStart, e),
            KeyBinding::new("ctrl-end", MoveDocEnd, e),
            KeyBinding::new("ctrl-shift-home", SelectDocStart, e),
            KeyBinding::new("ctrl-shift-end", SelectDocEnd, e),
            KeyBinding::new("ctrl-left", MoveWordLeft, e),
            KeyBinding::new("ctrl-right", MoveWordRight, e),
            KeyBinding::new("ctrl-shift-left", SelectWordLeft, e),
            KeyBinding::new("ctrl-shift-right", SelectWordRight, e),
            KeyBinding::new("ctrl-backspace", DeleteWordLeft, e),
            KeyBinding::new("ctrl-delete", DeleteWordRight, e),
        ]);
    }
    b
}

/// The JetBrains IDE keymaps ("macOS" and "Windows/Linux" in IntelliJ).
fn jetbrains(mac: bool) -> Vec<KeyBinding> {
    let e = Some("Editor");
    let mut b = vec![
        KeyBinding::new("secondary-r", Replace, None),
        KeyBinding::new("secondary-shift-r", ReplaceInFiles, None),
        KeyBinding::new("secondary-e", RecentFiles, None),
        KeyBinding::new("secondary-f12", FileStructure, None),
        KeyBinding::new("alt-f12", ToggleTerminal, None),
        KeyBinding::new("secondary-shift-\\", SplitDown, None),
        KeyBinding::new("secondary-b", GoToDeclaration, e),
        KeyBinding::new("secondary-alt-b", GoToImplementation, e),
        KeyBinding::new("ctrl-shift-b", GoToTypeDefinition, e),
        KeyBinding::new("alt-f7", FindUsages, e),
        KeyBinding::new("ctrl-alt-h", CallHierarchy, e),
        KeyBinding::new("ctrl-h", TypeHierarchy, e),
        // Debugger (JetBrains): F8 over, F7 into, ⇧F8 out.
        KeyBinding::new("f8", StepOver, None),
        KeyBinding::new("f7", StepInto, None),
        KeyBinding::new("shift-f8", StepOut, None),
        KeyBinding::new("alt-f1", SelectOpenedFile, None),
        KeyBinding::new("shift-f6", RenameSymbol, e),
        KeyBinding::new("alt-enter", ShowContextActions, e),
        KeyBinding::new("secondary-alt-l", ReformatCode, e),
        // ⌘-/⌘= fold in the editor, so zoom takes ⌥⌘-/⌥⌘=.
        KeyBinding::new("secondary-alt-=", IncreaseFontSize, None),
        KeyBinding::new("secondary-alt-+", IncreaseFontSize, None),
        KeyBinding::new("secondary-alt--", DecreaseFontSize, None),
        KeyBinding::new("secondary--", CollapseRegion, e),
        KeyBinding::new("secondary-=", ExpandRegion, e),
        KeyBinding::new("secondary-+", ExpandRegion, e),
        KeyBinding::new("secondary-shift--", CollapseAllRegions, e),
        // Shift+minus arrives as underscore on some layouts.
        KeyBinding::new("secondary-shift-_", CollapseAllRegions, e),
        KeyBinding::new("secondary-_", CollapseAllRegions, e),
        KeyBinding::new("secondary-shift-=", ExpandAllRegions, e),
        KeyBinding::new("secondary-shift-+", ExpandAllRegions, e),
        KeyBinding::new("secondary-shift-a", FindAction, None),
        KeyBinding::new("secondary-p", GoToFile, None),
        KeyBinding::new("secondary-shift-m", ToggleProblems, None),
        // "Quick Switch Scheme".
        KeyBinding::new("ctrl-`", SelectTheme, None),
        // Commit, Push, Update Project.
        KeyBinding::new("secondary-k", ShowCommit, None),
        KeyBinding::new("secondary-shift-k", GitPush, None),
        KeyBinding::new("secondary-t", GitPull, None),
        KeyBinding::new("ctrl-alt-shift-down", NextChange, e),
        KeyBinding::new("ctrl-alt-shift-up", PreviousChange, e),
        // Next / Previous Highlighted Error.
        KeyBinding::new("f2", NextProblem, e),
        KeyBinding::new("shift-f2", PreviousProblem, e),
        KeyBinding::new("shift-enter", NewlineBelow, e),
        KeyBinding::new("secondary-d", DuplicateLine, e),
        KeyBinding::new("secondary-shift-u", ToggleCase, e),
        KeyBinding::new("ctrl-shift-j", JoinLines, e),
        KeyBinding::new("alt-shift-up", MoveLineUp, e),
        KeyBinding::new("alt-shift-down", MoveLineDown, e),
    ];
    if mac {
        b.extend([
            // The Windows-style Ctrl variants also work on macOS.
            KeyBinding::new("ctrl-f", Find, None),
            KeyBinding::new("ctrl-shift-f", FindInFiles, None),
            // ⌃R runs, ⌃⌥R picks what to run, ⌃⇧R runs at the caret.
            KeyBinding::new("ctrl-r", Rerun, None),
            KeyBinding::new("ctrl-alt-r", RunConfiguration, None),
            KeyBinding::new("ctrl-shift-r", RunAtCaret, e),
            KeyBinding::new("cmd-f2", StopRun, None),
            KeyBinding::new("cmd-f8", ToggleBreakpoint, e),
            KeyBinding::new("cmd-shift-f8", AddConditionalBreakpoint, e),
            KeyBinding::new("ctrl-shift-d", DebugAtCaret, e),
            KeyBinding::new("ctrl-alt-d", DebugConfiguration, None),
            KeyBinding::new("alt-shift-f5", AttachToProcess, None),
            KeyBinding::new("cmd-alt-r", Resume, None),
            KeyBinding::new("cmd-shift-o", GoToFile, None),
            KeyBinding::new("cmd-l", GoToLine, None),
            KeyBinding::new("f1", QuickDocumentation, e),
            KeyBinding::new("alt-tab", NextPane, None),
            KeyBinding::new("ctrl-right", NextTab, None),
            KeyBinding::new("ctrl-left", PrevTab, None),
            KeyBinding::new("cmd-1", ToggleSidebar, None),
            KeyBinding::new("cmd-6", ToggleProblems, None),
            KeyBinding::new("cmd-3", ToggleResults, None),
            KeyBinding::new("cmd-0", ToggleCommit, None),
            KeyBinding::new("cmd-9", ToggleGitLog, None),
            KeyBinding::new("cmd-alt-z", RollbackHunk, e),
            KeyBinding::new("cmd-alt-o", OpenFolder, None),
            KeyBinding::new("cmd-backspace", DeleteLine, e),
            KeyBinding::new("alt-up", ExtendSelection, e),
            KeyBinding::new("alt-down", ShrinkSelection, e),
            KeyBinding::new("ctrl-g", SelectNextOccurrence, e),
            KeyBinding::new("ctrl-shift-g", UnselectOccurrence, e),
            KeyBinding::new("ctrl-cmd-g", SelectAllOccurrences, e),
            KeyBinding::new("cmd-alt-/", ToggleBlockComment, e),
            KeyBinding::new("cmd-alt-enter", NewlineAbove, e),
            KeyBinding::new("cmd-alt-up", AddCursorAbove, e),
            KeyBinding::new("cmd-alt-down", AddCursorBelow, e),
        ]);
    } else {
        b.extend([
            KeyBinding::new("ctrl-shift-]", NextTab, None),
            KeyBinding::new("ctrl-shift-[", PrevTab, None),
            KeyBinding::new("ctrl-shift-n", GoToFile, None),
            KeyBinding::new("ctrl-alt-shift-n", GoToSymbol, None),
            KeyBinding::new("ctrl-g", GoToLine, None),
            KeyBinding::new("ctrl-alt-s", OpenSettings, None),
            KeyBinding::new("ctrl-q", QuickDocumentation, e),
            KeyBinding::new("shift-f10", Rerun, None),
            KeyBinding::new("alt-shift-f10", RunConfiguration, None),
            KeyBinding::new("ctrl-shift-f10", RunAtCaret, e),
            KeyBinding::new("ctrl-f2", StopRun, None),
            KeyBinding::new("ctrl-f8", ToggleBreakpoint, e),
            KeyBinding::new("ctrl-shift-f8", AddConditionalBreakpoint, e),
            KeyBinding::new("ctrl-shift-f9", DebugAtCaret, e),
            KeyBinding::new("alt-shift-f9", DebugConfiguration, None),
            KeyBinding::new("ctrl-alt-f5", AttachToProcess, None),
            KeyBinding::new("f9", Resume, None),
            KeyBinding::new("alt-right", NextTab, None),
            KeyBinding::new("alt-left", PrevTab, None),
            KeyBinding::new("alt-1", ToggleSidebar, None),
            KeyBinding::new("alt-6", ToggleProblems, None),
            KeyBinding::new("alt-3", ToggleResults, None),
            KeyBinding::new("alt-0", ToggleCommit, None),
            KeyBinding::new("alt-9", ToggleGitLog, None),
            KeyBinding::new("ctrl-alt-z", RollbackHunk, e),
            KeyBinding::new("ctrl-alt-o", OpenFolder, None),
            KeyBinding::new("ctrl-y", DeleteLine, e),
            KeyBinding::new("ctrl-w", ExtendSelection, e),
            KeyBinding::new("ctrl-shift-w", ShrinkSelection, e),
            KeyBinding::new("alt-j", SelectNextOccurrence, e),
            KeyBinding::new("alt-shift-j", UnselectOccurrence, e),
            KeyBinding::new("ctrl-alt-shift-j", SelectAllOccurrences, e),
            KeyBinding::new("ctrl-shift-/", ToggleBlockComment, e),
            KeyBinding::new("ctrl-alt-enter", NewlineAbove, e),
            KeyBinding::new("ctrl-alt-up", AddCursorAbove, e),
            KeyBinding::new("ctrl-alt-down", AddCursorBelow, e),
        ]);
    }
    b
}

/// VS Code's default keyboard shortcuts.
fn vscode(mac: bool) -> Vec<KeyBinding> {
    let e = Some("Editor");
    let mut b = vec![
        KeyBinding::new("secondary-shift-p", FindAction, None),
        KeyBinding::new("f1", FindAction, None),
        KeyBinding::new("secondary-p", GoToFile, None),
        KeyBinding::new("ctrl-g", GoToLine, None),
        KeyBinding::new("secondary-shift-o", FileStructure, None),
        KeyBinding::new("secondary-shift-h", ReplaceInFiles, None),
        KeyBinding::new("secondary-b", ToggleSidebar, None),
        KeyBinding::new("secondary-shift-e", ToggleSidebar, None),
        KeyBinding::new("ctrl-shift-g", ShowCommit, None),
        KeyBinding::new("secondary-shift-m", ToggleProblems, None),
        KeyBinding::new("ctrl-`", ToggleTerminal, None),
        KeyBinding::new("ctrl-shift-`", NewTerminal, None),
        KeyBinding::new("secondary-k secondary-s", OpenKeymap, None),
        KeyBinding::new("secondary-k secondary-t", SelectTheme, None),
        KeyBinding::new("secondary-k secondary-o", OpenFolder, None),
        KeyBinding::new("ctrl-r", RecentProjects, None),
        KeyBinding::new("secondary-k secondary-\\", SplitDown, None),
        KeyBinding::new("secondary-k secondary-left", FocusPaneLeft, None),
        KeyBinding::new("secondary-k secondary-right", FocusPaneRight, None),
        KeyBinding::new("secondary-k secondary-up", FocusPaneUp, None),
        KeyBinding::new("secondary-k secondary-down", FocusPaneDown, None),
        KeyBinding::new("secondary-=", IncreaseFontSize, None),
        KeyBinding::new("secondary-+", IncreaseFontSize, None),
        KeyBinding::new("secondary--", DecreaseFontSize, None),
        KeyBinding::new("secondary-0", ResetFontSize, None),
        // F5 runs (VS Code debugs; Kuana runs without the debugger), ⇧F5 stops.
        // F5 debugs (or continues); Ctrl+F5 runs without the debugger.
        KeyBinding::new("f5", Resume, None),
        KeyBinding::new("ctrl-f5", Rerun, None),
        KeyBinding::new("shift-f5", StopRun, None),
        KeyBinding::new("f9", ToggleBreakpoint, e),
        KeyBinding::new("f10", StepOver, None),
        KeyBinding::new("f11", StepInto, None),
        KeyBinding::new("shift-f11", StepOut, None),
        KeyBinding::new("f6", PauseProgram, None),
        KeyBinding::new("f12", GoToDeclaration, e),
        KeyBinding::new("secondary-f12", GoToImplementation, e),
        KeyBinding::new("secondary-t", GoToSymbol, None),
        KeyBinding::new("secondary-k v", ToggleMarkdownPreview, e),
        // The preview has it too: it closes there.
        KeyBinding::new("secondary-k v", ToggleMarkdownPreview, Some("MarkdownPreview")),
        KeyBinding::new("shift-f12", FindUsages, e),
        KeyBinding::new("alt-shift-f12", FindUsages, e),
        KeyBinding::new("shift-alt-h", CallHierarchy, e),
        // Through the references (or search results) listed.
        KeyBinding::new("f4", NextResult, None),
        KeyBinding::new("shift-f4", PreviousResult, None),
        KeyBinding::new("f2", RenameSymbol, e),
        KeyBinding::new("secondary-.", ShowContextActions, e),
        KeyBinding::new("shift-alt-f", ReformatCode, e),
        KeyBinding::new("secondary-k secondary-i", QuickDocumentation, e),
        KeyBinding::new("secondary-k secondary-0", CollapseAllRegions, e),
        KeyBinding::new("secondary-k secondary-j", ExpandAllRegions, e),
        KeyBinding::new("shift-alt-a", ToggleBlockComment, e),
        KeyBinding::new("shift-alt-down", DuplicateLine, e),
        KeyBinding::new("shift-alt-up", DuplicateLine, e),
        KeyBinding::new("alt-up", MoveLineUp, e),
        KeyBinding::new("alt-down", MoveLineDown, e),
        KeyBinding::new("secondary-shift-k", DeleteLine, e),
        KeyBinding::new("secondary-shift-enter", NewlineAbove, e),
        KeyBinding::new("shift-enter", Newline, e),
        KeyBinding::new("secondary-d", SelectNextOccurrence, e),
        KeyBinding::new("secondary-shift-l", SelectAllOccurrences, e),
        KeyBinding::new("secondary-u", UnselectOccurrence, e),
        KeyBinding::new("alt-z", ToggleSoftWrap, e),
        KeyBinding::new("alt-f5", NextChange, e),
        KeyBinding::new("shift-alt-f5", PreviousChange, e),
        // Go to Next / Previous Problem.
        KeyBinding::new("f8", NextProblem, e),
        KeyBinding::new("shift-f8", PreviousProblem, e),
    ];
    if mac {
        b.extend([
            KeyBinding::new("cmd-alt-f", Replace, None),
            KeyBinding::new("cmd-alt-right", NextTab, None),
            KeyBinding::new("cmd-alt-left", PrevTab, None),
            KeyBinding::new("cmd-alt-[", CollapseRegion, e),
            KeyBinding::new("cmd-alt-]", ExpandRegion, e),
            KeyBinding::new("cmd-alt-up", AddCursorAbove, e),
            KeyBinding::new("cmd-alt-down", AddCursorBelow, e),
            KeyBinding::new("ctrl-shift-cmd-right", ExtendSelection, e),
            KeyBinding::new("ctrl-shift-cmd-left", ShrinkSelection, e),
            KeyBinding::new("ctrl-j", JoinLines, e),
        ]);
    } else {
        b.extend([
            KeyBinding::new("ctrl-h", Replace, None),
            KeyBinding::new("ctrl-,", OpenSettings, None),
            KeyBinding::new("ctrl-w", CloseTab, None),
            KeyBinding::new("ctrl-pagedown", NextTab, None),
            KeyBinding::new("ctrl-pageup", PrevTab, None),
            KeyBinding::new("ctrl-shift-n", NewWindow, None),
            KeyBinding::new("ctrl-shift-w", CloseWindow, None),
            KeyBinding::new("ctrl-shift-[", CollapseRegion, e),
            KeyBinding::new("ctrl-shift-]", ExpandRegion, e),
            KeyBinding::new("ctrl-alt-up", AddCursorAbove, e),
            KeyBinding::new("ctrl-alt-down", AddCursorBelow, e),
            KeyBinding::new("shift-alt-right", ExtendSelection, e),
            KeyBinding::new("shift-alt-left", ShrinkSelection, e),
            // Linux's Format Document.
            KeyBinding::new("ctrl-shift-i", ReformatCode, e),
            KeyBinding::new("ctrl-y", Redo, e),
        ]);
    }
    b
}

/// Sublime Text's default key bindings.
fn sublime_text(mac: bool) -> Vec<KeyBinding> {
    let e = Some("Editor");
    let mut b = vec![
        KeyBinding::new("secondary-shift-p", FindAction, None),
        KeyBinding::new("secondary-p", GoToFile, None),
        KeyBinding::new("ctrl-g", GoToLine, None),
        KeyBinding::new("secondary-r", FileStructure, None),
        KeyBinding::new("secondary-k secondary-b", ToggleSidebar, None),
        // The console.
        KeyBinding::new("ctrl-`", ToggleTerminal, None),
        // Build and Build With….
        KeyBinding::new("secondary-b", Rerun, None),
        KeyBinding::new("secondary-shift-b", RunConfiguration, None),
        KeyBinding::new("secondary-=", IncreaseFontSize, None),
        KeyBinding::new("secondary-+", IncreaseFontSize, None),
        KeyBinding::new("secondary--", DecreaseFontSize, None),
        KeyBinding::new("f12", GoToDeclaration, e),
        KeyBinding::new("secondary-shift-r", GoToSymbol, None),
        KeyBinding::new("shift-f12", FindUsages, e),
        KeyBinding::new("secondary-d", SelectNextOccurrence, e),
        KeyBinding::new("secondary-u", UnselectOccurrence, e),
        // Expand Selection to Tag / to Brackets.
        KeyBinding::new("secondary-shift-a", ExtendSelection, e),
        KeyBinding::new("ctrl-shift-m", ExtendSelection, e),
        KeyBinding::new("secondary-shift-d", DuplicateLine, e),
        KeyBinding::new("ctrl-shift-k", DeleteLine, e),
        KeyBinding::new("secondary-j", JoinLines, e),
        KeyBinding::new("secondary-shift-enter", NewlineAbove, e),
        KeyBinding::new("shift-enter", Newline, e),
        KeyBinding::new("secondary-k secondary-u", ToggleCase, e),
        KeyBinding::new("secondary-k secondary-1", CollapseAllRegions, e),
        KeyBinding::new("secondary-k secondary-0", ExpandAllRegions, e),
        KeyBinding::new("secondary-k secondary-j", ExpandAllRegions, e),
        // Next / previous modification.
        KeyBinding::new("ctrl-.", NextChange, e),
        KeyBinding::new("ctrl-,", PreviousChange, e),
        // Next / previous result (build errors).
        KeyBinding::new("f4", NextProblem, e),
        KeyBinding::new("shift-f4", PreviousProblem, e),
    ];
    if mac {
        b.extend([
            KeyBinding::new("cmd-t", GoToFile, None),
            KeyBinding::new("cmd-alt-f", Replace, None),
            KeyBinding::new("cmd-alt-right", NextTab, None),
            KeyBinding::new("cmd-alt-left", PrevTab, None),
            KeyBinding::new("cmd-alt-2", SplitRight, None),
            KeyBinding::new("cmd-alt-shift-2", SplitDown, None),
            KeyBinding::new("ctrl-cmd-p", RecentProjects, None),
            KeyBinding::new("alt-cmd-down", GoToDeclaration, e),
            KeyBinding::new("alt-shift-cmd-down", FindUsages, e),
            KeyBinding::new("ctrl-cmd-g", SelectAllOccurrences, e),
            KeyBinding::new("ctrl-shift-up", AddCursorAbove, e),
            KeyBinding::new("ctrl-shift-down", AddCursorBelow, e),
            KeyBinding::new("ctrl-cmd-up", MoveLineUp, e),
            KeyBinding::new("ctrl-cmd-down", MoveLineDown, e),
            KeyBinding::new("cmd-alt-/", ToggleBlockComment, e),
            KeyBinding::new("cmd-alt-[", CollapseRegion, e),
            KeyBinding::new("cmd-alt-]", ExpandRegion, e),
        ]);
    } else {
        b.extend([
            KeyBinding::new("ctrl-h", Replace, None),
            KeyBinding::new("ctrl-w", CloseTab, None),
            KeyBinding::new("ctrl-pagedown", NextTab, None),
            KeyBinding::new("ctrl-pageup", PrevTab, None),
            KeyBinding::new("ctrl-shift-n", NewWindow, None),
            KeyBinding::new("ctrl-shift-w", CloseWindow, None),
            KeyBinding::new("alt-shift-2", SplitRight, None),
            KeyBinding::new("alt-shift-8", SplitDown, None),
            KeyBinding::new("ctrl-alt-p", RecentProjects, None),
            KeyBinding::new("alt-f3", SelectAllOccurrences, e),
            KeyBinding::new("ctrl-alt-up", AddCursorAbove, e),
            KeyBinding::new("ctrl-alt-down", AddCursorBelow, e),
            KeyBinding::new("ctrl-shift-up", MoveLineUp, e),
            KeyBinding::new("ctrl-shift-down", MoveLineDown, e),
            KeyBinding::new("ctrl-shift-/", ToggleBlockComment, e),
            KeyBinding::new("ctrl-shift-[", CollapseRegion, e),
            KeyBinding::new("ctrl-shift-]", ExpandRegion, e),
            KeyBinding::new("ctrl-y", Redo, e),
        ]);
    }
    b
}

/// Zed's default keymap.
fn zed(mac: bool) -> Vec<KeyBinding> {
    let e = Some("Editor");
    let mut b = vec![
        KeyBinding::new("secondary-shift-p", FindAction, None),
        KeyBinding::new("secondary-p", GoToFile, None),
        KeyBinding::new("ctrl-g", GoToLine, None),
        KeyBinding::new("secondary-shift-o", FileStructure, None),
        KeyBinding::new("secondary-shift-h", ReplaceInFiles, None),
        KeyBinding::new("secondary-b", ToggleSidebar, None),
        KeyBinding::new("secondary-shift-e", ToggleSidebar, None),
        KeyBinding::new("ctrl-shift-g", ShowCommit, None),
        KeyBinding::new("secondary-shift-m", ToggleProblems, None),
        KeyBinding::new("ctrl-`", ToggleTerminal, None),
        KeyBinding::new("ctrl-~", NewTerminal, None),
        KeyBinding::new("secondary-alt-,", OpenSettingsFile, None),
        KeyBinding::new("secondary-k secondary-s", OpenKeymap, None),
        KeyBinding::new("secondary-k secondary-t", SelectTheme, None),
        KeyBinding::new("ctrl-r", RecentProjects, None),
        KeyBinding::new("secondary-k right", SplitRight, None),
        KeyBinding::new("secondary-k down", SplitDown, None),
        KeyBinding::new("secondary-k secondary-left", FocusPaneLeft, None),
        KeyBinding::new("secondary-k secondary-right", FocusPaneRight, None),
        KeyBinding::new("secondary-k secondary-up", FocusPaneUp, None),
        KeyBinding::new("secondary-k secondary-down", FocusPaneDown, None),
        KeyBinding::new("secondary-=", IncreaseFontSize, None),
        KeyBinding::new("secondary-+", IncreaseFontSize, None),
        KeyBinding::new("secondary--", DecreaseFontSize, None),
        KeyBinding::new("secondary-0", ResetFontSize, None),
        KeyBinding::new("ctrl-g up", GitPush, None),
        KeyBinding::new("ctrl-g down", GitPull, None),
        KeyBinding::new("f12", GoToDeclaration, e),
        KeyBinding::new("shift-f12", GoToImplementation, e),
        KeyBinding::new("secondary-f12", GoToTypeDefinition, e),
        KeyBinding::new("secondary-t", GoToSymbol, None),
        KeyBinding::new("secondary-k v", ToggleMarkdownPreview, e),
        // The preview has it too: it closes there.
        KeyBinding::new("secondary-k v", ToggleMarkdownPreview, Some("MarkdownPreview")),
        KeyBinding::new("f5", Resume, None),
        KeyBinding::new("shift-f5", StopRun, None),
        KeyBinding::new("f9", ToggleBreakpoint, e),
        KeyBinding::new("f10", StepOver, None),
        KeyBinding::new("f11", StepInto, None),
        KeyBinding::new("shift-f11", StepOut, None),
        KeyBinding::new("f6", PauseProgram, None),
        KeyBinding::new("alt-shift-f12", FindUsages, e),
        KeyBinding::new("f2", RenameSymbol, e),
        KeyBinding::new("secondary-.", ShowContextActions, e),
        KeyBinding::new("secondary-shift-i", ReformatCode, e),
        KeyBinding::new("secondary-k secondary-i", QuickDocumentation, e),
        KeyBinding::new("secondary-k secondary-0", CollapseAllRegions, e),
        KeyBinding::new("secondary-k secondary-j", ExpandAllRegions, e),
        KeyBinding::new("secondary-k secondary-/", ToggleBlockComment, e),
        KeyBinding::new("alt-up", MoveLineUp, e),
        KeyBinding::new("alt-down", MoveLineDown, e),
        KeyBinding::new("secondary-shift-k", DeleteLine, e),
        KeyBinding::new("secondary-shift-enter", NewlineAbove, e),
        KeyBinding::new("shift-enter", Newline, e),
        KeyBinding::new("secondary-d", SelectNextOccurrence, e),
        KeyBinding::new("secondary-shift-l", SelectAllOccurrences, e),
        KeyBinding::new("secondary-u", UnselectOccurrence, e),
        KeyBinding::new("secondary-k z", ToggleSoftWrap, e),
        // Go to (previous) diagnostic.
        KeyBinding::new("f8", NextProblem, e),
        KeyBinding::new("shift-f8", PreviousProblem, e),
    ];
    if mac {
        b.extend([
            KeyBinding::new("cmd-alt-f", Replace, None),
            KeyBinding::new("alt-cmd-o", RecentProjects, None),
            KeyBinding::new("alt-cmd-right", NextTab, None),
            KeyBinding::new("alt-cmd-left", PrevTab, None),
            // Tasks: Rerun and Spawn.
            KeyBinding::new("cmd-alt-r", Rerun, None),
            KeyBinding::new("cmd-shift-r", RunConfiguration, None),
            KeyBinding::new("cmd-alt-[", CollapseRegion, e),
            KeyBinding::new("cmd-alt-]", ExpandRegion, e),
            KeyBinding::new("alt-shift-down", DuplicateLine, e),
            KeyBinding::new("alt-shift-up", DuplicateLine, e),
            KeyBinding::new("cmd-alt-up", AddCursorAbove, e),
            KeyBinding::new("cmd-alt-down", AddCursorBelow, e),
            KeyBinding::new("ctrl-cmd-right", ExtendSelection, e),
            KeyBinding::new("ctrl-cmd-left", ShrinkSelection, e),
            KeyBinding::new("ctrl-j", JoinLines, e),
            KeyBinding::new("cmd-f8", NextChange, e),
            KeyBinding::new("cmd-shift-f8", PreviousChange, e),
            KeyBinding::new("cmd-alt-z", RollbackHunk, e),
        ]);
    } else {
        b.extend([
            KeyBinding::new("ctrl-h", Replace, None),
            KeyBinding::new("ctrl-,", OpenSettings, None),
            KeyBinding::new("ctrl-w", CloseTab, None),
            KeyBinding::new("ctrl-pagedown", NextTab, None),
            KeyBinding::new("ctrl-pageup", PrevTab, None),
            KeyBinding::new("ctrl-shift-n", NewWindow, None),
            KeyBinding::new("ctrl-shift-w", CloseWindow, None),
            KeyBinding::new("ctrl-alt-o", RecentProjects, None),
            KeyBinding::new("ctrl-k ctrl-o", OpenFolder, None),
            KeyBinding::new("ctrl-alt-r", Rerun, None),
            KeyBinding::new("alt-shift-t", RunConfiguration, None),
            KeyBinding::new("ctrl-shift-[", CollapseRegion, e),
            KeyBinding::new("ctrl-shift-]", ExpandRegion, e),
            KeyBinding::new("ctrl-alt-shift-down", DuplicateLine, e),
            KeyBinding::new("ctrl-alt-shift-up", DuplicateLine, e),
            KeyBinding::new("shift-alt-up", AddCursorAbove, e),
            KeyBinding::new("shift-alt-down", AddCursorBelow, e),
            KeyBinding::new("alt-shift-right", ExtendSelection, e),
            KeyBinding::new("alt-shift-left", ShrinkSelection, e),
            KeyBinding::new("ctrl-shift-j", JoinLines, e),
            KeyBinding::new("ctrl-f8", NextChange, e),
            KeyBinding::new("ctrl-shift-f8", PreviousChange, e),
            KeyBinding::new("ctrl-k ctrl-r", RollbackHunk, e),
            KeyBinding::new("ctrl-y", Redo, e),
        ]);
    }
    b
}

/// Everything Find Action / Search Everywhere can run.
pub fn action_entries() -> Vec<ActionEntry> {
    fn a(name: &'static str, action: impl Action) -> ActionEntry {
        ActionEntry { name, action: Box::new(action) }
    }
    vec![
        a("Search Everywhere", SearchEverywhere),
        a("Find", Find),
        a("Replace", Replace),
        a("Find in Files", FindInFiles),
        a("Replace in Files", ReplaceInFiles),
        a("Find Next Occurrence", FindNext),
        a("Find Previous Occurrence", FindPrevious),
        a("Go to File", GoToFile),
        a("Welcome Setup", WelcomeSetup),
        a("Go to Line:Column", GoToLine),
        a("File Structure", FileStructure),
        a("Go to Declaration", GoToDeclaration),
        a("Go to Implementation", GoToImplementation),
        a("Go to Type Definition", GoToTypeDefinition),
        a("Go to Symbol", GoToSymbol),
        a("Split Terminal", SplitTerminal),
        a("Rename Terminal…", RenameTerminal),
        a("Select Opened File", SelectOpenedFile),
        a("Toggle Markdown Preview", ToggleMarkdownPreview),
        a("Debug at Caret", DebugAtCaret),
        a("Debug…", DebugConfiguration),
        a("Toggle Line Breakpoint", ToggleBreakpoint),
        a("Add Conditional Breakpoint…", AddConditionalBreakpoint),
        a("Add Logpoint…", AddLogpoint),
        a("Attach to Process…", AttachToProcess),
        a("Resume Program", Resume),
        a("Pause Program", PauseProgram),
        a("Step Over", StepOver),
        a("Step Into", StepInto),
        a("Step Out", StepOut),
        a("Debug Panel", ToggleDebugPanel),
        a("Install 'kuana' Shell Command", InstallShellCommand),
        a("Find Usages", FindUsages),
        a("Call Hierarchy", CallHierarchy),
        a("Type Hierarchy", TypeHierarchy),
        a("Rename…", RenameSymbol),
        a("Show Context Actions", ShowContextActions),
        a("Reformat Code", ReformatCode),
        a("Collapse", CollapseRegion),
        a("Expand", ExpandRegion),
        a("Collapse All", CollapseAllRegions),
        a("Expand All", ExpandAllRegions),
        a("Basic Completion", ShowCompletions),
        a("Quick Documentation", QuickDocumentation),
        a("Restart Language Server", RestartLanguageServer),
        a("Copilot: Sign In", CopilotSignIn),
        a("Copilot: Sign Out", CopilotSignOut),
        a("Show Edit Prediction", ShowEditPrediction),
        a("Terminal", ToggleTerminal),
        a("Run…", RunConfiguration),
        a("Run", Rerun),
        a("Run Context Configuration", RunAtCaret),
        a("Stop", StopRun),
        a("Split Right", SplitRight),
        a("Split Down", SplitDown),
        a("Next Pane", NextPane),
        a("Previous Pane", PreviousPane),
        a("Focus Pane Left", FocusPaneLeft),
        a("Focus Pane Right", FocusPaneRight),
        a("Focus Pane Above", FocusPaneUp),
        a("Focus Pane Below", FocusPaneDown),
        a("Close Other Tabs", CloseOtherTabs),
        a("Close All Tabs", CloseAllTabs),
        a("Pin / Unpin Tab", TogglePinTab),
        a("New Terminal", NewTerminal),
        a("Recent Files", RecentFiles),
        a("New File", NewFile),
        a("Open File…", OpenFile),
        a("Open Folder…", OpenFolder),
        a("Open Folder in This Window…", OpenFolderInThisWindow),
        a("Recent Projects…", RecentProjects),
        a("New Window", NewWindow),
        a("Close Window", CloseWindow),
        a("Minimize Window", MinimizeWindow),
        a("Zoom Window", ZoomWindow),
        a("About Kuana", About),
        a("Save", Save),
        a("Save As…", SaveAs),
        a("Toggle Auto Save", ToggleAutoSave),
        a("Close Tab", CloseTab),
        a("Next Tab", NextTab),
        a("Previous Tab", PrevTab),
        a("Switch to Recent Tab", RecentTab),
        a("Switch to Recent Tab (Backward)", RecentTabBack),
        a("Toggle Project Sidebar", ToggleSidebar),
        a("Toggle Sidebar Position", ToggleSidebarPosition),
        a("Toggle Classic Layout", ToggleClassicLayout),
        a("Toggle Problems", ToggleProblems),
        a("Toggle Find Panel", ToggleResults),
        a("Next Result", NextResult),
        a("Rerun Failed Tests", RerunFailedTests),
        a("Local History…", ShowLocalHistory),
        a("Previous Result", PreviousResult),
        a("Duplicate Line or Selection", DuplicateLine),
        a("Delete Line", DeleteLine),
        a("Join Lines", JoinLines),
        a("Move Line Up", MoveLineUp),
        a("Move Line Down", MoveLineDown),
        a("Start New Line", NewlineBelow),
        a("Start New Line Before Current", NewlineAbove),
        a("Comment with Line Comment", ToggleComment),
        a("Comment with Block Comment", ToggleBlockComment),
        a("Extend Selection", ExtendSelection),
        a("Shrink Selection", ShrinkSelection),
        a("Add Selection for Next Occurrence", SelectNextOccurrence),
        a("Unselect Occurrence", UnselectOccurrence),
        a("Select All Occurrences", SelectAllOccurrences),
        a("Add Caret Above", AddCursorAbove),
        a("Add Caret Below", AddCursorBelow),
        a("Toggle Case", ToggleCase),
        a("Convert to Upper Case", ConvertToUpperCase),
        a("Convert to Lower Case", ConvertToLowerCase),
        a("Convert to Title Case", ConvertToTitleCase),
        a("Convert to snake_case", ConvertToSnakeCase),
        a("Convert to kebab-case", ConvertToKebabCase),
        a("Convert to camelCase", ConvertToCamelCase),
        a("Convert to PascalCase", ConvertToPascalCase),
        a("Sort Lines", SortLinesAscending),
        a("Sort Lines Descending", SortLinesDescending),
        a("Reverse Lines", ReverseLines),
        a("Remove Duplicate Lines", RemoveDuplicateLines),
        a("Select All", SelectAll),
        a("Select Line", SelectLine),
        a("Undo", Undo),
        a("Redo", Redo),
        a("Commit…", ShowCommit),
        a("Toggle Commit Panel", ToggleCommit),
        a("Push…", GitPush),
        a("Update Project (Pull)", GitPull),
        a("Fetch", GitFetch),
        a("Branches…", GitBranches),
        a("Stash Changes…", GitStash),
        a("Manage Remotes…", ManageRemotes),
        a("Clone Repository…", GitClone),
        a("Get from Version Control…", GitClone),
        a("Initialize Git Repository", GitInit),
        a("Open Repository in Browser", OpenRemote),
        a("Git Log", ToggleGitLog),
        a("Show History for File", ShowFileHistory),
        a("Stage Lines", StageHunk),
        a("Unstage Lines", UnstageHunk),
        a("Accept Current Change", AcceptCurrent),
        a("Accept Incoming Change", AcceptIncoming),
        a("Accept Both Changes", AcceptBoth),
        a("Rollback Lines", RollbackHunk),
        a("Next Change", NextChange),
        a("Previous Change", PreviousChange),
        a("Next Problem", NextProblem),
        a("Previous Problem", PreviousProblem),
        a("Toggle Inline Git Blame", ToggleInlineBlame),
        a("Soft-Wrap", ToggleSoftWrap),
        a("Toggle Inlay Hints", ToggleInlayHints),
        a("Settings…", OpenSettings),
        a("Extensions…", OpenExtensions),
        a("Project Trust…", ProjectTrust),
        a("Undo File Change", UndoFileChange),
        a("Redo File Change", RedoFileChange),
        a("Reload Extensions", ReloadExtensions),
        a("Preferences…", OpenSettings),
        a("Check for Updates…", CheckForUpdates),
        a("Open settings.json", OpenSettingsFile),
        a("Open Project Settings (.kuana/settings.json)", OpenProjectSettings),
        a("Open keymap.json", OpenKeymap),
        a("Increase Font Size", IncreaseFontSize),
        a("Decrease Font Size", DecreaseFontSize),
        a("Reset Font Size", ResetFontSize),
        a("Select Theme…", SelectTheme),
        a("Open Themes Folder", OpenThemesFolder),
        a("Open Snippets Folder", OpenSnippetsFolder),
        a("Reload Themes", ReloadThemes),
        a("Quit", Quit),
    ]
}

// ---- User keymap -----------------------------------------------------------------

/// Marks bindings from `keymap.json`, so shortcut hints show them first.
pub const USER_BINDING: KeyBindingMetaIndex = KeyBindingMetaIndex(1);

pub fn keymap_path() -> Option<PathBuf> {
    config_dir().map(|d| d.join("keymap.json"))
}

/// One block of `keymap.json`: bindings, optionally limited to a context.
#[derive(Deserialize)]
struct Section {
    #[serde(default)]
    context: Option<String>,
    #[serde(default)]
    bindings: HashMap<String, serde_json::Value>,
}

/// Replaces all key bindings with the built-in keymap plus the user's
/// `keymap.json`. User bindings come later, so they win. Returns problems
/// with the user file; the bindings that are fine still apply.
pub fn install(cx: &mut App) -> Vec<String> {
    let base = current(cx);
    let (user, problems) = user_bindings(cx);
    let defaults = bindings(base);
    let passthrough = terminal_passthrough(&defaults);
    cx.clear_key_bindings();
    cx.bind_keys(defaults);
    cx.bind_keys(passthrough);
    cx.bind_keys(user);
    // Menus show shortcuts and observe this to pick up new ones.
    cx.set_global(Installed(base));
    problems
}

/// In the terminal, keys belong to the shell (Ctrl+R searches history, Ctrl+F
/// moves the cursor). This turns off the app's global shortcuts there, except
/// ⌘/Cmd ones and a few for getting around; JetBrains does the same.
fn terminal_passthrough(defaults: &[KeyBinding]) -> Vec<KeyBinding> {
    const KEEP: &[&str] = &[
        "workspace::ToggleTerminal",
        "workspace::NewTerminal",
        "workspace::ToggleGitLog",
        "workspace::NextTab",
        "workspace::PrevTab",
        "workspace::RecentTab",
        "workspace::RecentTabBack",
        "workspace::FindAction",
        "workspace::ToggleSidebar",
        "workspace::ToggleProblems",
        "workspace::Quit",
    ];
    // Keys the terminal itself uses (Ctrl+Shift+C/V/F) stay its own.
    let terminal_keys: Vec<String> =
        defaults.iter().filter(|b| b.predicate().is_some_and(|p| p.to_string() == "Terminal")).map(binding_keys).collect();
    defaults
        .iter()
        .filter(|b| b.predicate().is_none() && !KEEP.contains(&b.action().name()))
        .filter(|b| !b.keystrokes().iter().any(|k| k.inner().modifiers.platform))
        .filter(|b| !terminal_keys.contains(&binding_keys(b)))
        .map(|b| {
            let keys: Vec<String> = b.keystrokes().iter().map(|k| k.inner().unparse()).collect();
            KeyBinding::new(&keys.join(" "), NoAction, Some("Terminal"))
        })
        .collect()
}

fn user_bindings(cx: &App) -> (Vec<KeyBinding>, Vec<String>) {
    let mut out = Vec::new();
    let mut problems = Vec::new();
    let Some(text) = keymap_path().and_then(|p| std::fs::read_to_string(p).ok()) else {
        return (out, problems);
    };
    if text.trim().is_empty() {
        return (out, problems);
    }
    let sections: Vec<Section> = match serde_json::from_str(&strip_jsonc(&text)) {
        Ok(sections) => sections,
        Err(err) => return (out, vec![format!("keymap.json: {err}")]),
    };
    // Find Action's names work too, so nobody has to look up ids.
    let by_label: HashMap<&'static str, Box<dyn Action>> =
        action_entries().into_iter().map(|e| (e.name, e.action)).collect();
    for section in sections {
        let context = match section.context.as_deref().map(str::trim).filter(|c| !c.is_empty()) {
            Some(c) => match KeyBindingContextPredicate::parse(c) {
                Ok(predicate) => Some(Rc::new(predicate)),
                Err(err) => {
                    problems.push(format!("keymap.json: invalid context \"{c}\": {err}"));
                    continue;
                }
            },
            None => None,
        };
        for (keys, value) in section.bindings {
            let action: Box<dyn Action> = match &value {
                // null removes a built-in binding.
                serde_json::Value::Null => Box::new(NoAction),
                serde_json::Value::String(name) => match by_label.get(name.as_str()) {
                    Some(action) => action.boxed_clone(),
                    None => match cx.build_action(name, None) {
                        Ok(action) => action,
                        Err(_) => {
                            let names = by_label.keys().copied().chain(cx.all_action_names().iter().copied());
                            problems.push(match theme::closest(name, names) {
                                Some(near) => format!("keymap.json: unknown action \"{name}\" (did you mean \"{near}\"?)"),
                                None => format!("keymap.json: unknown action \"{name}\""),
                            });
                            continue;
                        }
                    },
                },
                _ => {
                    problems.push(format!("keymap.json: \"{keys}\" should map to an action name or null"));
                    continue;
                }
            };
            match KeyBinding::load(&keys, action, context.clone(), false, None, cx.keyboard_mapper().as_ref()) {
                Ok(binding) => out.push(binding.with_meta(USER_BINDING)),
                Err(err) => problems.push(format!("keymap.json: invalid keys \"{keys}\": {err}")),
            }
        }
    }
    (out, problems)
}

/// The comment at the top of `keymap.json`.
const KEYMAP_HEADER: &str = r#"// Kuana key bindings, added on top of the built-in keymap
// ("base_keymap" in settings.json: JetBrains, VSCode, SublimeText or Zed).
// Changes apply when you save this file.
//
// Keys: "cmd-shift-d", "ctrl-alt-l", "f5", or two strokes like "cmd-k cmd-s".
//       "secondary" means Cmd on macOS and Ctrl elsewhere.
// Actions: a name from Find Action ("Duplicate Line or Selection") or an
//          action id ("editor::DuplicateLine"). null removes a built-in binding.
// Context: "Editor" for the code editor; leave it out to bind everywhere
//          (bindings in a more specific context still win there).
"#;

/// Written by Open Keymap when there's no file yet.
pub const KEYMAP_TEMPLATE: &str = r#"// Kuana key bindings, added on top of the built-in keymap
// ("base_keymap" in settings.json: JetBrains, VSCode, SublimeText or Zed).
// Changes apply when you save this file.
//
// Keys: "cmd-shift-d", "ctrl-alt-l", "f5", or two strokes like "cmd-k cmd-s".
//       "secondary" means Cmd on macOS and Ctrl elsewhere.
// Actions: a name from Find Action ("Duplicate Line or Selection") or an
//          action id ("editor::DuplicateLine"). null removes a built-in binding.
// Context: "Editor" for the code editor; leave it out to bind everywhere
//          (bindings in a more specific context still win there).
[
  // {
  //   "context": "Editor",
  //   "bindings": {
  //     "secondary-shift-d": "Duplicate Line or Selection",
  //     "secondary-d": null
  //   }
  // }
]
"#;

// ---- Editing keymap.json (Preferences) --------------------------------------------

/// What Preferences does to an action's shortcut.
#[derive(Clone, Debug, PartialEq)]
pub enum ShortcutChange {
    /// Use these keys (`unparse`d, e.g. "cmd-shift-d") instead of the current ones.
    Set(String),
    /// Leave the action without a shortcut.
    Remove,
    /// Back to the built-in shortcuts.
    Reset,
}

/// The keys of a binding as written in `keymap.json`.
pub fn binding_keys(binding: &KeyBinding) -> String {
    binding.keystrokes().iter().map(|k| k.unparse()).collect::<Vec<_>>().join(" ")
}

/// Changes an action's shortcut in `keymap.json`. `name` is its Find Action
/// name, which is what gets written. The file is rewritten (its header
/// comment is kept); a file that doesn't parse is left alone.
pub fn change_shortcut(name: &str, action: &dyn Action, change: ShortcutChange, cx: &App) -> Result<(), String> {
    let path = keymap_path().ok_or("no config directory")?;
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let mut sections: Vec<Value> = if text.trim().is_empty() {
        Vec::new()
    } else {
        serde_json::from_str(&strip_jsonc(&text)).map_err(|e| format!("keymap.json has an error: {e}"))?
    };
    let by_label: HashMap<&'static str, Box<dyn Action>> =
        action_entries().into_iter().map(|e| (e.name, e.action)).collect();
    let refers = |value: &str| match by_label.get(value) {
        Some(a) => a.partial_eq(action),
        None => cx.build_action(value, None).is_ok_and(|a| a.partial_eq(action)),
    };
    let defaults: Vec<(String, Option<String>)> = bindings(current(cx))
        .iter()
        .filter(|b| b.action().partial_eq(action))
        .map(|b| (binding_keys(b), b.predicate().map(|p| p.to_string())))
        .collect();
    apply_shortcut_change(&mut sections, refers, name, &defaults, &change);
    let json = serde_json::to_string_pretty(&sections).map_err(|e| e.to_string())?;
    crate::state::write_atomic(&path, format!("{KEYMAP_HEADER}{json}\n").as_bytes()).map_err(|e| e.to_string())
}

/// The bindings object of the section for `context`, added when missing.
fn section_bindings<'a>(sections: &'a mut Vec<Value>, context: Option<&str>) -> &'a mut serde_json::Map<String, Value> {
    let matches = |section: &Value| {
        let found = section.get("context").and_then(Value::as_str).map(str::trim).filter(|c| !c.is_empty());
        found == context
    };
    let ix = match sections.iter().position(matches) {
        Some(ix) => ix,
        None => {
            let mut section = serde_json::Map::new();
            if let Some(context) = context {
                section.insert("context".into(), context.into());
            }
            sections.push(Value::Object(section));
            sections.len() - 1
        }
    };
    let section = sections[ix].as_object_mut().expect("sections are objects");
    if !section.get("bindings").is_some_and(Value::is_object) {
        section.insert("bindings".into(), Value::Object(Default::default()));
    }
    section.get_mut("bindings").and_then(Value::as_object_mut).expect("inserted above")
}

/// The edit behind [`change_shortcut`]: `refers` tells whether an action name
/// in the file means this action, `defaults` are its built-in keys and contexts.
fn apply_shortcut_change(
    sections: &mut Vec<Value>,
    refers: impl Fn(&str) -> bool,
    name: &str,
    defaults: &[(String, Option<String>)],
    change: &ShortcutChange,
) {
    sections.retain(Value::is_object);
    // The user's own bindings for the action go; the change decides what's next.
    for section in sections.iter_mut() {
        if let Some(bindings) = section.get_mut("bindings").and_then(Value::as_object_mut) {
            bindings.retain(|_, v| !v.as_str().is_some_and(&refers));
        }
    }
    let new_keys = match change {
        ShortcutChange::Set(keys) => Some(keys.as_str()),
        _ => None,
    };
    for (keys, context) in defaults {
        let bindings = section_bindings(sections, context.as_deref());
        if *change == ShortcutChange::Reset || new_keys == Some(keys.as_str()) {
            // Bring the built-in binding back if it was turned off.
            if bindings.get(keys).is_some_and(Value::is_null) {
                bindings.shift_remove(keys);
            }
        } else if !bindings.contains_key(keys) {
            bindings.insert(keys.clone(), Value::Null);
        }
    }
    if let Some(keys) = new_keys
        && !defaults.iter().any(|(k, _)| k == keys)
    {
        // Where the built-in binding lives, so it applies in the same places.
        let context = defaults.first().and_then(|(_, c)| c.as_deref());
        section_bindings(sections, context).insert(keys.to_string(), name.into());
    }
    sections.retain(|s| s.get("bindings").and_then(Value::as_object).is_none_or(|b| !b.is_empty()));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a keystroke runs in the innermost of `contexts`, as GPUI resolves it.
    fn resolve(base: BaseKeymap, keys: &str, contexts: &[&str]) -> Option<&'static str> {
        let defaults = bindings(base);
        let mut all = defaults.clone();
        all.extend(terminal_passthrough(&defaults));
        let keymap = gpui::Keymap::new(all);
        let stack: Vec<gpui::KeyContext> = contexts.iter().map(|c| gpui::KeyContext::parse(c).unwrap()).collect();
        let input = [gpui::Keystroke::parse(keys).unwrap()];
        keymap.bindings_for_input(&input, &stack).0.first().map(|b| b.action().name())
    }

    #[test]
    fn context_bindings_win_where_they_apply() {
        if !cfg!(target_os = "macos") {
            return;
        }
        for base in [BaseKeymap::JetBrains, BaseKeymap::VSCode, BaseKeymap::SublimeText, BaseKeymap::Zed] {
            // A keymap's own ⌘T (Update Project, Go to Symbol, Go to File) stays outside the terminal.
            assert_eq!(resolve(base, "cmd-t", &["Workspace", "Terminal"]), Some("workspace::NewTerminal"), "{base:?}");
            assert_ne!(resolve(base, "cmd-t", &["Workspace", "Editor"]), Some("workspace::NewTerminal"), "{base:?}");
            assert_eq!(resolve(base, "cmd-k", &["Workspace", "Terminal"]), Some("terminal::TerminalClear"), "{base:?}");
            assert_eq!(resolve(base, "cmd-w", &["Workspace", "Terminal"]), Some("workspace::CloseTerminal"), "{base:?}");
        }
        assert_eq!(resolve(BaseKeymap::JetBrains, "cmd-t", &["Workspace", "Editor"]), Some("workspace::GitPull"));
        // JetBrains' F7: Next Change in a diff, Step Into elsewhere.
        assert_eq!(resolve(BaseKeymap::JetBrains, "f7", &["Workspace", "DiffView"]), Some("editor::NextChange"));
        assert_eq!(resolve(BaseKeymap::JetBrains, "f7", &["Workspace", "Editor"]), Some("workspace::StepInto"));
    }
    #[test]
    fn problem_keys_follow_each_editor() {
        let editor = &["Workspace", "Editor"];
        for (base, next, previous) in [
            (BaseKeymap::JetBrains, "f2", "shift-f2"),
            (BaseKeymap::VSCode, "f8", "shift-f8"),
            (BaseKeymap::Zed, "f8", "shift-f8"),
            (BaseKeymap::SublimeText, "f4", "shift-f4"),
        ] {
            assert_eq!(resolve(base, next, editor), Some("editor::NextProblem"), "{base:?}");
            assert_eq!(resolve(base, previous, editor), Some("editor::PreviousProblem"), "{base:?}");
        }
        // F8 stays JetBrains' Step Over, F2 VS Code's and Zed's Rename.
        assert_eq!(resolve(BaseKeymap::JetBrains, "f8", editor), Some("workspace::StepOver"));
        assert_eq!(resolve(BaseKeymap::VSCode, "f2", editor), Some("editor::RenameSymbol"));
        assert_eq!(resolve(BaseKeymap::Zed, "f2", editor), Some("editor::RenameSymbol"));
    }

    use serde_json::json;

    fn layers(base: BaseKeymap, mac: bool) -> Vec<KeyBinding> {
        let mut b = common(mac);
        b.extend(match base {
            BaseKeymap::JetBrains => jetbrains(mac),
            BaseKeymap::VSCode => vscode(mac),
            BaseKeymap::SublimeText => sublime_text(mac),
            BaseKeymap::Zed => zed(mac),
        });
        b
    }

    #[test]
    fn keymaps_have_no_conflicts_and_cover_the_essentials() {
        let essentials: [(&str, Box<dyn Action>); 8] = [
            ("Find Action", Box::new(FindAction)),
            ("Go to File", Box::new(GoToFile)),
            ("Go to Line", Box::new(GoToLine)),
            ("Terminal", Box::new(ToggleTerminal)),
            ("Go to Declaration", Box::new(GoToDeclaration)),
            ("Duplicate Line", Box::new(DuplicateLine)),
            ("Delete Line", Box::new(DeleteLine)),
            ("Select Next Occurrence", Box::new(SelectNextOccurrence)),
        ];
        for base in BaseKeymap::ALL {
            // GPUI turns `secondary` into the host's ⌘ or Ctrl, so only the
            // host's variant can be checked (CI runs on macOS, Linux and Windows).
            {
                let mac = cfg!(target_os = "macos");
                let bindings = layers(base, mac);
                // The same keys in the same context must mean one thing.
                let mut seen: HashMap<(String, String), &'static str> = HashMap::new();
                for b in &bindings {
                    let place = (binding_keys(b), b.predicate().map(|p| p.to_string()).unwrap_or_default());
                    if let Some(other) = seen.insert(place.clone(), b.action().name()) {
                        assert_eq!(other, b.action().name(), "{base:?} (mac: {mac}): {place:?} is bound twice");
                    }
                }
                for (name, action) in &essentials {
                    assert!(
                        bindings.iter().any(|b| b.action().partial_eq(action.as_ref())),
                        "{base:?} (mac: {mac}) has no shortcut for {name}"
                    );
                }
            }
        }
    }

    #[test]
    fn edits_shortcuts() {
        let refers = |v: &str| v == "Duplicate" || v == "editor::DuplicateLine";
        let defaults = vec![("cmd-d".to_string(), Some("Editor".to_string()))];
        let mut sections = vec![json!({"bindings": {"f5": "Run", "cmd-1": "editor::DuplicateLine"}})];

        let set = ShortcutChange::Set("cmd-shift-d".into());
        apply_shortcut_change(&mut sections, refers, "Duplicate", &defaults, &set);
        assert_eq!(
            sections,
            vec![
                json!({"bindings": {"f5": "Run"}}),
                json!({"context": "Editor", "bindings": {"cmd-d": null, "cmd-shift-d": "Duplicate"}}),
            ]
        );

        // Setting the built-in keys again needs no entries of its own.
        let back = ShortcutChange::Set("cmd-d".into());
        apply_shortcut_change(&mut sections, refers, "Duplicate", &defaults, &back);
        assert_eq!(sections, vec![json!({"bindings": {"f5": "Run"}})]);

        apply_shortcut_change(&mut sections, refers, "Duplicate", &defaults, &ShortcutChange::Remove);
        assert_eq!(sections[1], json!({"context": "Editor", "bindings": {"cmd-d": null}}));
        apply_shortcut_change(&mut sections, refers, "Duplicate", &defaults, &ShortcutChange::Reset);
        assert_eq!(sections, vec![json!({"bindings": {"f5": "Run"}})]);

        // An action without built-in keys binds everywhere.
        let mut sections = Vec::new();
        apply_shortcut_change(&mut sections, |_| false, "Git Log", &[], &ShortcutChange::Set("f9".into()));
        assert_eq!(sections, vec![json!({"bindings": {"f9": "Git Log"}})]);
    }
}

