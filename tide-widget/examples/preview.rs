//! Host preview: render both editions at a few sizes to PNGs, no phone needed, with each render's time. Also runs on the phone itself (built for arm64 and run over adb from /data/local/tmp) to time the real thing without reinstalling the widget.

use std::fs::File;
use std::io::BufWriter;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use tide_widget::render::{render_argb, Edition};

fn main() {
    let out_dir = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    let unix = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
    for (edition, tag) in [(Edition::Dozenal, ""), (Edition::Hourly, "hourly_")] {
        // Panel-native, then typical phone widget sizes in px (4×2 and 4×1 at ~2.6x density, plus a big one).
        for (w, h) in [(384, 180), (910, 470), (910, 230), (1400, 720)] {
            let started = Instant::now();
            let px = render_argb(unix, w, h, edition);
            let ms = started.elapsed().as_millis();
            let mut rgba = Vec::with_capacity(w * h * 4);
            for p in px {
                let p = p as u32;
                rgba.extend_from_slice(&[(p >> 16) as u8, (p >> 8) as u8, p as u8, (p >> 24) as u8]);
            }
            let path = format!("{out_dir}/tide_{tag}{w}x{h}.png");
            let mut enc = png::Encoder::new(BufWriter::new(File::create(&path).unwrap()), w as u32, h as u32);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            enc.write_header().unwrap().write_image_data(&rgba).unwrap();
            println!("{path} ({ms} ms)");
        }
    }
}
