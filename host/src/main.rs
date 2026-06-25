//! eink-host — USB CDC CLI for the ferros SSD1680 e-paper driver.

use anyhow::{Context, Result, anyhow};
use clap::{Parser, Subcommand};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::time::Duration;

// SSD1680 panel native (chip-portrait) dimensions for the Adafruit 6392
// 2.9" 296×152 panel: 152 source × 296 gates.
const PANEL_W: u32 = 152;
const PANEL_H: u32 = 296;
const ROW_BYTES: usize = ((PANEL_W as usize) + 7) / 8; // 19
const FB_BYTES: usize = ROW_BYTES * PANEL_H as usize; // 5624

#[derive(Parser)]
#[command(name = "eink-host", about = "Drive the ferros eink board")]
struct Cli {
    #[arg(short, long, default_value = "/dev/inksurf")]
    device: String,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Trigger the firmware's built-in stripe test pattern.
    Test,

    /// Send a raw BW plane file to the panel (size must match FB_BYTES).
    Raw { file: PathBuf },

    /// Convert and send an image file (jpeg/png) to the panel.
    Image {
        file: PathBuf,
        /// Rotate the source image clockwise before fitting (0/90/180/270).
        /// Defaults to 180 to compensate for panel mounting orientation.
        #[arg(long, default_value = "180")]
        rotate: u16,
        /// Invert black/white before packing.
        #[arg(long)]
        invert: bool,
        /// Dithering algorithm.
        #[arg(long, default_value = "floyd")]
        dither: DitherKind,
        /// Save the packed framebuffer to this file for debugging.
        #[arg(long)]
        save_raw: Option<PathBuf>,
    },

    /// Drive panel to all-white (OTP refresh).
    White,

    /// Drive panel to all-black (OTP refresh).
    Black,

    /// Fill BW plane with a byte value without refreshing the panel.
    Fill { byte: String },

    /// Apply a single-phase pulse and refresh.
    /// voltage: 0x80=VSH1 (push-white), 0x40=VSL (push-black), 0xC0=VSH2.
    Pulse {
        /// Voltage code (hex, e.g. 0x80).
        voltage: String,
        /// Frame count for TPa (1-255).
        frames: u8,
    },

    /// Apply a multi-phase LUT: sequence of (voltage:frames) pairs.
    /// Phases applied in order, each phase = one waveform applied for N frames.
    /// Example: --phases "0x40:2,0x80:1" pushes black for 2 frames then pulls white for 1.
    MultiPulse {
        /// Comma-separated voltage:frames pairs, max 12.
        /// voltage is hex (0x40, 0x80, 0xC0), frames is decimal 1-255.
        phases: String,
    },

    /// Fast render: 4 levels in 2 refreshes using the chip's transition logic.
    /// Refresh 1 paints high bit (sets initial W or B per pixel).
    /// Refresh 2 paints low bit + custom LUT — each (old,new) bit pair gets
    /// a different drive, producing 4 distinct end states in one shot.
    /// Floyd-Steinberg dither bridges the 4 levels. Total: ~8 seconds.
    RenderFast {
        file: PathBuf,
        #[arg(long, default_value = "180")]
        rotate: u16,
        #[arg(long, default_value = "floyd")]
        dither: DitherKind,
    },

    /// Render a horizontal-band gradient covering all achievable gray levels.
    /// Each band is one L* level from the curated recipe table.
    /// No dithering, no quantization noise — pure level test.
    Gradient {
        /// Number of bands (defaults to full recipe table).
        #[arg(long)]
        bands: Option<u8>,
        /// Use the shadow-optimised 10-level recipe set instead of the full table.
        #[arg(long)]
        shadow: bool,
        /// Use grouped-plane rendering (see render --planes).
        #[arg(long)]
        planes: bool,
    },

    /// Auto-characterize: paint the WHOLE panel uniformly at each target level
    /// (running all N multi-pass passes for proper drift context). i1Pro
    /// stays anywhere on the panel — measures the uniform field after each
    /// iteration, both raw and after a "freshen-blacks" BB-only pulse.
    /// Each run starts by measuring panel-white as a normalization reference,
    /// removing i1Pro cal whitepoint drift between recals.
    /// Use --start/--end to split into multiple sessions for mid-run recal.
    /// Output appends rows; same --out across split runs gives unified CSV.
    AutoCharacterize {
        /// Output CSV (appends if file exists).
        #[arg(long, default_value = "auto_char.csv")]
        out: PathBuf,
        /// VSL frame count for the freshen-blacks BB pass after each measurement.
        /// 0 disables the freshen-pass column (default — proved ineffective).
        #[arg(long, default_value_t = 0)]
        freshen: u8,
        /// First level index to characterize (inclusive). Default 0 (white).
        #[arg(long, default_value_t = 0)]
        start: usize,
        /// Last level index to characterize (inclusive). Default = last level.
        #[arg(long)]
        end: Option<usize>,
    },

    /// Render the gradient, then prompt for spotread measurement per band.
    /// Outputs the as-rendered L* per recipe so we can recalibrate gray_recipes.
    GradientMeasure {
        /// Number of bands (default 8 for easier i1Pro positioning).
        #[arg(long, default_value_t = 8)]
        bands: u8,
        /// Output CSV.
        #[arg(long, default_value = "gradient_measured.csv")]
        out: PathBuf,
    },

    /// Render an image with multi-pass EOTF-calibrated grayscale.
    /// Each gray level is added in one refresh pass. Total time ~N×3.5s.
    Render {
        file: PathBuf,
        /// Rotate the source image clockwise before fitting (0/90/180/270).
        #[arg(long, default_value = "180")]
        rotate: u16,
        /// Number of gray levels (4, 8, 16). Includes white as level 0.
        #[arg(long, default_value_t = 4)]
        grays: u8,
        /// Dither algorithm against the achievable levels.
        #[arg(long, default_value = "floyd")]
        dither: DitherKind,
        /// Use the shadow-optimised 10-level recipe set (ignores --grays).
        /// Clean sat+pull anchors for highlights/mids, stable singles for
        /// deep shadows. 9 passes total, ~32s.
        #[arg(long)]
        shadow: bool,
        /// Use grouped-plane rendering: one shared VSL:32 push for all sat+pull
        /// pixels, then cumulative VSH1 back-pull passes graduating pixels out,
        /// then single-pulse passes. Reduces inter-level interaction during push.
        #[arg(long)]
        planes: bool,
    },

    /// Pyramid sweep: enumerate (away, back) frame pairs at increasing pyramid
    /// levels, where a level-N pair has away+back == N. For start=white,
    /// "away" = VSL frames and "back" = VSH1 frames. For start=black, swap.
    /// Each measurement: reset to start, apply 2-phase LUT, measure.
    /// With --positions >1, repeats the sweep at each position with a prompt
    /// to reposition i1Pro between rounds.
    Pyramid {
        /// Highest pyramid level to test (level N has N entries).
        #[arg(long, default_value_t = 5)]
        levels: u8,
        /// Starting state: "white" or "black".
        #[arg(long, default_value = "white")]
        start: String,
        /// Number of panel positions to test (prompts for repositioning).
        #[arg(long, default_value_t = 1)]
        positions: u8,
        /// Output CSV.
        #[arg(long, default_value = "pyramid.csv")]
        out: PathBuf,
    },

    /// Multi-phase sweep: vary one of the phase parameters across a range
    /// and measure each result. Useful for mapping push+pull combos.
    SweepMulti {
        /// Base phases as template, e.g. "0x40:1,0x80:VAR"
        /// where VAR is the parameter being swept. The VAR phase's frames
        /// will be replaced with each value from --vals.
        template: String,
        /// Comma-separated values to substitute for VAR (frame counts).
        #[arg(long, default_value = "0,1,2,3,4,5,6,8,10,12,16,20")]
        vals: String,
        /// Reset to white between each step.
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        reset_between: bool,
        /// Output CSV.
        #[arg(long, default_value = "sweep_multi.csv")]
        out: PathBuf,
    },

    /// Run a sweep: clear to startstate, then apply pulses of increasing
    /// duration, prompting for spotread measurement between each step.
    Sweep {
        /// Starting state: "white" or "black".
        #[arg(long, default_value = "white")]
        start: String,
        /// Voltage code (hex). Default 0x40 (VSL, drive toward black).
        #[arg(long, default_value = "0x40")]
        voltage: String,
        /// Frame counts to test, comma-separated.
        #[arg(long, default_value = "1,2,3,4,5,6,8,10,12,16,20,24,32,48,64")]
        frames: String,
        /// Reset to start-state between each pulse (cleaner EOTF).
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        reset_between: bool,
        /// Output CSV log file.
        #[arg(long, default_value = "sweep.csv")]
        out: PathBuf,
    },

    /// Quantize, dither, and send an image to a JD79667 BWRY panel (Adafruit 6414).
    /// Output is the panel's 4-color palette: black, white, yellow, red.
    BwryImage {
        file: PathBuf,
        /// Rotate the source image clockwise before fitting (0/90/180/270).
        #[arg(long, default_value = "0")]
        rotate: u16,
        /// Dither algorithm against the 4-color palette.
        #[arg(long, default_value = "floyd")]
        dither: DitherKind,
    },
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum DitherKind {
    None,
    Floyd,
    Atkinson,
    Ordered,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Test => {
            let mut port = open_port(&cli.device)?;
            port.write_all(b"T")?;
            drain_status(&mut port)?;
        }
        Cmd::Raw { file } => {
            let bytes = std::fs::read(&file)?;
            if bytes.len() != FB_BYTES {
                return Err(anyhow!(
                    "raw file must be exactly {FB_BYTES} bytes, got {}",
                    bytes.len()
                ));
            }
            send_image(&cli.device, &bytes)?;
        }
        Cmd::Image {
            file,
            rotate,
            invert,
            dither,
            save_raw,
        } => {
            let img = load_image(&file)?;
            let img = apply_rotation(img, rotate)?;
            let gray = fit_and_grayscale(&img);
            let mut fb = vec![0u8; FB_BYTES];
            pack(&gray, dither, invert, &mut fb);
            if let Some(p) = save_raw {
                std::fs::write(&p, &fb).with_context(|| format!("writing {}", p.display()))?;
                eprintln!("wrote raw framebuffer to {}", p.display());
            }
            send_image(&cli.device, &fb)?;
        }
        Cmd::White => {
            let mut port = open_port(&cli.device)?;
            port.write_all(b"W")?;
            drain_status(&mut port)?;
        }
        Cmd::Black => {
            let mut port = open_port(&cli.device)?;
            port.write_all(b"K")?;
            drain_status(&mut port)?;
        }
        Cmd::Fill { byte } => {
            let b = parse_byte(&byte)?;
            let mut port = open_port(&cli.device)?;
            port.write_all(&[b'F', b])?;
            drain_status(&mut port)?;
        }
        Cmd::Pulse { voltage, frames } => {
            let v = parse_byte(&voltage)?;
            let mut port = open_port(&cli.device)?;
            port.write_all(&[b'P', v, frames])?;
            drain_status(&mut port)?;
        }
        Cmd::MultiPulse { phases } => {
            let parsed = parse_phases(&phases)?;
            let lut = build_multi_phase_lut(&parsed);
            send_lut(&cli.device, &lut)?;
        }
        Cmd::SweepMulti { template, vals, reset_between, out } => {
            run_sweep_multi(&cli.device, &template, &vals, reset_between, &out)?;
        }
        Cmd::Pyramid { levels, start, positions, out } => {
            run_pyramid(&cli.device, levels, &start, positions, &out)?;
        }
        Cmd::Render { file, rotate, grays, dither, shadow, planes } => {
            render_eotf(&cli.device, &file, rotate, grays, dither, shadow, planes)?;
        }
        Cmd::RenderFast { file, rotate, dither } => {
            render_fast(&cli.device, &file, rotate, dither)?;
        }
        Cmd::Gradient { bands, shadow, planes } => {
            render_gradient(&cli.device, bands, shadow, planes)?;
        }
        Cmd::GradientMeasure { bands, out } => {
            measure_gradient(&cli.device, bands, &out)?;
        }
        Cmd::AutoCharacterize { out, freshen, start, end } => {
            auto_characterize(&cli.device, &out, freshen, start, end)?;
        }
        Cmd::Sweep { start, voltage, frames, reset_between, out } => {
            run_sweep(&cli.device, &start, &voltage, &frames, reset_between, &out)?;
        }
        Cmd::BwryImage { file, rotate, dither } => {
            send_bwry_image(&cli.device, &file, rotate, dither)?;
        }
    }
    Ok(())
}

