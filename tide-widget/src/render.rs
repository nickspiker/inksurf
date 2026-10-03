//! The panel's tide chart for a phone: full colour, anti-aliased, any pixel size.
//!
//! Same layout and data as the e-ink renderer (tideglyph-fw render_tide): ±12h window with now centred, tide fill on the fixed MLLW axis, dozenal-hour ticks, hi/lo markers with dozenal times, the now-line label, rotated sunrise/sunset times, and a hard day/night seam by sun altitude (plus a moon seam: the sun drives the sky's red + green, the moon its blue). Phone-specific: sun and moon altitude curves (horizon = the centreline, which is also mid-tide) instead of the panel's edge indicators, which rounded widget corners would clip; real glyph outlines from the dozenal font. Every line, edge and stroke is exactly 1px (the sun and moon curves 2px) at any widget size so it stays crisp; only the type scales. Colours are solid primaries for now, to be tuned.
//!
//! Compositing is fluor-style, not Porter-Duff over: the frame starts empty in fluor's α + darkness format and is built topmost-first, each layer composed UNDER the running composite with fluor's `Blend::under` kernel (Normal mode, mirrored below — fluor's no-default-features build doesn't currently compile, so it isn't a dependency yet). Draw order, top to bottom: labels; now bar; hi/lo bars; ticks; sun curve, its stroke; moon curve, its stroke; centreline; water stroke; water fill; sky (one pass over whatever is still transparent). Labels have no stroke: each glyph pixel is the solid fill beneath it XOR the label's colour (now time: all three channels; tide times: green + blue; sunrise/sunset: red + green), so it always contrasts — and since labels go down first and opaque, every later layer early-outs under them: no line or curve is ever painted beneath type. tiny-skia only rasterizes anti-aliased coverage masks; one `^ 0x00FFFFFF` at the end flips darkness back to visible RGB.

use std::sync::OnceLock;

use tiny_skia::{FillRule, LineCap, LineJoin, Mask, Path, PathBuilder, Stroke, Transform};
use ttf_parser::{Face, OutlineBuilder};

/// Oxanium Regular + the twelve dozenal glyphs (Zil..Stelor) at U+0010..U+001B.
static FONT_TTF: &[u8] = include_bytes!("../../assets/font/Oxanium-Regular+glyphs.ttf");
/// Vertical centre of the font's em box ((ascender 790 + descender −210) / 2), in font units; labels centre on this.
const EM_MID: f32 = 290.0;
const UNITS_PER_EM: f32 = 1000.0;

const TIDE_MIN_FT: f32 = -4.61; // MLLW y-axis bounds (match tide-display and the panel)
const TIDE_MAX_FT: f32 = 14.09;
const MSL_TO_MLLW: f32 = 6.814; // tide-core predicts MSL; display axis is MLLW
const N_SAMPLES: usize = 241; // ±12h at 6-min steps

// Bremerton (Southworth WA) for sun/moon positions.
const SUN_LAT: f64 = 47.5126;
const SUN_LON: f64 = -122.5054;

// Palette (0xRRGGBB, visible): solid primaries throughout as the starting point for tuning.
const NOW_INK: u32 = 0xFFFFFF; // now time and now bar
const TIDE_INK: u32 = 0x00FFFF; // high/low times and bars
const SUN_INK: u32 = 0xFFFF00; // sunrise/sunset times
const TICK: u32 = 0xFFFFFF;
const SUN: u32 = 0xFFFF00;
const MOON: u32 = 0x0000FF;
const WATER: u32 = 0x00FFFF;
const SKY_SUN: u32 = 0xFFFF00; // sun up lights the sky's red + green channels
const SKY_MOON: u32 = 0x0000FF; // moon up lights its blue channel — so: yellow day, white day with the moon up, blue moonlit night, black moonless night
const STROKE: u32 = 0x000000; // every stroke and bar edge

/// Vertical centre of Oxanium's digits and colon (half the 690-unit cap height), in font units; Arabic labels centre on this.
const DIGIT_MID: f32 = 345.0;

/// How times are written. Dozenal: two Stelor-system glyphs counting the day's 144 ten-minute marks, ticks every dozenal hour (2 decimal hours). Hourly: `HH:MM` in Arabic numerals with the colon on the bar, ticks every hour.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Edition {
    Dozenal,
    Hourly,
}

/// Render the chart centred on `unix` as row-major ARGB ints (Android's Bitmap int layout).
pub fn render_argb(unix: i64, w: usize, h: usize, edition: Edition) -> Vec<i32> {
    render(unix, w.max(1), h.max(1), edition).into_iter().map(|p| p as i32).collect()
}

