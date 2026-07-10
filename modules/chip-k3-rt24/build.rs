use std::path::Path;
use std::process::Command;

fn main() {
    let ws = xtask::config::workspace_dir_from_manifest();
    let config = xtask::config::load_amp_toml(&ws);
    let out_dir_str = std::env::var("OUT_DIR").unwrap();
    let out_dir = Path::new(&out_dir_str);
    xtask::config::generate_amp_rs(&config, out_dir);
    println!("cargo:rerun-if-changed={}/amp.toml", ws.display());

    // 编译期内嵌 DTB：.dts → cc -E（展开 #include/#define 宏）→ .pp.dts
    // → dtc（求值算术表达式，生成 .dtb）→ include_bytes!(env!("K3_DTB_PATH"))。
    // DTS 用官方 esos 宏写法（K3_PADCONF / MUX_MODE4 等，定义见 k3-pinctrl.h），
    // cc 做宏展开，dtc 做算术求值（dtc 原生支持 < (a*b) (c|d) > 表达式）。
    // 不再追踪 .dtb 产物——fresh clone 后自动派生，仅需装 cc + dtc。
    let its_dir = ws.join("its");
    let dts = its_dir.join("rt-async-k3.dts");
    let pp_dts = out_dir.join("rt-async-k3.pp.dts");
    let dtb = out_dir.join("rt-async-k3.dtb");

    // 1. C 预处理：展开 #include "k3-pinctrl.h" 和 #define 宏（与 esos 编译链一致）。
    //    -E: 仅预处理；-P: 去掉行标记；-nostdinc: 不搜系统头；
    //    -undef: 不预定义系统宏；-x assembler-with-cpp: 处理 #include/#define。
    let cpp_out = Command::new("cc")
        .args(["-E", "-P", "-nostdinc", "-undef", "-x", "assembler-with-cpp"])
        .arg("-I")
        .arg(&its_dir)
        .arg(&dts)
        .output()
        .unwrap_or_else(|_| panic!("cc (C compiler) not found"));
    if !cpp_out.status.success() {
        panic!(
            "cc -E failed on {}:\n{}",
            dts.display(),
            String::from_utf8_lossy(&cpp_out.stderr)
        );
    }
    std::fs::write(&pp_dts, &cpp_out.stdout).expect("write pp.dts");

    // 2. dtc 编译：算术表达式由 dtc 求值。
    let out = Command::new("dtc")
        .args([
            "-I",
            "dts",
            "-O",
            "dtb",
            "-o",
            &dtb.to_string_lossy(),
            &pp_dts.to_string_lossy(),
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
    println!(
        "cargo:rerun-if-changed={}",
        its_dir.join("k3-pinctrl.h").display()
    );
}
