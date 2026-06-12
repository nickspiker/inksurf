//! tide-display — daemon that renders Bremerton WA tide predictions to the Adafruit 6414 BWRY e-ink panel every 6 minutes.
//!
//! Layout (panel is 384 wide × 180 tall):
//!   - Pixels above the tide curve: white
//!   - Pixels at or below the tide curve: yellow
//!   - 24-hour window with current time centered at x=192 (±12 hours).
//!
//! No anti-aliasing, no curve line — the curve is just the boundary between the two filled regions.

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Datelike, Duration, Local, TimeZone, Timelike, Utc};
use chrono_tz::US::Pacific;
use serde::Deserialize;
use std::collections::HashMap;
use std::io::Write;
use std::sync::OnceLock;
use std::time::Duration as StdDuration;

// ============================================================================
// Custom variable-width 12-px bitmap font. Each glyph is a separate PNG in /mnt/Octopus/Code/eink/assets/font/, embedded at compile time. Glyph height is uniform (12 px); width varies per glyph for proportional spacing.
// ============================================================================

struct Glyph {
    width: usize,
    /// width * 12 bytes, row-major. Non-zero = pixel is "on".
    bits: Vec<u8>,
}

static GLYPHS: OnceLock<HashMap<char, Glyph>> = OnceLock::new();

macro_rules! glyph {
    ($ch:expr, $path:literal) => {
        ($ch, decode_png(include_bytes!(concat!("../../assets/font/", $path))))
    };
}

fn glyphs() -> &'static HashMap<char, Glyph> {
    GLYPHS.get_or_init(|| {
        let entries = [
            glyph!('0', "0.png"),
            glyph!('1', "1.png"),
            glyph!('2', "2.png"),
            glyph!('3', "3.png"),
            glyph!('4', "4.png"),
            glyph!('5', "5.png"),
            glyph!('6', "6.png"),
            glyph!('7', "7.png"),
            glyph!('8', "8.png"),
            glyph!('9', "9.png"),
            glyph!(':', ":.png"),
        ];
        entries.into_iter().collect()
    })
}

fn decode_png(bytes: &[u8]) -> Glyph {
    let mut decoder = png::Decoder::new(bytes);
    // Expand 1/2/4-bit indexed/grayscale to 8 bit so we get one byte per pixel.
    decoder.set_transformations(png::Transformations::EXPAND);
    let mut reader = decoder.read_info().expect("png read_info");
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).expect("png next_frame");
    let w = info.width as usize;
    let h = info.height as usize;
    assert_eq!(h, GLYPH_H, "all font PNGs must be {} px tall", GLYPH_H);
    let bpp = (info.line_size / w).max(1);
    let mut bits = Vec::with_capacity(w * h);
    for y in 0..h {
        for x in 0..w {
            let idx = (y * w + x) * bpp;
            // "On" = pixel brighter than mid-gray (assumes white-on-black PNG).
            bits.push(if buf[idx] > 128 { 1 } else { 0 });
        }
    }
    Glyph { width: w, bits }
}

const GLYPH_H: usize = 12;
/// Horizontal gap inserted between glyphs (px).
const GLYPH_KERN: i32 = 1;

/// Paint mode for the drawing primitives. `Solid(c)` stamps `c` directly; `Invert` flips the underlying pixel: BLACK↔WHITE, RED↔YELLOW. The invert mode keeps the "now" indicator visible across sunrise/sunset transitions without snapping the whole line/text from one solid color to another.
#[derive(Copy, Clone)]
enum Paint {
    Solid(u8),
    Invert,
}

fn invert_code(c: u8) -> u8 {
    match c {
        BLACK  => WHITE,
        WHITE  => BLACK,
        YELLOW => RED,
        RED    => YELLOW,
        _      => c,
    }
}

#[derive(Copy, Clone)]
enum TextAnchor {
    Center,
    #[allow(dead_code)]
    Left,
    /// Align the *center* of the first occurrence of this glyph with `anchor_x`. Text builds outward (left and right) from that pivot. Lets a separator like ':' sit exactly on a marker line regardless of HH-portion width.
    AlignChar(char),
}