/// Render the chart centred on `unix`: row-major visible ARGB, fully opaque.
pub fn render(unix: i64, w: usize, h: usize, edition: Edition) -> Vec<u32> {
    let (wf, hf) = (w as f32, h as f32);
    let em = (0.09 * hf).max(16.0); // label size in px: the panel's 16px-of-180 proportion
    let k = em / UNITS_PER_EM; // font units → px
    let (nx, mid_y) = ((w / 2) as f32, hf / 2.0);
    let t_at = |x: f32| unix + ((x - wf / 2.0) / wf * 86400.0) as i64;

    // Day/night: hard seam per pixel column by sun altitude at the column's time. The sky colour is additive per column — the sun owns red + green, the moon owns blue — so moonrise/moonset are hard seams too.
    let night: Vec<bool> = (0..w).map(|x| sun_altitude_deg(t_at(x as f32 + 0.5)) < 0.0).collect();
    let sky: Vec<u32> = (0..w)
        .map(|x| {
            let moon_up = moon_altitude_deg(t_at(x as f32 + 0.5)) > 0.0;
            (if night[x] { 0 } else { SKY_SUN }) | (if moon_up { SKY_MOON } else { 0 })
        })
        .collect();
    let mut f = Frame::new(w, h, night);

    // Tide: 241 samples over ±12h, MLLW feet.
    let mut samples = [0f32; N_SAMPLES];
    let base = unix - 12 * 3600;
    for (i, s) in samples.iter_mut().enumerate() {
        *s = tide_core::predict(tide_core::BREMERTON, (base + 360 * i as i64) as f64) as f32 + MSL_TO_MLLW;
    }

    // High/low extrema: the dozenal event time goes OPPOSITE the curve — HIGH labels centred at 2/3 of the height, LOW at 1/3.
    let mut extrema: Vec<(f32, f32, i64)> = Vec::new(); // (bar x, label centre y, event time)
    let mut prev_dir: i8 = 0;
    let mut last_pivot = 0usize;
    for i in 1..N_SAMPLES {
        let (a, b) = (samples[i - 1], samples[i]);
        let cur: i8 = if b > a { 1 } else if b < a { -1 } else { 0 };
        if cur == 0 {
            continue;
        }
        if prev_dir != 0 && cur != prev_dir {
            let t = refine_extremum(base + ((last_pivot + i - 1) / 2) as i64 * 360);
            let ex = wf / 2.0 + (t - unix) as f32 / 86400.0 * wf;
            if (0.0..wf).contains(&ex) {
                extrema.push((ex, if prev_dir == 1 { hf * 2.0 / 3.0 } else { hf / 3.0 }, t));
            }
        }
        prev_dir = cur;
        last_pivot = i;
    }
    let label_gap = |cy: f32| ((cy - em / 2.0 - em * 0.15) as i32, (cy + em / 2.0 + em * 0.15).ceil() as i32);
    let font = face();

    // Water surface y at any x (the tide curve on the fixed MLLW axis).
    let tide_y = |x: f32| {
        let si = (x / wf * 86400.0) / 360.0;
        let tt = ((interp(&samples, si) - TIDE_MIN_FT) / (TIDE_MAX_FT - TIDE_MIN_FT)).clamp(0.0, 1.0);
        hf * (1.0 - tt)
    };

    // 1. Labels, topmost and opaque: each glyph pixel is the solid fill beneath it (sky, or water below the surface) XOR the label's colour, composed at its coverage. Every later layer early-outs on those pixels, so no line, curve or stroke is ever painted under a glyph — the knockout falls out of topmost-first compositing with no mask or extra pass. Edge pixels take their coverage and leave the rest of the budget to whatever lies beside the glyph.
    // A time straddling a bar (dozenal: the two glyphs either side; hourly: HH:MM with the colon on it), and the same time rotated and top-aligned. Event times are exact; the now time is rounded to the nearest 10-minute mark in both editions, since the widget redraws every ten minutes.
    let straddle = |hh: u32, mm: u32, line_x: f32, cy: f32| match edition {
        Edition::Dozenal => {
            let (hi, lo) = dozenal_indices(hh, mm);
            straddle_paths(font, hi, lo, line_x, cy, k)
        }
        Edition::Hourly => colon_paths(font, &hhmm(hh, mm), line_x, cy, k),
    };
    let (nh, nm) = round_to_10(local_hh_mm(unix));
    let now_glyphs = straddle(nh, nm, nx, mid_y);
    let mut tide_glyphs = Vec::new();
    for &(ex, cy, t) in &extrema {
        let (lh, lm) = local_hh_mm(t);
        tide_glyphs.extend(keep_inside(straddle(lh, lm, ex, cy), wf));
    }
    let mut sun_glyphs = Vec::new(); // rotated, on the seam, top-aligned clear of the midnight ticks
    let sun_top = (em * 0.2).max(6.0);
    for (xf, t, sunrise) in sun_crossings(unix, wf) {
        let (lh, lm) = local_hh_mm(t);
        sun_glyphs.extend(keep_inside(
            match edition {
                Edition::Dozenal => {
                    let (hi, lo) = dozenal_indices(lh, lm);
                    rotated_paths(font, hi, lo, xf, sun_top, sunrise, k)
                }
                Edition::Hourly => rotated_text_paths(font, &hhmm(lh, lm), xf, sun_top, sunrise, k),
            },
            wf,
        ));
    }
    // The solid fill at a pixel (sky, or water below the surface — water's share of the pixel row as the blend) XOR `ink`'s channels. XOR with 00/FF channels is affine, so XOR-then-blend equals blend-then-XOR.
    let fill_xor = |x: usize, y: usize, ink: u32| {
        let wc = (((y as f32 + 1.0) - tide_y(x as f32 + 0.5)).clamp(0.0, 1.0) * 255.0) as u32;
        mix(sky[x] ^ ink, WATER ^ ink, wc)
    };
    for (glyphs, ink) in [(&now_glyphs, NOW_INK), (&tide_glyphs, TIDE_INK), (&sun_glyphs, SUN_INK)] {
        for g in glyphs {
            f.under_path_with(g, |x, y| fill_xor(x, y, ink));
        }
    }

    // Now bar down the centre, split around its label.
    let (g0, g1) = label_gap(mid_y);
    edged_vline(&mut f, nx, 0, g0, NOW_INK);
    edged_vline(&mut f, nx, g1, h as i32, NOW_INK);

    // High/low tide bars, split around their labels. Bare 1px, no black edges.
    for &(ex, cy, _) in &extrema {
        let (g0, g1) = label_gap(cy);
        vline(&mut f, ex, 0, g0, TIDE_INK);
        vline(&mut f, ex, g1, h as i32, TIDE_INK);
    }

    // Ticks (top + bottom edge), 2px tall, local midnight 4px: every dozenal hour (2 decimal hours) in the dozenal edition, every hour in the hourly one. Anchored to the current local-hour boundary so they land on true hour marks.
    let hour0 = unix - (unix + tz_offset_secs(unix)).rem_euclid(3600);
    for hh in -12..=12i64 {
        let tick_time = hour0 + hh * 3600;
        let x = wf / 2.0 + (tick_time - unix) as f32 / 86400.0 * wf;
        let local_hour = (tick_time + tz_offset_secs(tick_time)).rem_euclid(86400) / 3600;
        if x < 0.0 || x >= wf || (edition == Edition::Dozenal && local_hour % 2 != 0) {
            continue;
        }
        let len = if local_hour == 0 { 4 } else { 2 };
        edged_vline(&mut f, x, 0, len, TICK);
        edged_vline(&mut f, x, h as i32 - len, h as i32, TICK);
    }

    // Sun curve, its stroke; moon curve, its stroke. Horizon on the centreline; each body's highest altitude over the surrounding year at the top edge (its lowest, the mirror image, at the bottom).
    let span = |max: f64| move |a: f64| mid_y - (a / max) as f32 * (mid_y - 1.0);
    let sun_curve = altitude_path(wf, &t_at, &span(yearly_max_altitude(unix, sun_altitude_deg)), sun_altitude_deg);
    f.curve_then_stroke(&sun_curve, SUN);
    let moon_curve = altitude_path(wf, &t_at, &span(yearly_max_altitude(unix, moon_altitude_deg)), moon_altitude_deg);
    f.curve_then_stroke(&moon_curve, MOON);

    // Centreline: mid-tide on the fixed axis, and the horizon for the curves. 1px, the fill beneath XOR all three channels.
    let mid_row = h / 2;
    for x in 0..w {
        f.under(mid_row * w + x, fill_xor(x, mid_row, 0xFFFFFF), 255);
    }

    // Water stroke (1px along the surface), then the water fill from the curve down.
    let mut surface = PathBuilder::new();
    let mut x = 0.0;
    loop {
        if x == 0.0 { surface.move_to(x, tide_y(x)) } else { surface.line_to(x, tide_y(x)) }
        if x >= wf {
            break;
        }
        x = (x + 2.0).min(wf);
    }
    let surface = surface.finish().unwrap();
    if let Some(s) = stroked(&surface, 1.0) {
        f.under_path(&s, STROKE, STROKE);
    }
    let mut water = PathBuilder::new();
    water.move_to(0.0, hf);
    for p in surface.points() {
        water.line_to(p.x, p.y);
    }
    water.line_to(wf, hf);
    water.close();
    f.under_path(&water.finish().unwrap(), WATER, WATER);

    // Sky: one pass over everything still not opaque.
    f.under_fill(&sky);
    f.finish()
}

