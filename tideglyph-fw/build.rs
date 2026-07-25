use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());

    let bytes = fs::read("memory.x").expect("read memory.x");
    fs::write(out.join("memory.x"), bytes).expect("write memory.x");
    println!("cargo::rustc-link-search={}", out.display());
    println!("cargo::rerun-if-changed=memory.x");
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rustc-link-arg-bins=--nmagic");
    println!("cargo::rustc-link-arg-bins=-Tlink.x");
    println!("cargo::rustc-link-arg-bins=-Tdefmt.x");

    // Bake this build's Eagle-time stamp as the downgrade FLOOR: an OTA manifest
    // is only accepted if its stamp strictly exceeds this, so a device can never
    // be pushed an image older than the one it's running. Recomputed every build
    // (never cached) so the floor always advances.
    let stamp = vsf::eagle_time_oscillations();
    // Build-time Unix seconds — used as an approximate "now" for the first tide
    // render until the device gets a real time source (set over BLE).
    let unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    fs::write(
        out.join("build_stamp.rs"),
        format!("pub const FIRMWARE_BUILD_STAMP: i64 = {stamp};\npub const BUILD_UNIX_SECS: i64 = {unix};\n"),
    )
    .expect("write build_stamp.rs");
    println!("cargo::rerun-if-changed=../ota_key.bin");

    // Decode the decimal font PNGs (0-9 then colon) into const bitmaps for the
    // on-device now-time label. Each glyph = width + width*12 on/off bytes.
    let names = ["0", "1", "2", "3", "4", "5", "6", "7", "8", "9", ":"];
    let mut font_src = String::from(
        "pub const GLYPH_H: usize = 12;\npub const GLYPH_KERN: i32 = 1;\npub struct Glyph { pub w: u8, pub bits: &'static [u8] }\npub static DIGITS: [Glyph; 11] = [\n",
    );
    for n in names {
        let path = format!("../assets/font/{n}.png");
        println!("cargo::rerun-if-changed={path}");
        let (w, h, bits) = decode_png_file(&path);
        assert_eq!(h, 12, "glyph {n} must be 12px tall");
        let bits_str = bits.iter().map(|b| b.to_string()).collect::<Vec<_>>().join(",");
        font_src.push_str(&format!("    Glyph {{ w: {w}, bits: &[{bits_str}] }},\n"));
    }
    font_src.push_str("];\n");
    fs::write(out.join("font_glyphs.rs"), font_src).expect("write font_glyphs.rs");
}

/// Decode a font PNG to (width, height, on/off bytes) — "on" = brighter than mid-gray.
fn decode_png_file(path: &str) -> (u8, usize, Vec<u8>) {
    let bytes = fs::read(path).unwrap_or_else(|_| panic!("read {path}"));
    let mut dec = png::Decoder::new(bytes.as_slice());
    dec.set_transformations(png::Transformations::EXPAND);
    let mut reader = dec.read_info().expect("png read_info");
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).expect("png next_frame");
    let w = info.width as usize;
    let h = info.height as usize;
    let bpp = (info.line_size / w).max(1);
    let mut bits = Vec::with_capacity(w * h);
    for y in 0..h {
        for x in 0..w {
            bits.push(if buf[(y * w + x) * bpp] > 128 { 1u8 } else { 0 });
        }
    }
    (w as u8, h, bits)
}
