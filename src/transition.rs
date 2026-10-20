//! CSS-like transitions. GPUI redraws from scratch each frame and doesn't
//! interpolate styles, so an element that should ease between two looks (focused
//! or not, on or off) is wrapped in a [`Transition`]: it remembers the state
//! across frames, hands the element's builder a progress from 0 (off) to 1 (on)
//! and requests frames until it settles.
//!
//! ```ignore
//! div().with_transition(id, focused, FOCUS, |el, t| el.border_color(mix(normal, accent, t)))
//! ```
//!
//! `with_hover_transition` is the same driven by the mouse, in place of `.hover()`
//! (which GPUI applies at paint time, out of the builder's reach): the element
//! tracks its own hover like GPUI's hover styles do, with a hitbox and a redraw
//! when it changes.
//!
//! The first frame shows the current state without animating (nothing fades out
//! at startup) unless it's built with `appear` (a toast sliding in), a change
//! midway reverses from where it is, and with Reduce visual effects it snaps.

use std::{
    cell::Cell,
    rc::Rc,
    time::{Duration, Instant},
};

use gpui::{
    AnyElement, App, Bounds, DispatchPhase, Element, ElementId, GlobalElementId, Hitbox,
    HitboxBehavior, Hsla, InspectorElementId, InteractiveElement, Interactivity, IntoElement,
    LayoutId, MouseMoveEvent, ParentElement, Pixels, Rgba, StatefulInteractiveElement,
    StyleRefinement, Styled, Window,
};

use crate::theme::Theme;

/// Focus rings and other small state changes.
pub const FOCUS: Duration = Duration::from_millis(250);
/// List rows: the mouse sweeps across them, so they settle quickly.
pub const ROW: Duration = Duration::from_millis(150);
/// Toasts sliding in and out.
pub const TOAST: Duration = Duration::from_millis(220);

pub trait TransitionExt: IntoElement + Sized + 'static {
    /// Eases this element toward `on`: `style` gets it and the progress, and
    /// returns it styled for that point. `id` must be stable across frames.
    fn with_transition(
        self,
        id: impl Into<ElementId>,
        on: bool,
        duration: Duration,
        style: impl FnOnce(Self, f32) -> Self + 'static,
    ) -> Transition<Self> {
        Transition::new(id.into(), Trigger::State(on), duration, self, Box::new(style))
    }

    /// Eases this element toward its hovered look while the mouse is over it.
    fn with_hover_transition(
        self,
        id: impl Into<ElementId>,
        duration: Duration,
        style: impl FnOnce(Self, f32) -> Self + 'static,
    ) -> Transition<Self> {
        Transition::new(id.into(), Trigger::Hover, duration, self, Box::new(style))
    }
}

impl<E: IntoElement + 'static> TransitionExt for E {}

pub struct Transition<E> {
    id: ElementId,
    trigger: Trigger,
    duration: Duration,
    element: Option<E>,
    style: Option<Box<dyn FnOnce(E, f32) -> E>>,
    /// While laying out a hover transition: the hover state it was styled for.
    hovered: Option<Rc<Cell<bool>>>,
    /// Whether its first frame starts from off.
    appear: bool,
}

enum Trigger {
    State(bool),
    Hover,
}

impl<E> Transition<E> {
    fn new(id: ElementId, trigger: Trigger, duration: Duration, element: E, style: Box<dyn FnOnce(E, f32) -> E>) -> Self {
        Self { id, trigger, duration, element: Some(element), style: Some(style), hovered: None, appear: false }
    }

    /// Animates in from off when first shown, instead of starting where it is:
    /// for elements that come and go, like toasts.
    pub fn appear(mut self) -> Self {
        self.appear = true;
        self
    }
}

/// Kept across frames under the element's id.
struct State {
    progress: Progress,
    /// Hover as last seen; only hover transitions update it.
    hovered: Rc<Cell<bool>>,
}

// Transparent to callers, like GPUI's `Stateful`: they keep styling, adding
// children and listeners to the wrapped element (its `style` runs last).
impl<E: Styled> Styled for Transition<E> {
    fn style(&mut self) -> &mut StyleRefinement {
        self.element.as_mut().expect("built before layout").style()
    }
}

impl<E: ParentElement> ParentElement for Transition<E> {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.element.as_mut().expect("built before layout").extend(elements)
    }
}