/// Stamp `text` into `codes` using the custom 12-px bitmap font. `top_y` is the y-coordinate of the glyph's top row. Off-canvas pixels are silently clipped.
fn draw_text(codes: &mut [u8], text: &str, anchor_x: i32, top_y: i32, anchor: TextAnchor, color: u8) {
    let gs = glyphs();
    // Compute total width = sum(glyph widths) + kerning between glyphs.
    let widths: Vec<i32> = text.chars()
        .map(|ch| gs.get(&ch).map(|g| g.width as i32).unwrap_or(0))
        .collect();
    let total_w: i32 = widths.iter().sum::<i32>() + GLYPH_KERN * (widths.len() as i32 - 1).max(0);
    let start_x = match anchor {
        TextAnchor::Center => anchor_x - total_w / 2,
        TextAnchor::Left   => anchor_x,
        TextAnchor::AlignChar(pivot) => {
            // Find the pivot's index in `text`, compute the cumulative left-of-pivot width, then start_x = anchor_x - pivot_center_offset.
            let pivot_idx = text.chars().position(|c| c == pivot);
            match pivot_idx {
                Some(idx) => {
                    // Width up to (but not including) the pivot, plus the trailing kern that separates pivot from preceding glyph.
                    let lead: i32 = widths[..idx].iter().sum::<i32>()
                        + GLYPH_KERN * idx as i32;
                    let pivot_w = widths[idx];
                    anchor_x - lead - pivot_w / 2
                }
                None => anchor_x - total_w / 2,
            }
        }
    };

    let mut cursor_x = start_x;
    for ch in text.chars() {
        let Some(g) = gs.get(&ch) else { continue; };
        for gy in 0..GLYPH_H as i32 {
            let py = top_y + gy;
            if py < 0 || py >= PANEL_H as i32 { continue; }
            for gx in 0..g.width as i32 {
                let px = cursor_x + gx;
                if px < 0 || px >= PANEL_W as i32 { continue; }
                if g.bits[gy as usize * g.width + gx as usize] != 0 {
                    codes[py as usize * PANEL_W + px as usize] = color;
                }
            }
        }
        cursor_x += g.width as i32 + GLYPH_KERN;
    }
}

const STATION_ID: &str = "9445958"; // Bremerton, WA
const DEVICE: &str = "/dev/ttyACM0";

// Sunrise/sunset location — Southworth, WA (close enough to Bremerton for solar timing; same time zone, same minute-of-sunrise to within seconds).
const SUN_LAT: f64 = 47.5126;
const SUN_LON: f64 = -122.5054;

// Fixed tide-height bounds for the vertical axis (MLLW datum, in feet). These are HAT/LAT for Bremerton WA, converted from NAVD88 by subtracting the MLLW→NAVD88 offset of 11.38 ft. Hard-coding keeps the scale stable day-to-day so a viewer can read absolute height by eye.
const TIDE_MIN_FT: f32 = -4.61; // LAT
const TIDE_MAX_FT: f32 = 14.09; // HAT

// Panel geometry (matches src/panel_jd79667.rs).
const PANEL_W: usize = 384;
const PANEL_H: usize = 180;
const CHIP_W: usize = 180;
const CHIP_ROW_BYTES: usize = CHIP_W / 4; // 45
const FB_BYTES: usize = 17_664;

// Color codes packed into chip RAM.
const BLACK:  u8 = 0b00;
const WHITE:  u8 = 0b01;
const YELLOW: u8 = 0b10;
#[allow(dead_code)]
const RED:    u8 = 0b11;

// Refresh cadence — pick a fresh random interval each cycle inside this
// window so the panel updates don't land on identical wall-clock minutes
// hour after hour.
const TICK_MIN_SECS: u64 = 7 * 60;
const TICK_MAX_SECS: u64 = 9 * 60;

fn main() -> Result<()> {
    eprintln!("tide-display: station {STATION_ID}, refresh every {}–{}s", TICK_MIN_SECS, TICK_MAX_SECS);
    loop {
        let errored = match tick() {
            Ok(()) => false,
            Err(e) => {
                eprintln!("[{}] cycle error: {e:#}", Local::now().format("%H:%M:%S"));
                true
            }
        };
        let sleep_secs = if errored {
            30
        } else {
            fastrand::u64(TICK_MIN_SECS..=TICK_MAX_SECS)
        };
        eprintln!("[{}] sleeping {sleep_secs}s", Local::now().format("%H:%M:%S"));
        std::thread::sleep(StdDuration::from_secs(sleep_secs));
    }
}

fn tick() -> Result<()> {
    if std::env::var_os("TIDE_CAL").is_some() {
        let canvas = calibration_canvas();
        let fb = pack_to_chip(&canvas);
        send_to_panel(&fb)?;
        return Ok(());
    }
    let now = Utc::now();

    // Run a deep-clean color cycle if we just crossed solar midnight within
    // the last tick — exercises all four ink particles to clear ghosting.
    // Use TICK_MAX_SECS so the longest possible gap between ticks still
    // catches the event.
    let solar_mid = most_recent_solar_midnight(now);
    let since = (now - solar_mid).num_seconds();
    if since >= 0 && since < TICK_MAX_SECS as i64 {
        eprintln!("  solar midnight crossed ({}); running deep clean",
            solar_mid.with_timezone(&Pacific).format("%Y-%m-%d %H:%M:%S %Z"));
        if let Err(e) = deep_clean() {
            eprintln!("  deep clean error: {e:#}");
        }
    }

    let preds = fetch_predictions(now)?;
    let canvas = render(&preds, now)?;
    let fb = pack_to_chip(&canvas);
    send_to_panel(&fb)?;
    Ok(())
}

