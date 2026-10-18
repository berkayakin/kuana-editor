# Key bindings

Kuana starts from the JetBrains IDE keymaps: the macOS keymap on macOS and the
Windows/Linux keymap elsewhere. On macOS the Ctrl variants of Find/Replace
(Ctrl+F, Ctrl+R, Ctrl+Shift+F, Ctrl+Shift+R) work too. Every action is also
reachable through Find Action, which shows its shortcut.

To use other shortcuts, pick a **Base keymap** in Settings → Keymap (or set `"base_keymap"` in
`settings.json`): **JetBrains** (the default), **VS Code**, **Sublime Text** or **Zed**, each in its
macOS or Windows/Linux variant. Shortcuts you change yourself
(`keymap.json`) stay on top of whichever you pick, and menus and Find Action show the current keys.
The table below is the JetBrains keymap.

| Action | macOS | Windows / Linux |
| --- | --- | --- |
| Search Everywhere | Double ⇧ | Double Shift |
| Go to File | ⌘⇧O | Ctrl+Shift+N |
| Recent Files | ⌘E | Ctrl+E |
| Find Action | ⌘⇧A | Ctrl+Shift+A |
| Go to Line:Column | ⌘L | Ctrl+G |
| File Structure | ⌘F12 | Ctrl+F12 |
| Find / Replace | ⌘F / ⌘R | Ctrl+F / Ctrl+R |
| Next / previous occurrence | ⌘G / ⌘⇧G, F3 / ⇧F3 | F3 / Shift+F3 |
| Find / Replace in Files | ⌘⇧F / ⌘⇧R | Ctrl+Shift+F / Ctrl+Shift+R |
| Extend / shrink selection | ⌥↑ / ⌥↓ | Ctrl+W / Ctrl+Shift+W |
| Add next occurrence / unselect / select all | ⌃G / ⌃⇧G / ⌃⌘G | Alt+J / Alt+Shift+J / Ctrl+Alt+Shift+J |
| Duplicate / delete line | ⌘D / ⌘⌫ | Ctrl+D / Ctrl+Y |
| Move line up / down | ⌥⇧↑ / ⌥⇧↓ | Alt+Shift+↑ / ↓ |
| Join lines | ⌃⇧J | Ctrl+Shift+J |
| Line / block comment | ⌘/ / ⌥⌘/ | Ctrl+/ / Ctrl+Shift+/ |
| Toggle case | ⌘⇧U | Ctrl+Shift+U |
| Start new line / line above | ⇧↩ / ⌥⌘↩ | Shift+Enter / Ctrl+Alt+Enter |
| Add caret above / below | ⌥⌘↑ / ⌥⌘↓ | Ctrl+Alt+↑ / ↓ |
| Project sidebar / Problems | ⌘1 / ⌘6 | Alt+1 / Alt+6 |
| Terminal (open, focus, hide) | ⌥F12 | Alt+F12 |
| Run… / Run again | ⌃⌥R / ⌃R | Alt+Shift+F10 / Shift+F10 |
| Run test or function at caret | ⌃⇧R | Ctrl+Shift+F10 |
| Stop | ⌘F2 | Ctrl+F2 |
| Debug… / Debug at caret | ⌃⌥D / ⌃⇧D | Alt+Shift+F9 / Ctrl+Shift+F9 |
| Toggle line breakpoint / conditional breakpoint | ⌘F8 / ⇧⌘F8 | Ctrl+F8 / Ctrl+Shift+F8 |
| Attach to Process… | ⌥⇧F5 | Ctrl+Alt+F5 |
| Resume / Step Over / Into / Out | ⌥⌘R / F8 / F7 / ⇧F8 | F9 / F8 / F7 / Shift+F8 |
| Split Right / Down | ⌘\\ / ⌘⇧\\ | Ctrl+\\ / Ctrl+Shift+\\ |
| Next pane | ⌥⇥ | — |
| New / close terminal tab (in the terminal) | ⌘T / ⌘W | Ctrl+Shift+T / Ctrl+Shift+W |
| Copy / paste / clear (in the terminal) | ⌘C / ⌘V / ⌘K | Ctrl+Shift+C / Ctrl+Shift+V / — |
| Find (in the terminal) | ⌘F | Ctrl+Shift+F |
| Close tab / next / previous | ⌘W / ⌃→ / ⌃← | Ctrl+F4 / Alt+→ / Alt+← |
| Last used tab (keep ⌃ down and press ⇥ again for older ones) | ⌃⇥ / ⌃⇧⇥ | Ctrl+Tab / Ctrl+Shift+Tab |
| Open file / folder | ⌘O / ⌥⌘O | Ctrl+O / Ctrl+Alt+O |
| New window / close window | ⌘⇧N / ⌘⇧W | — / Alt+F4 |
| Select theme | ⌃\` | Ctrl+\` |
| Commit panel / toggle it | ⌘K / ⌘0 | Ctrl+K / Alt+0 |
| Push / Update Project (pull) | ⌘⇧K / ⌘T | Ctrl+Shift+K / Ctrl+T |
| Rollback lines (change under caret) | ⌥⌘Z | Ctrl+Alt+Z |
| Next / previous change | ⌃⌥⇧↓ / ⌃⌥⇧↑ | Ctrl+Alt+Shift+↓ / ↑ |
| Next / previous problem (VS Code, Zed: F8 / ⇧F8; Sublime Text: F4 / ⇧F4) | F2 / ⇧F2 | F2 / Shift+F2 |
| Git log | ⌘9 | Alt+9 |
| Next / previous change (in a diff) | F7 / ⇧F7 | F7 / Shift+F7 |
| Settings | ⌘, | Ctrl+Alt+S |
| Increase / decrease font size | ⌥⌘= / ⌥⌘- | Ctrl+Alt+= / Ctrl+Alt+- |
| Collapse / expand (fold) | ⌘- / ⌘= | Ctrl+- / Ctrl+= |
| Collapse all / expand all | ⌘⇧- / ⌘⇧= | Ctrl+Shift+- / Ctrl+Shift+= |
| Basic completion | ⌃Space | Ctrl+Space |
| Go to Declaration (on the declaration itself: its usages) | ⌘B or ⌘-click | Ctrl+B or Ctrl+click |
| Go to Implementation | ⌥⌘B | Ctrl+Alt+B |
| Go to Type Definition | ⌃⇧B | Ctrl+Shift+B |
| Go to Symbol (project) | Find Action | Ctrl+Alt+Shift+N |
| Find Usages (in the Find panel) | ⌥F7 | Alt+F7 |
| Find panel | ⌘3 | Alt+3 |
| Next / previous result in the Find panel (VS Code: F4 / ⇧F4) | Find Action | Find Action |
| Call Hierarchy / Type Hierarchy (VS Code: ⇧⌥H for Call Hierarchy) | ⌃⌥H / ⌃H | Ctrl+Alt+H / Ctrl+H |
| Rename | ⇧F6 | Shift+F6 |
| Show Context Actions | ⌥↩ | Alt+Enter |
| Quick Documentation | F1 | Ctrl+Q |
| Reformat Code | ⌥⌘L | Ctrl+Alt+L |

### Your own bindings

Change a shortcut in **Settings → Keymap**: click an action and press the new keys (Esc
cancels, **No Shortcut** removes it, the undo button restores the built-in keys). These go into
`keymap.json` in the config folder, which **Open keymap.json** (on that page and in Find Action)
opens for editing by hand. Its bindings are added on top of the built-in keymap, apply when you
save, and show in menus and Find Action:

```jsonc
[
  {
    "context": "Editor",
    "bindings": {
      "secondary-shift-d": "Duplicate Line or Selection", // a Find Action name…
      "f6": "editor::DeleteLine",                          // …or an action id
      "secondary-d": null                                  // null removes a built-in binding
    }
  }
]
```

`secondary` is ⌘ on macOS and Ctrl elsewhere. Leave out `context` to bind everywhere; a binding
in a more specific context (like `"Editor"`) still wins there. Mistakes show up as a notification.
