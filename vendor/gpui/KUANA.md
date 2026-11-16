# GPUI 0.2.2, patched for Kuana

This is GPUI 0.2.2 from crates.io (its `src`, `resources`, `build.rs`, manifest and
license; the examples and tests aren't copied, and their tables are taken out of
`Cargo.toml`), patched in through `[patch.crates-io]` in Kuana's `Cargo.toml`.

## Changes

All in `src/window.rs`, backported from Zed's GPUI:

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

And in `src/taffy.rs`, two float literals are typed (`0.0_f32`, `1.0_f32`): a path
dependency's warnings aren't silenced as a registry crate's are, and Kuana's build
stays warning-free.

## Updating

When Kuana moves to a GPUI release that has these changes (any published after
December 2025), delete this folder and the `gpui` line under `[patch.crates-io]`.
Until then, a new 0.2.x copy needs the same edits (search for `input_rate_tracker`
and `update_count`).
