//! The panel's tide chart for a phone: full colour, anti-aliased, any pixel size.
//!
//! Same data as the e-ink renderer (tideglyph-fw render_tide), with now centred: tide fill on the fixed MLLW axis, dozenal-hour ticks, hi/lo markers with dozenal times, the now-line label, rotated sunrise/sunset times, and day/night by sun altitude (the sun drives the sky's red + green, the moon its blue). Phone-specific: a time axis that is nearly linear over the day around now (the centre half of the width) and tapers like a circle's edge out to half a year either side ([`TimeAxis`]); sun and moon altitude curves (horizon = the centreline, which is also mid-tide) instead of the panel's edge indicators, which rounded widget corners would clip; real glyph outlines from the dozenal font.
//!
//! Sky, water, waterline and the curves are drawn as vertical slices, one per sample instant, averaged per pixel column in linear light ([`column`]): no stroked paths, so near now they are crisp and out where a column spans weeks they become the tide's density and the curves' glow. Lines (the hi/lo bars, ticks and centreline) flip the channels their colour carries in the finished pixel, so they show on any fill; the now bar stays solid white with black edges, and the labels flip the fill beneath them. Every overlay is a fixed number of pixels at any widget size; only the type scales. Colours are solid primaries for now, to be tuned.
//!
//! Compositing is fluor-style, not Porter-Duff over: the frame starts empty in fluor's α + darkness format and is built topmost-first, each layer composed UNDER the running composite with fluor's `Blend::under` kernel (Normal mode, mirrored below — fluor's no-default-features build doesn't currently compile, so it isn't a dependency yet). Draw order, top to bottom: labels; now bar; hi/lo bars; ticks; sun curve, its stroke; moon curve, its stroke; centreline; water stroke; water fill; sky (one pass over whatever is still transparent). Labels have no stroke: each glyph pixel is the solid fill beneath it XOR the label's colour (now time: all three channels; tide times: green + blue; sunrise/sunset: red + green), so it always contrasts — and since labels go down first and opaque, every later layer early-outs under them: no line or curve is ever painted beneath type. tiny-skia only rasterizes anti-aliased coverage masks; one `^ 0x00FFFFFF` at the end flips darkness back to visible RGB.

use std::sync::OnceLock;

use tiny_skia::{FillRule, Mask, Path, PathBuilder, Transform};
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
    let axis = TimeAxis::new(unix, wf);
    // The span of time each pixel column covers: minutes near now, months at the edges.
    let spans: Vec<(i64, i64)> = (0..w).map(|x| (axis.t(x as f32), axis.t(x as f32 + 1.0))).collect();

    // Each body's highest altitude over the surrounding year, for its curve's full height (horizon on the centreline).
    let maxes = (yearly_max_altitude(unix, sun_altitude_deg), yearly_max_altitude(unix, moon_altitude_deg));
    // Sky and water, each column the average of its instants: see `column`. `fill` is the sky and water alone (what labels knock out of), `full` adds the waterline stroke.
    let mut fill = vec![0u32; w * h];
    let mut full = vec![0u32; w * h];
    for x in 0..w {
        let c = column(spans[x], h, unix as u64 ^ (x as u64) << 32, maxes);
        for y in 0..h {
            (fill[y * w + x], full[y * w + x]) = (c.fill[y], c.full[y]);
        }
    }
    // Centreline: mid-tide on the fixed axis, and the horizon for the curves. 1px, every channel of the finished pixel flipped (like the bars, which are drawn against this so a crossing flips both).
    for x in 0..w {
        full[h / 2 * w + x] ^= 0xFF_FFFF;
    }
    let mut f = Frame::new(w, h);

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
            let ex = axis.x(t);
            if (0.0..wf).contains(&ex) {
                extrema.push((ex, if prev_dir == 1 { hf * 2.0 / 3.0 } else { hf / 3.0 }, t));
            }
        }
        prev_dir = cur;
        last_pivot = i;
    }
    let label_gap = |cy: f32| ((cy - em / 2.0 - em * 0.15) as i32, (cy + em / 2.0 + em * 0.15).ceil() as i32);
    let font = face();


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
    for (xf, t, sunrise) in sun_crossings(&axis) {
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
    // The fill at a pixel (sky and water, without the waterline stroke) XOR `ink`'s channels.
    let fill_xor = |x: usize, y: usize, ink: u32| fill[y * w + x] ^ ink;
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
        vline(&mut f, ex, 0, g0, TIDE_INK, &full);
        vline(&mut f, ex, g1, h as i32, TIDE_INK, &full);
    }

    // Ticks (top + bottom edge) across the whole axis, 2px tall; local midnight 6px tall and twice as wide, so the day boundaries stand out from the hour marks: every dozenal hour (2 decimal hours) in the dozenal edition, every hour in the hourly one, on true local hour marks. Each tick is a fixed span of time, one pixel's worth at the centre: near now it's a 1px tick, and further out it narrows with the axis, so the ruler keeps the same density of ink per unit of time instead of piling up. A column's coverage is the share of its time span under ticks (exact, no snapping, so a tick between two pixels splits across them), and the tick flips the pixel's channels by that share, blended in linear light.
    let step_hours = if edition == Edition::Dozenal { 2 } else { 1 };
    let tick_s = axis.t(wf / 2.0 + 0.5) - axis.t(wf / 2.0 - 0.5);
    // The share of a column's span [a, b) under ticks of `width` seconds centred on every `step`-hour mark.
    let coverage = |(a, b): (i64, i64), step: i64, width: i64| {
        let half = width / 2;
        let (mut sum, mut m) = (0, next_mark(a - half, step));
        while m - half < b {
            sum += (m + half).min(b) - (m - half).max(a);
            m = next_mark(m + 1, step);
        }
        sum as f32 / (b - a) as f32
    };
    for x in 0..w {
        // The top 2 rows: the hour marks, with each midnight's own 1px replaced by its 2px; rows 2 to 6: the midnights alone.
        let midnights = coverage(spans[x], 24, 2 * tick_s);
        let marks = coverage(spans[x], step_hours, tick_s) - coverage(spans[x], 24, tick_s) + midnights;
        for (rows, cover) in [(0..2, marks), (h - 2..h, marks), (2..6, midnights), (h - 6..h - 2, midnights)] {
            if cover > 0.0 {
                for y in rows {
                    let i = y * w + x;
                    let (base, flipped) = (linear(full[i]), linear(full[i] ^ TICK));
                    f.under(i, encode(core::array::from_fn(|c| base[c] + (flipped[c] - base[c]) * cover.min(1.0))), 255);
                }
            }
        }
    }

    // Sky, water, waterline, the sun and moon curves and the centreline: one opaque pass under everything still not opaque.
    for (i, &c) in full.iter().enumerate() {
        f.under(i, c, 255);
    }
    f.finish()
}