// ============================================================================
// JD79667 BWRY image upload.
// ============================================================================

// Source-image (user-facing landscape) geometry. Real visible panel area
// is 384×180 — Adafruit's 340 spec underreports by 44 px on the long axis.
const BWRY_W: usize = 384;
const BWRY_H: usize = 180;

// Chip RAM: 180-wide × 384-tall (per Adafruit's ThinkInk_352_Quadcolor_AJHE5
// wrapper passing 180×384 to the JD79667 constructor). Source maps directly
// to chip RAM: chip_col = y_src, chip_row = x_src. Buffer = 17,664 bytes.
const BWRY_CHIP_W: usize = 180;
const BWRY_CHIP_ROW_BYTES: usize = BWRY_CHIP_W / 4; // 45
const BWRY_FB_BYTES: usize = 17_664;

// Palette in sRGB — measured/typical BWRY ink colors.
// Each row is (R, G, B) and the index is the 2-bit code stored in the framebuffer.
const BWRY_PALETTE: [(u8, u8, u8); 4] = [
    (  0,   0,   0), // 0b00 = black
    (255, 255, 255), // 0b01 = white
    (235, 195,  35), // 0b10 = yellow
    (200,  30,  30), // 0b11 = red
];

fn nearest_palette_idx(r: f32, g: f32, b: f32) -> u8 {
    let mut best_i = 0u8;
    let mut best_d = f32::INFINITY;
    for (i, &(pr, pg, pb)) in BWRY_PALETTE.iter().enumerate() {
        let dr = r - pr as f32;
        let dg = g - pg as f32;
        let db = b - pb as f32;
        let d = dr * dr + dg * dg + db * db;
        if d < best_d {
            best_d = d;
            best_i = i as u8;
        }
    }
    best_i
}

fn send_bwry_image(device: &str, file: &std::path::Path, rotate: u16, dither: DitherKind) -> Result<()> {
    let img = load_image(file)?;
    let img = apply_rotation(img, rotate)?;
    let rgb = fit_landscape(&img, BWRY_W, BWRY_H);

    // Work in f32 RGB so we can carry dither error.
    let mut buf: Vec<(f32, f32, f32)> = rgb.pixels
        .chunks_exact(3)
        .map(|p| (p[0] as f32, p[1] as f32, p[2] as f32))
        .collect();

    let mut codes = vec![0u8; BWRY_W * BWRY_H];

    match dither {
        DitherKind::None => {
            for (i, &(r, g, b)) in buf.iter().enumerate() {
                codes[i] = nearest_palette_idx(r, g, b);
            }
        }
        DitherKind::Floyd => {
            for y in 0..BWRY_H {
                for x in 0..BWRY_W {
                    let idx = y * BWRY_W + x;
                    let (r, g, b) = buf[idx];
                    let code = nearest_palette_idx(r, g, b);
                    codes[idx] = code;
                    let (pr, pg, pb) = BWRY_PALETTE[code as usize];
                    let (er, eg, eb) = (r - pr as f32, g - pg as f32, b - pb as f32);
                    let mut spread = |x: i32, y: i32, w: f32| {
                        if x < 0 || x >= BWRY_W as i32 || y >= BWRY_H as i32 { return; }
                        let j = y as usize * BWRY_W + x as usize;
                        buf[j].0 += er * w;
                        buf[j].1 += eg * w;
                        buf[j].2 += eb * w;
                    };
                    spread(x as i32 + 1, y as i32,     7.0/16.0);
                    spread(x as i32 - 1, y as i32 + 1, 3.0/16.0);
                    spread(x as i32,     y as i32 + 1, 5.0/16.0);
                    spread(x as i32 + 1, y as i32 + 1, 1.0/16.0);
                }
            }
        }
        DitherKind::Atkinson => {
            for y in 0..BWRY_H {
                for x in 0..BWRY_W {
                    let idx = y * BWRY_W + x;
                    let (r, g, b) = buf[idx];
                    let code = nearest_palette_idx(r, g, b);
                    codes[idx] = code;
                    let (pr, pg, pb) = BWRY_PALETTE[code as usize];
                    let (er, eg, eb) = ((r - pr as f32)/8.0, (g - pg as f32)/8.0, (b - pb as f32)/8.0);
                    let mut spread = |x: i32, y: i32| {
                        if x < 0 || x >= BWRY_W as i32 || y >= BWRY_H as i32 { return; }
                        let j = y as usize * BWRY_W + x as usize;
                        buf[j].0 += er;
                        buf[j].1 += eg;
                        buf[j].2 += eb;
                    };
                    spread(x as i32 + 1, y as i32);
                    spread(x as i32 + 2, y as i32);
                    spread(x as i32 - 1, y as i32 + 1);
                    spread(x as i32,     y as i32 + 1);
                    spread(x as i32 + 1, y as i32 + 1);
                    spread(x as i32,     y as i32 + 2);
                }
            }
        }
        DitherKind::Ordered => {
            // 4x4 Bayer matrix scaled to ±32 sRGB-step bias.
            let bayer: [[f32; 4]; 4] = [
                [ -24.0,   8.0, -16.0,  16.0],
                [  24.0,  -8.0,  16.0, -16.0],
                [ -12.0,  20.0,  -4.0,  28.0],
                [  12.0, -20.0,   4.0, -28.0],
            ];
            for y in 0..BWRY_H {
                for x in 0..BWRY_W {
                    let idx = y * BWRY_W + x;
                    let (r, g, b) = buf[idx];
                    let bias = bayer[y % 4][x % 4];
                    codes[idx] = nearest_palette_idx(
                        (r + bias).clamp(0.0, 255.0),
                        (g + bias).clamp(0.0, 255.0),
                        (b + bias).clamp(0.0, 255.0),
                    );
                }
            }
        }
    }

    // Histogram for debugging.
    let mut hist = [0usize; 4];
    for &c in &codes { hist[c as usize] += 1; }
    let total = codes.len() as f32;
    eprintln!("Palette histogram:");
    eprintln!("  black : {:>6}  ({:.1}%)", hist[0], 100.0 * hist[0] as f32 / total);
    eprintln!("  white : {:>6}  ({:.1}%)", hist[1], 100.0 * hist[1] as f32 / total);
    eprintln!("  yellow: {:>6}  ({:.1}%)", hist[2], 100.0 * hist[2] as f32 / total);
    eprintln!("  red   : {:>6}  ({:.1}%)", hist[3], 100.0 * hist[3] as f32 / total);

    // Transpose into chip RAM layout: 180-wide × 384-tall, 45 bytes/row.
    // The panel's chip_row 0 maps to the physical right edge, so we mirror
    // along X: source (x_src, y_src) → chip (chip_col=y_src, chip_row=W-1-x_src).
    let mut fb = vec![0x55u8; BWRY_FB_BYTES];
    for y_src in 0..BWRY_H {
        for x_src in 0..BWRY_W {
            let code = codes[y_src * BWRY_W + x_src] & 0x3;
            let chip_row = BWRY_W - 1 - x_src;
            let chip_col = y_src;
            let byte_idx = chip_row * BWRY_CHIP_ROW_BYTES + chip_col / 4;
            let shift = (3 - (chip_col % 4)) * 2;
            let mask = !(0b11u8 << shift);
            fb[byte_idx] = (fb[byte_idx] & mask) | (code << shift);
        }
    }

    eprintln!("Sending {} bytes to panel...", fb.len());
    let mut port = open_panel(device, MODE_JD79667)?;
    port.write_all(&[b'I'])?;
    std::thread::sleep(Duration::from_millis(30));
    for chunk in fb.chunks(64) {
        port.write_all(chunk)?;
    }
    port.flush()?;
    drain_status(&mut port)?;
    Ok(())
}

/// Fit + center-crop a source RGB image into the given landscape dimensions.
/// Auto-rotates portrait sources 90° to land on the long axis.
fn fit_landscape(img: &RgbImage, dst_w: usize, dst_h: usize) -> RgbImage {
    let src_landscape = img.w > img.h;
    let (work_w, work_h, rotate_after) = if src_landscape {
        (dst_w, dst_h, false)
    } else {
        (dst_h, dst_w, true)
    };

    let src_aspect = img.w as f32 / img.h as f32;
    let dst_aspect = work_w as f32 / work_h as f32;
    let (resize_w, resize_h) = if src_aspect > dst_aspect {
        let h = work_h;
        let w = (h as f32 * src_aspect).round() as usize;
        (w, h)
    } else {
        let w = work_w;
        let h = (w as f32 / src_aspect).round() as usize;
        (w, h)
    };
    let resized = resize_bilinear(img, resize_w, resize_h);
    let crop_x = resize_w.saturating_sub(work_w) / 2;
    let crop_y = resize_h.saturating_sub(work_h) / 2;

    let mut out = vec![0u8; work_w * work_h * 3];
    for y in 0..work_h {
        for x in 0..work_w {
            let src = ((crop_y + y) * resize_w + (crop_x + x)) * 3;
            let dst = (y * work_w + x) * 3;
            out[dst..dst + 3].copy_from_slice(&resized.pixels[src..src + 3]);
        }
    }
    let cropped = RgbImage { w: work_w, h: work_h, pixels: out };

    if rotate_after {
        // Rotate 90° CW: new dims = work_h × work_w
        let mut rot = vec![0u8; work_w * work_h * 3];
        for y in 0..work_h {
            for x in 0..work_w {
                let src = (y * work_w + x) * 3;
                let nx = work_h - 1 - y;
                let ny = x;
                let dst = (ny * work_h + nx) * 3;
                rot[dst..dst + 3].copy_from_slice(&cropped.pixels[src..src + 3]);
            }
        }
        RgbImage { w: work_h, h: work_w, pixels: rot }
    } else {
        cropped
    }
}

const LUT_LEN: usize = 159;
const TAIL_DEFAULT: [u8; 6] = [0x22, 0x17, 0x41, 0x00, 0x32, 0x36];