/// Solar midnight = midpoint between yesterday's sunset and today's sunrise. Returns None if the underlying sunrise/sunset computation hits an edge case (e.g., polar day / night where the sun doesn't rise or set — irrelevant for our 47° N location but cleanly handled).
fn solar_midnight_for_date(date: chrono::NaiveDate) -> Option<DateTime<Utc>> {
    let yesterday = date.checked_sub_days(chrono::Days::new(1))?;
    let (_, ss) = sunrise::sunrise_sunset(
        SUN_LAT, SUN_LON,
        yesterday.year(), yesterday.month() as u32, yesterday.day() as u32,
    );
    let (sr, _) = sunrise::sunrise_sunset(
        SUN_LAT, SUN_LON,
        date.year(), date.month() as u32, date.day() as u32,
    );
    DateTime::<Utc>::from_timestamp((ss + sr) / 2, 0)
}

fn most_recent_solar_midnight(now: DateTime<Utc>) -> DateTime<Utc> {
    let local_today = now.with_timezone(&Pacific).date_naive();
    let today = solar_midnight_for_date(local_today)
        .expect("solar midnight today");
    if today <= now {
        today
    } else {
        // Today's solar midnight hasn't happened yet (likely we're between local midnight and dawn). Use yesterday's.
        let yesterday = local_today - chrono::Duration::days(1);
        solar_midnight_for_date(yesterday).expect("solar midnight yesterday")
    }
}

/// Cycle the panel through all four BWRY ink colors as a ghosting-clear. Each fill triggers a full chip refresh (~60s), so this blocks for ~4 min.
fn deep_clean() -> Result<()> {
    let mut port = serialport::new(DEVICE, 115_200)
        .timeout(StdDuration::from_secs(2))
        .open()
        .with_context(|| format!("opening {DEVICE}"))?;
    // Re-select panel mode in case the firmware just booted (no-op otherwise).
    port.write_all(&[b'M', MODE_JD79667])?;
    port.flush()?;
    std::thread::sleep(StdDuration::from_millis(100));
    for ch in ['K', 'Y', 'R', 'W'] {
        eprintln!("  deep clean: fill '{}'", ch);
        port.write_all(&[ch as u8])?;
        port.flush()?;
        std::thread::sleep(StdDuration::from_secs(65));
    }
    eprintln!("  deep clean done");
    Ok(())
}

/// Diagnostic test pattern: TL=BLACK, TR=YELLOW, BL=RED, BR=WHITE. Tells us unambiguously how source (x, y) maps to physical panel corners.
fn calibration_canvas() -> Canvas {
    let mut codes = vec![WHITE; PANEL_W * PANEL_H];
    for y in 0..PANEL_H {
        for x in 0..PANEL_W {
            let top = y < PANEL_H / 2;
            let left = x < PANEL_W / 2;
            let c = match (top, left) {
                (true,  true)  => BLACK,
                (true,  false) => YELLOW,
                (false, true)  => RED,
                (false, false) => WHITE,
            };
            codes[y * PANEL_W + x] = c;
        }
    }
    eprintln!("  CALIBRATION pattern: TL=BLK TR=YEL BL=RED BR=WHT");
    if let Err(e) = dump_ppm(&codes, "/tmp/tide-render.ppm") {
        eprintln!("  ppm dump failed: {e}");
    }
    Canvas { codes }
}

// ============================================================================
// NOAA CO-OPS predictions fetch.
// ============================================================================

#[derive(Debug, Deserialize)]
struct PredictionResponse {
    #[serde(default)]
    predictions: Vec<Prediction>,
    #[serde(default)]
    error: Option<NoaaError>,
}

#[derive(Debug, Deserialize)]
struct NoaaError {
    message: String,
}

#[derive(Debug, Deserialize, Clone)]
struct Prediction {
    /// "YYYY-MM-DD HH:MM" in the station's local time (lst_ldt = automatic DST).
    t: String,
    /// Tide height in feet (string in the API).
    v: String,
}

#[derive(Debug, Clone)]
struct Sample {
    when: DateTime<Utc>,
    height_ft: f32,
}