// ── Time axis ───────────────────────────────────────────────────────────────

/// The day either side of now across the centre half of the width.
const CENTRE_HALF_S: f64 = 12.0 * 3600.0;
/// Half a year either side at the edges.
const EDGE_S: f64 = 182.5 * 86400.0;

/// The horizontal time axis: nearly linear around now, tapering like the edge of a circle so the edges reach half a year out. With u = −1..1 across the width, t − now = k·u / √(c − u²): the slope of a circle of radius √c, steepening without end as u nears the rim, with the offset c − 1 > 0 keeping the rim itself finite. The two constraints (t = ±12 h at u = ±½, ±182.5 days at u = ±1) fix both constants in closed form: with R = 182.5 days / 12 h = 365, c = (R² − 1) / (R² − 4) and k = 2 · 12 h · √(c − ¼).
struct TimeAxis {
    now: i64,
    half_w: f32,
    k: f64,
    c: f64,
}

impl TimeAxis {
    fn new(now: i64, wf: f32) -> TimeAxis {
        let r2 = (EDGE_S / CENTRE_HALF_S).powi(2);
        let c = (r2 - 1.0) / (r2 - 4.0);
        TimeAxis { now, half_w: wf / 2.0, k: 2.0 * CENTRE_HALF_S * (c - 0.25).sqrt(), c }
    }

    /// The time at pixel x (0 at the left edge, the width at the right).
    fn t(&self, x: f32) -> i64 {
        let u = ((x - self.half_w) / self.half_w).clamp(-1.0, 1.0) as f64;
        self.now + (self.k * u / (self.c - u * u).sqrt()) as i64
    }

    /// The pixel x of time t: the exact inverse, u = s·√(c / (1 + s²)) with s = (t − now) / k.
    fn x(&self, t: i64) -> f32 {
        let s = (t - self.now) as f64 / self.k;
        self.half_w + (s * (self.c / (1.0 + s * s)).sqrt()) as f32 * self.half_w
    }
}

/// One pixel column, per row as visible 0xRRGGBB: `fill` is its sky and water alone, `full` adds the waterline and the sun and moon curves (the centreline is XORed in afterwards, like the bars).
struct Column {
    fill: Vec<u32>,
    full: Vec<u32>,
}