// ── Fluor-style frame ───────────────────────────────────────────────────────

/// Fluor pixel: `0xααRRGGBB`, α = opacity, RGB = darkness (complement of visible RGB), empty = 0.
type Argb8 = u32;

/// Fluor's `Blend::under`, Normal mode (fluor/src/pixel.rs): compose `bottom` underneath the partial composite `top`. Integer `>> 8` math, early-out once `top` is opaque; `consumed` is how much of the remaining opacity budget the new layer fills, and its darkness deposits in proportion.
#[inline]
fn under(top: Argb8, bottom: Argb8) -> Argb8 {
    if top >= 0xFF00_0000 {
        return top;
    }
    let consumed = ((256 - (top >> 24)) * (bottom >> 24)) >> 8;
    let ch = |s: u32| (((top >> s) & 0xFF) + ((((bottom >> s) & 0xFF) * consumed) >> 8)) << s;
    (((top >> 24) + consumed) << 24) | ch(16) | ch(8) | ch(0)
}

/// The running composite in fluor's α + darkness format, built topmost-first with `under`. Carries a reusable coverage mask for path rasterization and the per-column day/night flags for side-coloured layers.
struct Frame {
    w: usize,
    h: usize,
    px: Vec<Argb8>,
    mask: Mask,
    night: Vec<bool>,
}