fn fetch_predictions(now: DateTime<Utc>) -> Result<Vec<Sample>> {
    // Fetch ±12 hours centered on now, in 6-min intervals (~241 samples). NOAA's `time_zone=lst_ldt` means it interprets begin_date / end_date as the station's local time — so we must format in Pacific, not UTC, or we end up requesting a window that's offset by the TZ delta.
    let begin = (now - Duration::hours(12)).with_timezone(&Pacific);
    let end   = (now + Duration::hours(12)).with_timezone(&Pacific);
    let url = format!(
        "https://api.tidesandcurrents.noaa.gov/api/prod/datagetter\
         ?begin_date={}&end_date={}\
         &station={}\
         &product=predictions&datum=MLLW&interval=6\
         &units=english&time_zone=lst_ldt&format=json\
         &application=tide-display",
        begin.format("%Y%m%d %H:%M"),
        end.format("%Y%m%d %H:%M"),
        STATION_ID,
    );

    let resp: PredictionResponse = reqwest::blocking::Client::builder()
        .timeout(StdDuration::from_secs(30))
        .build()?
        .get(&url)
        .send()
        .context("NOAA request")?
        .error_for_status()?
        .json()
        .context("NOAA JSON decode")?;

    if let Some(e) = resp.error {
        return Err(anyhow!("NOAA error: {}", e.message));
    }
    if resp.predictions.is_empty() {
        return Err(anyhow!("NOAA returned no predictions"));
    }

    let mut samples = Vec::with_capacity(resp.predictions.len());
    for p in &resp.predictions {
        let naive = chrono::NaiveDateTime::parse_from_str(&p.t, "%Y-%m-%d %H:%M")
            .with_context(|| format!("parse time {:?}", p.t))?;
        let pacific = Pacific.from_local_datetime(&naive).single()
            .ok_or_else(|| anyhow!("ambiguous local time {:?}", p.t))?;
        let utc = pacific.with_timezone(&Utc);
        let height: f32 = p.v.parse().with_context(|| format!("parse height {:?}", p.v))?;
        samples.push(Sample { when: utc, height_ft: height });
    }
    eprintln!("  fetched {} samples (height range {:.2}..{:.2} ft)",
        samples.len(),
        samples.iter().map(|s| s.height_ft).fold(f32::INFINITY, f32::min),
        samples.iter().map(|s| s.height_ft).fold(f32::NEG_INFINITY, f32::max),
    );
    Ok(samples)
}

// ============================================================================
// Rendering: fill 384×180 codes directly. Above curve = WHITE, at/below = YELLOW.
// ============================================================================

struct Canvas {
    /// One color code per pixel, length PANEL_W × PANEL_H.
    codes: Vec<u8>,
}