/// The column covering times [a, b) as the average of one-pixel-wide slices, one per sample instant, with no lines drawn as paths anywhere. Each slice, bottom to top: that instant's sky above its waterline (the sun lights red + green, the moon blue) and water below; a 1-row stroke on the waterline; and the moon's and then the sun's altitude as a mark of fixed height, 2 rows of colour with a black row above and below. Averaged in linear light and encoded to sRGB once. Near now the slices barely differ and this is the crisp chart; months out each pixel shows the share of time the water was above it, and the curves become glows across their range of altitudes. At least 9 samples, one per 10 minutes beyond that up to 256 (a wide column's picture converges statistically, so its span doesn't matter past that: 256 puts each sample at one 8-bit step's weight), at jittered times. Every overlay is a fixed number of rows in every slice, whatever the slope: the waterline 1, each curve 4 (2 of colour between 2 of black). `maxes`: the sun's and moon's highest altitudes over the year, at the top edge.
fn column((a, b): (i64, i64), h: usize, seed: u64, maxes: (f64, f64)) -> Column {
    let hf = h as f32;
    let (water, stroke, sun_ink, moon_ink) = (linear(WATER), linear(STROKE), linear(SKY_SUN), linear(SKY_MOON));
    let (sun_curve, moon_curve) = (linear(SUN), linear(MOON));
    let n = ((b - a) / 600).clamp(9, 256);
    let mid = hf / 2.0;
    let altitude_y = |alt: f64, max: f64| mid - (alt / max) as f32 * (mid - 1.0);
    // Each instant: waterline y, sky colour (linear), sun and moon curve y.
    let slices: Vec<(f32, [f32; 3], f32, f32)> = jittered(a, b, n, seed)
        .map(|t| {
            let ft = tide_core::predict(tide_core::BREMERTON, t as f64) as f32 + MSL_TO_MLLW;
            let y = hf * (1.0 - ((ft - TIDE_MIN_FT) / (TIDE_MAX_FT - TIDE_MIN_FT)).clamp(0.0, 1.0));
            let (sun_alt, moon_alt) = (sun_altitude_deg(t), moon_altitude_deg(t));
            let (sun, moon) = (sun_alt > 0.0, moon_alt > 0.0);
            let sky: [f32; 3] = core::array::from_fn(|c| if sun { sun_ink[c] } else { 0.0 } + if moon { moon_ink[c] } else { 0.0 });
            (y, sky, altitude_y(sun_alt, maxes.0), altitude_y(moon_alt, maxes.1))
        })
        .collect();
    // Sums over the slices per row: sky above each waterline and water below it go in as difference arrays (prefix-summed at the end), the waterline's own row directly; `delta` is what the overlays (strokes, centreline, curves) change on the rows they touch.
    let mut sum = vec![[0f32; 3]; h];
    let mut sky_d = vec![[0f32; 3]; h + 1];
    let mut water_d = vec![0f32; h + 1];
    let mut delta = vec![[0f32; 3]; h];
    let beneath = |y: usize, line: f32, sky: &[f32; 3]| -> [f32; 3] {
        let wv = ((y as f32 + 1.0) - line).clamp(0.0, 1.0);
        core::array::from_fn(|c| wv * water[c] + (1.0 - wv) * sky[c])
    };
    for &(y, sky, sun_y, moon_y) in &slices {
        let fy = (y as usize).min(h);
        for c in 0..3 {
            sky_d[0][c] += sky[c];
            sky_d[fy][c] -= sky[c];
        }
        if fy < h {
            let u = beneath(fy, y, &sky);
            for c in 0..3 {
                sum[fy][c] += u[c];
            }
            water_d[fy + 1] += 1.0;
        }
        // The overlays, bottom to top: (top, bottom, ink).
        let overlays: [(f32, f32, [f32; 3]); 5] = [
            (y - 0.5, y + 0.5, stroke),
            (moon_y - 2.0, moon_y + 2.0, stroke),
            (moon_y - 1.0, moon_y + 1.0, moon_curve),
            (sun_y - 2.0, sun_y + 2.0, stroke),
            (sun_y - 1.0, sun_y + 1.0, sun_curve),
        ];
        let mut rows: Vec<usize> = overlays.iter().flat_map(|&(top, bottom, _)| (top.max(0.0) as usize)..(bottom.ceil().min(hf) as usize)).collect();
        rows.sort_unstable();
        rows.dedup();
        for r in rows {
            let base = beneath(r, y, &sky);
            let mut px = base;
            for &(top, bottom, ink) in &overlays {
                let cover = (bottom.min(r as f32 + 1.0) - top.max(r as f32)).clamp(0.0, 1.0);
                if cover > 0.0 {
                    for c in 0..3 {
                        px[c] = cover * ink[c] + (1.0 - cover) * px[c];
                    }
                }
            }
            for c in 0..3 {
                delta[r][c] += px[c] - base[c];
            }
        }
    }
    let (mut sky_run, mut water_run) = ([0f32; 3], 0f32);
    let mut out = Column { fill: vec![0; h], full: vec![0; h] };
    for y in 0..h {
        water_run += water_d[y];
        for c in 0..3 {
            sky_run[c] += sky_d[y][c];
            sum[y][c] += sky_run[c] + water_run * water[c];
        }
        let k = 1.0 / n as f32;
        out.fill[y] = encode(core::array::from_fn(|c| sum[y][c] * k));
        out.full[y] = encode(core::array::from_fn(|c| (sum[y][c] + delta[y][c]) * k));
    }
    out
}

