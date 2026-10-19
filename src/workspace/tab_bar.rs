//! Browser-like tab strips: fitting tabs and their shape.

use super::*;

/// How a tab fits its strip.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) enum TabSlot {
    /// As wide as its name needs.
    Natural,
    /// This wide, the name shortened to fit.
    Fixed(f32),
    /// This wide, the icon and as much of the name as fits, without the
    /// close button (it still closes with a middle click or its menu).
    Compact(f32),
    /// This wide, just the icon.
    Icon(f32),
    /// In the overflow menu.
    Hidden,
}


/// Fits tabs whose names want `naturals` widths into `available`: as they
/// are when they fit; else only the widest shrink, all to the same limit
/// (short names stay whole); else the current tab keeps its name and the
/// others show their icons; and when even those don't fit, the tabs farthest
/// from the current one go to the overflow menu. The current tab is never
/// hidden. Widths are whole points, so tab edges stay on pixels.
pub(super) fn fit_tabs(naturals: &[f32], active: usize, available: f32) -> Vec<TabSlot> {
    let n = naturals.len();
    if n == 0 || naturals.iter().sum::<f32>() <= available {
        return vec![TabSlot::Natural; n];
    }
    let limit = shared_limit(naturals, available);
    if limit >= TAB_NAMED_MIN {
        return naturals.iter().map(|&w| if w <= limit { TabSlot::Natural } else { TabSlot::Fixed(limit) }).collect();
    }
    let active_width = naturals[active].clamp(TAB_ICON_WIDTH, TAB_ACTIVE_MIN).floor();
    let others = (n - 1) as f32;
    let mut slots = vec![TabSlot::Hidden; n];
    slots[active] = TabSlot::Fixed(active_width);
    // The others drop their close buttons and share what's left the same way,
    // keeping as much of their names as fits.
    let compact: Vec<f32> =
        (0..n).filter(|&i| i != active).map(|i| naturals[i] - (TAB_CHROME - TAB_COMPACT_CHROME)).collect();
    let compact_limit = shared_limit(&compact, available - active_width);
    if n > 1 && compact_limit >= TAB_COMPACT_MIN {
        for (ix, slot) in slots.iter_mut().enumerate() {
            if ix != active {
                let wanted = naturals[ix] - (TAB_CHROME - TAB_COMPACT_CHROME);
                *slot = TabSlot::Compact(wanted.min(compact_limit).floor());
            }
        }
        return slots;
    }
    if n == 1 || active_width + others * TAB_ICON_WIDTH <= available {
        let width = ((available - active_width) / others.max(1.)).min(TAB_NAMED_MIN).floor();
        for (ix, slot) in slots.iter_mut().enumerate() {
            if ix != active {
                *slot = TabSlot::Icon(width);
            }
        }
        return slots;
    }
    // Room for this many icons beside the current tab and the overflow button,
    // taken nearest the current tab first; they share all of that room, so
    // they keep the start of their names when it allows.
    let space = available - active_width - TAB_OVERFLOW_WIDTH;
    let room = (space / TAB_ICON_WIDTH).max(0.) as usize;
    let width = if room > 0 { (space / room as f32).min(TAB_NAMED_MIN).floor() } else { 0. };
    let mut shown = 0;
    for distance in 1..n {
        for ix in [active.checked_sub(distance), Some(active + distance)].into_iter().flatten() {
            if ix < n && shown < room {
                let wanted = naturals[ix] - (TAB_CHROME - TAB_COMPACT_CHROME);
                slots[ix] = if width >= TAB_COMPACT_MIN { TabSlot::Compact(width.min(wanted).floor()) } else { TabSlot::Icon(width) };
                shown += 1;
            }
        }
    }
    slots
}


/// Remembers an area's bounds for the next frame's layout (tab strips fit
/// their tabs to them), and asks for that frame when they changed, or a
/// resize would leave the tabs fitted to the old size. Called while drawing,
/// where `window.refresh()` is ignored: the next frame redraws the view being
/// drawn (a cached one wouldn't be drawn again otherwise).
pub(super) fn track_bounds(cell: &std::cell::Cell<Bounds<Pixels>>, bounds: Bounds<Pixels>, window: &mut Window) {
    if cell.get() != bounds {
        cell.set(bounds);
        window.request_animation_frame();
    }
}