fn parse_phases(s: &str) -> Result<Vec<(u8, u8)>> {
    let mut out = Vec::new();
    for chunk in s.split(',') {
        let parts: Vec<&str> = chunk.split(':').collect();
        if parts.len() != 2 {
            return Err(anyhow!("bad phase '{chunk}', expected 'voltage:frames'"));
        }
        let v = parse_byte(parts[0])?;
        let f: u8 = parts[1].trim().parse().context("bad frame count")?;
        out.push((v, f));
    }
    if out.is_empty() || out.len() > 12 {
        return Err(anyhow!("phase count must be 1..12, got {}", out.len()));
    }
    Ok(out)
}

/// Build a 159-byte SSD1680 LUT applying the given phase sequence to all
/// four transition types (BB/BW/WB/WW). VCOM stays at 0.
///
/// Layout (verified against Waveshare WS_20_30_2IN13_V3):
///   bytes 0..11   : BB phase voltages (12 phases)
///   bytes 12..23  : BW phase voltages
///   bytes 24..35  : WB phase voltages
///   bytes 36..47  : WW phase voltages
///   bytes 48..59  : VCOM phase voltages
///   bytes 60..143 : 12 phases × 7 timing bytes each (TPa, TPb, TPc, TPd, ..., reps?)
///   bytes 144..152: 9 bytes repeat counts (one per phase, only 9 used)
///   bytes 153..158: voltage/timing tail (EOPT, VGH, VSH1, VSH2, VSL, VCOM)
fn build_multi_phase_lut(phases: &[(u8, u8)]) -> [u8; LUT_LEN] {
    let mut out = [0u8; LUT_LEN];
    for (i, &(voltage, frames)) in phases.iter().enumerate() {
        // Voltage byte at byte i within each transition group.
        out[i] = voltage;       // BB phase i
        out[12 + i] = voltage;  // BW phase i
        out[24 + i] = voltage;  // WB phase i
        out[36 + i] = voltage;  // WW phase i
        // VCOM (bytes 48..59) stays 0.

        // Phase i timing: TPa at byte 60 + 7*i
        out[60 + i * 7] = frames;
        // TPb..TPg stay 0.

        // Phase i repeat count at byte 144 + i (only valid for phases 0..8).
        if i < 9 {
            out[144 + i] = 1;
        }
    }
    out[153..159].copy_from_slice(&TAIL_DEFAULT);
    out
}

fn send_lut(device: &str, lut: &[u8; LUT_LEN]) -> Result<()> {
    let mut port = open_port(device)?;
    port.write_all(&[b'L'])?;
    std::thread::sleep(Duration::from_millis(50));
    for chunk in lut.chunks(64) {
        port.write_all(chunk)?;
    }
    port.flush()?;
    drain_status(&mut port)?;
    Ok(())
}

/// Generate pyramid (away, back) pairs thru level `max_level` inclusive.
/// Level N has N pairs: (1, N-1), (2, N-2), ..., (N, 0).
fn pyramid_pairs(max_level: u8) -> Vec<(u8, u8)> {
    let mut out = Vec::new();
    for n in 1..=max_level {
        for away in 1..=n {
            out.push((away, n - away));
        }
    }
    out
}

// ============================================================================
// EOTF-calibrated multi-pass rendering.
//
// Per-pass logic:
//   - Pixels at this pass's target gray level: BW=0 (WB transition, drive)
//   - Pixels already darkened (from prior pass): BW=0 (chip sees as B,
//     uses BB transition which is configured 0V → preserved)
//   - All other pixels: BW=1 (WW transition, 0V → stay white)
//
// LUT per pass:
//   - WB group, phase 0: target-level recipe voltage; phase 0 frames: recipe N
//   - WB group, phase 1 (if recipe is 2-phase): back voltage; back frames
//   - BB, WW, BW groups all 0x00 (no drive)
//   - VCOM all 0x00
// ============================================================================

/// Calibrated recipe set for the Adafruit 6392 SSD1680 296×152 panel,
/// measured via auto_char.csv. L* values are normalized to panel-white = 100.
/// Sorted lightest → darkest. Note this panel has less contrast than the 6383
/// (deepest L* = 30.9 vs 23.1) and highlight recipes saturate at white.
fn gray_recipes(n_levels: u8) -> Vec<(f32, Vec<(u8, u8)>)> {
    let all: &[(f32, &[(u8, u8)])] = &[
        (100.0, &[]),                             // white
        (100.0, &[(0x40, 32), (0x80, 24)]),       // sat+24 (saturates white)
        (100.0, &[(0x40, 32), (0x80, 16)]),       // sat+16 (saturates white)
        ( 99.9, &[(0x40, 32), (0x80, 12)]),       // sat+12
        ( 99.2, &[(0x40, 32), (0x80, 10)]),       // sat+10
        ( 98.0, &[(0x40, 32), (0x80,  8)]),       // sat+8
        ( 97.0, &[(0x40,  1), (0x80,  2)]),       // (1,2)
        ( 96.1, &[(0x40,  1), (0x80,  3)]),       // (1,3)
        ( 94.7, &[(0x40, 32), (0x80,  6)]),       // sat+6
        ( 90.7, &[(0x40, 32), (0x80,  5)]),       // sat+5
        ( 89.8, &[(0x40,  2), (0x80,  2)]),       // (2,2)
        ( 86.9, &[(0x40,  1), (0x80,  1)]),       // (1,1)
        ( 83.3, &[(0x40, 32), (0x80,  4)]),       // sat+4
        ( 70.8, &[(0x40, 32), (0x80,  3)]),       // sat+3
        ( 70.4, &[(0x40,  1)]),                   // VSL:1
        ( 66.0, &[(0x40,  2), (0x80,  1)]),       // (2,1)
        ( 55.8, &[(0x40,  3), (0x80,  1)]),       // (3,1)
        ( 55.2, &[(0x40, 32), (0x80,  2)]),       // sat+2
        ( 52.4, &[(0x40,  2)]),                   // VSL:2
        ( 45.5, &[(0x40,  3)]),                   // VSL:3
        ( 44.4, &[(0x40,  4)]),                   // VSL:4
        ( 42.3, &[(0x40,  5)]),                   // VSL:5
        ( 42.1, &[(0x40, 32), (0x80,  1)]),       // sat+1
        ( 40.9, &[(0x40,  6)]),                   // VSL:6
        ( 39.2, &[(0x40,  8)]),                   // VSL:8
        ( 38.8, &[(0x40, 10)]),                   // VSL:10
        ( 37.4, &[(0x40, 12)]),                   // VSL:12
        ( 36.0, &[(0x40, 16)]),                   // VSL:16
        ( 35.0, &[(0x40, 20)]),                   // VSL:20
        ( 34.1, &[(0x40, 24)]),                   // VSL:24
        ( 30.9, &[(0x40, 32)]),                   // VSL:32 — deepest
    ];

    let n = n_levels.max(2).min(all.len() as u8) as usize;
    let mut picked = Vec::new();
    for i in 0..n {
        let idx = (i * (all.len() - 1)) / (n - 1);
        let (l, phases) = all[idx];
        picked.push((l, phases.to_vec()));
    }
    picked
}

/// Shadow-optimised 10-level recipe set.
///
/// Highlights/mids: sat+pull family (same VSL:32 base — clean, no inter-level ringing).
/// Deep shadows: stable single-pulse VSL recipes (VSL:6/10/20 — long enough to be settled).
/// Level distribution biased toward the dark end where the eye resolves detail.
///
/// Pass order: all sat+pull first (shared VSL:32 base, no cross-family borders),
/// then singles (VSL:6/10/20). Within each group, lightest→darkest.
///
///   L*=100  white
///   --- sat+pull passes ---
///   L*= 88  sat+6
///   L*= 76  sat+4
///   L*= 64  sat+3
///   L*= 47  sat+2
///   L*= 34  sat+1
///   L*= 23  sat+0
///   --- single-pulse passes ---
///   L*= 39  VSL:6
///   L*= 32  VSL:10
///   L*= 27  VSL:20
fn shadow_recipes() -> Vec<(f32, Vec<(u8, u8)>)> {
    vec![
        // "White" rides the same push+pull plane as the rest of the sat family.
        // sat+24 reaches paper-white on the 6392 (clipped at L*=100).
        (100.0, vec![(0x40, 32), (0x80, 24)]),
        // sat+pull family — all share VSL:32 push base
        ( 94.7, vec![(0x40, 32), (0x80, 6)]),
        ( 83.3, vec![(0x40, 32), (0x80, 4)]),
        ( 70.8, vec![(0x40, 32), (0x80, 3)]),
        ( 55.2, vec![(0x40, 32), (0x80, 2)]),
        ( 42.1, vec![(0x40, 32), (0x80, 1)]),
        ( 30.9, vec![(0x40, 32)]),
        // single-pulse family — VSL:6 is now too close to sat+1, so dropped
        // in favor of more shadow-direction coverage.
        ( 38.8, vec![(0x40, 10)]),
        ( 36.0, vec![(0x40, 16)]),
        ( 35.0, vec![(0x40, 20)]),
    ]
}

/// Build the per-pass LUT for a given target recipe.
/// WB transition gets the recipe phases; BB, WW, BW, VCOM all stay at 0x00.
fn build_render_pass_lut(recipe_phases: &[(u8, u8)]) -> [u8; LUT_LEN] {
    let mut out = [0u8; LUT_LEN];
    for (i, &(voltage, frames)) in recipe_phases.iter().enumerate() {
        if i >= 12 { break; }
        // WB group is bytes 24..35 (one byte per phase)
        out[24 + i] = voltage;
        // Phase i timing: TPa at byte 60 + 7*i
        out[60 + i * 7] = frames;
        // Phase i repeat: bytes 144..152 (only phases 0..8)
        if i < 9 {
            out[144 + i] = 1;
        }
    }
    out[153..159].copy_from_slice(&TAIL_DEFAULT);
    out
}