/// Visible 0xRRGGBB → linear light per channel (the sRGB curve).
fn linear(hex: u32) -> [f32; 3] {
    core::array::from_fn(|c| {
        let v = ((hex >> (16 - 8 * c)) & 0xFF) as f32 / 255.0;
        if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
    })
}

/// Linear light per channel → visible 0xRRGGBB through the sRGB curve, floored onto the 256 codes (each code an equal share of the encoded range).
fn encode(l: [f32; 3]) -> u32 {
    l.iter().enumerate().fold(0, |acc, (c, &v)| {
        let v = v.clamp(0.0, 1.0);
        let e = if v <= 0.003_130_8 { 12.92 * v } else { 1.055 * v.powf(1.0 / 2.4) - 0.055 };
        acc | ((e * 256.0).min(255.0) as u32) << (16 - 8 * c)
    })
}

/// `n` times across [a, b), one at a random point in each of n equal strata (stratified jitter: never clumped, never on a fixed grid that could beat against the tides). `seed` fixes the draw, so a frame is reproducible.
fn jittered(a: i64, b: i64, n: i64, seed: u64) -> impl Iterator<Item = i64> {
    (0..n).map(move |i| a + ((b - a) as f64 * (i as f64 + unit_random(seed ^ (i as u64).wrapping_mul(0xD1B5_4A32_D192_ED03))) / n as f64) as i64)
}

/// A uniform number in [0, 1) from `x`: splitmix64's finaliser, top 53 bits.
fn unit_random(x: u64) -> f64 {
    let mut z = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    ((z ^ (z >> 31)) >> 11) as f64 / (1u64 << 53) as f64
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

/// The running composite in fluor's α + darkness format, built topmost-first with `under`. Carries a reusable coverage mask for path rasterization (the glyphs).
struct Frame {
    w: usize,
    h: usize,
    px: Vec<Argb8>,
    mask: Mask,
}

impl Frame {
    fn new(w: usize, h: usize) -> Self {
        Self { w, h, px: vec![0; w * h], mask: Mask::new(w as u32, h as u32).expect("mask size") }
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

    /// Flip darkness back to visible RGB (fluor's present-boundary XOR). The sky saturated α, so every pixel is opaque.
    fn finish(self) -> Vec<u32> {
        self.px.into_iter().map(|p| p ^ 0x00FF_FFFF).collect()
    }
}

/// Bare 1px vertical bar in the pixel column containing `x`, rows y0..y1, no edges (the hi/lo bars): each pixel is what's beneath it (`under`, the frame's background) with the channels `ink` carries flipped, so the bar shows on any fill.
fn vline(f: &mut Frame, x: f32, y0: i32, y1: i32, ink: u32, under: &[u32]) {
    let col = x.floor() as i32;
    if !(0..f.w as i32).contains(&col) {
        return;
    }
    for y in y0.max(0)..y1.min(f.h as i32) {
        let i = y as usize * f.w + col as usize;
        f.under(i, under[i] ^ ink, 255);
    }
}

/// Solid 1px vertical bar in the pixel column containing `x`, rows y0..y1, then its 1px black edge on each side (the now bar).
fn edged_vline(f: &mut Frame, x: f32, y0: i32, y1: i32, hex: u32) {
    if y1 <= y0 {
        return;
    }
    let col = x.floor() as i32;
    f.under_rect(col, y0, col + 1, y1, hex);
    f.under_rect(col - 1, y0, col, y1, STROKE);
    f.under_rect(col + 1, y0, col + 2, y1, STROKE);
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

/// Sunrise/sunset in the ±12h around now: (x, crossing time, is sunrise), where the sun's altitude crosses 0° (the seam), bisected to the second.
fn sun_crossings(axis: &TimeAxis) -> Vec<(f32, i64, bool)> {
    let (unix, wf) = (axis.now, 2.0 * axis.half_w);
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
            let xf = axis.x(hi_t);
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

/// The first local hour mark at or after `t` whose local hour is a multiple of `step` (1: every hour, 2: every dozenal hour, 24: midnight). Pacific offsets are whole hours, so local hour marks are UTC hour marks; across a daylight-saving change the local hours skip or repeat and this just takes the next one that fits.
fn next_mark(t: i64, step: i64) -> i64 {
    let mut m = t + (-(t + tz_offset_secs(t))).rem_euclid(3600);
    while (m + tz_offset_secs(m)).rem_euclid(86400) / 3600 % step != 0 {
        m += 3600;
    }
    m
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
