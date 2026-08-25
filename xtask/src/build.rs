use std::fs;
use std::path::Path;

use xtask::config::Config;

use crate::env_profile::EnvProfile;
use crate::util;

/// 产物类型：`Bin` = objcopy 成 flat binary（供 QEMU loader 加载）；
/// `Elf` = 直接复制 ELF（如 K3，由 esos 脚本整合进 itb）。
pub enum Artifact {
    Bin,
    Elf,
}

/// 一个 rt-async 应用 bin 的完整自描述。
///
/// 命名约定：`build` 用 `target_name`（带 `<platform>-` 前缀，如 `qemu-demo`/`k3-sched-demo`）；
/// `run --bin` 用 `name`（短名，如 `demo`，因 run 仅服务 QEMU）。
pub struct RtAsyncBin {
    /// cargo `--bin` 名（源码里的 bin 名，如 "demo"、"sched_demo"）。
    pub name: &'static str,
    /// xtask `build` 的 target 名（带平台前缀，如 "qemu-demo"、"k3-sched-demo"）。
    pub target_name: &'static str,
    /// 平台："qemu" / "k3"。用于 `build qemu` / `build k3` 聚合与环境归属。
    pub platform: &'static str,
    /// `build/<env>/` 下产物文件名（如 "rt-async.bin"、"rt-async-k3-sched-demo.elf"）。
    pub out: &'static str,
    /// app crate 目录（如 "apps/rt-async-app"、"apps/rt-async-k3"）。
    pub app_dir: &'static str,
    /// cargo `-p` 包名（如 "rt-async-app"、"rt-async-k3"）。
    pub package: &'static str,
    /// 目标 triple（QEMU bins："riscv64imac-unknown-none-elf"；K3 bins：
    /// 标记 K3_CS_TARGET → 仓库内 atomic-cas:false 自定义 target JSON）。
    pub target: &'static str,
    /// 产物类型。
    pub artifact: Artifact,
    /// 附加 cargo features（如 K3 固件的 `probe`——测量探针服务默认关闭，
    /// 板上产物经 xtask 恒带上；`bench` 门控 rtbench 这类纯测量 bin）。
    /// 见 apps/rt-async-k3/Cargo.toml 注释。
    pub features: &'static [&'static str],
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
        features: &[],
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
        features: &[],
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
        features: &[],
    },
    RtAsyncBin {
        name: "sched_demo",
        target_name: "k3-sched-demo",
        platform: "k3",
        out: "rt-async-k3-sched-demo.elf",
        app_dir: "apps/rt-async-k3",
        package: "rt-async-k3",
        target: K3_CS_TARGET,
        artifact: Artifact::Elf,
        features: &["probe"],
    },
    RtAsyncBin {
        name: "ipc_demo",
        target_name: "k3-ipc-demo",
        platform: "k3",
        out: "rt-async-k3-ipc-demo.elf",
        app_dir: "apps/rt-async-k3",
        package: "rt-async-k3",
        target: K3_CS_TARGET,
        artifact: Artifact::Elf,
        features: &["probe"],
    },
    RtAsyncBin {
        name: "shm_ping",
        target_name: "k3-shm-ping",
        platform: "k3",
        out: "rt-async-k3-shm-ping.elf",
        app_dir: "apps/rt-async-k3",
        package: "rt-async-k3",
        target: K3_CS_TARGET,
        artifact: Artifact::Elf,
        features: &["probe"],
    },
    RtAsyncBin {
        name: "rtbench",
        target_name: "k3-rtbench",
        platform: "k3",
        out: "rt-async-k3-rtbench.elf",
        app_dir: "apps/rt-async-k3",
        package: "rt-async-k3",
        target: K3_CS_TARGET,
        artifact: Artifact::Elf,
        features: &["bench"],
    },
    // PBMT 用户态兑现性实验的 RP 侧伴随固件（与 user-test-pbmt 配对，
    // 不跑 intercom 协议，无需 probe feature）。
    RtAsyncBin {
        name: "pbmt_probe",
        target_name: "k3-pbmt-probe",
        platform: "k3",
        out: "rt-async-k3-pbmt-probe.elf",
        app_dir: "apps/rt-async-k3",
        package: "rt-async-k3",
        target: K3_CS_TARGET,
        artifact: Artifact::Elf,
        features: &[],
    },
];