/// Auto-characterize: i1Pro stays anywhere on panel. Per iteration we render
/// the WHOLE PANEL uniformly at one target level — but still run the full
/// multi-pass schedule (pass 1..N) so each level experiences the same number
/// of subsequent "preserve" passes (and their drift) it would in a real image.
///
/// On iteration k, every pixel quantizes to level k:
///   passes 1..(k-1): active mask empty → no pixels driven, panel stays white
///   pass k:          active mask all → all pixels driven via WB to recipe k
///   passes (k+1)..N: active mask empty for "new", but ALL pixels are now B
///                    in chip view → BB transition with 0V drive applied
///                    (real-world drift accumulates here)
///
/// After all passes the whole panel reads as level k with realistic drift.
fn auto_characterize(
    device: &str,
    out_path: &std::path::Path,
    freshen_frames: u8,
    start: usize,
    end: Option<usize>,
) -> Result<()> {
    let levels = gray_recipes(255);
    let n = levels.len();
    let end = end.unwrap_or(n - 1).min(n - 1);
    if start > end {
        return Err(anyhow!("--start ({start}) must be <= --end ({end})"));
    }
    let count = end - start + 1;

    eprintln!("Auto-characterize:");
    eprintln!("  Range: level {start}..={end} ({count} levels of {n} total)");
    eprintln!("  ~55s/level → ~{} min for this run", (count * 55) / 60);
    eprintln!("  Freshen-blacks: {freshen_frames} VSL frames per iteration");
    eprintln!("  Output: {} (append mode)\n", out_path.display());

    // Open in append mode; write header only if file is new.
    let need_header = !out_path.exists();
    let mut log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(out_path)?;
    if need_header {
        writeln!(
            log,
            "session_white_Y,iter,target_level,target_L,recipe,Y_raw,L_raw,Y_fresh,L_fresh,L_raw_norm,L_fresh_norm"
        )?;
    }

    eprintln!("Place i1Pro anywhere on panel. Press enter (tty) to start.");
    wait_enter_tty();

    // Always render+measure white FIRST to establish this session's whitepoint
    // reference, regardless of the --start range. Log as iter=0 with marker.
    eprintln!("\n=== SESSION WHITE REFERENCE ===");
    let white_level_idx = vec![0u8; PANEL_W as usize * PANEL_H as usize];
    multi_pass_render(device, &levels, &white_level_idx)?;
    std::thread::sleep(Duration::from_millis(500));
    let white_ref = take_measurement()?;
    let panel_white_y = Some(white_ref.y);
    eprintln!(
        "  panel_white Y = {:.3} (raw L* = {:.3}) — used as norm reference for this session",
        white_ref.y, white_ref.a
    );

    for level_id in start..=end {
        let (target_l, target_phases) = &levels[level_id];
        eprintln!(
            "\n=== ITER {}/{} : level {} (predicted L*={:.1}, recipe={:?}) ===",
            level_id + 1, n, level_id, target_l, target_phases
        );

        // For level 0 we already measured it as the white reference — reuse.
        let raw = if level_id == 0 {
            white_ref
        } else {
            let level_idx = vec![level_id as u8; PANEL_W as usize * PANEL_H as usize];
            multi_pass_render(device, &levels, &level_idx)?;
            std::thread::sleep(Duration::from_millis(500));
            take_measurement()?
        };

        let level_idx = vec![level_id as u8; PANEL_W as usize * PANEL_H as usize];
        let fresh = if freshen_frames > 0 {
            apply_freshen_pass(device, &level_idx, freshen_frames)?;
            std::thread::sleep(Duration::from_millis(500));
            Some(take_measurement()?)
        } else {
            None
        };

        let l_raw_norm = normalize_to_white(raw.y, panel_white_y);
        let y_fresh = fresh.as_ref().map(|m| m.y).unwrap_or(f32::NAN);
        let l_fresh = fresh.as_ref().map(|m| m.a).unwrap_or(f32::NAN);
        let l_fresh_norm = fresh
            .as_ref()
            .map(|m| normalize_to_white(m.y, panel_white_y))
            .unwrap_or(f32::NAN);

        let recipe_str = format!("{:?}", target_phases);
        writeln!(
            log,
            "{:.3},{},{},{:.2},\"{}\",{:.3},{:.3},{:.3},{:.3},{:.3},{:.3}",
            white_ref.y,
            level_id + 1, level_id, target_l, recipe_str,
            raw.y, raw.a, y_fresh, l_fresh, l_raw_norm, l_fresh_norm
        )?;
        log.flush()?;
        eprintln!("  raw   Y={:.3} L*={:.3} (norm L*={:.3})", raw.y, raw.a, l_raw_norm);
        if let Some(f) = &fresh {
            eprintln!(
                "  fresh Y={:.3} L*={:.3} (norm L*={:.3})  ΔL={:+.3}",
                f.y, f.a, l_fresh_norm, f.a - raw.a
            );
        }
    }

    // End-of-session white recheck: render white, measure, compare against
    // the session's panel-white reference. Reports in-session panel drift
    // (temperature, charge buildup, VCOM creep, i1Pro cal-window drift).
    eprintln!("\n=== END-OF-SESSION WHITE RECHECK ===");
    let white_idx = vec![0u8; PANEL_W as usize * PANEL_H as usize];
    multi_pass_render(device, &levels, &white_idx)?;
    std::thread::sleep(Duration::from_millis(500));
    let white_end = take_measurement()?;
    let dy = white_end.y - white_ref.y;
    let dy_pct = 100.0 * dy / white_ref.y;
    let dl = white_end.a - white_ref.a;
    eprintln!(
        "  start white: Y={:.3} L*={:.3}",
        white_ref.y, white_ref.a
    );
    eprintln!(
        "  end   white: Y={:.3} L*={:.3}",
        white_end.y, white_end.a
    );
    eprintln!(
        "  drift: ΔY={:+.3} ({:+.2}%)  ΔL*={:+.3}",
        dy, dy_pct, dl
    );
    if dy_pct.abs() > 2.0 {
        eprintln!("  ⚠ in-session drift exceeds 2% — consider shorter sessions");
    }

    eprintln!("\nAuto-characterization complete. Log: {}", out_path.display());
    Ok(())
}

/// Apply a BB-only re-darken pulse after a uniform-level render. Pixels
/// currently dark (chip-state B) get a `frames`-long VSL nudge to compensate
/// for any preserve-drift lightening. Whites are untouched (transition fires
/// at 0V).
fn apply_freshen_pass(device: &str, level_idx: &[u8], frames: u8) -> Result<()> {
    // Prev plane = what was just rendered. Pixels at non-white level = bit 0.
    let mut prev_fb = vec![0xFFu8; FB_BYTES];
    for y in 0..PANEL_H as usize {
        for x in 0..PANEL_W as usize {
            let pl = level_idx[y * PANEL_W as usize + x];
            if pl > 0 {
                let byte_idx = y * ROW_BYTES + (x / 8);
                let bit = 7 - (x % 8);
                prev_fb[byte_idx] &= !(1 << bit);
            }
        }
    }
    // New plane = all 0 (every pixel target B).
    let new_fb = vec![0u8; FB_BYTES];

    // BB-only LUT.
    let mut lut = [0u8; LUT_LEN];
    lut[0] = 0x40;       // BB phase 0 voltage = VSL
    lut[60] = frames;    // BB phase 0 TPa
    lut[144] = 1;        // BB phase 0 repeat
    lut[153..159].copy_from_slice(&TAIL_DEFAULT);

    let mut port = open_port(device)?;
    port.write_all(&[b'A'])?;
    std::thread::sleep(Duration::from_millis(30));
    for chunk in prev_fb.chunks(64) { port.write_all(chunk)?; }
    for chunk in new_fb.chunks(64) { port.write_all(chunk)?; }
    for chunk in lut.chunks(64) { port.write_all(chunk)?; }
    port.flush()?;
    drain_status(&mut port)?;
    Ok(())
}

/// Convert a Y measurement to L* normalized against the panel-white reference.
/// Removes i1Pro cal whitepoint drift between recals.
fn normalize_to_white(y: f32, white_y: Option<f32>) -> f32 {
    let wy = match white_y {
        Some(w) if w > 0.0 => w,
        _ => return f32::NAN,
    };
    let yn = (y / wy * 100.0).clamp(0.0, 100.0);
    if yn > 0.008856 * 100.0 {
        116.0 * (yn / 100.0).powf(1.0 / 3.0) - 16.0
    } else {
        903.3 * yn / 100.0
    }
}

fn measure_gradient(device: &str, n_bands: u8, out_path: &std::path::Path) -> Result<()> {
    let all_levels = gray_recipes(16);
    let n = (n_bands as usize).max(2).min(all_levels.len());
    let levels: Vec<(f32, Vec<(u8, u8)>)> = (0..n)
        .map(|i| {
            let idx = (i * (all_levels.len() - 1)) / (n - 1);
            all_levels[idx].clone()
        })
        .collect();

    // Render the gradient.
    let mut level_idx = vec![0u8; PANEL_W as usize * PANEL_H as usize];
    for y in 0..PANEL_H as usize {
        let band = (y * levels.len()) / PANEL_H as usize;
        let band = band.min(levels.len() - 1) as u8;
        for x in 0..PANEL_W as usize {
            level_idx[y * PANEL_W as usize + x] = band;
        }
    }
    multi_pass_render(device, &levels, &level_idx)?;

    // Now measure each band.
    eprintln!("\n=== MEASUREMENT PHASE ===");
    eprintln!("{} bands rendered. Place i1Pro centered on each band when prompted.", n);
    eprintln!("Each band occupies {} rows (~{:.1}mm).", PANEL_H as usize / n, (PANEL_H as f32 / n as f32) * 0.215);

    let mut log = std::fs::File::create(out_path)?;
    writeln!(log, "band,predicted_L,recipe,X,Y,Z,Lab1,Lab2,Lab3,measured_L")?;

    for (i, (l, phases)) in levels.iter().enumerate() {
        eprintln!("\n---- BAND {i}/{} (predicted L*={:.1}) ----", n - 1, l);
        eprintln!("Move i1Pro to band {i} and press enter (tty).");
        wait_enter_tty();
        let m = take_measurement()?;
        let measured_l = m.a; // L* is in the "a" field per our parser quirk
        let recipe_str = format!("{:?}", phases);
        writeln!(
            log,
            "{i},{l:.1},\"{recipe_str}\",{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{measured_l:.3}",
            m.x, m.y, m.z, m.l, m.a, m.b
        )?;
        log.flush()?;
        eprintln!("  Y={:.3} measured-L*={:.3} (delta from predicted: {:+.2})", m.y, measured_l, measured_l - l);
    }

    eprintln!("\nGradient measurement complete. Log: {}", out_path.display());
    Ok(())
}