impl Frame {
    fn new(w: usize, h: usize, night: Vec<bool>) -> Self {
        Self { w, h, px: vec![0; w * h], mask: Mask::new(w as u32, h as u32).expect("mask size"), night }
    }

    /// Compose visible colour `hex` at opacity `a` (0..=255) under pixel `i`.
    fn under(&mut self, i: usize, hex: u32, a: u32) {
        if a != 0 {
            self.px[i] = under(self.px[i], (a << 24) | (!hex & 0x00FF_FFFF));
        }
    }

    /// Compose a solid pixel-aligned rectangle [x0, x1) × [y0, y1) under the frame (clipped).
    fn under_rect(&mut self, x0: i32, y0: i32, x1: i32, y1: i32, hex: u32) {
        for y in y0.max(0)..y1.min(self.h as i32) {
            for x in x0.max(0)..x1.min(self.w as i32) {
                self.under(y as usize * self.w + x as usize, hex, 255);
            }
        }
    }

    /// Rasterize `path` (pixel space) to anti-aliased coverage and compose it under the frame: `day` on day columns, `night` on night columns. Only the path's bounding box is visited, and the mask is cleared behind itself for reuse.
    fn under_path(&mut self, path: &Path, day: u32, night: u32) {
        let b = path.bounds();
        let (x0, x1) = ((b.left().floor() as i32).max(0) as usize, (b.right().ceil() as i32 + 1).clamp(0, self.w as i32) as usize);
        let (y0, y1) = ((b.top().floor() as i32).max(0) as usize, (b.bottom().ceil() as i32 + 1).clamp(0, self.h as i32) as usize);
        if x0 >= x1 || y0 >= y1 {
            return;
        }
        self.mask.fill_path(path, FillRule::Winding, true, Transform::identity());
        for y in y0..y1 {
            for x in x0..x1 {
                let i = y * self.w + x;
                let cov = std::mem::take(&mut self.mask.data_mut()[i]) as u32;
                if cov != 0 {
                    let hex = if self.night[x] { night } else { day };
                    self.under(i, hex, cov);
                }
            }
        }
    }

    /// Rasterize `path` (pixel space) to anti-aliased coverage and compose it under the frame with a per-pixel colour `colour(x, y)` (visible RGB). Same bbox walk and mask reuse as [`Self::under_path`].
    fn under_path_with(&mut self, path: &Path, colour: impl Fn(usize, usize) -> u32) {
        let b = path.bounds();
        let (x0, x1) = ((b.left().floor() as i32).max(0) as usize, (b.right().ceil() as i32 + 1).clamp(0, self.w as i32) as usize);
        let (y0, y1) = ((b.top().floor() as i32).max(0) as usize, (b.bottom().ceil() as i32 + 1).clamp(0, self.h as i32) as usize);
        if x0 >= x1 || y0 >= y1 {
            return;
        }
        self.mask.fill_path(path, FillRule::Winding, true, Transform::identity());
        for y in y0..y1 {
            for x in x0..x1 {
                let i = y * self.w + x;
                let cov = std::mem::take(&mut self.mask.data_mut()[i]) as u32;
                if cov != 0 {
                    self.under(i, colour(x, y), cov);
                }
            }
        }
    }

    /// 2px curve, then its 1px stroke each side (a 4px stroke centred on it) under it.
    fn curve_then_stroke(&mut self, curve: &Path, hex: u32) {
        if let Some(c) = stroked(curve, 2.0) {
            self.under_path(&c, hex, hex);
        }
        if let Some(s) = stroked(curve, 4.0) {
            self.under_path(&s, STROKE, STROKE);
        }
    }

    /// Opaque fill under everything, one colour per column. Already-opaque pixels early-out.
    fn under_fill(&mut self, columns: &[u32]) {
        for y in 0..self.h {
            for (x, &hex) in columns.iter().enumerate() {
                self.under(y * self.w + x, hex, 255);
            }
        }
    }

    /// Flip darkness back to visible RGB (fluor's present-boundary XOR). The sky saturated α, so every pixel is opaque.
    fn finish(self) -> Vec<u32> {
        self.px.into_iter().map(|p| p ^ 0x00FF_FFFF).collect()
    }
}

/// Per-channel blend of visible colours `a` → `b` by `t` (0..=255).
fn mix(a: u32, b: u32, t: u32) -> u32 {
    let ch = |s: u32| ((((a >> s) & 0xFF) * (255 - t) + ((b >> s) & 0xFF) * t + 127) / 255) << s;
    ch(16) | ch(8) | ch(0)
}

/// Bare 1px vertical bar in the pixel column containing `x`, rows y0..y1 — no edges (the hi/lo bars).
fn vline(f: &mut Frame, x: f32, y0: i32, y1: i32, hex: u32) {
    let col = x.floor() as i32;
    f.under_rect(col, y0, col + 1, y1, hex);
}

