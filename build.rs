use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    let bytes = fs::read("memory.x").expect("read memory.x");
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    fs::write(out.join("memory.x"), bytes).expect("write memory.x");
    println!("cargo:rustc-link-search={}", out.display());
    println!("cargo:rerun-if-changed=memory.x");
    println!("cargo:rerun-if-changed=build.rs");
}
