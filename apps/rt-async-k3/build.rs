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
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let (year, mon, day, hour, min, sec) = epoch_to_ymdhms(now);
    let build_time = format!("{year:04}-{mon:02}-{day:02} {hour:02}:{min:02}:{sec:02}");
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

/// Unix epoch 秒 → (年, 月, 日, 时, 分, 秒)（UTC）。
fn epoch_to_ymdhms(secs: u64) -> (u32, u32, u32, u32, u32, u32) {
    let days = (secs / 86400) as u32;
    let rem = (secs % 86400) as u32;
    let (hour, min, sec) = (rem / 3600, rem % 3600 / 60, rem % 60);
    // Howard Hinnant 的 civil_from_days 算法（无 chrono 依赖）。
    let z = days as i64 + 719_468; // 1970-01-01 偏移
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097) as u32; // [0, 146097)
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    ((y + if m <= 2 { 1 } else { 0 }) as u32, m, d, hour, min, sec)
}