/// 1px vertical bar in the pixel column containing `x`, rows y0..y1, then its 1px black edge on each side (the now bar and the ticks).
fn edged_vline(f: &mut Frame, x: f32, y0: i32, y1: i32, hex: u32) {
    if y1 <= y0 {
        return;
    }
    let col = x.floor() as i32;
    f.under_rect(col, y0, col + 1, y1, hex);
    f.under_rect(col - 1, y0, col, y1, STROKE);
    f.under_rect(col + 1, y0, col + 2, y1, STROKE);
}

fn stroked(path: &Path, width: f32) -> Option<Path> {
    path.stroke(&Stroke { width, line_cap: LineCap::Round, line_join: LineJoin::Round, ..Default::default() }, 1.0)
}

/// A body's altitude across the window as a polyline, sampled every 2px.
fn altitude_path(wf: f32, t_at: &dyn Fn(f32) -> i64, alt_y: &dyn Fn(f64) -> f32, alt: fn(i64) -> f64) -> Path {
    let mut pb = PathBuilder::new();
    let mut x = 0.0f32;
    loop {
        let y = alt_y(alt(t_at(x)));
        if x == 0.0 { pb.move_to(x, y) } else { pb.line_to(x, y) }
        if x >= wf {
            break;
        }
        x = (x + 2.0).min(wf);
    }
    pb.finish().unwrap()
}

/// Sharpen a high/low found on the 6-minute sample grid (good to ±3 min, and the grid moves with now, so the time would jitter between redraws) to the instant the tide's slope crosses zero: bisect a central-difference derivative of tide-core's prediction across ±12 min, to the second.
fn refine_extremum(t0: i64) -> i64 {
    let slope = |t: i64| tide_core::predict(tide_core::BREMERTON, (t + 30) as f64) - tide_core::predict(tide_core::BREMERTON, (t - 30) as f64);
    let (mut a, mut b) = (t0 - 720, t0 + 720);
    let sa = slope(a) > 0.0;
    if sa == (slope(b) > 0.0) {
        return t0; // no sign change in the bracket — keep the grid estimate
    }
    while b - a > 1 {
        let m = (a + b) / 2;
        if (slope(m) > 0.0) == sa { a = m } else { b = m }
    }
    (a + b) / 2
}

/// Sunrise/sunset in the ±12h window: (x, crossing time, is sunrise), where the sun's altitude crosses 0° (the seam), bisected to the second.
fn sun_crossings(unix: i64, wf: f32) -> Vec<(f32, i64, bool)> {
    let mut out = Vec::new();
    let mut prev_t = unix - 12 * 3600;
    let mut prev_a = sun_altitude_deg(prev_t);
    let mut t = prev_t + 600;
    while t <= unix + 12 * 3600 {
        let a = sun_altitude_deg(t);
        if (prev_a < 0.0) != (a < 0.0) {
            let (mut lo_t, mut hi_t) = (prev_t, t);
            for _ in 0..24 {
                let m = (lo_t + hi_t) / 2;
                if (sun_altitude_deg(m) < 0.0) == (prev_a < 0.0) { lo_t = m } else { hi_t = m }
            }
            let xf = wf / 2.0 + (hi_t - unix) as f32 / 86400.0 * wf;
            if (0.0..wf).contains(&xf) {
                out.push((xf, hi_t, a > prev_a));
            }
        }
        prev_t = t;
        prev_a = a;
        t += 600;
    }
    out
}

// ── Dozenal glyphs ──────────────────────────────────────────────────────────

fn face() -> &'static Face<'static> {
    static FACE: OnceLock<Face<'static>> = OnceLock::new();
    FACE.get_or_init(|| Face::parse(FONT_TTF, 0).expect("dozenal font"))
}

struct Glyph {
    path: Path,
    x_min: f32,
    x_max: f32,
    adv: f32,
}

struct Builder(PathBuilder);

impl OutlineBuilder for Builder {
    fn move_to(&mut self, x: f32, y: f32) {
        self.0.move_to(x, y);
    }
    fn line_to(&mut self, x: f32, y: f32) {
        self.0.line_to(x, y);
    }
    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        self.0.quad_to(x1, y1, x, y);
    }
    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        self.0.cubic_to(x1, y1, x2, y2, x, y);
    }
    fn close(&mut self) {
        self.0.close();
    }
}

/// Outline of character `c` in font units, y up, with its ink extent and advance.
fn glyph_char(font: &Face, c: char) -> Glyph {
    let gid = font.glyph_index(c).expect("glyph in font");
    let mut b = Builder(PathBuilder::new());
    let bbox = font.outline_glyph(gid, &mut b).expect("glyph outline");
    let adv = font.glyph_hor_advance(gid).unwrap_or(0) as f32;
    Glyph { path: b.0.finish().expect("glyph path"), x_min: bbox.x_min as f32, x_max: bbox.x_max as f32, adv }
}

/// Outline of dozenal digit `d` (0 = Zil … 11 = Stelor).
fn glyph(font: &Face, d: usize) -> Glyph {
    glyph_char(font, char::from_u32(0x10 + d as u32).unwrap())
}