fn render(samples: &[Sample], now: DateTime<Utc>) -> Result<Canvas> {
    // X axis: ±12h around `now`, with now at PANEL_W/2. Y axis: fixed HAT/LAT bounds in MLLW coords, inverted (high = top).
    let min_h = TIDE_MIN_FT;
    let range = TIDE_MAX_FT - TIDE_MIN_FT;

    let y_for = |height: f32| -> usize {
        let t = (height - min_h) / range;
        let y = (PANEL_H as f32 - 1.0) - t * (PANEL_H as f32 - 1.0);
        y.clamp(0.0, (PANEL_H - 1) as f32) as usize
    };

    // Render everything in "day style"; a final pass inverts each column that's nighttime so the whole night theme (BLACK above, RED below, YELLOW markers, WHITE now line) falls out of one invert step.

    let window_secs = 24.0 * 3600.0;
    let time_at_x = |x: usize| -> DateTime<Utc> {
        let frac = (x as f32 - PANEL_W as f32 / 2.0) / PANEL_W as f32;
        let offset_ms = (frac * window_secs * 1000.0) as i64;
        now + Duration::milliseconds(offset_ms)
    };

    let mut codes = vec![WHITE; PANEL_W * PANEL_H];
    for x in 0..PANEL_W {
        let t = time_at_x(x);
        let h = interpolate_height(samples, t);
        let cy = y_for(h);
        for y in cy..PANEL_H {
            codes[y * PANEL_W + x] = YELLOW;
        }
    }

    // Hi/Lo tide markers (RED). Marker lines span the full panel height; labels sit OPPOSITE the curve at that x — HIGH tide labels in the bottom third (chart peak is at top), LOW tide labels in the top third (chart trough is at bottom). Colon ':' is anchored on the line so it visually melts into it.
    let extrema = find_extrema(samples, now);
    let top_third_y:    i32 = (PANEL_H as i32 / 3 - GLYPH_H as i32) / 2;
    let bottom_third_y: i32 = PANEL_H as i32 - (PANEL_H as i32 / 3 + GLYPH_H as i32) / 2;
    let center_y:       i32 = (PANEL_H as i32 - GLYPH_H as i32) / 2;

    for (ev, kind) in &extrema {
        let dt = (ev.when - now).num_seconds() as f32;
        let frac = dt / (24.0 * 3600.0);
        let x = (PANEL_W as f32 / 2.0 + frac * PANEL_W as f32).round() as i32;
        if x < 0 || x >= PANEL_W as i32 { continue; }
        let label_y = match kind {
            ExtremumKind::High => bottom_third_y,
            ExtremumKind::Low  => top_third_y,
        };
        // Line in two segments — above and below the label cell — so the colon and digit interiors don't get a red bar painted through them.
        draw_v_line_split(&mut codes, x, label_y, GLYPH_H as i32, RED);
        let local = ev.when.with_timezone(&Pacific);
        let label = local.format("%H:%M").to_string();
        draw_text(&mut codes, &label, x, label_y, TextAnchor::AlignChar(':'), RED);
    }

    // Hourly tick marks (1 px BLACK in day style, top + bottom edges). Painted before the now line + invert pass so they invert with the column scheme automatically.
    {
        let local_start = (now - Duration::hours(12)).with_timezone(&Pacific);
        let mut t_hour = local_start
            .with_minute(0).unwrap()
            .with_second(0).unwrap()
            .with_nanosecond(0).unwrap();
        if t_hour < local_start { t_hour = t_hour + Duration::hours(1); }
        let window_end = now + Duration::hours(12);
        let mut t = t_hour.with_timezone(&Utc);
        while t < window_end {
            let dt = (t - now).num_seconds() as f32;
            let frac = dt / (24.0 * 3600.0);
            let x = (PANEL_W as f32 / 2.0 + frac * PANEL_W as f32).round() as i32;
            // Local midnight gets a taller (2 px) tick to anchor day boundaries.
            let tick_h: i32 = if t.with_timezone(&Pacific).hour() == 0 { 2 } else { 1 };
            // Skip the leftmost / rightmost 3 columns — reserved for the moon and solar-year cycle indicators.
            if x >= 3 && x < PANEL_W as i32 - 3 {
                for dy in 0..tick_h {
                    codes[dy as usize * PANEL_W + x as usize] = BLACK;
                    codes[(PANEL_H as i32 - 1 - dy) as usize * PANEL_W + x as usize] = BLACK;
                }
            }
            t = t + Duration::hours(1);
        }
    }

    // "Now" marker (BLACK in day style; the column-invert pass below will flip it to WHITE in any night columns automatically).
    let now_x = (PANEL_W / 2) as i32;
    draw_v_line_split(&mut codes, now_x, center_y, GLYPH_H as i32, BLACK);
    let now_local = now.with_timezone(&Pacific);
    let now_label = now_local.format("%H:%M").to_string();
    draw_text(&mut codes, &now_label, now_x, center_y, TextAnchor::AlignChar(':'), BLACK);

    // Final pass: invert every column whose time is in the dark hours. Maps BLACK↔WHITE and RED↔YELLOW — that's the whole night theme.
    for x in 0..PANEL_W {
        if !is_night(time_at_x(x)) { continue; }
        for y in 0..PANEL_H {
            let i = y * PANEL_W + x;
            codes[i] = invert_code(codes[i]);
        }
    }

    // Vertical sunrise/sunset time labels — drawn after the column invert so pixels in night columns end up double-inverted (= day style) and pixels in day columns get inverted once. Either way the labels read as the opposite of whatever was painted beneath.
    let panel_mid_y = PANEL_H as i32 / 2;
    for (ev_time, kind) in find_sun_events(now) {
        let dt = (ev_time - now).num_seconds() as f32;
        let frac = dt / (24.0 * 3600.0);
        let x_f = PANEL_W as f32 / 2.0 + frac * PANEL_W as f32;
        if x_f < 0.0 || x_f >= PANEL_W as f32 { continue; }
        let label = ev_time.with_timezone(&Pacific).format("%H:%M").to_string();
        draw_text_rotated_invert(&mut codes, &label, x_f, panel_mid_y, kind);
    }

    // Moon (left edge) + solar year (right edge) cycle indicators. 2-pixel diagonal arrow most of the time; 2-pixel horizontal line within 12h of a max/min event. All pixels invert what's under them.
    let now_ts = now.timestamp();
    let moon_phase  = cycle_phase(now_ts, NEW_MOON_REF_UNIX, SYNODIC_SECS);
    let year_phase  = cycle_phase(now_ts, WINTER_SOLSTICE_REF_UNIX, YEAR_SECS);
    let moon_kind   = cycle_event_kind(moon_phase, EVENT_WINDOW_SECS / SYNODIC_SECS);
    let year_kind   = cycle_event_kind(year_phase, EVENT_WINDOW_SECS / YEAR_SECS);
    draw_cycle_indicator(&mut codes, /*left_side=*/ true,  moon_kind, phase_to_arrow_y_top(moon_phase));
    draw_cycle_indicator(&mut codes, /*left_side=*/ false, year_kind, phase_to_arrow_y_top(year_phase));
    // Side-effect: dump a PPM so we can compare what we packed against what the panel actually displays.
    if let Err(e) = dump_ppm(&codes, "/tmp/tide-render.ppm") {
        eprintln!("  ppm dump failed: {e}");
    } else {
        eprintln!("  wrote /tmp/tide-render.ppm ({}×{} px)", PANEL_W, PANEL_H);
    }
    Ok(Canvas { codes })
}

fn dump_ppm(codes: &[u8], path: &str) -> Result<()> {
    use std::fs::File;
    use std::io::BufWriter;
    let mut f = BufWriter::new(File::create(path)?);
    write!(f, "P6\n{} {}\n255\n", PANEL_W, PANEL_H)?;
    for &c in codes {
        let (r, g, b) = match c {
            BLACK  => (  0u8,   0u8,   0u8),
            WHITE  => (255,   255,   255),
            YELLOW => (235,   195,    35),
            RED    => (200,    30,    30),
            _      => (128,   128,   128),
        };
        f.write_all(&[r, g, b])?;
    }
    Ok(())
}