/// Fast 4-level render in 2 refreshes using transition-bit-pair LUT.
///
/// Quantize image to 4 levels. Encode each pixel value as 2 bits:
///   bit_high → set initial state in refresh 1 (high=0 → W, high=1 → B)
///   bit_low  → set target in refresh 2 (low=0 → W in new plane, low=1 → B)
///
/// Refresh 2's chip-view transitions per pixel:
///   (high=0, low=0) → WW  → preserve white                                (val 00 → L*≈100)
///   (high=0, low=1) → WB  → drive white → intermediate (LUT.WB recipe)    (val 01 → L*≈70)
///   (high=1, low=0) → BW  → pull black → intermediate (LUT.BW recipe)     (val 10 → L*≈50)
///   (high=1, low=1) → BB  → preserve black                                (val 11 → L*≈25)
fn render_fast(
    device: &str,
    file: &std::path::Path,
    rotate: u16,
    dither: DitherKind,
) -> Result<()> {
    let img = load_image(file)?;
    let img = apply_rotation(img, rotate)?;
    let gray = fit_and_grayscale(&img);

    // 4 target levels — picked to span the panel range.
    let target_l: [f32; 4] = [100.0, 70.0, 50.0, 25.0];
    let level_idx = quantize_to_levels(&gray, &target_l, dither);

    let mut hist = [0usize; 4];
    for &i in &level_idx {
        hist[i as usize] += 1;
    }
    eprintln!("Fast-render histogram:");
    for (i, count) in hist.iter().enumerate() {
        let pct = 100.0 * *count as f32 / level_idx.len() as f32;
        eprintln!("  val {i} (L*≈{:.0}): {count} pixels ({pct:.1}%)", target_l[i]);
    }

    // Build the two BW planes.
    // initial_fb: high bit → 0 (white) if high=0, 1→0 if high=1 (we INVERT for BW
    // semantics: in BW plane, 1=white, 0=black. so high=1 (target black initial)
    // means bit=0 in initial_fb.
    let mut initial_fb = vec![0xFFu8; FB_BYTES];
    let mut final_fb = vec![0xFFu8; FB_BYTES];
    for y in 0..PANEL_H as usize {
        for x in 0..PANEL_W as usize {
            let v = level_idx[y * PANEL_W as usize + x];
            let high = (v >> 1) & 1;
            let low = v & 1;
            let byte_idx = y * ROW_BYTES + (x / 8);
            let bit = 7 - (x % 8);
            if high == 1 {
                initial_fb[byte_idx] &= !(1 << bit);
            }
            if low == 1 {
                final_fb[byte_idx] &= !(1 << bit);
            }
        }
    }

    let mut port = open_port(device)?;

    // Refresh 1: clean white, then 'I' to paint high-bit (sets initial state).
    eprintln!("\n[1/2] reset white + paint initial state (high bit)");
    port.write_all(b"W")?;
    drain_status(&mut port)?;
    port.write_all(b"I")?;
    std::thread::sleep(Duration::from_millis(50));
    for chunk in initial_fb.chunks(64) {
        port.write_all(chunk)?;
    }
    port.flush()?;
    drain_status(&mut port)?;

    // Refresh 2: send final state with 4-transition LUT.
    eprintln!("\n[2/2] 4-transition refresh — final image in one shot");
    let lut = build_4_transition_lut(
        &[],                       // BB: 0V (preserve black) — val 11
        &[(0x40, 32), (0x80, 4)],  // BW: pull black → lighter — val 10
        &[(0x40, 4)],              // WB: drive white → darker — val 01
        &[],                       // WW: 0V (preserve white) — val 00
    );

    port.write_all(&[b'A'])?;
    std::thread::sleep(Duration::from_millis(30));
    // prev_fb = initial state (what's on panel now)
    for chunk in initial_fb.chunks(64) {
        port.write_all(chunk)?;
    }
    // new_fb = final state
    for chunk in final_fb.chunks(64) {
        port.write_all(chunk)?;
    }
    for chunk in lut.chunks(64) {
        port.write_all(chunk)?;
    }
    port.flush()?;
    drain_status(&mut port)?;

    eprintln!("\nFast render complete (2 refreshes total).");
    Ok(())
}

/// Build a LUT where each of the 4 transition groups gets its own phase
/// sequence. Lengths up to 12 phases each.
fn build_4_transition_lut(
    bb: &[(u8, u8)],
    bw: &[(u8, u8)],
    wb: &[(u8, u8)],
    ww: &[(u8, u8)],
) -> [u8; LUT_LEN] {
    let mut out = [0u8; LUT_LEN];
    // Group voltage byte offsets: BB=0, BW=12, WB=24, WW=36, VCOM=48
    for (group_offset, phases) in [(0, bb), (12, bw), (24, wb), (36, ww)] {
        for (i, &(voltage, _)) in phases.iter().enumerate() {
            if i >= 12 { break; }
            out[group_offset + i] = voltage;
        }
    }
    // Phase timings are global (one per phase index, not per group). Use the
    // longest sequence's frame counts. Different groups using the same phase
    // index share the same timing.
    let longest = [bb, bw, wb, ww].iter().map(|p| p.len()).max().unwrap_or(0);
    for i in 0..longest {
        // Pick the max frames at phase i across all 4 groups.
        let frames = [bb, bw, wb, ww]
            .iter()
            .map(|p| p.get(i).map(|&(_, f)| f).unwrap_or(0))
            .max()
            .unwrap_or(0);
        out[60 + i * 7] = frames;
        if i < 9 {
            out[144 + i] = 1;
        }
    }
    out[153..159].copy_from_slice(&TAIL_DEFAULT);
    out
}

fn render_gradient(device: &str, n_bands: Option<u8>, shadow: bool, planes: bool) -> Result<()> {
    let all_levels = if shadow { shadow_recipes() } else { gray_recipes(255) };
    let n = n_bands
        .map(|n| n.max(2).min(all_levels.len() as u8) as usize)
        .unwrap_or(all_levels.len());
    let levels: Vec<(f32, Vec<(u8, u8)>)> = (0..n)
        .map(|i| {
            let idx = if n > 1 { (i * (all_levels.len() - 1)) / (n - 1) } else { 0 };
            all_levels[idx].clone()
        })
        .collect();

    eprintln!("Gradient: {} bands{}", levels.len(), if planes { " [planes mode]" } else { "" });
    for (l, phases) in &levels {
        eprintln!("  L*={l:.1}  phases={phases:?}");
    }

    // Assign each pixel to a band based on its Y coordinate.
    // Panel is PANEL_W wide × PANEL_H tall (portrait); use Y axis for bands.
    let mut level_idx = vec![0u8; PANEL_W as usize * PANEL_H as usize];
    for y in 0..PANEL_H as usize {
        let band = (y * levels.len()) / PANEL_H as usize;
        let band = band.min(levels.len() - 1) as u8;
        for x in 0..PANEL_W as usize {
            level_idx[y * PANEL_W as usize + x] = band;
        }
    }

    if planes {
        multi_pass_render_planes(device, &levels, &level_idx)
    } else {
        multi_pass_render(device, &levels, &level_idx)
    }
}

fn send_a_frame(
    port: &mut Box<dyn serialport::SerialPort>,
    prev: &[u8],
    new: &[u8],
    lut: &[u8; LUT_LEN],
) -> Result<()> {
    port.write_all(&[b'A'])?;
    std::thread::sleep(Duration::from_millis(30));
    for c in prev.chunks(64) { port.write_all(c)?; }
    for c in new.chunks(64) { port.write_all(c)?; }
    for c in lut.chunks(64) { port.write_all(c)?; }
    port.flush()?;
    drain_status(port)
}

fn set_pixel_black(fb: &mut [u8], y: usize, x: usize) {
    fb[y * ROW_BYTES + x / 8] &= !(1u8 << (7 - x % 8));
}

/// Grouped-plane rendering for shadow_recipes():
///
/// Phase 1 — one shared VSL:32 push for ALL sat+pull pixels simultaneously.
/// Phase 2 — cumulative VSH1 back-pull passes; pixels graduate out as they
///            reach their target back-frame count (prev=B,new=W → BW=0V holds them).
/// Phase 3 — individual single-pulse passes for VSL:N levels as normal.
///
/// Net effect: all sat+pull pixels experience identical push history, removing
/// the inter-level fringing caused by neighbouring pixels being pushed at
/// different times in the naive pass ordering.
fn multi_pass_render_planes(
    device: &str,
    levels: &[(f32, Vec<(u8, u8)>)],
    level_idx: &[u8],
) -> Result<()> {
    let is_sat = |phases: &[(u8, u8)]| -> bool {
        phases.first() == Some(&(0x40, 32))
    };
    let back_of = |phases: &[(u8, u8)]| -> u8 {
        phases.get(1).map(|&(_, f)| f).unwrap_or(0)
    };

    let mut port = open_port(device)?;
    eprintln!("\n[reset] driving panel to clean white...");
    port.write_all(b"W")?;
    drain_status(&mut port)?;

    let mut prev_fb = vec![0xFFu8; FB_BYTES];
    let mut new_fb = vec![0xFFu8; FB_BYTES];

    // ── PHASE 1: SHARED SAT PUSH ──────────────────────────────────────
    // ALL sat-family pixels (including white-target now) go to black here.
    // Eliminates the edge between pushed and unpushed regions.
    eprintln!("\n[plane: sat push] VSL:32 → all sat-family pixels (incl. white)");
    new_fb.fill(0xFF);
    for y in 0..PANEL_H as usize {
        for x in 0..PANEL_W as usize {
            let pl = level_idx[y * PANEL_W as usize + x] as usize;
            if is_sat(&levels[pl].1) {
                set_pixel_black(&mut new_fb, y, x);
            }
        }
    }
    let mut lut = [0u8; LUT_LEN];
    lut[24] = 0x40; lut[60] = 32; lut[144] = 1;  // WB = VSL:32
    lut[153..159].copy_from_slice(&TAIL_DEFAULT);
    send_a_frame(&mut port, &prev_fb, &new_fb, &lut)?;
    std::mem::swap(&mut prev_fb, &mut new_fb);

    // ── PHASE 2: CUMULATIVE BACK-PULLS ───────────────────────────────
    // Collect unique non-zero back values, sorted ascending.
    let back_steps: Vec<u8> = {
        let mut v: Vec<u8> = levels.iter()
            .filter(|(_, p)| is_sat(p) && back_of(p) > 0)
            .map(|(_, p)| back_of(p))
            .collect();
        v.sort(); v.dedup(); v
    };

    let mut cumulative = 0u8;
    for &target in &back_steps {
        let step = target - cumulative;
        cumulative = target;
        eprintln!("\n[plane: back-pull] VSH1:{step} (cumulative {cumulative})");

        // Pixels still needing more back-pull (target > cumulative) stay B.
        // Pixels at or past their target flip to W (BW=0V preserves them).
        new_fb.fill(0xFF);
        for y in 0..PANEL_H as usize {
            for x in 0..PANEL_W as usize {
                let pl = level_idx[y * PANEL_W as usize + x] as usize;
                if is_sat(&levels[pl].1) && back_of(&levels[pl].1) >= cumulative {
                    set_pixel_black(&mut new_fb, y, x);
                }
            }
        }

        let mut lut = [0u8; LUT_LEN];
        lut[0] = 0x80; lut[60] = step; lut[144] = 1;  // BB = VSH1:step
        lut[153..159].copy_from_slice(&TAIL_DEFAULT);
        send_a_frame(&mut port, &prev_fb, &new_fb, &lut)?;
        std::mem::swap(&mut prev_fb, &mut new_fb);
    }

    // After all back-pulls every sat+pull pixel is chip-state W (graduated).
    // Force-reset prev_fb so single passes start from a clean white slate.
    prev_fb.fill(0xFF);

    // ── PHASE 3: SINGLE PASSES ───────────────────────────────────────
    let single_indices: Vec<usize> = levels.iter()
        .enumerate()
        .skip(1)
        .filter(|(_, (_, p))| !is_sat(p))
        .map(|(i, _)| i)
        .collect();

    for (pass_num, &si) in single_indices.iter().enumerate() {
        let (l, phases) = &levels[si];
        eprintln!("\n[plane: single {}/{}] L*={l:.1}", pass_num + 1, single_indices.len());

        new_fb.fill(0xFF);
        for y in 0..PANEL_H as usize {
            for x in 0..PANEL_W as usize {
                let pl = level_idx[y * PANEL_W as usize + x] as usize;
                let active = pl == si;
                let already_done = single_indices[..pass_num].contains(&pl);
                if active || already_done {
                    set_pixel_black(&mut new_fb, y, x);
                }
            }
        }

        let lut = build_render_pass_lut(phases);
        send_a_frame(&mut port, &prev_fb, &new_fb, &lut)?;
        std::mem::swap(&mut prev_fb, &mut new_fb);
    }

    eprintln!("\nRender complete.");
    Ok(())
}