/// Glyph `g` in pixel space with font point (`rx`, `ry`) at `anchor`, advancing along unit vector `u` with glyph-up along unit vector `v` (screen coords, y down), `k` px per font unit.
#[allow(clippy::too_many_arguments)]
fn place_at(g: &Glyph, anchor: (f32, f32), u: (f32, f32), v: (f32, f32), k: f32, rx: f32, ry: f32) -> Option<Path> {
    let tx = anchor.0 - u.0 * k * rx - v.0 * k * ry;
    let ty = anchor.1 - u.1 * k * rx - v.1 * k * ry;
    g.path.clone().transform(Transform::from_row(u.0 * k, u.1 * k, v.0 * k, v.1 * k, tx, ty))
}

/// Dozenal glyph `g` with its left ink edge and em-box centre at `anchor`.
fn place(g: &Glyph, anchor: (f32, f32), u: (f32, f32), v: (f32, f32), k: f32) -> Option<Path> {
    place_at(g, anchor, u, v, k, g.x_min, EM_MID)
}

/// Two-symbol dozenal time straddling the 1px edged bar at `line_x`: hi ending just left of it, lo starting just right, centred on `cy`. A trailing Zil is dropped and the lone hi centres on the bar (midnight shows a single Zil), as on the panel.
fn straddle_paths(font: &Face, hi: usize, lo: usize, line_x: f32, cy: f32, k: f32) -> Vec<Path> {
    let (u, v) = ((1.0, 0.0), (0.0, -1.0));
    let centre = line_x.floor() + 0.5;
    let gh = glyph(font, hi);
    let wh = (gh.x_max - gh.x_min) * k;
    if lo == 0 {
        return place(&gh, (centre - wh / 2.0, cy), u, v, k).into_iter().collect();
    }
    let gap = 3.5; // half the 1px bar + its 1px edge + the type's 1px stroke + 1px air
    let gl = glyph(font, lo);
    [place(&gh, (centre - gap - wh, cy), u, v, k), place(&gl, (centre + gap, cy), u, v, k)].into_iter().flatten().collect()
}

/// Dozenal time rotated 90° in the column at `cx`, its top end at `top`: sunrise reads bottom→top (CCW), sunset top→bottom (CW), hi first. Trailing Zil dropped.
fn rotated_paths(font: &Face, hi: usize, lo: usize, cx: f32, top: f32, sunrise: bool, k: f32) -> Vec<Path> {
    let glyphs: Vec<Glyph> = if lo == 0 { vec![glyph(font, hi)] } else { vec![glyph(font, hi), glyph(font, lo)] };
    let kern = 80.0 * k;
    let total: f32 = glyphs.iter().map(|g| (g.x_max - g.x_min) * k).sum::<f32>() + kern * (glyphs.len() as f32 - 1.0);
    let (u, v, mut at) = if sunrise { ((0.0, -1.0), (-1.0, 0.0), (cx, top + total)) } else { ((0.0, 1.0), (1.0, 0.0), (cx, top)) };
    let mut out = Vec::new();
    for g in &glyphs {
        out.extend(place(g, at, u, v, k));
        let step = (g.x_max - g.x_min) * k + kern;
        at = (at.0 + u.0 * step, at.1 + u.1 * step);
    }
    out
}

/// Shift a label horizontally so its ink stays fully inside [1, w − 1]: a label on a bar near the edge slides off its bar rather than losing digits.
fn keep_inside(paths: Vec<Path>, wf: f32) -> Vec<Path> {
    let (l, r) = paths.iter().map(|p| p.bounds()).fold((f32::MAX, f32::MIN), |(l, r), b| (l.min(b.left()), r.max(b.right())));
    let dx = if l < 1.0 { 1.0 - l } else if r > wf - 1.0 { (wf - 1.0 - r).max(1.0 - l) } else { 0.0 };
    if dx == 0.0 {
        return paths;
    }
    paths.into_iter().filter_map(|p| p.transform(Transform::from_translate(dx, 0.0))).collect()
}

/// "HH:MM", zero-padded 24-hour.
fn hhmm(hh: u32, mm: u32) -> [char; 5] {
    let d = |n: u32| char::from_digit(n, 10).unwrap();
    [d(hh / 10), d(hh % 10), ':', d(mm / 10), d(mm % 10)]
}

/// A run of characters at the font's own advances: each glyph with its pen position (font units), plus the run's ink extent [first glyph's left ink edge, last glyph's right ink edge].
fn text_run(font: &Face, text: &[char]) -> (Vec<(Glyph, f32)>, f32, f32) {
    let mut pen = 0.0;
    let mut run = Vec::with_capacity(text.len());
    for &c in text {
        let g = glyph_char(font, c);
        let adv = g.adv;
        run.push((g, pen));
        pen += adv;
    }
    let ink0 = run.first().map_or(0.0, |(g, p)| p + g.x_min);
    let ink1 = run.last().map_or(0.0, |(g, p)| p + g.x_max);
    (run, ink0, ink1)
}