impl<E: InteractiveElement> InteractiveElement for Transition<E> {
    fn interactivity(&mut self) -> &mut Interactivity {
        self.element.as_mut().expect("built before layout").interactivity()
    }
}

impl<E: StatefulInteractiveElement> StatefulInteractiveElement for Transition<E> {}

impl<E: IntoElement + 'static> IntoElement for Transition<E> {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

impl<E: IntoElement + 'static> Element for Transition<E> {
    type RequestLayoutState = AnyElement;
    type PrepaintState = Option<Hitbox>;

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        id: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, AnyElement) {
        let snap = Theme::get(cx).reduce_effects;
        let (duration, appear) = (self.duration, self.appear);
        let (progress, running, hovered) = window.with_element_state(id.expect("has an id"), |state: Option<State>, _| {
            let now = Instant::now();
            let hovered = state.as_ref().map_or_else(Rc::default, |state| state.hovered.clone());
            let on = match self.trigger {
                Trigger::State(on) => on,
                Trigger::Hover => hovered.get(),
            };
            let mut progress = match state {
                _ if snap => Progress::settled(on),
                Some(state) => state.progress,
                None => Progress::settled(on && !appear),
            };
            progress.set(on, now, duration);
            let result = (progress.value(now, duration), !progress.done(now, duration), hovered.clone());
            (result, State { progress, hovered })
        });
        if matches!(self.trigger, Trigger::Hover) {
            self.hovered = Some(hovered);
        }
        if running {
            window.request_animation_frame();
        }
        let element = self.element.take().expect("laid out once");
        let style = self.style.take().expect("laid out once");
        let mut element = style(element, progress).into_any_element();
        (element.request_layout(window, cx), element)
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        element: &mut AnyElement,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Hitbox> {
        element.prepaint(window, cx);
        // Inserted after the element's own, so it's hit-tested first; `Normal`
        // doesn't keep the mouse from what's beneath.
        self.hovered.is_some().then(|| window.insert_hitbox(bounds, HitboxBehavior::Normal))
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        element: &mut AnyElement,
        hitbox: &mut Option<Hitbox>,
        window: &mut Window,
        cx: &mut App,
    ) {
        element.paint(window, cx);
        let (Some(hitbox), Some(hovered)) = (hitbox.take(), self.hovered.take()) else { return };
        // Redraw when the hover changes, as GPUI does for `.hover()` styles: on
        // mouse moves, and now if the element moved under a still mouse.
        if hitbox.is_hovered(window) != hovered.get() {
            hovered.set(!hovered.get());
            window.request_animation_frame();
        }
        let view = window.current_view();
        window.on_mouse_event(move |_: &MouseMoveEvent, phase, window, cx| {
            if phase == DispatchPhase::Capture && hitbox.is_hovered(window) != hovered.get() {
                hovered.set(!hovered.get());
                cx.notify(view);
            }
        });
    }
}

/// Where a transition is: heading for `on` since `since`, having started at `from`.
#[derive(Clone, Copy, Debug)]
struct Progress {
    on: bool,
    from: f32,
    since: Option<Instant>,
}

impl Progress {
    fn settled(on: bool) -> Self {
        Self { on, from: target(on), since: None }
    }

    fn set(&mut self, on: bool, now: Instant, duration: Duration) {
        if on != self.on {
            // A reversal continues from where it is, CSS-style.
            *self = Self { on, from: self.value(now, duration), since: Some(now) };
        }
    }

    /// The eased progress toward on (1) or off (0).
    fn value(&self, now: Instant, duration: Duration) -> f32 {
        let eased = ease_out(self.elapsed(now, duration));
        self.from + (target(self.on) - self.from) * eased
    }

    fn done(&self, now: Instant, duration: Duration) -> bool {
        self.elapsed(now, duration) >= 1.
    }

    /// How far along the way it is in time, 0–1. A reversal midway has less
    /// distance to cover, so it takes less time.
    fn elapsed(&self, now: Instant, duration: Duration) -> f32 {
        let Some(since) = self.since else { return 1. };
        let span = duration.as_secs_f32() * (target(self.on) - self.from).abs();
        if span <= 0. { 1. } else { ((now - since).as_secs_f32() / span).min(1.) }
    }
}