/// K3 专属 target 标记（RtAsyncBin.target 用）：解析为仓库内
/// targets/riscv64imac-k3-none-elf.json（atomic-cas:false——core 原生
/// RMW 被 cfg 掉，本地原子经 portable-atomic critical-section 后端 =
/// mstatus MIE 屏蔽 ~90ns/笔，替代 X100 Atomics Wrapper 序列化的原生
/// AMO ~2.2µs/笔）。自定义 target 无预编译 core，构建需 -Zbuild-std=core。
const K3_CS_TARGET: &str = "k3-cs-atomics";
const K3_CS_TARGET_JSON: &str = "targets/riscv64imac-k3-none-elf.json";

/// 按 xtask build target 名查找（带平台前缀，如 "qemu-demo"）。
pub fn find_by_target(target_name: &str) -> Option<&'static RtAsyncBin> {
    RTASYNC_BINS.iter().find(|b| b.target_name == target_name)
}

/// 按 cargo bin 短名查找（如 "demo"）。`run --bin` 用此（run 仅服务 QEMU）。
pub fn find_by_name(name: &str) -> Option<&'static RtAsyncBin> {
    RTASYNC_BINS.iter().find(|b| b.name == name)
}

/// 构建一个 rt-async bin：cargo build 后按 artifact 类型产出。
/// 产物落 `build/<env_name>/`（单 bin 构建传平台默认环境名）。
pub fn build_rt_async(root: &Path, bin: &RtAsyncBin, env_name: &str) {
    // target 解析：标准 triple 直用；K3 标记解析为仓库内自定义 target JSON
    // （绝对路径传给 cargo，产物目录取 JSON 文件名 stem）。
    let (target_arg, target_dir, build_std): (String, String, bool) = if bin.target == K3_CS_TARGET
    {
        let json = root.join(K3_CS_TARGET_JSON);
        assert!(
            json.exists(),
            "missing {K3_CS_TARGET_JSON}（K3 单核原子后端 target spec）"
        );
        (
            json.to_string_lossy().into_owned(),
            Path::new(K3_CS_TARGET_JSON)
                .file_stem()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            true,
        )
    } else {
        (bin.target.to_string(), bin.target.to_string(), false)
    };

    // 固定参数 + 条目声明的附加 features（非空时拼 --features）。
    let mut args: Vec<String> = vec!["build".to_string()];
    if build_std {
        // 自定义 JSON target：新版 nightly 要求显式放行 + 无预编译 core。
        args.push("-Zjson-target-spec".into());
        args.push("-Zbuild-std=core".into());
    }
    args.extend(
        [
            "--target",
            &target_arg,
            "--release",
            "-p",
            bin.package,
            "--bin",
            bin.name,
        ]
        .iter()
        .map(|s| s.to_string()),
    );
    let features = bin.features.join(",");
    if !bin.features.is_empty() {
        args.push("--features".into());
        args.push(features);
    }
    util::run(&root.join(bin.app_dir), "cargo", &args);

    let build_dir = root.join("build").join(env_name);
    fs::create_dir_all(&build_dir).unwrap();

    let elf = root
        .join("target")
        .join(&target_dir)
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

    eprintln!("rt-async ({}) → build/{}/{}", bin.target_name, env_name, bin.out);
}

pub fn opensbi(root: &Path, cfg: &Config, env_name: &str) {
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

    let build_dir = root.join("build").join(env_name);
    fs::create_dir_all(&build_dir).unwrap();

    let src = dir.join("build/platform/generic/firmware/fw_dynamic.bin");
    let dst = build_dir.join("fw_dynamic.bin");
    fs::copy(&src, &dst).unwrap();
    eprintln!("OpenSBI → {}", dst.display());
}

