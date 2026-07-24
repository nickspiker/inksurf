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
}