/// Each tab name's width (in the current tab's medium weight, the wider one)
/// and character count, so tabs are as wide as their names and add up exactly.
pub(super) fn measure_tab_names(names: &[String], window: &Window, theme: &Theme) -> Vec<(f32, usize)> {
    let font = gpui::Font { weight: FontWeight::MEDIUM, ..gpui::font(theme.ui_font.clone()) };
    names
        .iter()
        .map(|name| {
            let run = gpui::TextRun {
                len: name.len(),
                font: font.clone(),
                color: theme.text.into(),
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            let width = window.text_system().shape_line(name.clone().into(), px(theme.ui_font_size), &[run], None).width;
            (f32::from(width), name.chars().count().max(1))
        })
        .collect()
}


/// A tab's name as its slot shows it: shortened only when the slot is
/// narrower than the name needs, by the name's own average character width.
pub(super) fn tab_label(name: &str, slot: TabSlot, natural: f32, (text, len): (f32, usize)) -> String {
    let per_char = text / len as f32;
    match slot {
        TabSlot::Fixed(w) if w < natural => ellipsize(name, ((w - (natural - text)) / per_char).max(1.) as usize),
        TabSlot::Compact(w) if w - TAB_COMPACT_CHROME < text => {
            ellipsize(name, ((w - TAB_COMPACT_CHROME) / per_char).max(1.) as usize)
        }
        _ => name.to_string(),
    }
}


/// A tab's width and padding for its slot.
pub(super) fn tab_slot_width(el: Stateful<gpui::Div>, slot: TabSlot, natural: f32) -> Stateful<gpui::Div> {
    match slot {
        TabSlot::Natural => el.w(px(natural)).pl_3().pr_1p5(),
        TabSlot::Fixed(w) | TabSlot::Compact(w) => el.w(px(w)).pl_3().pr_1p5(),
        TabSlot::Icon(w) => el.w(px(w)).justify_center(),
        TabSlot::Hidden => el,
    }
}


/// The widest any of `widths` may be so they add up to `available`: the
/// narrowest keep their widths while what's left, shared by the rest, is still
/// less than they want. Whole points; infinite when they all fit.
pub(super) fn shared_limit(widths: &[f32], available: f32) -> f32 {
    let mut sorted = widths.to_vec();
    sorted.sort_by(f32::total_cmp);
    let mut remaining = available;
    for (i, width) in sorted.iter().enumerate() {
        let rest = (sorted.len() - i) as f32;
        if width * rest > remaining {
            return (remaining / rest).floor();
        }
        remaining -= width;
    }
    f32::INFINITY
}


/// A browser-like tab's shape, around a tab of `TAB_STRIP_HEIGHT - TAB_STRIP_TOP`
/// on the chrome: the current one island-colored, rounded on top, outlined in
/// the island's border color `edge` down to its feet, which join it to the
/// island; the others plain, with a hairline after them (`separator`, hidden
/// while the tab's `group` is hovered) and, fading in on hover, the same shape,
/// borderless, the chrome moved toward the text color (lighter on dark themes,
/// darker on light ones: the chrome and islands are too close in color for a
/// shade between them to show), stopping above the island's border.
///
/// Tabs paint left to right, so the hover shape of the tab right after the
/// current one would cover the current tab's right foot: it repaints that
/// foot on top (`after_active`).
pub(super) fn tab_shape(
    el: Stateful<gpui::Div>,
    active: bool,
    after_active: bool,
    separator: bool,
    group: SharedString,
    edge: Hsla,
    theme: &Theme,
) -> Stateful<gpui::Div> {
    let island: Hsla = theme.background.into();
    if active {
        // Painted, not laid out: every edge comes from the same pixel-snapped
        // coordinates, so the outline and the feet meet exactly wherever the
        // tab lands (separate elements round to device pixels apart).
        return el.child(
            gpui::canvas(
                |_, _, _| {},
                move |bounds, _, window, _| {
                    let s = window.scale_factor();
                    let snap = |v: Pixels| px((f32::from(v) * s).round() / s);
                    let (r, radius) = (px(TAB_FOOT), px(TAB_RADIUS));
                    let left = snap(bounds.left() + r);
                    let right = snap(bounds.right() - r);
                    let (top, bottom) = (snap(bounds.top()), snap(bounds.bottom()));
                    let corners = gpui::Corners { top_left: radius, top_right: radius, ..Default::default() };
                    window.paint_quad(gpui::fill(Bounds::from_corners(point(left, top), point(right, bottom)), island).corner_radii(corners));
                    window.paint_quad(
                        gpui::quad(
                            Bounds::from_corners(point(left, top), point(right, bottom - r)),
                            corners,
                            // Not transparent black: GPUI blends a rounded border's inner edge
                            // toward the background color, and black darkens the curves.
                            edge.alpha(0.),
                            gpui::Edges { top: px(1.), right: px(1.), bottom: px(0.), left: px(1.) },
                            edge,
                            Default::default(),
                        ),
                    );
                    paint_tab_foot(window, left, bottom, true, island, Some(edge));
                    paint_tab_foot(window, right, bottom, false, island, Some(edge));
                },
            )
            .absolute()
            .top_0()
            .bottom_0()
            .left(px(-TAB_FOOT))
            .right(px(-TAB_FOOT)),
        );
    }
    let hover_fill = Hsla::from(theme.panel).blend(Hsla::from(theme.text).alpha(0.07));
    el.child(
        div()
            .absolute()
            .top_0()
            .bottom(px(1.))
            // 1px wider on the left, over the previous tab's separator.
            .left(px(if after_active { 0. } else { -1. }))
            .right_0()
            .rounded_t(px(TAB_RADIUS))
            .bg(hover_fill)
            .when(!after_active, |el| el.child(tab_foot(true, hover_fill, None)))
            .child(tab_foot(false, hover_fill, None))
            // The current tab's right foot, back on top, from the same snapped
            // edge (this tab's left is the current tab's right).
            .when(after_active, |el| {
                el.child(
                    gpui::canvas(
                        |_, _, _| {},
                        move |bounds, _, window, _| {
                            let s = window.scale_factor();
                            let snap = |v: Pixels| px((f32::from(v) * s).round() / s);
                            let edge_x = snap(bounds.left() + px(TAB_FOOT));
                            paint_tab_foot(window, edge_x, snap(bounds.bottom()), false, island, Some(edge));
                        },
                    )
                    .absolute()
                    .top_0()
                    // The hover shape stops 1px above the tab's bottom.
                    .bottom(px(-1.))
                    .left(px(-TAB_FOOT))
                    .w(px(2. * TAB_FOOT)),
                )
            })
            // It covers the tab, so its own hover is the tab's.
            .with_hover_transition("tab-hover", transition::FOCUS, |el, t| el.opacity(t)),
    )
    .when(separator, |el| {
        el.child(
            div()
                .absolute()
                .right_0()
                .top(px(9.))
                .bottom(px(10.))
                .w(px(1.))
                .bg(theme.border)
                .group_hover(group, |s| s.invisible()),
        )
    })
}


/// Paints a current tab's foot beside its side edge at `x` (the tab's left
/// edge for the left foot, its right edge for the right one), down to `bottom`:
/// the same shape as `tab_foot`, from snapped coordinates.
pub(super) fn paint_tab_foot(window: &mut Window, x: Pixels, bottom: Pixels, left: bool, fill: Hsla, edge: Option<Hsla>) {
    let r = px(TAB_FOOT);
    let one = px(1.);
    // The square overlaps the tab's 1px border column.
    let square_x = if left { x - r + one } else { x - one };
    let square = Bounds { origin: point(square_x, bottom - r), size: size(r, r) };
    let center = point(if left { square_x } else { square_x + r }, bottom - r);
    let ring = |width: Pixels, color: Hsla| {
        let outer = r - one + width;
        gpui::quad(
            Bounds { origin: point(center.x - outer, center.y - outer), size: size(outer * 2., outer * 2.) },
            outer,
            // The ring's own color, clear: a transparent black background would
            // darken its inner edge (see `tab_shape`).
            color.alpha(0.),
            width,
            color,
            Default::default(),
        )
    };
    window.with_content_mask(Some(gpui::ContentMask { bounds: square }), |window| {
        window.paint_quad(ring(r, fill));
        if let Some(edge) = edge {
            window.paint_quad(ring(one, edge));
        }
    });
}


/// The concave corner joining the current tab to its island, like a browser
/// tab's: a square beside the tab's bottom, island-colored except for a
/// quarter circle at its corner away from the tab, with the island's border
/// along the curve. GPUI can't clip to curves, so the shape is a thick ring
/// around that circle, clipped to the square.
///
/// The square overlaps the tab's 1px border column and the curve's border runs
/// from the tab's side border (radius `TAB_FOOT - 1` to `TAB_FOOT` around the
/// corner) down to the island's top border row, so the lines meet exactly.
pub(super) fn tab_foot(left: bool, fill: Hsla, edge: Option<Hsla>) -> gpui::Div {
    let r = TAB_FOOT;
    let hole = r - 1.;
    let ring = |width: f32, color: Hsla| {
        let outer = hole + width;
        // Centered on the square's top corner away from the tab.
        div()
            .absolute()
            .top(px(-outer))
            .left(px(if left { -outer } else { r - outer }))
            .size(px(2. * outer))
            .rounded_full()
            .border(px(width))
            .border_color(color)
            // Its own color, clear (see `paint_tab_foot`).
            .bg(color.alpha(0.))
    };
    div()
        .absolute()
        .bottom_0()
        .size(px(r))
        .overflow_hidden()
        .map(|el| if left { el.left(px(1. - r)) } else { el.right(px(1. - r)) })
        .child(ring(r, fill))
        .when_some(edge, |el, edge| el.child(ring(1., edge)))
}
