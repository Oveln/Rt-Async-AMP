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

    let rtasync_base = amp.get("RT24RCPU1BASE").expect("missing RT24RCPU1BASE");
    let rtasync_size =
        xtask::config::parse_size(amp.get("RT24RCPU1SIZE").expect("missing RT24RCPU1SIZE"));

    let memory_x = format!(
        "ENTRY(__start);\n\nMEMORY\n{{\n    RAM : ORIGIN = {rtasync_base}, LENGTH = 0x{rtasync_size:x}\n}}\n\n_max_hart_id = 0;\n_hart_stack_size = 8192;\n"
    );
    std::fs::write(Path::new(&out_dir).join("memory.x"), memory_x).unwrap();

    // 生成编译时间戳常量，供 main 启动时输出（标识 ELF 构建版本）。
    // 使用本地时间（构建机时区），格式 "YYYY-MM-DD HH:MM:SS"。
    let build_time = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let build_time_rs = format!(
        "// 本 ELF 的编译时间（build.rs 在编译期生成）。\n\
         pub const BUILD_TIME: &str = \"{build_time}\";\n"
    );
    std::fs::write(
        Path::new(&out_dir).join("build_time.rs"),
        build_time_rs,
    )
    .unwrap();
}
