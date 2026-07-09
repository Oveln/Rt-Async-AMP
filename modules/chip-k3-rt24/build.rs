use std::path::Path;
use std::process::Command;

fn main() {
    let ws = xtask::config::workspace_dir_from_manifest();
    let config = xtask::config::load_amp_toml(&ws);
    let out_dir_str = std::env::var("OUT_DIR").unwrap();
    let out_dir = Path::new(&out_dir_str);
    xtask::config::generate_amp_rs(&config, out_dir);
    println!("cargo:rerun-if-changed={}/amp.toml", ws.display());

    // 编译期内嵌 DTB：从 .dts（唯一真源，已追踪）用 dtc 生成 .dtb 到 OUT_DIR，
    // 经 cargo:rustc-env 传路径给 include_bytes!(env!("K3_DTB_PATH"))。
    // 不再追踪 .dtb 产物——fresh clone 后自动派生，仅需装 dtc。
    let dts = ws.join("its/rt-async-k3.dts");
    let dtb = out_dir.join("rt-async-k3.dtb");

    let out = Command::new("dtc")
        .args([
            "-I",
            "dts",
            "-O",
            "dtb",
            "-o",
            &dtb.to_string_lossy(),
            &dts.to_string_lossy(),
        ])
        .output()
        .unwrap_or_else(|_| {
            panic!(
                "dtc not found. Install device-tree-compiler (dtc): \
                 brew install dtc / apt install device-tree-compiler"
            )
        });
    if !out.status.success() {
        panic!("dtc failed:\n{}", String::from_utf8_lossy(&out.stderr));
    }

    println!("cargo:rustc-env=K3_DTB_PATH={}", dtb.display());
    println!("cargo:rerun-if-changed={}", dts.display());
}