fn multi_pass_render(
    device: &str,
    levels: &[(f32, Vec<(u8, u8)>)],
    level_idx: &[u8],
) -> Result<()> {
    let mut port = open_port(device)?;

    // Initial reset to clean white.
    eprintln!("\n[reset] driving panel to clean white...");
    port.write_all(b"W")?;
    drain_status(&mut port)?;

    let mut prev_fb = vec![0xFFu8; FB_BYTES];
    let mut new_fb = vec![0u8; FB_BYTES];
    for (pass_idx, (l, phases)) in levels.iter().enumerate().skip(1) {
        eprintln!("\n[pass {}/{}] target L*={l:.1}", pass_idx, levels.len() - 1);

        new_fb.fill(0xFF);
        for y in 0..PANEL_H as usize {
            for x in 0..PANEL_W as usize {
                let pl = level_idx[y * PANEL_W as usize + x];
                let active_now = pl == pass_idx as u8;
                let already_set = pl > 0 && pl < pass_idx as u8;
                if active_now || already_set {
                    let byte_idx = y * ROW_BYTES + (x / 8);
                    let bit = 7 - (x % 8);
                    new_fb[byte_idx] &= !(1 << bit);
                }
            }
        }

        let lut = build_render_pass_lut(phases);
        port.write_all(&[b'A'])?;
        std::thread::sleep(Duration::from_millis(30));
        for chunk in prev_fb.chunks(64) {
            port.write_all(chunk)?;
        }
        for chunk in new_fb.chunks(64) {
            port.write_all(chunk)?;
        }
        for chunk in lut.chunks(64) {
            port.write_all(chunk)?;
        }
        port.flush()?;
        drain_status(&mut port)?;

        std::mem::swap(&mut prev_fb, &mut new_fb);
    }

    eprintln!("\nRender complete.");
    Ok(())
}

fn render_eotf(
    device: &str,
    file: &std::path::Path,
    rotate: u16,
    grays: u8,
    dither: DitherKind,
    shadow: bool,
    planes: bool,
) -> Result<()> {
    let img = load_image(file)?;
    let img = apply_rotation(img, rotate)?;
    let gray = fit_and_grayscale(&img);

    let levels = if shadow {
        eprintln!("Using shadow-optimised 10-level recipe set.");
        shadow_recipes()
    } else {
        gray_recipes(grays)
    };
    eprintln!("Rendering with {} levels:", levels.len());
    for (l, phases) in &levels {
        eprintln!("  L*={l:.1}  phases={phases:?}");
    }

    // Quantize each pixel to nearest achievable L* (with optional dithering).
    // We need to convert sRGB-ish luma (0..255) to L*-equivalent, then dither
    // against the achievable L* set, then back to a level index.
    let l_targets: Vec<f32> = levels.iter().map(|(l, _)| *l).collect();
    let level_idx = quantize_to_levels(&gray, &l_targets, dither);

    // Histogram for debugging.
    let mut hist = vec![0usize; levels.len()];
    for &i in &level_idx {
        hist[i as usize] += 1;
    }
    eprintln!("Level histogram:");
    for (i, count) in hist.iter().enumerate() {
        let pct = 100.0 * *count as f32 / level_idx.len() as f32;
        eprintln!("  level {i} (L*={:.1}): {count} pixels ({pct:.1}%)", l_targets[i]);
    }

    if planes {
        multi_pass_render_planes(device, &levels, &level_idx)
    } else {
        multi_pass_render(device, &levels, &level_idx)
    }
}

/// Quantize a grayscale image to indices into `targets` (L* values).
/// Returns one u8 index per pixel (PANEL_W × PANEL_H pixels).
///
/// Image L* values (range 0..100) are linearly remapped onto the panel's
/// achievable L* range (panel_min..panel_max) before quantization. Otherwise
/// everything below panel_min clips to deepest-black and loses shadow detail.
fn quantize_to_levels(
    gray: &GrayImage,
    targets: &[f32],
    dither: DitherKind,
) -> Vec<u8> {
    let w = gray.w;
    let h = gray.h;

    // Panel's L* range comes from the target list.
    let panel_min = targets.iter().cloned().fold(f32::INFINITY, f32::min);
    let panel_max = targets.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
    eprintln!(
        "Panel L* range: {:.1}..{:.1} (remapping image 0..100 → this)",
        panel_min, panel_max
    );

    // Convert sRGB luma 0..255 to L* (gamma 2.2), then remap to panel range.
    let remap = |l: f32| -> f32 {
        let t = (l / 100.0).clamp(0.0, 1.0);
        panel_min + t * (panel_max - panel_min)
    };

    let mut buf: Vec<f32> = gray.pixels.iter()
        .map(|&p| {
            let lin = (p as f32 / 255.0).powf(2.2);
            let y = lin * 100.0;
            let l = if y > 0.008856 * 100.0 {
                116.0 * (y / 100.0).powf(1.0 / 3.0) - 16.0
            } else {
                903.3 * y / 100.0
            };
            remap(l)
        })
        .collect();

    // Sorted index → L* lookup so we can find nearest.
    let mut sorted: Vec<(usize, f32)> = targets.iter().enumerate().map(|(i, &l)| (i, l)).collect();
    sorted.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());

    let nearest_idx = |l: f32| -> u8 {
        let mut best = 0u8;
        let mut best_d = f32::MAX;
        for (i, tl) in targets.iter().enumerate() {
            let d = (tl - l).abs();
            if d < best_d {
                best_d = d;
                best = i as u8;
            }
        }
        best
    };

    let mut out = vec![0u8; w * h];

    match dither {
        DitherKind::None | DitherKind::Ordered => {
            for (i, &v) in buf.iter().enumerate() {
                out[i] = nearest_idx(v);
            }
        }
        DitherKind::Floyd => {
            for y in 0..h {
                for x in 0..w {
                    let i = y * w + x;
                    let cur = buf[i];
                    let idx = nearest_idx(cur);
                    let quantized = targets[idx as usize];
                    out[i] = idx;
                    let err = cur - quantized;
                    let right = x + 1 < w;
                    let down = y + 1 < h;
                    if right {
                        buf[i + 1] += err * 7.0 / 16.0;
                    }
                    if down {
                        if x > 0 {
                            buf[i + w - 1] += err * 3.0 / 16.0;
                        }
                        buf[i + w] += err * 5.0 / 16.0;
                        if right {
                            buf[i + w + 1] += err * 1.0 / 16.0;
                        }
                    }
                }
            }
        }
        DitherKind::Atkinson => {
            for y in 0..h {
                for x in 0..w {
                    let i = y * w + x;
                    let cur = buf[i];
                    let idx = nearest_idx(cur);
                    let quantized = targets[idx as usize];
                    out[i] = idx;
                    let err = (cur - quantized) / 8.0;
                    let offsets: [(i32, i32); 6] =
                        [(1, 0), (2, 0), (-1, 1), (0, 1), (1, 1), (0, 2)];
                    for (dx, dy) in offsets {
                        let nx = x as i32 + dx;
                        let ny = y as i32 + dy;
                        if nx >= 0 && nx < w as i32 && ny < h as i32 {
                            buf[ny as usize * w + nx as usize] += err;
                        }
                    }
                }
            }
        }
    }

    out
}

fn run_pyramid(
    device: &str,
    levels: u8,
    start: &str,
    positions: u8,
    out_path: &std::path::Path,
) -> Result<()> {
    let (reset_cmd, away_volt, back_volt) = match start {
        "white" => (b'W', 0x40u8, 0x80u8),
        "black" => (b'K', 0x80u8, 0x40u8),
        _ => return Err(anyhow!("start must be 'white' or 'black'")),
    };

    let pairs = pyramid_pairs(levels);
    eprintln!(
        "Pyramid sweep: start={start} levels={levels} ({} measurements × {positions} positions)",
        pairs.len()
    );
    eprintln!("Output: {}", out_path.display());

    let mut log = std::fs::File::create(out_path)?;
    writeln!(
        log,
        "position,step,level,away,back,recipe,X,Y,Z,Lab1,Lab2,Lab3"
    )?;

    eprintln!("\n(i1Pro should be calibrated and on the panel)");

    let mut port = open_port(device)?;

    for pos in 1..=positions {
        if positions > 1 {
            eprintln!("\n========================================");
            eprintln!("==== MOVE i1Pro to POSITION {pos}/{positions} ====");
            eprintln!("========================================");
            eprintln!("Press enter when in position (no other prompts will appear).");
            wait_enter_tty();
        }

        eprintln!("\n[init pos={pos}] reset to {start}");
        port.write_all(&[reset_cmd])?;
        drain_status(&mut port)?;
        std::thread::sleep(Duration::from_millis(500));
        let baseline = take_measurement()?;
        writeln!(
            log,
            "{pos},baseline,0,0,0,(reset),{:.3},{:.3},{:.3},{:.3},{:.3},{:.3}",
            baseline.x, baseline.y, baseline.z, baseline.l, baseline.a, baseline.b
        )?;
        log.flush()?;
        eprintln!("Baseline: Y={:.3} (a={:.3} = L*)", baseline.y, baseline.a);

        for (i, &(away, back)) in pairs.iter().enumerate() {
            let level = away + back;
            eprintln!(
                "\n[pos {pos} | {}/{}] level {level}: away={away} back={back}",
                i + 1,
                pairs.len()
            );

            port.write_all(&[reset_cmd])?;
            drain_status(&mut port)?;
            std::thread::sleep(Duration::from_millis(300));

            let mut phases = Vec::new();
            phases.push((away_volt, away));
            if back > 0 {
                phases.push((back_volt, back));
            }
            let lut = build_multi_phase_lut(&phases);
            let recipe = format!("0x{:02x}:{},0x{:02x}:{}", away_volt, away, back_volt, back);

            port.write_all(&[b'L'])?;
            std::thread::sleep(Duration::from_millis(50));
            for chunk in lut.chunks(64) {
                port.write_all(chunk)?;
            }
            port.flush()?;
            drain_status(&mut port)?;
            std::thread::sleep(Duration::from_millis(300));

            let m = take_measurement()?;
            writeln!(
                log,
                "{pos},{i},{level},{away},{back},{recipe},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3}",
                m.x, m.y, m.z, m.l, m.a, m.b
            )?;
            log.flush()?;
            eprintln!("  Y={:.3} (a={:.3} = L*)", m.y, m.a);
        }
    }

    eprintln!("\nPyramid sweep complete. Log: {}", out_path.display());
    Ok(())
}

