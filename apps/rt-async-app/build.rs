use std::env;

fn main() {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").unwrap();
    // memory.x from this crate
    println!("cargo:rustc-link-search={}", manifest_dir);
    // link.x from riscv64-rt (resolved via platform dependency)
    let rt_dir = format!("{}/../../rt-async/modules/platform/archs/riscv64-rt", manifest_dir);
    println!("cargo:rustc-link-search={}", rt_dir);
    println!("cargo:rustc-link-arg=-Tmemory.x");
    println!("cargo:rustc-link-arg=-Tlink.x");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=memory.x");
}
