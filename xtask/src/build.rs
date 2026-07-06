use std::fs;
use std::path::Path;
use std::process::Command;

use xtask::config::Config;

use crate::util;

/// 产物类型：`Bin` = objcopy 成 flat binary（供 QEMU loader 加载）；
/// `Elf` = 直接复制 ELF（如 K3，由 esos 脚本整合进 itb）。
pub enum Artifact {
    Bin,
    Elf,
}

/// 一个 rt-async 应用 bin 的完整自描述。
///
/// 命名约定：`build` 用 `target_name`（带 `<platform>-` 前缀，如 `qemu-demo`/`k3-minimal`）；
/// `run --bin` 用 `name`（短名，如 `demo`，因 run 仅服务 QEMU）。
pub struct RtAsyncBin {
    /// cargo `--bin` 名（源码里的 bin 名，如 "demo"、"minimal"）。
    pub name: &'static str,
    /// xtask `build` 的 target 名（带平台前缀，如 "qemu-demo"、"k3-minimal"）。
    pub target_name: &'static str,
    /// 平台："qemu" / "k3"。用于 `build qemu` / `build k3` 聚合。
    pub platform: &'static str,
    /// `build/` 下产物文件名（如 "rt-async.bin"、"rt-async-k3-minimal.elf"）。
    pub out: &'static str,
    /// app crate 目录（如 "apps/rt-async-app"、"apps/rt-async-k3"）。
    pub app_dir: &'static str,
    /// cargo `-p` 包名（如 "rt-async-app"、"rt-async-k3"）。
    pub package: &'static str,
    /// 目标 triple（均为 "riscv64imac-unknown-none-elf"）。
    pub target: &'static str,
    /// 产物类型。
    pub artifact: Artifact,
}

/// 所有 rt-async bin 的统一注册表（QEMU + K3）。
/// 加新 bin 只需在此追加一行，自动获得 `build <target_name>` 与纳入 `build <platform>`。
pub const RTASYNC_BINS: &[RtAsyncBin] = &[
    RtAsyncBin {
        name: "demo",
        target_name: "qemu-demo",
        platform: "qemu",
        out: "rt-async.bin",
        app_dir: "apps/rt-async-app",
        package: "rt-async-app",
        target: "riscv64imac-unknown-none-elf",
        artifact: Artifact::Bin,
    },
    RtAsyncBin {
        name: "console",
        target_name: "qemu-console",
        platform: "qemu",
        out: "rt-async-console.bin",
        app_dir: "apps/rt-async-app",
        package: "rt-async-app",
        target: "riscv64imac-unknown-none-elf",
        artifact: Artifact::Bin,
    },
    RtAsyncBin {
        name: "console_interrupt",
        target_name: "qemu-console-interrupt",
        platform: "qemu",
        out: "rt-async-console-interrupt.bin",
        app_dir: "apps/rt-async-app",
        package: "rt-async-app",
        target: "riscv64imac-unknown-none-elf",
        artifact: Artifact::Bin,
    },
    RtAsyncBin {
        name: "minimal",
        target_name: "k3-minimal",
        platform: "k3",
        out: "rt-async-k3-minimal.elf",
        app_dir: "apps/rt-async-k3",
        package: "rt-async-k3",
        target: "riscv64imac-unknown-none-elf",
        artifact: Artifact::Elf,
    },
];

/// 按 xtask build target 名查找（带平台前缀，如 "qemu-demo"）。
pub fn find_by_target(target_name: &str) -> Option<&'static RtAsyncBin> {
    RTASYNC_BINS.iter().find(|b| b.target_name == target_name)
}

/// 按 cargo bin 短名查找（如 "demo"）。`run --bin` 用此（run 仅服务 QEMU）。
pub fn find_by_name(name: &str) -> Option<&'static RtAsyncBin> {
    RTASYNC_BINS.iter().find(|b| b.name == name)
}

/// 构建一个 rt-async bin：cargo build 后按 artifact 类型产出。
pub fn build_rt_async(root: &Path, bin: &RtAsyncBin) {
    util::run(
        &root.join(bin.app_dir),
        "cargo",
        &[
            "build",
            "--target",
            bin.target,
            "--release",
            "-p",
            bin.package,
            "--bin",
            bin.name,
        ],
    );

    let build_dir = root.join("build");
    fs::create_dir_all(&build_dir).unwrap();

    let elf = root
        .join("target")
        .join(bin.target)
        .join("release")
        .join(bin.name);
    let out = build_dir.join(bin.out);

    match bin.artifact {
        Artifact::Bin => util::run(
            root,
            "riscv64-elf-objcopy",
            &["-O", "binary", &elf.to_string_lossy(), &out.to_string_lossy()],
        ),
        Artifact::Elf => {
            fs::copy(&elf, &out).unwrap();
        }
    }

    eprintln!("rt-async ({}) → build/{}", bin.target_name, bin.out);
}

