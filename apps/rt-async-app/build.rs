fn main() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    println!("cargo:rustc-link-search={}", manifest_dir);
    let rt_dir = format!("{}/../../rt-async/modules/platform/archs/riscv64-rt", manifest_dir);
    println!("cargo:rustc-link-search={}", rt_dir);
    println!("cargo:rustc-link-arg=-Tmemory.x");
    println!("cargo:rustc-link-arg=-Tlink.x");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=memory.x");
}