/// Arabic time straddling the bar at `line_x`: the colon's ink centred on the bar, digits either side at the font's advances, centred on `cy` — the panel's decimal-edition anchor.
fn colon_paths(font: &Face, text: &[char], line_x: f32, cy: f32, k: f32) -> Vec<Path> {
    let (run, _, _) = text_run(font, text);
    let Some(i) = text.iter().position(|&c| c == ':') else { return Vec::new() };
    let (colon, cp) = &run[i];
    let c = cp + (colon.x_min + colon.x_max) / 2.0;
    let centre = line_x.floor() + 0.5;
    run.iter().filter_map(|(g, p)| place_at(g, (centre + (p - c) * k, cy), (1.0, 0.0), (0.0, -1.0), k, 0.0, DIGIT_MID)).collect()
}

/// Arabic time rotated 90° in the column at `cx`, its top ink end at `top`: sunrise reads bottom→top (CCW), sunset top→bottom (CW).
fn rotated_text_paths(font: &Face, text: &[char], cx: f32, top: f32, sunrise: bool, k: f32) -> Vec<Path> {
    let (run, ink0, ink1) = text_run(font, text);
    let (u, v, origin) = if sunrise { ((0.0, -1.0), (-1.0, 0.0), (cx, top + ink1 * k)) } else { ((0.0, 1.0), (1.0, 0.0), (cx, top - ink0 * k)) };
    run.iter().filter_map(|(g, p)| place_at(g, (origin.0 + u.0 * k * p, origin.1 + u.1 * k * p), u, v, k, 0.0, DIGIT_MID)).collect()
}

// ── Time, tide and sky math (shared with the panel firmware) ───────────────

fn interp(s: &[f32; N_SAMPLES], si: f32) -> f32 {
    if si <= 0.0 {
        return s[0];
    }
    if si >= (N_SAMPLES - 1) as f32 {
        return s[N_SAMPLES - 1];
    }
    let i = si as usize;
    let frac = si - i as f32;
    s[i] + frac * (s[i + 1] - s[i])
}

/// Wall-clock rounded to the nearest 10-minute mark (the same rounding the dozenal odometer uses), wrapping at midnight.
fn round_to_10((hh, mm): (u32, u32)) -> (u32, u32) {
    let counter = ((hh * 60 + mm + 5) / 10) % 144;
    (counter / 6, counter % 6 * 10)
}

/// Wall-clock → (hi, lo) dozenal-symbol indices, rounded to the nearest 10-min mark: a 2-digit base-12 odometer of the day's 144 ten-minute marks.
fn dozenal_indices(hh: u32, mm: u32) -> (usize, usize) {
    let counter = ((hh * 60 + mm + 5) / 10) % 144;
    ((counter / 12) as usize, (counter % 12) as usize)
}

/// Highest altitude (degrees) a body reaches over the year centred on `unix`, sampled hourly (within ~0.3° of the true culmination). Rolling rather than calendar so the scale never jumps on Jan 1; the lowest is the mirror image, so the horizon stays centred.
fn yearly_max_altitude(unix: i64, alt: fn(i64) -> f64) -> f64 {
    const HALF_YEAR: i64 = 365 * 86400 / 2;
    (-HALF_YEAR..=HALF_YEAR).step_by(3600).map(|dt| alt(unix + dt)).fold(1.0, f64::max)
}

// US Pacific local time, DST-aware. Pure integer calendar math (Howard Hinnant) — the chart is in station time, same as the panel.
fn civil_from_days(z0: i64) -> (i64, u32, u32) {
    let z = z0 + 719468;
    let era = (if z >= 0 { z } else { z - 146096 }) / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn days_from_civil(y0: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y0 - 1 } else { y0 };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) as i64 + 2) / 5 + (d as i64 - 1);
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// Unix seconds of `hour_utc` on the `n`th Sunday of `month` in `year`.
fn nth_sunday_unix(year: i64, month: u32, n: i64, hour_utc: i64) -> i64 {
    let first = days_from_civil(year, month, 1);
    let wd = (first + 4).rem_euclid(7); // 0 = Sunday (1970-01-01 was Thursday)
    let first_sunday_dom = 1 + ((7 - wd) % 7);
    days_from_civil(year, month, (first_sunday_dom + 7 * (n - 1)) as u32) * 86400 + hour_utc * 3600
}

/// Pacific offset at `unix`: PDT (-7h) inside the US DST window (2nd Sun Mar 02:00 → 1st Sun Nov 02:00 local), else PST (-8h).
fn tz_offset_secs(unix: i64) -> i64 {
    let (year, _, _) = civil_from_days(unix.div_euclid(86400));
    let dst_start = nth_sunday_unix(year, 3, 2, 10); // 02:00 PST = 10:00 UTC
    let dst_end = nth_sunday_unix(year, 11, 1, 9); // 02:00 PDT = 09:00 UTC
    if unix >= dst_start && unix < dst_end {
        -7 * 3600
    } else {
        -8 * 3600
    }
}

fn local_hh_mm(unix: i64) -> (u32, u32) {
    let sod = (unix + tz_offset_secs(unix)).rem_euclid(86400);
    ((sod / 3600) as u32, ((sod % 3600) / 60) as u32)
}