/// 构建环境对应的 StarryOS（经 tg-xtask，板级配置来自环境 profile）。
pub fn starryos(root: &Path, profile: &EnvProfile) {
    let tgoskits_root = root.join("tgoskits");
    let starry_dir = tgoskits_root.join("os/StarryOS");
    assert!(
        starry_dir.is_dir(),
        "tgoskits/os/StarryOS not found. Run 'git submodule update --init tgoskits'."
    );

    // tgoskits 新版已移除 make/axconfig 流程，StarryOS 构建统一走 tg-xtask：
    //   cargo starry build --config os/StarryOS/configs/board/<board>.toml
    // 板级配置（features/log/target）由 tgoskits 侧 checked-in 的 board TOML 声明，
    // 由环境 profile 指定（envs/<env>.toml 的 [starry].config）。
    let board_config = tgoskits_root.join(&profile.starry_config);
    assert!(
        board_config.is_file(),
        "StarryOS board config not found: {}. Check envs/{}.toml 与 tgoskits checkout.",
        board_config.display(),
        profile.name
    );

    let config_arg = board_config.to_string_lossy().to_string();
    // cargo xtask 在 rustup 下运行，env 带 RUSTUP_TOOLCHAIN（主仓工具链）。
    // 原样继承会让 tg-xtask 的 starry 构建忽略 tgoskits 自己的
    // rust-toolchain.toml 而用错工具链；去掉后 rustup 按 cwd 重新解析。
    let mut cmd = std::process::Command::new("cargo");
    cmd.args([
        "run", "--release", "-p", "tg-xtask", "--",
        "starry", "build",
        "--config", &config_arg,
    ]);
    // AMP 场景 hart 1 归属 rt-async（OpenSBI mret 到 M-mode），StarryOS
    // 必须单核。不传则 StarryOS 从 FDT 探测到 2 个 hart 并尝试启动
    // CPU 1，与 rt-async 抢占同一核导致启动卡死。
    if let Some(smp) = profile.starry_smp {
        cmd.arg("--smp").arg(smp.to_string());
    }
    cmd.env_remove("RUSTUP_TOOLCHAIN")
        .current_dir(&tgoskits_root);
    // lwprintf-rs（starry-kernel 的 C 依赖）构建脚本直接调
    // riscv64-linux-musl-gcc，需保证 musl 工具链 bin 在 PATH 上。
    if let Ok(cross) = std::env::var("RISCV64_MUSL_CROSS") {
        let mut path = std::path::PathBuf::from(&cross).join("bin");
        if let Some(existing) = std::env::var_os("PATH") {
            path.push(":");
            path.push(existing);
        }
        cmd.env("PATH", path);
    }
    let st = cmd.status().expect("cargo not found");
    assert!(st.success(), "StarryOS build failed");

    let build_dir = profile.env_build_dir(root);
    fs::create_dir_all(&build_dir).unwrap();

    // tg-xtask 产物（固定包 starryos）落在 tgoskits/target/riscv64gc-unknown-none-elf/release/
    // 下；qemu 用 .bin（kallsyms 后 rust-objcopy 刷新，供 QEMU loader），
    // k3 用 .uimg（board toml 同名 .its 触发 mkimage，供 fastboot bootm）。
    let artifact_src = tgoskits_root
        .join("target/riscv64gc-unknown-none-elf/release")
        .join(&profile.starry_artifact);
    assert!(
        artifact_src.exists(),
        "tg-xtask 产物缺失: {}（board config 旁是否缺同名 .its？）",
        artifact_src.display()
    );
    let dst = build_dir.join(&profile.starry_artifact);
    fs::copy(&artifact_src, &dst).unwrap();
    eprintln!("StarryOS → {}", dst.display());
}

/// 环境聚合构建：一个环境一条命令产出全部可运行产物。
///
/// qemu 环境：OpenSBI + StarryOS + 全部 qemu bins + user-apps（rootfs 注入用）。
/// k3 环境：全部 k3 bins + esos.itb（pack.bin）+ starryos.uimg——两个交付产物。
pub fn build_env(root: &Path, cfg: &Config, profile: &EnvProfile, envs: &[EnvProfile]) {
    let bins: Vec<_> = RTASYNC_BINS
        .iter()
        .filter(|b| b.platform == profile.platform)
        .collect();

    match profile.platform.as_str() {
        "qemu" => {
            opensbi(root, cfg, &profile.name);
            starryos(root, profile);
            for bin in &bins {
                build_rt_async(root, bin, &profile.name);
            }
            user_test(root, cfg);
            user_test_mbox(root, cfg);
            user_test_rpc(root, cfg);
            user_test_sched(root, cfg);
            eprintln!(
                "Env {} build complete. Run 'cargo xtask run --env {}' to start QEMU.",
                profile.name, profile.name
            );
        }
        "k3" => {
            for bin in &bins {
                build_rt_async(root, bin, &profile.name);
            }
            pack_itb(root, profile);
            starryos(root, profile);
            eprintln!(
                "Env {} build complete. Deliverables: build/{}/{{esos.itb, {}}}",
                profile.name, profile.name, profile.starry_artifact
            );
        }
        other => panic!("unknown platform in env profile: {other}"),
    }
    let _ = envs; // 预留：后续按 env 间依赖扩展（如共享产物复用）
}