fn run_sweep_multi(
    device: &str,
    template: &str,
    vals_csv: &str,
    reset_between: bool,
    out_path: &std::path::Path,
) -> Result<()> {
    let vals: Vec<u8> = vals_csv
        .split(',')
        .map(|s| s.trim().parse::<u8>().context("bad val"))
        .collect::<Result<_>>()?;

    eprintln!("Sweep multi: template='{template}' vals={vals:?}");
    eprintln!("Output: {}", out_path.display());

    let mut log = std::fs::File::create(out_path)?;
    writeln!(log, "step,var,phases,X,Y,Z,Lab1,Lab2,Lab3")?;

    eprintln!("\nEnsure i1Pro is calibrated. Press enter to start.");
    wait_enter();

    let mut port = open_port(device)?;

    eprintln!("\n[init] reset to white");
    port.write_all(b"W")?;
    drain_status(&mut port)?;
    eprintln!("Press enter for baseline measurement.");
    wait_enter();
    let baseline = take_measurement()?;
    writeln!(
        log,
        "baseline,0,(reset),{:.3},{:.3},{:.3},{:.3},{:.3},{:.3}",
        baseline.x, baseline.y, baseline.z, baseline.l, baseline.a, baseline.b
    )?;
    log.flush()?;
    eprintln!("Baseline: Y={:.3} (parsed L={:.3} a={:.3})", baseline.y, baseline.l, baseline.a);

    for (i, &v) in vals.iter().enumerate() {
        if reset_between && i > 0 {
            eprintln!("\n[{}/{}] reset to white", i + 1, vals.len());
            port.write_all(b"W")?;
            drain_status(&mut port)?;
            eprintln!("Press enter when ready.");
            wait_enter();
        }
        let phase_str = template.replace("VAR", &v.to_string());
        let phases = parse_phases(&phase_str)?;
        let lut = build_multi_phase_lut(&phases);

        eprintln!(
            "\n[{}/{}] var={v} → phases={phase_str}",
            i + 1,
            vals.len()
        );
        // Send LUT + refresh via 'L' command directly on this port.
        port.write_all(&[b'L'])?;
        std::thread::sleep(Duration::from_millis(50));
        for chunk in lut.chunks(64) {
            port.write_all(chunk)?;
        }
        port.flush()?;
        drain_status(&mut port)?;
        eprintln!("Press enter to measure.");
        wait_enter();
        let m = take_measurement()?;
        writeln!(
            log,
            "{i},{v},{phase_str},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3}",
            m.x, m.y, m.z, m.l, m.a, m.b
        )?;
        log.flush()?;
        eprintln!("  Y={:.3} (a={:.3} = L*)", m.y, m.a);
    }

    eprintln!("\nSweep complete. Log: {}", out_path.display());
    Ok(())
}

fn parse_byte(s: &str) -> Result<u8> {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u8::from_str_radix(hex, 16).context("invalid hex byte")
    } else {
        s.parse::<u8>().context("invalid decimal byte")
    }
}

fn run_sweep(
    device: &str,
    start: &str,
    voltage_str: &str,
    frames_csv: &str,
    reset_between: bool,
    out_path: &std::path::Path,
) -> Result<()> {
    let voltage = parse_byte(voltage_str)?;
    let frame_list: Vec<u8> = frames_csv
        .split(',')
        .map(|s| s.trim().parse::<u8>().context("bad frame count"))
        .collect::<Result<_>>()?;

    let reset_cmd: u8 = match start {
        "white" => b'W',
        "black" => b'K',
        _ => return Err(anyhow!("start must be 'white' or 'black'")),
    };

    eprintln!(
        "Sweep: start={start} voltage={voltage_str} reset_between={reset_between} frames={frame_list:?}"
    );
    eprintln!("Output: {}", out_path.display());

    let mut log = std::fs::File::create(out_path)
        .with_context(|| format!("creating {}", out_path.display()))?;
    writeln!(log, "step,frames,voltage_hex,start,X,Y,Z,L,a,b")?;

    eprintln!("\nFirst: align i1Pro on panel and run spotread calibration manually:");
    eprintln!("   spotread -O");
    eprintln!("(do the calibration on its reference tile, then ctrl-C)");
    eprintln!("Press enter when calibration is done.");
    wait_enter();

    let mut port = open_port(device)?;

    // Initial reset.
    eprintln!("\n[init] reset to {start}");
    port.write_all(&[reset_cmd])?;
    drain_status(&mut port)?;

    eprintln!("Place i1Pro on panel surface. Press enter for baseline measurement.");
    wait_enter();
    let baseline = take_measurement()?;
    writeln!(
        log,
        "baseline,0,{voltage_str},{start},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3}",
        baseline.x, baseline.y, baseline.z, baseline.l, baseline.a, baseline.b
    )?;
    log.flush()?;
    eprintln!("Baseline: Y={:.3} L*={:.3}", baseline.y, baseline.l);

    for (i, &nframes) in frame_list.iter().enumerate() {
        if reset_between && i > 0 {
            eprintln!("\n[{}/{}] reset to {start}", i + 1, frame_list.len());
            port.write_all(&[reset_cmd])?;
            drain_status(&mut port)?;
            eprintln!("Lift i1Pro briefly during refresh, then place back. Press enter.");
            wait_enter();
        }
        eprintln!("\n[{}/{}] pulse {nframes} frames @ {voltage_str}", i + 1, frame_list.len());
        port.write_all(&[b'P', voltage, nframes])?;
        drain_status(&mut port)?;
        eprintln!("Press enter to measure.");
        wait_enter();
        let m = take_measurement()?;
        writeln!(
            log,
            "{i},{nframes},{voltage_str},{start},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3}",
            m.x, m.y, m.z, m.l, m.a, m.b
        )?;
        log.flush()?;
        eprintln!("  Y={:.3} L*={:.3}", m.y, m.l);
    }

    eprintln!("\nSweep complete. Log: {}", out_path.display());
    Ok(())
}

#[derive(Debug, Clone, Copy)]
struct Measurement {
    x: f32,
    y: f32,
    z: f32,
    l: f32,
    a: f32,
    b: f32,
}

fn take_measurement() -> Result<Measurement> {
    loop {
        let out = std::process::Command::new("spotread")
            .args(["-O", "-N"])
            .output()
            .context("running spotread")?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        let combined = format!("{stdout}{}", String::from_utf8_lossy(&out.stderr));
        if let Some(m) = parse_spotread(&combined) {
            return Ok(m);
        }
        // Detect the i1Pro's stale-calibration condition and let user recal.
        if combined.contains("needs a calibration")
            || combined.contains("Measurement misread")
        {
            eprintln!("\n!! i1Pro needs recalibration.");
            eprintln!("   In ANOTHER terminal: spotread -O");
            eprintln!("   (place on cal tile, hit enter, then ctrl-C out)");
            eprintln!("   Place i1Pro back on panel, press enter here to retry.");
            wait_enter_tty();
            continue;
        }
        return Err(anyhow!("could not parse spotread output:\n{combined}"));
    }
}

fn parse_spotread(s: &str) -> Option<Measurement> {
    // Looking for a line like:
    //   Result is XYZ: 89.36 92.84 95.12, D50 Lab: 97.20 -0.61 0.84
    for line in s.lines() {
        if let Some(rest) = line.find("XYZ:").map(|i| &line[i + 4..]) {
            let nums: Vec<f32> = rest
                .split(|c: char| !c.is_ascii_digit() && c != '.' && c != '-')
                .filter(|s| !s.is_empty())
                .filter_map(|s| s.parse().ok())
                .collect();
            if nums.len() >= 6 {
                return Some(Measurement {
                    x: nums[0],
                    y: nums[1],
                    z: nums[2],
                    l: nums[3],
                    a: nums[4],
                    b: nums[5],
                });
            }
        }
    }
    None
}

fn wait_enter() {
    use std::io::BufRead;
    let stdin = std::io::stdin();
    let _ = stdin.lock().lines().next();
}

/// Wait for enter from /dev/tty directly, bypassing any redirected stdin.
/// Use for prompts that require user physical action (e.g., repositioning).
fn wait_enter_tty() {
    let f = std::fs::OpenOptions::new().read(true).open("/dev/tty");
    let Ok(mut f) = f else {
        // Can't open /dev/tty (no terminal). Fall back to long sleep.
        eprintln!("(no tty available — sleeping 20s for repositioning)");
        std::thread::sleep(Duration::from_secs(20));
        return;
    };
    let mut buf = [0u8; 64];
    loop {
        match f.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if buf[..n].iter().any(|&b| b == b'\n') {
                    break;
                }
            }
            Err(_) => break,
        }
    }
}

// Unified-firmware panel select bytes.
const MODE_SSD1680: u8 = 0;
const MODE_JD79667: u8 = 1;

/// Open the serial port and select the SSD1680 driver in the unified firmware.
fn open_port(device: &str) -> Result<Box<dyn serialport::SerialPort>> {
    open_panel(device, MODE_SSD1680)
}

/// Open the serial port and send `M`+mode to pin the firmware to a panel.
/// First call after firmware reset is the one that sticks; subsequent calls
/// (after the driver loop has taken over) are silently ignored, which is fine.
fn open_panel(device: &str, mode: u8) -> Result<Box<dyn serialport::SerialPort>> {
    let mut port = serialport::new(device, 115_200)
        .timeout(Duration::from_millis(500))
        .open()
        .with_context(|| format!("opening {device}"))?;
    // Send panel-select handshake.
    port.write_all(&[b'M', mode])?;
    port.flush()?;
    // Drain until silent for 500ms (handles post-boot banner + test refresh).
    let mut sink = [0u8; 1024];
    let mut last_data = std::time::Instant::now();
    loop {
        match port.read(&mut sink) {
            Ok(n) if n > 0 => {
                last_data = std::time::Instant::now();
            }
            _ => {
                if last_data.elapsed() > Duration::from_millis(500) {
                    break;
                }
            }
        }
        if last_data.elapsed() > Duration::from_secs(15) {
            break; // safety bound
        }
    }
    port.set_timeout(Duration::from_secs(30)).ok();
    Ok(port)
}

fn drain_status(port: &mut Box<dyn serialport::SerialPort>) -> Result<()> {
    let deadline = std::time::Instant::now() + Duration::from_secs(8);
    let mut buf = [0u8; 256];
    while std::time::Instant::now() < deadline {
        match port.read(&mut buf) {
            Ok(0) => {}
            Ok(n) => {
                let s = String::from_utf8_lossy(&buf[..n]);
                eprint!("{s}");
                if s.contains("refreshed") || s.contains("done") {
                    break;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => break,
            Err(e) => return Err(e.into()),
        }
    }
    eprintln!();
    Ok(())
}

fn send_image(device: &str, fb: &[u8]) -> Result<()> {
    let mut port = open_port(device)?;
    port.write_all(b"I")?;
    std::thread::sleep(Duration::from_millis(50));
    for chunk in fb.chunks(64) {
        port.write_all(chunk)?;
    }
    port.flush()?;
    drain_status(&mut port)?;
    Ok(())
}

// ============================================================================
// Image loading — no `image` crate, just zune-jpeg + png directly.
// ============================================================================

struct RgbImage { w: usize, h: usize, pixels: Vec<u8> }  // packed RGB
struct GrayImage { w: usize, h: usize, pixels: Vec<u8> } // one byte per pixel

fn load_image(path: &std::path::Path) -> Result<RgbImage> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "jpg" | "jpeg" => decode_jpeg(&bytes),
        "png"          => decode_png(&bytes),
        _ => {
            // Try jpeg first, then png.
            decode_jpeg(&bytes).or_else(|_| decode_png(&bytes))
                .context("unknown image format")
        }
    }
}

