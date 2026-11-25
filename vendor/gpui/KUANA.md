# GPUI 0.2.2, patched for Kuana

This is GPUI 0.2.2 from crates.io (its `src`, `resources`, `build.rs`, manifest and
license; the examples and tests aren't copied, and their tables are taken out of
`Cargo.toml`), patched in through `[patch.crates-io]` in Kuana's `Cargo.toml`.

## Changes

In `src/window.rs`, backported from Zed's GPUI:

- **Presenting after input** (zed-industries/zed#45369, merged December 2025, after
  0.2.2 was published). GPUI kept re-rendering the last frame on every display
  refresh for one second after *any* input event, mouse moves over a still window
  included, so the display wouldn't lower its refresh rate (ProMotion). That's a
  full GPU render of the window up to 120 times a second while nothing changes.
  Now `InputRateTracker` only counts input that invalidated something, and only
  keeps presenting when such input arrives at 60 events a second or more (6 in
  100 ms: scrolling, dragging), for a second after the last burst.
- **Which input changed something**: `WindowInvalidator::update_count`, bumped by
  `invalidate_view` and `set_dirty(true)`, compared before and after
  `dispatch_event` (as Zed does now), so input still counts while a redraw is
  already pending.

Kuana's own, in `src/platform/mac/window.rs`, `src/platform/mac/display_link.rs`,
`src/platform.rs` and `src/window.rs`:

- **A display link that rests** (macOS). GPUI's `CVDisplayLink` fires on every
  display refresh while the window is visible and wakes the main thread to ask
  whether anything needs drawing, even when nothing does: about 0.5–1 % CPU for a
  still window at 120 Hz. Now `step` counts refreshes without a frame presented
  (`MacWindowState::presented`, set in `draw`) and after `IDLE_TICKS_BEFORE_PAUSE`
  (60) stops the link (`DisplayLink::start`/`stop` are idempotent now, tracked by
  `running`, so dispatch suspends and resumes balance). The window wakes it again
  through `PlatformWindow::frame_waker` (a default method, `None` elsewhere), which
  `WindowInvalidator` calls whenever the window becomes dirty (`set_dirty(true)`,
  `invalidate_view`) and `Window::on_next_frame` calls through `wake`. The waker
  also sets `woken`, and a refresh in which it was called doesn't count as idle,
  so a window marked dirty on the refresh that would have paused still draws.

- **`TextSystem::has_font`** (`src/text_system.rs`): whether one family loads, by
  name. `theme::apply_fonts` used `all_font_names`, which on macOS lists every
  installed family and took ~100 ms of each start, before the window showed.
  Unlike `font_id`, it keeps no misses (and drops one already kept), so a font
  installed while Kuana runs is found on the next settings change.

- **Truncated text with several runs** (`src/elements/text.rs`): a text cut with
  an ellipsis (`text_ellipsis`, `truncate`) shortens its runs to the cut text, and
  the measuring closure kept those for its next measurement of the whole text
  (taffy measures a text more than once a frame, at different widths), so the
  text and its runs disagreed, shaping failed and the text came out empty. Each
  measurement now cuts a copy of the runs.

And in `src/taffy.rs`, two float literals are typed (`0.0_f32`, `1.0_f32`): a path
dependency's warnings aren't silenced as a registry crate's are, and Kuana's build
stays warning-free.

## Updating

When Kuana moves to a GPUI release that has these changes (any published after
December 2025), delete this folder and the `gpui` line under `[patch.crates-io]`.
Until then, a new 0.2.x copy needs the same edits (search for `input_rate_tracker`
and `update_count`). Check whether a newer GPUI rests its display link before
dropping the display link change; otherwise carry it over (search for `Kuana:`).