/// k3：把 [pack].bin 打包进 esos.itb（调用 scripts/flash/k3-pack-itb.sh）。
/// 脚本负责 cp ELF + lzo + mkimage，产物落 build/<env>/esos.itb。
pub fn pack_itb(root: &Path, profile: &EnvProfile) {
    let target_name = profile
        .pack_bin
        .as_deref()
        .unwrap_or_else(|| panic!("envs/{}.toml 缺 [pack].bin（k3 环境必填）", profile.name));
    let bin = find_by_target(target_name)
        .unwrap_or_else(|| panic!("envs/{}.toml [pack].bin 未知 target: {target_name}", profile.name));
    assert!(
        bin.platform == profile.platform,
        "envs/{}.toml [pack].bin {target_name} 不属于平台 {}",
        profile.name, profile.platform
    );

    let elf_rel = format!("build/{}/{}", profile.name, bin.out);
    let elf = root.join(&elf_rel);
    assert!(
        elf.exists(),
        "rcpu1 ELF 缺失: {}（先构建 {}）",
        elf.display(),
        target_name
    );

    let script = root.join("scripts/flash/k3-pack-itb.sh");
    let st = std::process::Command::new("bash")
        .arg(&script)
        .env("ELF_SRC", &elf_rel)
        .current_dir(root)
        .status()
        .unwrap_or_else(|e| panic!("bash: {e}"));
    assert!(st.success(), "k3-pack-itb.sh failed");
    let itb = profile.env_build_dir(root).join("esos.itb");
    assert!(itb.exists(), "k3-pack-itb.sh 未产出 {}", itb.display());
    eprintln!("esos.itb → {}", itb.display());
}

pub fn user_test(root: &Path, _cfg: &Config) {
    build_user_app(root, "user-apps/user-test-ipc", "user-test-ipc", &[]);
}

pub fn user_test_mbox(root: &Path, _cfg: &Config) {
    build_user_app(root, "user-apps/user-test-mbox", "user-test-mbox", &[]);
}

pub fn user_test_rpc(root: &Path, _cfg: &Config) {
    build_user_app(root, "user-apps/user-test-rpc", "user-test-rpc", &[]);
}

pub fn user_test_sched(root: &Path, _cfg: &Config) {
    build_user_app(root, "user-apps/user-test-sched", "user-test-sched", &[]);
}

/// IPC 延迟基准（唯一形态 = user-cbo 默认开，见 user-test-bench Cargo.toml
/// default；2026-08-21 起普通整窗 ioctl 变体已删）。
pub fn user_test_bench(root: &Path, _cfg: &Config) {
    build_user_app(root, "user-apps/user-test-bench", "user-test-bench", &[]);
}

/// PBMT 用户态兑现性实验的 AP 侧（与 RP 固件 pbmt_probe 配对）。
pub fn user_test_pbmt(root: &Path, _cfg: &Config) {
    build_user_app(root, "user-apps/user-test-pbmt", "user-test-pbmt", &[]);
}

fn build_user_app(root: &Path, app_dir: &str, artifact_name: &str, features: &[&str]) {
    let target = "riscv64gc-unknown-linux-musl";
    let mut args: Vec<String> = ["build", "--target", target, "--release"]
        .into_iter()
        .map(String::from)
        .collect();
    if !features.is_empty() {
        // workspace 内单包构建，feature 直接传给该包
        args.push("--features".into());
        args.push(features.join(","));
    }
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    util::run(&root.join(app_dir), "cargo", &arg_refs);
    // user-apps 与环境无关（qemu rootfs 注入 / k3 串口传输共用），落 build/ 顶层。
    let build_dir = root.join("build");
    fs::create_dir_all(&build_dir).unwrap();
    let src = root
        .join("target")
        .join(target)
        .join("release")
        .join(artifact_name.trim_end_matches("-cbo"));
    let dst = build_dir.join(artifact_name);
    fs::copy(&src, &dst).unwrap();
    eprintln!("{artifact_name} → {}", dst.display());
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