fn decode_jpeg(bytes: &[u8]) -> Result<RgbImage> {
    let mut dec = jpeg_decoder::Decoder::new(std::io::Cursor::new(bytes));
    let pixels = dec.decode().context("jpeg decode")?;
    let info = dec.info().context("jpeg info")?;
    // Convert to RGB if needed (jpeg-decoder may return grayscale or CMYK).
    let rgb = match info.pixel_format {
        jpeg_decoder::PixelFormat::RGB24 => pixels,
        jpeg_decoder::PixelFormat::L8 => {
            pixels.iter().flat_map(|&g| [g, g, g]).collect()
        }
        jpeg_decoder::PixelFormat::CMYK32 => {
            pixels.chunks_exact(4).flat_map(|p| {
                let c = p[0] as f32 / 255.0;
                let m = p[1] as f32 / 255.0;
                let y = p[2] as f32 / 255.0;
                let k = p[3] as f32 / 255.0;
                let r = ((1.0 - c) * (1.0 - k) * 255.0) as u8;
                let g = ((1.0 - m) * (1.0 - k) * 255.0) as u8;
                let b = ((1.0 - y) * (1.0 - k) * 255.0) as u8;
                [r, g, b]
            }).collect()
        }
        _ => return Err(anyhow!("unsupported JPEG pixel format: {:?}", info.pixel_format)),
    };
    Ok(RgbImage { w: info.width as usize, h: info.height as usize, pixels: rgb })
}

fn decode_png(bytes: &[u8]) -> Result<RgbImage> {
    use png::Transformations;
    let mut dec = png::Decoder::new(std::io::Cursor::new(bytes));
    dec.set_transformations(Transformations::EXPAND | Transformations::STRIP_16);
    let mut reader = dec.read_info().context("png read_info")?;
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).context("png decode")?;
    let w = info.width as usize;
    let h = info.height as usize;
    // Convert to RGB, handling grayscale and RGBA.
    let rgb = match info.color_type {
        png::ColorType::Rgb => buf[..w*h*3].to_vec(),
        png::ColorType::Rgba => {
            let mut out = Vec::with_capacity(w*h*3);
            for px in buf[..w*h*4].chunks_exact(4) {
                out.extend_from_slice(&px[..3]);
            }
            out
        }
        png::ColorType::Grayscale => {
            let mut out = Vec::with_capacity(w*h*3);
            for &g in &buf[..w*h] {
                out.extend_from_slice(&[g, g, g]);
            }
            out
        }
        png::ColorType::GrayscaleAlpha => {
            let mut out = Vec::with_capacity(w*h*3);
            for px in buf[..w*h*2].chunks_exact(2) {
                out.extend_from_slice(&[px[0], px[0], px[0]]);
            }
            out
        }
        ct => return Err(anyhow!("unsupported PNG color type: {ct:?}")),
    };
    Ok(RgbImage { w, h, pixels: rgb })
}


fn apply_rotation(img: RgbImage, deg: u16) -> Result<RgbImage> {
    let RgbImage { w, h, pixels } = img;
    let mut out = vec![0u8; pixels.len()];
    match deg % 360 {
        0 => return Ok(RgbImage { w, h, pixels }),
        90 => {
            // (x,y) -> (h-1-y, x)  new dims: h×w
            for y in 0..h {
                for x in 0..w {
                    let src = (y * w + x) * 3;
                    let dx = h - 1 - y;
                    let dy = x;
                    let dst = (dy * h + dx) * 3;
                    out[dst..dst+3].copy_from_slice(&pixels[src..src+3]);
                }
            }
            return Ok(RgbImage { w: h, h: w, pixels: out });
        }
        180 => {
            for y in 0..h {
                for x in 0..w {
                    let src = (y * w + x) * 3;
                    let dst = ((h-1-y) * w + (w-1-x)) * 3;
                    out[dst..dst+3].copy_from_slice(&pixels[src..src+3]);
                }
            }
            return Ok(RgbImage { w, h, pixels: out });
        }
        270 => {
            // (x,y) -> (y, w-1-x)  new dims: h×w
            for y in 0..h {
                for x in 0..w {
                    let src = (y * w + x) * 3;
                    let dx = y;
                    let dy = w - 1 - x;
                    let dst = (dy * h + dx) * 3;
                    out[dst..dst+3].copy_from_slice(&pixels[src..src+3]);
                }
            }
            return Ok(RgbImage { w: h, h: w, pixels: out });
        }
        n => return Err(anyhow!("rotation must be a multiple of 90, got {n}")),
    }
}

/// Bilinear resize to (dst_w × dst_h).
fn resize_bilinear(img: &RgbImage, dst_w: usize, dst_h: usize) -> RgbImage {
    let mut out = vec![0u8; dst_w * dst_h * 3];
    let sx = img.w as f32 / dst_w as f32;
    let sy = img.h as f32 / dst_h as f32;
    for dy in 0..dst_h {
        for dx in 0..dst_w {
            let fx = (dx as f32 + 0.5) * sx - 0.5;
            let fy = (dy as f32 + 0.5) * sy - 0.5;
            let x0 = (fx as i32).clamp(0, img.w as i32 - 1) as usize;
            let y0 = (fy as i32).clamp(0, img.h as i32 - 1) as usize;
            let x1 = (x0 + 1).min(img.w - 1);
            let y1 = (y0 + 1).min(img.h - 1);
            let xf = (fx - fx.floor()).clamp(0.0, 1.0);
            let yf = (fy - fy.floor()).clamp(0.0, 1.0);
            let dst = (dy * dst_w + dx) * 3;
            for c in 0..3 {
                let p00 = img.pixels[(y0*img.w+x0)*3+c] as f32;
                let p10 = img.pixels[(y0*img.w+x1)*3+c] as f32;
                let p01 = img.pixels[(y1*img.w+x0)*3+c] as f32;
                let p11 = img.pixels[(y1*img.w+x1)*3+c] as f32;
                let v = p00*(1.0-xf)*(1.0-yf) + p10*xf*(1.0-yf)
                      + p01*(1.0-xf)*yf       + p11*xf*yf;
                out[dst+c] = v.round() as u8;
            }
        }
    }
    RgbImage { w: dst_w, h: dst_h, pixels: out }
}

/// Fit-and-center-crop the image to the panel's native 122 × 250 portrait.
/// If the source is landscape, we rotate the final crop 90° so the image
/// fills the panel with the long edge running along the panel's long edge.
fn fit_and_grayscale(img: &RgbImage) -> GrayImage {
    let src_landscape = img.w > img.h;
    let (dst_w, dst_h, rotate_after) = if src_landscape {
        (PANEL_H as usize, PANEL_W as usize, true)
    } else {
        (PANEL_W as usize, PANEL_H as usize, false)
    };

    let src_aspect = img.w as f32 / img.h as f32;
    let dst_aspect = dst_w as f32 / dst_h as f32;
    let (resize_w, resize_h) = if src_aspect > dst_aspect {
        let h = dst_h;
        let w = (h as f32 * src_aspect).round() as usize;
        (w, h)
    } else {
        let w = dst_w;
        let h = (w as f32 / src_aspect).round() as usize;
        (w, h)
    };

    let resized = resize_bilinear(img, resize_w, resize_h);
    let crop_x = (resize_w.saturating_sub(dst_w)) / 2;
    let crop_y = (resize_h.saturating_sub(dst_h)) / 2;

    // Crop + convert to gray in one pass.
    let mut gray_pixels = vec![0u8; dst_w * dst_h];
    for y in 0..dst_h {
        for x in 0..dst_w {
            let src = ((crop_y + y) * resize_w + (crop_x + x)) * 3;
            let r = resized.pixels[src] as f32;
            let g = resized.pixels[src+1] as f32;
            let b = resized.pixels[src+2] as f32;
            gray_pixels[y * dst_w + x] = (0.2126*r + 0.7152*g + 0.0722*b).round() as u8;
        }
    }
    let cropped = GrayImage { w: dst_w, h: dst_h, pixels: gray_pixels };

    if rotate_after {
        // Rotate gray 90°: (x,y) -> (h-1-y, x), new dims h×w
        let mut rot = vec![0u8; dst_w * dst_h];
        for y in 0..dst_h {
            for x in 0..dst_w {
                rot[x * dst_h + (dst_h - 1 - y)] = cropped.pixels[y * dst_w + x];
            }
        }
        GrayImage { w: dst_h, h: dst_w, pixels: rot }
    } else {
        cropped
    }
}

fn pack(gray: &GrayImage, dither: DitherKind, invert: bool, fb: &mut [u8]) {
    debug_assert_eq!(gray.w, PANEL_W as usize);
    debug_assert_eq!(gray.h, PANEL_H as usize);

    let mut buf: Vec<f32> = gray.pixels.iter().map(|&p| p as f32).collect();
    let stride = PANEL_W as usize;

    match dither {
        DitherKind::None => {}
        DitherKind::Ordered => {
            const BAYER: [[i32; 4]; 4] = [
                [0, 8, 2, 10],
                [12, 4, 14, 6],
                [3, 11, 1, 9],
                [15, 7, 13, 5],
            ];
            for y in 0..PANEL_H as usize {
                for x in 0..PANEL_W as usize {
                    let threshold = (BAYER[y % 4][x % 4] as f32) * 16.0 - 120.0;
                    buf[y * stride + x] += threshold;
                }
            }
        }
        DitherKind::Floyd => {
            for y in 0..PANEL_H as usize {
                for x in 0..PANEL_W as usize {
                    let old = buf[y * stride + x];
                    let new = if old >= 128.0 { 255.0 } else { 0.0 };
                    let err = old - new;
                    buf[y * stride + x] = new;
                    let right = x + 1 < stride;
                    let down = y + 1 < PANEL_H as usize;
                    if right {
                        buf[y * stride + x + 1] += err * 7.0 / 16.0;
                    }
                    if down {
                        if x > 0 {
                            buf[(y + 1) * stride + x - 1] += err * 3.0 / 16.0;
                        }
                        buf[(y + 1) * stride + x] += err * 5.0 / 16.0;
                        if right {
                            buf[(y + 1) * stride + x + 1] += err * 1.0 / 16.0;
                        }
                    }
                }
            }
        }
        DitherKind::Atkinson => {
            for y in 0..PANEL_H as usize {
                for x in 0..PANEL_W as usize {
                    let old = buf[y * stride + x];
                    let new = if old >= 128.0 { 255.0 } else { 0.0 };
                    let err = (old - new) / 8.0;
                    buf[y * stride + x] = new;
                    let off: [(i32, i32); 6] =
                        [(1, 0), (2, 0), (-1, 1), (0, 1), (1, 1), (0, 2)];
                    for (dx, dy) in off {
                        let nx = x as i32 + dx;
                        let ny = y as i32 + dy;
                        if nx >= 0 && nx < stride as i32 && ny < PANEL_H as i32 {
                            buf[ny as usize * stride + nx as usize] += err;
                        }
                    }
                }
            }
        }
    }

    // Pack to 1 bit per pixel, MSB-first, 1 = white.
    fb.fill(0xFF);
    for y in 0..PANEL_H as usize {
        for x in 0..PANEL_W as usize {
            let v = buf[y * stride + x];
            let mut is_white = v >= 128.0;
            if invert { is_white = !is_white; }
            if !is_white {
                let byte_idx = y * ROW_BYTES + (x / 8);
                let bit = 7 - (x % 8);
                fb[byte_idx] &= !(1 << bit);
            }
        }
    }
}