pub fn opensbi(root: &Path, cfg: &Config) {
    let dir = root.join("opensbi");
    assert!(
        dir.join(".patched").exists(),
        "opensbi not ready. Run 'cargo xtask setup' first."
    );

    let nproc = std::thread::available_parallelism()
        .map(|n| n.to_string())
        .unwrap_or_else(|_| "4".into());

    let fw_text_start = cfg.get("OPENSBIBASE");

    util::run(
        &dir,
        "make",
        &[
            &format!("-j{nproc}"),
            "PLATFORM=generic",
            "CROSS_COMPILE=riscv64-elf-",
            "O=build",
            &format!("FW_TEXT_START={fw_text_start}"),
        ],
    );

    let build_dir = root.join("build");
    fs::create_dir_all(&build_dir).unwrap();

    let src = dir.join("build/platform/generic/firmware/fw_dynamic.bin");
    let dst = build_dir.join("fw_dynamic.bin");
    fs::copy(&src, &dst).unwrap();
    eprintln!("OpenSBI → {}", dst.display());
}

pub fn starryos(root: &Path, cfg: &Config) {
    let dir = root.join("tgoskits/os/StarryOS");
    assert!(
        dir.is_dir(),
        "tgoskits/os/StarryOS not found. Run 'git submodule update --init tgoskits'."
    );

    // Read board-level features from the virt-rt-async config file.
    let board_config = root.join("tgoskits/os/StarryOS/configs/board/virt-rt-async.toml");
    let (board_features, plat_dyn): (Vec<String>, bool) = if board_config.exists() {
        let content = std::fs::read_to_string(&board_config).unwrap_or_default();
        let v: toml::Value = content.parse().unwrap_or(toml::Value::Table(Default::default()));
        let features = v
            .get("features")
            .and_then(|f| f.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default();
        let plat_dyn = v
            .get("plat_dyn")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        (features, plat_dyn)
    } else {
        (vec!["qemu".to_string()], false)
    };
    let app_features = board_features.join(" ");

    // For plat-dyn builds, resolve the PLAT_CONFIG via tg-xtask.
    let tgoskits_root = root.join("tgoskits");
    let plat_config: String = if plat_dyn {
        let output = Command::new("cargo")
            .args([
                "run", "--release", "-p", "tg-xtask", "--",
                "config", "inspect", "--makefile",
                "--manifest-dir", "os/StarryOS/starryos",
                "--package", "axplat-dyn",
            ])
            .current_dir(&tgoskits_root)
            .output()
            .expect("failed to run tg-xtask config inspect");
        if !output.status.success() {
            panic!(
                "tg-xtask config inspect failed:\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        stdout
            .split_whitespace()
            .find_map(|pair| {
                let (k, v) = pair.split_once('=')?;
                if k == "PLAT_CONFIG" { Some(v.to_string()) } else { None }
            })
            .unwrap_or_else(|| panic!("PLAT_CONFIG not found in tg-xtask output: {}", stdout))
    } else {
        String::new()
    };

    // Read QEMU memory size from amp.toml (must match what QEMU gets at runtime).
    let qemu_ram = cfg.get("QEMURAM");

    // lwprintf-rs (a starry-kernel hard-dep via kmod/printk) compiles a C
    // library with the cc crate. Point per-target CC/AR at the musl toolchain.
    let musl_cross = std::env::var("RISCV64_MUSL_CROSS")
        .map(std::path::PathBuf::from)
        .ok()
        .or_else(|| home::home_dir().map(|h| h.join("bin/riscv64-linux-musl-cross")));
    let target = "riscv64gc-unknown-none-elf";
    let cc_key = format!("CC_{target}");
    let ar_key = format!("AR_{target}");
    let make_env: Vec<(String, std::ffi::OsString)> = {
        let mut env: Vec<(String, std::ffi::OsString)> = Vec::new();
        env.push((
            "AMP_TOML_PATH".to_string(),
            root.join("amp.toml").into_os_string(),
        ));
        if let Some(ref cross) = musl_cross {
            let bin = cross.join("bin");
            let mut path = bin.clone().into_os_string();
            path.push(":");
            path.push(std::env::var_os("PATH").unwrap_or_default());
            env.push(("PATH".to_string(), path));
            env.push((cc_key.clone(), bin.join("riscv64-linux-musl-gcc").into_os_string()));
            env.push((ar_key.clone(), bin.join("riscv64-linux-musl-ar").into_os_string()));
        }
        env.push(("APP_FEATURES".to_string(), app_features.clone().into()));
        env
    };

    let mut make_args: Vec<String> = vec![
        "ARCH=riscv64".into(),
        format!("MEM={qemu_ram}"),
        "LOG=info".into(),
        "build".into(),
    ];
    if plat_dyn {
        make_args.push(format!("PLAT_CONFIG={plat_config}"));
        make_args.push("PLAT_DYN=y".into());
        eprintln!(
            "cd {} && APP_FEATURES='{app_features}' make ARCH=riscv64 PLAT_DYN=y PLAT_CONFIG={plat_config} MEM={qemu_ram} LOG=info build",
            dir.display(),
        );
    } else {
        make_args.push("MYPLAT=ax-plat-riscv64-qemu-virt".into());
        eprintln!(
            "cd {} && APP_FEATURES='{app_features}' make ARCH=riscv64 MYPLAT=ax-plat-riscv64-qemu-virt MEM={qemu_ram} LOG=info build",
            dir.display(),
        );
    }

    let make_args_ref: Vec<&str> = make_args.iter().map(|s| s.as_str()).collect();
    let st = Command::new("make")
        .args(&make_args_ref)
        .envs(make_env.iter().map(|(k, v)| (k.as_str(), v.clone())))
        .current_dir(&dir)
        .status()
        .expect("make not found");
    assert!(st.success(), "StarryOS build failed");

    let build_dir = root.join("build");
    fs::create_dir_all(&build_dir).unwrap();

    // The Makeflow emits starryos_qemu-rt-async.{elf,bin} next to the
    // starryos crate; copy the flat binary into build/ for the QEMU loader.
    let bin_src = dir
        .join("starryos")
        .join("starryos_qemu-rt-async.bin");
    let bin = build_dir.join("starryos.bin");
    fs::copy(&bin_src, &bin).unwrap();
    eprintln!("StarryOS → {}", bin.display());
}

pub fn user_test(root: &Path, _cfg: &Config) {
    build_user_app(root, "user-apps/user-test-ipc", "user-test-ipc");
}

pub fn user_test_rpc(root: &Path, _cfg: &Config) {
    build_user_app(root, "user-apps/user-test-rpc", "user-test-rpc");
}

pub fn user_test_sched(root: &Path, _cfg: &Config) {
    build_user_app(root, "user-apps/user-test-sched", "user-test-sched");
}

fn build_user_app(root: &Path, app_dir: &str, bin_name: &str) {
    let target = "riscv64gc-unknown-linux-musl";
    util::run(
        &root.join(app_dir),
        "cargo",
        &["build", "--target", target, "--release"],
    );
    let build_dir = root.join("build");
    fs::create_dir_all(&build_dir).unwrap();
    let src = root
        .join("target")
        .join(target)
        .join("release")
        .join(bin_name);
    let dst = build_dir.join(bin_name);
    fs::copy(&src, &dst).unwrap();
    eprintln!("{bin_name} → {}", dst.display());
}

pub fn qemu(root: &Path, _cfg: &Config) {
    let src_dir = root.join("qemu");
    assert!(
        src_dir.join(".patched").exists(),
        "qemu not ready. Run 'cargo xtask setup' first."
    );

    let build_dir = src_dir.join("build");
    let bin = build_dir.join("qemu-system-riscv64-unsigned");
    fs::create_dir_all(&build_dir).unwrap();

    util::run(
        &build_dir,
        "../configure",
        &[
            "--target-list=riscv64-softmmu",
            "--disable-docs",
            "--disable-tools",
            "--disable-guest-agent",
            "--python=python3",
        ],
    );

    let nproc = std::thread::available_parallelism()
        .map(|n| n.to_string())
        .unwrap_or_else(|_| "4".into());
    util::run(&build_dir, "make", &["-j", &nproc]);
    eprintln!("QEMU → {}", bin.display());
}