#[derive(Copy, Clone, Debug)]
enum ExtremumKind {
    High,
    Low,
}

/// Local maxima/minima of the tide curve within the visible 24h window centered on `now`. Direction-change detection: walk consecutive samples tracking the last non-flat slope; when the slope flips sign, the last sample before the flip is the extremum. Handles plateaus (NOAA predictions round to 0.001 ft, so the true minimum often spans 2-3 identical samples around the inflection).
fn find_extrema(samples: &[Sample], now: DateTime<Utc>) -> Vec<(Sample, ExtremumKind)> {
    let win_start = now - Duration::hours(12);
    let win_end   = now + Duration::hours(12);
    let mut out = Vec::new();
    if samples.len() < 2 { return out; }
    #[derive(PartialEq, Clone, Copy)]
    enum Dir { Up, Down }
    let mut prev_dir: Option<Dir> = None;
    let mut last_pivot: usize = 0;
    for i in 1..samples.len() {
        let a = samples[i - 1].height_ft;
        let b = samples[i].height_ft;
        let cur = if b > a { Some(Dir::Up) } else if b < a { Some(Dir::Down) } else { None };
        if let Some(cur) = cur {
            if let Some(prev) = prev_dir {
                if cur != prev {
                    // Pivot is the midpoint of the flat run between the last directional sample and this one - that's the symmetric minimum/maximum given the underlying quadratic shape near an extremum.
                    let mid = (last_pivot + i - 1) / 2;
                    let s = &samples[mid];
                    if s.when >= win_start && s.when <= win_end {
                        let kind = if prev == Dir::Up { ExtremumKind::High } else { ExtremumKind::Low };
                        out.push((s.clone(), kind));
                    }
                }
            }
            prev_dir = Some(cur);
            last_pivot = i;
        }
    }
    out
}

/// Paint a 1-px vertical line in `color` from y=0 to y=PANEL_H, skipping the y range `[gap_top, gap_top + gap_h)` so a text glyph cell stays clear.
fn draw_v_line_split(codes: &mut [u8], x: i32, gap_top: i32, gap_h: i32, color: u8) {
    if x < 0 || x >= PANEL_W as i32 { return; }
    let gap_end = gap_top + gap_h;
    for y in 0..PANEL_H as i32 {
        if y >= gap_top && y < gap_end { continue; }
        codes[y as usize * PANEL_W + x as usize] = color;
    }
}

#[derive(Copy, Clone)]
enum SunEvent {
    /// Sunrise — text rotated 90° CCW, reads bottom-to-top.
    Sunrise,
    /// Sunset — text rotated 90° CW, reads top-to-bottom.
    Sunset,
}

/// All sunrise/sunset events that fall inside the ±12h visible window centered on `now`. We probe yesterday, today, and tomorrow to catch whichever events land in the window regardless of what time of day it is.
fn find_sun_events(now: DateTime<Utc>) -> Vec<(DateTime<Utc>, SunEvent)> {
    let mut out = Vec::new();
    let win_start = now - Duration::hours(12);
    let win_end   = now + Duration::hours(12);
    let local_today = now.with_timezone(&Pacific).date_naive();
    for day_offset in -1..=1i64 {
        let d = local_today + chrono::Duration::days(day_offset);
        let (sr_ts, ss_ts) = sunrise::sunrise_sunset(
            SUN_LAT, SUN_LON, d.year(), d.month() as u32, d.day() as u32,
        );
        for (ts, kind) in [(sr_ts, SunEvent::Sunrise), (ss_ts, SunEvent::Sunset)] {
            if let Some(t) = DateTime::<Utc>::from_timestamp(ts, 0) {
                if t >= win_start && t < win_end { out.push((t, kind)); }
            }
        }
    }
    out
}