/// Sun altitude (degrees) at `unix`, seen from SUN_LAT/LON. Meeus low-precision solar position. `> 0` = sun up.
fn sun_altitude_deg(unix: i64) -> f64 {
    const J2000_UNIX: f64 = 946_728_000.0;
    let d = (unix as f64 - J2000_UNIX) / 86_400.0;
    let l = norm360(280.460 + 0.9856474 * d);
    let g = norm360(357.528 + 0.9856003 * d);
    let lam = rad(l + 1.915 * libm::sin(rad(g)) + 0.020 * libm::sin(rad(2.0 * g)));
    let eps = rad(23.439 - 0.0000004 * d);
    let ra = libm::atan2(libm::cos(eps) * libm::sin(lam), libm::cos(lam));
    let dec = libm::asin(libm::sin(eps) * libm::sin(lam));
    altitude_deg(d, ra, dec)
}

/// Moon altitude (degrees) at `unix` from SUN_LAT/LON — Meeus low-precision lunar theory with the dominant periodic terms. `> 0` = moon up.
fn moon_altitude_deg(unix: i64) -> f64 {
    const J2000_UNIX: f64 = 946_728_000.0;
    let d = (unix as f64 - J2000_UNIX) / 86_400.0;
    let lp = norm360(218.316 + 13.176396 * d); // mean longitude
    let m = norm360(134.963 + 13.064993 * d); // mean anomaly
    let f = norm360(93.272 + 13.229350 * d); // argument of latitude
    let dd = norm360(297.850 + 12.190749 * d); // mean elongation
    let lambda = lp + 6.289 * libm::sin(rad(m)) - 1.274 * libm::sin(rad(2.0 * (lp - dd) - m)) + 0.658 * libm::sin(rad(2.0 * (lp - dd))) - 0.186 * libm::sin(rad(norm360(357.529 + 0.985600 * d)));
    let beta = 5.128 * libm::sin(rad(f)) + 0.281 * libm::sin(rad(m + f)) - 0.278 * libm::sin(rad(f - m));
    let eps = rad(23.439 - 0.0000004 * d);
    let lam = rad(lambda);
    let bet = rad(beta);
    let ra = libm::atan2(libm::sin(lam) * libm::cos(eps) - libm::tan(bet) * libm::sin(eps), libm::cos(lam));
    let dec = libm::asin(libm::sin(bet) * libm::cos(eps) + libm::cos(bet) * libm::sin(eps) * libm::sin(lam));
    altitude_deg(d, ra, dec)
}

/// Equatorial (ra, dec in radians) → altitude in degrees at SUN_LAT/LON, `d` days from J2000.
fn altitude_deg(d: f64, ra: f64, dec: f64) -> f64 {
    let gmst = norm360(280.46061837 + 360.98564736629 * d);
    let lst = rad(norm360(gmst + SUN_LON));
    let ha = lst - ra;
    let lat = rad(SUN_LAT);
    libm::asin(libm::sin(lat) * libm::sin(dec) + libm::cos(lat) * libm::cos(dec) * libm::cos(ha)) * 180.0 / core::f64::consts::PI
}

fn rad(x: f64) -> f64 {
    x * core::f64::consts::PI / 180.0
}

fn norm360(x: f64) -> f64 {
    let m = libm::fmod(x, 360.0);
    if m < 0.0 {
        m + 360.0
    } else {
        m
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Yearly altitude maxima at Bremerton (47.5°N): sun = 90 − lat + obliquity ≈ 65.9°; moon = 90 − lat + max declination, ~69–71° near the 2025 major standstill.
    #[test]
    fn yearly_max_altitudes_match_geometry() {
        let unix = 1_790_000_000; // 2026-09
        let sun = yearly_max_altitude(unix, sun_altitude_deg);
        let moon = yearly_max_altitude(unix, moon_altitude_deg);
        assert!((65.4..=66.2).contains(&sun), "sun max {sun}");
        assert!((67.0..=72.0).contains(&moon), "moon max {moon}");
    }

    /// A refined high/low doesn't move when it's sampled from a different grid phase (now shifted by a minute), unlike the raw 6-minute grid.
    #[test]
    fn refined_extremum_is_grid_independent() {
        let slope = |t: i64| tide_core::predict(tide_core::BREMERTON, (t + 30) as f64) - tide_core::predict(tide_core::BREMERTON, (t - 30) as f64);
        let mut t = 1_790_000_000; // walk the 6-minute grid to the next real high or low
        while (slope(t) > 0.0) == (slope(t + 360) > 0.0) {
            t += 360;
        }
        let (a, b, c) = (refine_extremum(t), refine_extremum(t + 200), refine_extremum(t - 150));
        assert!((a - b).abs() <= 2 && (a - c).abs() <= 2, "{a} {b} {c}");
        assert!((a - t).abs() <= 360, "refined {a} strayed from the grid bracket at {t}");
    }

    /// The sky saturates α, so every pixel of a finished frame is opaque.
    #[test]
    fn frame_is_fully_opaque() {
        let px = render(1_790_000_000, 384, 180, Edition::Dozenal);
        assert!(px.iter().all(|&p| p >> 24 == 0xFF));
    }
}
