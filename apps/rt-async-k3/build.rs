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
    // 源码变化时重跑 build.rs，刷新编译时间戳（否则 OUT_DIR 里是旧缓存值）。
    println!("cargo:rerun-if-changed=src/");

    // K3 RT24 rcpu1 链接布局——本 app 的板级常量（K3 地址布局不走 amp.toml）。
    // 与官方 esos baremetal.ld 的 rcpu1 区一致：DDR 0x1_0080_4000 起 3MB。
    // 不变式：U-Boot k3-rproc 只 memcpy ELF PT_LOAD、无 DTB handoff，
    // 链接地址 = 加载地址 = 执行地址；esos.itb 的 rcpu1-fw load 地址与此对齐。
    const RT24_RCPU1_BASE: &str = "0x100804000";
    const RT24_RCPU1_SIZE: u64 = 0x300000; // 3M

    let memory_x = format!(
        "ENTRY(__start);\n\nMEMORY\n{{\n    RAM : ORIGIN = {RT24_RCPU1_BASE}, LENGTH = 0x{RT24_RCPU1_SIZE:x}\n}}\n\n_max_hart_id = 0;\n_hart_stack_size = 8192;\n"
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
