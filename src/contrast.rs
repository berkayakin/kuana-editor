//! Text that stays readable over tinted backgrounds. A diff's green and red
//! rows (and their stronger changed-word fills) lower the contrast of what's
//! drawn on them, dim colors like comments most of all; `keep_legible` moves a
//! color's lightness (hue and saturation kept) until it reads again. Contrast
//! is APCA's lightness contrast (Lc, 0.0.98G-4g constants), which, unlike
//! WCAG 2's ratio, holds for light text on dark backgrounds too.

use std::{cell::RefCell, collections::HashMap};

use gpui::{Hsla, Rgba};

/// The least contrast text keeps over a tint: APCA's minimum for fluent text.
pub const MIN_LC: f32 = 45.;
/// Text keeps the contrast it has over the plain background up to this much;
/// past it, a tint may cost a little (white text needn't stay pure white).
const KEEP_LC: f32 = 75.;

/// APCA's screen luminance of an sRGB color, soft-clamped near black.
fn luminance(c: Hsla) -> f32 {
    let c = Rgba::from(c);
    let y = 0.2126729 * c.r.powf(2.4) + 0.7151522 * c.g.powf(2.4) + 0.0721750 * c.b.powf(2.4);
    if y < 0.022 { y + (0.022 - y).powf(1.414) } else { y }
}

/// APCA lightness contrast of `text` on `bg`: about 0 to 106 for dark text on a
/// light background, 0 to −108 for light text on a dark one.
pub fn lc(text: Hsla, bg: Hsla) -> f32 {
    let (t, b) = (luminance(text), luminance(bg));
    if (b - t).abs() < 0.0005 {
        return 0.;
    }
    let s = if b > t { (b.powf(0.56) - t.powf(0.57)) * 1.14 } else { (b.powf(0.65) - t.powf(0.62)) * 1.14 };
    let s = if s.abs() < 0.1 { 0. } else { s - 0.027 * s.signum() };
    s * 100.
}

/// `text` changed as little as needed to reach `min` Lc on `bg`: lighter when
/// it's lighter than `bg` (darker otherwise), at worst white or black.
pub fn legible(text: Hsla, bg: Hsla, min: f32) -> Hsla {
    if lc(text, bg).abs() >= min {
        return text;
    }
    let lighter = luminance(text) >= luminance(bg);
    let with = |l: f32| Hsla { l, ..text };
    let (mut near, mut far) = (text.l, if lighter { 1. } else { 0. });
    if lc(with(far), bg).abs() < min {
        return with(far);
    }
    // The lightness between `near` (too little) and `far` (enough) closest to `near`.
    for _ in 0..14 {
        let mid = (near + far) / 2.;
        if lc(with(mid), bg).abs() >= min { far = mid } else { near = mid }
    }
    with(far)
}

thread_local! {
    static CACHE: RefCell<HashMap<[u32; 12], Hsla>> = RefCell::new(HashMap::new());
}

/// `text` over `tint` (a highlight drawn on the opaque `base`), as readable as
/// it is on `base` (up to `KEEP_LC`) and never under `MIN_LC`. Cached: a frame
/// asks this for every span on a tinted row, from a handful of colors.
pub fn keep_legible(text: Hsla, tint: Hsla, base: Hsla) -> Hsla {
    let bits = |c: Hsla| [c.h.to_bits(), c.s.to_bits(), c.l.to_bits(), c.a.to_bits()];
    let [a, b, c] = [bits(text), bits(tint), bits(base)];
    let key = [a[0], a[1], a[2], a[3], b[0], b[1], b[2], b[3], c[0], c[1], c[2], c[3]];
    CACHE.with(|cache| {
        if let Some(found) = cache.borrow().get(&key) {
            return *found;
        }
        let min = lc(text, base).abs().clamp(MIN_LC, KEEP_LC);
        let found = legible(text, tint, min);
        let mut cache = cache.borrow_mut();
        // Colors come from themes: this only grows past a few hundred over many theme switches.
        if cache.len() > 4096 {
            cache.clear();
        }
        cache.insert(key, found);
        found
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::rgb;

    fn hsla(hex: u32) -> Hsla {
        rgb(hex).into()
    }

    #[test]
    fn matches_apca_reference_values() {
        // From APCA's published examples.
        assert!((lc(hsla(0x000000), hsla(0xffffff)) - 106.04).abs() < 0.1);
        assert!((lc(hsla(0xffffff), hsla(0x000000)) + 107.88).abs() < 0.1);
        assert!((lc(hsla(0x888888), hsla(0xffffff)) - 63.06).abs() < 0.1);
        assert!((lc(hsla(0xffffff), hsla(0x888888)) + 68.54).abs() < 0.1);
    }

    #[test]
    fn dim_text_on_a_tint_gets_readable() {
        // Ember's comment color on a red diff fill.
        let (comment, tint) = (hsla(0x757376), hsla(0x672236));
        assert!(lc(comment, tint).abs() < MIN_LC);
        let fixed = legible(comment, tint, MIN_LC);
        assert!(lc(fixed, tint).abs() >= MIN_LC);
        assert!(fixed.l > comment.l && fixed.l < 1.);
        assert_eq!((fixed.h, fixed.s), (comment.h, comment.s));
        // Light themes: darker.
        let (comment, tint) = (hsla(0x8f8d85), hsla(0xf1c9c3));
        assert!(legible(comment, tint, MIN_LC).l < comment.l);
    }

    #[test]
    fn readable_text_is_left_alone() {
        let (text, base) = (hsla(0xf4f3ef), hsla(0x2a292c));
        assert_eq!(legible(text, base, MIN_LC), text);
        assert_eq!(keep_legible(text, base, base), text);
    }
}