/// Stamp `text` into `codes` with each glyph rotated 90° (CCW for sunrise, CW for sunset), stacked so the text reads bottom→top (sunrise) or top→bottom (sunset). Centered horizontally on `center_x` (sub-pixel float so the 12-wide rotated cell can pick the nearest-pixel position rather than always biasing left) and vertically on `center_y`. Each "on" glyph pixel inverts the underlying pixel.
fn draw_text_rotated_invert(
    codes: &mut [u8],
    text: &str,
    center_x: f32,
    center_y: i32,
    event: SunEvent,
) {
    let gs = glyphs();
    let widths: Vec<i32> = text.chars()
        .map(|ch| gs.get(&ch).map(|g| g.width as i32).unwrap_or(0))
        .collect();
    let count = widths.len() as i32;
    // After rotation each char takes its original width as new vertical height.
    let total_h: i32 = widths.iter().sum::<i32>() + GLYPH_KERN * (count - 1).max(0);
    let block_top = center_y - total_h / 2;
    let block_bot = block_top + total_h;

    let rot_w = GLYPH_H as i32; // 12 px wide after rotation
    // Place the cell so its visual center sits on the seam (the boundary between the last night column and the first day column). For sunrise at `x_f`, the seam is at `floor(x_f) + 0.5` when x_f is strictly between integers, and at `x_f - 0.5` when x_f is exactly an integer. `ceil(x_f) - rot_w/2` matches both cases.
    let glyph_left = center_x.ceil() as i32 - rot_w / 2;

    // Sunrise: first char of `text` (e.g. '0' of "05:14") sits at the BOTTOM. Sunset: first char sits at the TOP.
    let mut cursor_bottom = block_bot; // bottom edge of next sunrise glyph
    let mut cursor_top    = block_top; // top edge of next sunset glyph

    for ch in text.chars() {
        let Some(g) = gs.get(&ch) else { continue; };
        let rot_h = g.width as i32;
        let (glyph_top, glyph_bot_excl) = match event {
            SunEvent::Sunrise => (cursor_bottom - rot_h, cursor_bottom),
            SunEvent::Sunset  => (cursor_top, cursor_top + rot_h),
        };

        for src_y in 0..GLYPH_H as i32 {
            for src_x in 0..g.width as i32 {
                if g.bits[src_y as usize * g.width + src_x as usize] == 0 { continue; }
                let (rx, ry) = match event {
                    // CCW: (src_x, src_y) → (src_y, src_w - 1 - src_x)
                    SunEvent::Sunrise => (src_y, g.width as i32 - 1 - src_x),
                    // CW:  (src_x, src_y) → (src_h - 1 - src_y, src_x)
                    SunEvent::Sunset  => (GLYPH_H as i32 - 1 - src_y, src_x),
                };
                let px = glyph_left + rx;
                let py = glyph_top + ry;
                if px < 0 || px >= PANEL_W as i32 { continue; }
                if py < 0 || py >= PANEL_H as i32 { continue; }
                let _ = glyph_bot_excl; // keep var alive for clarity in code review
                let idx = py as usize * PANEL_W + px as usize;
                codes[idx] = invert_code(codes[idx]);
            }
        }

        match event {
            SunEvent::Sunrise => cursor_bottom -= rot_h + GLYPH_KERN,
            SunEvent::Sunset  => cursor_top    += rot_h + GLYPH_KERN,
        }
    }
}

// ── Cosmological-cycle indicators (left edge = moon, right edge = year). ──

/// Reference moments + cycle lengths for the two cycles we visualize.
const NEW_MOON_REF_UNIX: i64        = 947_182_440;   // 2000-01-06 18:14 UTC
const SYNODIC_SECS: f64             = 29.530_588_853 * 86400.0;
const WINTER_SOLSTICE_REF_UNIX: i64 = 1_734_772_860; // 2024-12-21 09:21 UTC
const YEAR_SECS: f64                = 365.25 * 86400.0;
/// Half-window for the "we hit the extreme" 2-px horizontal line marker.
const EVENT_WINDOW_SECS: f64        = 12.0 * 3600.0;

/// Fraction of the cycle since the reference event, in [0, 1). `phase = 0` → reference event (new moon / winter solstice). `phase = 0.5` → opposite (full moon / summer solstice).
fn cycle_phase(now_ts: i64, ref_unix: i64, cycle_secs: f64) -> f64 {
    ((now_ts - ref_unix) as f64 / cycle_secs).rem_euclid(1.0)
}

#[derive(Copy, Clone, Debug)]
enum CycleEventKind {
    /// Within `EVENT_WINDOW_SECS` of the min event (new moon / winter solstice).
    AtMin,
    /// Within `EVENT_WINDOW_SECS` of the max event (full moon / summer solstice).
    AtMax,
    /// Heading from min toward max (waxing moon, days getting longer).
    Rising,
    /// Heading from max toward min (waning moon, days getting shorter).
    Falling,
}

fn cycle_event_kind(phase: f64, window_phase: f64) -> CycleEventKind {
    if phase < window_phase || phase > 1.0 - window_phase {
        CycleEventKind::AtMin
    } else if (phase - 0.5).abs() < window_phase {
        CycleEventKind::AtMax
    } else if phase < 0.5 {
        CycleEventKind::Rising
    } else {
        CycleEventKind::Falling
    }
}

/// Top-pixel y of the 2-row arrow, computed sinusoidally from `phase`. y=0 at full/summer, y=PANEL_H-2 at new/winter.
fn phase_to_arrow_y_top(phase: f64) -> i32 {
    let illum = 0.5 * (1.0 - (2.0 * std::f64::consts::PI * phase).cos());
    let y = ((1.0 - illum) * (PANEL_H as f64 - 2.0)).round() as i32;
    y.clamp(0, PANEL_H as i32 - 2)
}

