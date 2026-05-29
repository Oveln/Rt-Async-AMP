use std::path::Path;

fn main() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let out_dir = std::env::var("OUT_DIR").unwrap();

    println!("cargo:rustc-link-search={}", out_dir);
    let rt_dir = format!("{}/../../rt-async/modules/platform/archs/riscv64-rt", manifest_dir);
    println!("cargo:rustc-link-search={}", rt_dir);
    println!("cargo:rustc-link-arg=-Tmemory.x");
    println!("cargo:rustc-link-arg=-Tlink.x");
    println!("cargo:rerun-if-changed=build.rs");

    let ws = xtask::config::workspace_dir_from_manifest();
    println!("cargo:rerun-if-changed={}/amp.toml", ws.display());

    let amp = xtask::config::load_amp_toml(&ws);

    let rtasync_base = amp.get("RTASYNCBASE").expect("missing RTASYNCBASE");
    let rtasync_size = xtask::config::parse_size(amp.get("RTASYNCSIZE").expect("missing RTASYNCSIZE"));

    let memory_x = format!(
        "ENTRY(__start);\n\nMEMORY\n{{\n    RAM : ORIGIN = {rtasync_base}, LENGTH = 0x{rtasync_size:x}\n}}\n\n_max_hart_id = 0;\n_hart_stack_size = 4096;\n"
    );
    std::fs::write(Path::new(&out_dir).join("memory.x"), memory_x).unwrap();
}