/// Cubic ease-out: quick to respond, gentle to settle. (GPUI's quint version
/// is nearly done after a fifth of the time, which reads as no transition.)
fn ease_out(t: f32) -> f32 {
    1. - (1. - t).powi(3)
}

fn target(on: bool) -> f32 {
    if on { 1. } else { 0. }
}

/// `a` blended toward `b` by `t` (0–1), in RGB so grays don't pick up a hue.
/// Premultiplied, like CSS: drawn over anything, it's a crossfade between the
/// two. (Blending the channels apart, an opaque light fill fading to a faint
/// dark one passes through a half-opaque dark: darker midway than either end.)
pub fn mix(a: impl Into<Hsla>, b: impl Into<Hsla>, t: f32) -> Hsla {
    let (a, b) = (Rgba::from(a.into()), Rgba::from(b.into()));
    let lerp = |x: f32, y: f32| x + (y - x) * t.clamp(0., 1.);
    let alpha = lerp(a.a, b.a);
    if alpha <= f32::EPSILON {
        return Rgba { r: lerp(a.r, b.r), g: lerp(a.g, b.g), b: lerp(a.b, b.b), a: 0. }.into();
    }
    let channel = |x: f32, y: f32| (lerp(x * a.a, y * b.a) / alpha).clamp(0., 1.);
    Rgba { r: channel(a.r, b.r), g: channel(a.g, b.g), b: channel(a.b, b.b), a: alpha }.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_eases_toward_the_target_and_reverses_from_where_it_is() {
        let d = Duration::from_millis(100);
        let t0 = Instant::now();
        let ms = |n| t0 + Duration::from_millis(n);

        // Settled at first: no animation.
        let mut p = Progress::settled(false);
        assert_eq!(p.value(t0, d), 0.);
        assert!(p.done(t0, d));

        p.set(true, t0, d);
        assert_eq!(p.value(t0, d), 0.);
        let half = p.value(ms(50), d);
        assert!(half > 0.5 && half < 1., "ease-out is past halfway at half time: {half}");
        assert!(!p.done(ms(50), d));
        assert_eq!(p.value(ms(100), d), 1.);
        assert!(p.done(ms(100), d));

        // Reversed midway: starts from the current value, in proportionally less time.
        let mut p = Progress::settled(false);
        p.set(true, t0, d);
        let at = p.value(ms(20), d);
        p.set(false, ms(20), d);
        assert!((p.value(ms(20), d) - at).abs() < 1e-6);
        assert!(p.value(ms(30), d) < at);
        assert!(!p.done(ms(20) + d.mul_f32(at * 0.9), d));
        assert!(p.done(ms(20) + d.mul_f32(at * 1.01), d));
        assert_eq!(p.value(ms(200), d), 0.);
    }

    #[test]
    fn mix_blends_in_rgb() {
        let black = gpui::hsla(0., 0., 0., 1.);
        let white = gpui::hsla(0., 0., 1., 1.);
        assert_eq!(mix(black, white, 0.), black);
        let mid = Rgba::from(mix(black, white, 0.5));
        assert!((mid.r - 0.5).abs() < 1e-3 && (mid.g - 0.5).abs() < 1e-3 && (mid.b - 0.5).abs() < 1e-3);
        let faded = mix(black, black.alpha(0.), 0.25);
        assert!((faded.a - 0.75).abs() < 1e-6);
        assert!(Rgba::from(faded).r.abs() < 1e-6);
    }

    #[test]
    fn mix_to_a_faint_fill_never_overshoots() {
        // A light opaque fill easing to a faint dark one (a light theme's
        // `hover`), over the same light backdrop: every step lies between.
        let light = Rgba { r: 0.98, g: 0.976, b: 0.96, a: 1. };
        let faint = Rgba { r: 0.12, g: 0.118, b: 0.114, a: 0.043 };
        let over = |c: Rgba| c.r * c.a + light.r * (1. - c.a);
        let (start, end) = (over(light), over(faint));
        let mut last = start;
        for i in 0..=10 {
            let shown = over(Rgba::from(mix(light, faint, i as f32 / 10.)));
            assert!(shown <= last + 1e-4 && shown >= end - 1e-4, "step {i}: {shown}");
            last = shown;
        }
    }
}