/// Stamp the cycle indicator into `codes` on the chosen edge. `outer` is the column at the very edge (x=0 left / x=PANEL_W-1 right); `inner` is one column in. Pixels are inverted relative to whatever is already there.
fn draw_cycle_indicator(codes: &mut [u8], left_side: bool, kind: CycleEventKind, y_top: i32) {
    let outer: i32 = if left_side { 0 } else { PANEL_W as i32 - 1 };
    let inner: i32 = if left_side { 1 } else { PANEL_W as i32 - 2 };
    let put = |codes: &mut [u8], x: i32, y: i32| {
        if x < 0 || x >= PANEL_W as i32 || y < 0 || y >= PANEL_H as i32 { return; }
        let i = y as usize * PANEL_W + x as usize;
        codes[i] = invert_code(codes[i]);
    };
    match kind {
        CycleEventKind::AtMin => {
            // 2-px horizontal line on the very bottom row.
            put(codes, outer, PANEL_H as i32 - 1);
            put(codes, inner, PANEL_H as i32 - 1);
        }
        CycleEventKind::AtMax => {
            // 2-px horizontal line on the very top row.
            put(codes, outer, 0);
            put(codes, inner, 0);
        }
        CycleEventKind::Rising => {
            // X .
            // . X        (top at outer, bottom at inner)
            put(codes, outer, y_top);
            put(codes, inner, y_top + 1);
        }
        CycleEventKind::Falling => {
            // . X
            // X .        (top at inner, bottom at outer)
            put(codes, inner, y_top);
            put(codes, outer, y_top + 1);
        }
    }
}

/// True if at instant `t_utc` the sun is below the horizon at Southworth WA. Looks up that local date's sunrise + sunset and tests whether `t` falls outside the lit interval.
fn is_night(t_utc: DateTime<Utc>) -> bool {
    let local = t_utc.with_timezone(&Pacific);
    let date = local.date_naive();
    let (sunrise_ts, sunset_ts) = sunrise::sunrise_sunset(
        SUN_LAT, SUN_LON,
        date.year(), date.month() as u32, date.day() as u32,
    );
    let t_ts = t_utc.timestamp();
    t_ts < sunrise_ts || t_ts >= sunset_ts
}

/// Linear-interpolate tide height at an arbitrary time between the nearest bracketing NOAA samples. Clamps to endpoints if `t` is outside the range.
fn interpolate_height(samples: &[Sample], t: DateTime<Utc>) -> f32 {
    let i = samples.partition_point(|s| s.when <= t);
    if i == 0 { return samples[0].height_ft; }
    if i >= samples.len() { return samples.last().unwrap().height_ft; }
    let a = &samples[i - 1];
    let b = &samples[i];
    let span_ms = (b.when - a.when).num_milliseconds() as f32;
    if span_ms <= 0.0 { return a.height_ft; }
    let frac = (t - a.when).num_milliseconds() as f32 / span_ms;
    a.height_ft + frac * (b.height_ft - a.height_ft)
}

// ============================================================================
// Pack 384×180 codes → 17,664-byte chip RAM (180×384 chip orientation).
// ============================================================================

fn pack_to_chip(canvas: &Canvas) -> Vec<u8> {
    let mut fb = vec![0x55u8; FB_BYTES];
    for y_src in 0..PANEL_H {
        for x_src in 0..PANEL_W {
            let code = canvas.codes[y_src * PANEL_W + x_src] & 0x3;
            // Panel's chip_row 0 lives at the physical right edge; mirror x.
            let chip_row = PANEL_W - 1 - x_src;
            let chip_col = y_src;
            let byte_idx = chip_row * CHIP_ROW_BYTES + chip_col / 4;
            let shift = (3 - (chip_col % 4)) * 2;
            let mask = !(0b11u8 << shift);
            fb[byte_idx] = (fb[byte_idx] & mask) | (code << shift);
        }
    }
    fb
}

// ============================================================================
// Serial: send 'I' + 17,664 bytes over USB CDC.
// ============================================================================

/// Unified-firmware panel-select byte for the JD79667 driver.
const MODE_JD79667: u8 = 1;

fn send_to_panel(fb: &[u8]) -> Result<()> {
    // 90 s write timeout: long enough to ride through a firmware boot/refresh window where the CDC buffer fills before the chip starts consuming bytes.
    let mut port = serialport::new(DEVICE, 115_200)
        .timeout(StdDuration::from_secs(90))
        .open()
        .with_context(|| format!("opening {DEVICE}"))?;
    // Select JD79667 panel mode (no-op if firmware already pinned).
    port.write_all(&[b'M', MODE_JD79667])?;
    port.flush()?;
    std::thread::sleep(StdDuration::from_millis(100));
    port.write_all(&[b'I'])?;
    std::thread::sleep(StdDuration::from_millis(30));
    for chunk in fb.chunks(64) {
        port.write_all(chunk)?;
    }
    port.flush()?;
    eprintln!("  sent {} bytes; firmware refresh ~60s in background", fb.len());
    Ok(())
}
