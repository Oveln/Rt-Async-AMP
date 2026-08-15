use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use xtask::config::Config;

use crate::build::RtAsyncBin;
use crate::env_profile::{DtbSource, EnvProfile, QemuMachine};

const TMUX_SESSION: &str = "rt-async-amp";
const UART_SOCK: &str = "/tmp/rt-async-uart.sock";

/// rt-async 专属 DTB（hart1 + UART1 视角）源。经 QEMU loader 摆到
/// RTASYNCDTBBASE，board_init 的 esos 同款扫描从此地址认领
/// compatible="ov,rt-async" 的 DTB。
const RTASYNC_DTS: &str = "its/rt-async-qemu-virt-amp.dts";

/// 共享的 dts→dtb 编译逻辑：按 mtime 增量编译。
///
/// 编译链与 K3（`modules/chip-k3-rt24/build.rs`）一致：
/// `cc -E`（展开 #include/#define，-I its/）→ `dtc`（求值算术表达式）。
/// DTS 经 `#include "rt-async-shm.dtsi"` 引用跨核共享内存节点单一真相源，
/// 故依赖追踪需包含 its/ 下所有 .dts/.dtsi 的 mtime。
fn compile_dtb(root: &Path, out_dir: &Path, dts_rel: &str, dtb_rel: &str) -> PathBuf {
    let its_dir = root.join("its");
    // dts_rel 形如 "its/<name>.dts"，含 its/ 前缀，故从 root join（勿与 its_dir 再拼）。
    let dts = root.join(dts_rel);
    let dtb = out_dir.join(dtb_rel);
    let pp_dts = out_dir.join(format!("{}.pp.dts", dtb_rel));

    let inputs: Vec<_> = std::fs::read_dir(&its_dir)
        .expect("its/ dir missing")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| matches!(p.extension().and_then(|e| e.to_str()), Some("dts" | "dtsi")))
        .collect();

    let newest_input = inputs.iter().filter_map(|p| p.metadata().ok()).filter_map(|m| m.modified().ok()).max();
    let dtb_mtime = std::fs::metadata(&dtb).ok().and_then(|m| m.modified().ok());

    // 任一 dts/dtsi 比 dtb 新则重编译（dtsi 改动会影响所有引用它的 DTB）。
    let stale = newest_input.map_or(true, |t| dtb_mtime.map_or(true, |d| t > d));

    if stale {
        eprintln!("DTB: compiling {} -> {}", dts.display(), dtb.display());
        std::fs::create_dir_all(out_dir).unwrap();

        // 1. C 预处理：展开 #include/#define（与 K3 build.rs 一致）。
        let cpp_out = Command::new("cc")
            .args(["-E", "-P", "-nostdinc", "-undef", "-x", "assembler-with-cpp"])
            .arg("-I")
            .arg(&its_dir)
            .arg(&dts)
            .output()
            .unwrap_or_else(|_| panic!("cc (C compiler) not found"));
        if !cpp_out.status.success() {
            eprintln!("cc -E failed:\n{}", String::from_utf8_lossy(&cpp_out.stderr));
            panic!("DTS preprocess failed");
        }
        std::fs::write(&pp_dts, &cpp_out.stdout).expect("write pp.dts");

        // 2. dtc 编译：算术表达式由 dtc 求值。
        let out = Command::new("dtc")
            .args(["-I", "dts", "-O", "dtb", "-o", &dtb.to_string_lossy(), &pp_dts.to_string_lossy()])
            .output()
            .expect("dtc not found. Install device-tree-compiler (dtc) via your system package manager");
        if !out.status.success() {
            eprintln!("dtc failed:\n{}", String::from_utf8_lossy(&out.stderr));
            panic!("DTB compilation failed");
        }
    }
    dtb
}

/// AP 侧 DTB：按环境 profile 声明的来源生成（dts 编译 / dumpdtb+overlay）。
fn ensure_ap_dtb(root: &Path, profile: &EnvProfile, machine: &QemuMachine) -> PathBuf {
    let out_dir = profile.env_build_dir(root);
    match profile.dtb.as_ref().expect("qemu 环境的 env toml 缺 [dtb] 节") {
        DtbSource::Dts(dts) => compile_dtb(root, &out_dir, dts, "ap.dtb"),
        DtbSource::DumpdtbOverlay { overlay } => {
            dumpdtb_overlay(root, &out_dir, machine, overlay)
        }
    }
}

/// dumpdtb + overlay：从本环境机器导出基线 DTB（自带正确的 imsics/aplic
/// 节点，中断拓扑自动跟随 -machine 参数），再叠加共享窗 overlay。
///
/// 基线随 machine 字符串变化（如 aia 开关），stamp 文件记录生成参数，
/// 参数不一致即重新导出。fdtoverlay 不携带 /memreserve/——StarryOS 全链
/// 不解析 memreserve（qemu-plic 手写 dts 里的条目同样无效），行为等价。
fn dumpdtb_overlay(root: &Path, out_dir: &Path, machine: &QemuMachine, overlay_dts: &str) -> PathBuf {
    let dtb = out_dir.join("ap.dtb");
    let base = out_dir.join("qemu-base.dtb");
    let dtbo = out_dir.join("ap.overlay.dtbo");
    let stamp = out_dir.join("ap.dtb.stamp");

    let its_newest = newest_mtime(&root.join("its"), &["dts", "dtsi"]);
    let up_to_date = std::fs::read_to_string(&stamp)
        .ok()
        .map(|s| {
            s.trim()
                == format!("{}\n{}\n{}", machine.machine, machine.smp, overlay_dts)
        })
        .unwrap_or(false)
        && std::fs::metadata(&dtb)
            .ok()
            .and_then(|m| m.modified().ok())
            .zip(its_newest)
            .is_some_and(|(d, t)| t < d);

    if !up_to_date {
        std::fs::create_dir_all(out_dir).unwrap();
        let qemu_bin = root.join("qemu/build/qemu-system-riscv64-unsigned");
        assert!(
            qemu_bin.exists(),
            "定制 QEMU 缺失: {}（先 'cargo xtask qemu'）",
            qemu_bin.display()
        );

        // 1. 基线 DTB：-machine <machine>,dumpdtb=<file> 导出后 QEMU 立即退出。
        //    必须带运行时同款 -smp/-m，CPU/memory 节点才与实际机器一致
        //    （缺省 1 核会把 CPU 节点 dump 成 1 个，OpenSBI 就只见到 1 个 hart）。
        eprintln!(
            "DTB: dumpdtb (machine={}, smp={}, ram={}) -> {}",
            machine.machine,
            machine.smp,
            machine.ram,
            base.display()
        );
        let st = Command::new(&qemu_bin)
            .args([
                "-machine", &format!("{},dumpdtb={}", machine.machine, base.display()),
                "-smp", &machine.smp,
                "-m", &machine.ram,
                "-display", "none",
            ])
            .output()
            .unwrap_or_else(|e| panic!("qemu: {e}"));
        assert!(
            st.status.success() && base.exists(),
            "dumpdtb failed:\n{}",
            String::from_utf8_lossy(&st.stderr)
        );

        // 2. overlay 源编译（cc -E → dtc，target-path fragment 无需 -@ 符号表；
        //    reg_format 等警告源于 fragment 上下文的 cells 缺省，叠加后按根节点
        //    cells（2/2）解释，值不变）。
        let its_dir = root.join("its");
        let dts = root.join(overlay_dts);
        let pp = out_dir.join("ap.overlay.pp.dts");
        let cpp_out = Command::new("cc")
            .args(["-E", "-P", "-nostdinc", "-undef", "-x", "assembler-with-cpp"])
            .arg("-I").arg(&its_dir).arg(&dts)
            .output()
            .expect("cc (C compiler) not found");
        assert!(cpp_out.status.success(), "overlay preprocess failed");
        std::fs::write(&pp, &cpp_out.stdout).unwrap();
        let out = Command::new("dtc")
            .args(["-I", "dts", "-O", "dtb", "-o", &dtbo.to_string_lossy(), &pp.to_string_lossy()])
            .output()
            .expect("dtc not found");
        assert!(out.status.success(), "overlay dtc failed");

        // 3. 叠加。
        let out = Command::new("fdtoverlay")
            .args(["-i", &base.to_string_lossy(), "-o", &dtb.to_string_lossy(), &dtbo.to_string_lossy()])
            .output()
            .expect("fdtoverlay not found. Install device-tree-compiler");
        assert!(
            out.status.success(),
            "fdtoverlay failed:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );

        std::fs::write(&stamp, format!("{}\n{}\n{}\n", machine.machine, machine.smp, overlay_dts))
            .unwrap();
    }
    dtb
}

fn newest_mtime(dir: &Path, exts: &[&str]) -> Option<std::time::SystemTime> {
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| exts.contains(&e))
        })
        .filter_map(|p| p.metadata().ok())
        .filter_map(|m| m.modified().ok())
        .max()
}

/// QEMU 启动参数唯一构造点：前台与 tmux 共用同一 argv 生成逻辑。
///
/// 两种模式的差异只有两处：
///   - UART1 chardev：前台 server=on,wait=off（QEMU 监听，socat 事后连）；
///     tmux server=off（socat 先监听、QEMU 作客户端连，不丢启动期输出）。
///   - tmux 结尾多 -nographic（等价 -display none + stdio 重定向，保持既有行为）。
#[allow(clippy::too_many_arguments)]
fn qemu_argv(
    qemu_bin: &Path,
    machine: &QemuMachine,
    dtb: &Path,
    opensbi_fw: &Path,
    starryos_bin: &Path,
    app_bin: &Path,
    rtasync_dtb: &Path,
    rtasync_base: &str,
    rtasync_dtb_base: &str,
    rootfs: &Path,
    uart_chardev_mode: &str,
    nographic: bool,
) -> Vec<String> {
    let mut argv: Vec<String> = [
        "-machine", &machine.machine,
        "-display", "none",
        "-serial", "mon:stdio",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    argv.push("-chardev".into());
    argv.push(format!("socket,id=uart1,path={UART_SOCK},{uart_chardev_mode}"));
    argv.extend(["-serial", "chardev:uart1"].map(String::from));
    argv.extend(["-smp", &machine.smp, "-m", &machine.ram].map(String::from));
    argv.extend(["-dtb", &dtb.to_string_lossy()].map(String::from));
    argv.extend(["-bios", &opensbi_fw.to_string_lossy()].map(String::from));
    argv.extend(["-kernel", &starryos_bin.to_string_lossy()].map(String::from));
    argv.push("-device".into());
    argv.push(format!("loader,addr={rtasync_base},file={}", app_bin.display()));
    argv.push("-device".into());
    argv.push(format!(
        "loader,addr={rtasync_dtb_base},file={}",
        rtasync_dtb.display()
    ));
    argv.push("-drive".into());
    argv.push(format!("file={},format=raw,if=none,id=hd0", rootfs.display()));
    argv.push("-device".into());
    argv.push("nvme,drive=hd0,serial=tgoskits,max_ioqpairs=64,msix_qsize=65".into());
    if nographic {
        argv.push("-nographic".into());
    }
    argv.insert(0, qemu_bin.to_string_lossy().to_string());
    argv
}

/// 环境启动前的公共准备：断言产物存在 + 编译两侧 DTB。
struct AmpImage {
    app_bin: PathBuf,
    dtb: PathBuf,
    rtasync_dtb: PathBuf,
}

fn prepare_image(root: &Path, profile: &EnvProfile, bin: &RtAsyncBin) -> AmpImage {
    let build = profile.env_build_dir(root);
    let opensbi_fw = build.join("fw_dynamic.bin");
    let app_bin = build.join(bin.out);
    let starryos_bin = build.join(&profile.starry_artifact);
    let _rootfs = crate::util::resolve_rootfs(root);

    assert!(
        opensbi_fw.exists(),
        "Run 'cargo xtask build --env {}'（或 build opensbi）first.",
        profile.name
    );
    assert!(
        app_bin.exists(),
        "Run 'cargo xtask build {}' first.",
        bin.target_name
    );
    if !starryos_bin.exists() {
        eprintln!("Warning: no StarryOS binary ({})", starryos_bin.display());
    }

    let machine = profile.qemu.as_ref().expect("qemu 环境的 env toml 缺 [qemu] 节");
    let dtb = ensure_ap_dtb(root, profile, machine);
    let rtasync_dtb = compile_dtb(root, &build, RTASYNC_DTS, "rt-async.dtb");
    AmpImage { app_bin, dtb, rtasync_dtb }
}

/// rootfs 解析失败时的统一提示（tgoskits 正统准备方式）。
fn rootfs_or_die(root: &Path) -> std::path::PathBuf {
    crate::util::resolve_rootfs(root).unwrap_or_else(|| {
        panic!(
            "rootfs 镜像缺失。准备方式（tgoskits 正统流程）：\n  \
             cd tgoskits && cargo xtask starry rootfs --arch riscv64\n  \
             （legacy 备选：make -C tgoskits/os/StarryOS rootfs）"
        )
    })
}

pub fn run_bin(root: &Path, cfg: &Config, profile: &EnvProfile, bin: &RtAsyncBin) {
    let machine = profile.qemu.as_ref().expect("qemu 环境的 env toml 缺 [qemu] 节");
    let img = prepare_image(root, profile, bin);
    let build = profile.env_build_dir(root);
    let opensbi_fw = build.join("fw_dynamic.bin");
    let starryos_bin = build.join(&profile.starry_artifact);
    let rootfs = rootfs_or_die(root);
    let qemu_bin = root.join("qemu/build/qemu-system-riscv64-unsigned");

    let rtasync_base = cfg.get("RTASYNCBASE");
    let rtasync_dtb_base = cfg.get("RTASYNCDTBBASE");

    let _ = std::fs::remove_file(UART_SOCK);

    eprintln!(
        "Starting QEMU [env={}, machine={}, {} cores, {} RAM, bin={}]...",
        profile.name, machine.machine, machine.smp, machine.ram, bin.name
    );
    eprintln!("  UART0 → stdio (OpenSBI/StarryOS)");
    eprintln!(
        "  UART1 → unix socket {} (rt-async, bidirectional)",
        UART_SOCK
    );
    eprintln!(
        "  rt-async DTB → {rtasync_dtb_base} ({} bytes)",
        img.rtasync_dtb.metadata().map(|m| m.len()).unwrap_or(0)
    );
    eprintln!("  Connect with: socat - UNIX-CONNECT:{}", UART_SOCK);

    let argv = qemu_argv(
        &qemu_bin,
        machine,
        &img.dtb,
        &opensbi_fw,
        &starryos_bin,
        &img.app_bin,
        &img.rtasync_dtb,
        rtasync_base,
        rtasync_dtb_base,
        &rootfs,
        "server=on,wait=off",
        false,
    );

    let st = Command::new(&argv[0])
        .args(&argv[1..])
        .status()
        .expect("qemu not found");

    if !st.success() {
        eprintln!("QEMU exited with {st}");
    }
}

pub fn run_tmux_bin(root: &Path, cfg: &Config, profile: &EnvProfile, bin: &RtAsyncBin) {
    let machine = profile.qemu.as_ref().expect("qemu 环境的 env toml 缺 [qemu] 节");
    let img = prepare_image(root, profile, bin);
    let build = profile.env_build_dir(root);
    let opensbi_fw = build.join("fw_dynamic.bin");
    let starryos_bin = build.join(&profile.starry_artifact);
    let rootfs = rootfs_or_die(root);
    let qemu_bin = root.join("qemu/build/qemu-system-riscv64-unsigned");

    let rtasync_base = cfg.get("RTASYNCBASE");
    let rtasync_dtb_base = cfg.get("RTASYNCDTBBASE");
    let root_str = root.to_string_lossy().to_string();

    let _ = Command::new("tmux")
        .args(["kill-session", "-t", TMUX_SESSION])
        .status();
    let _ = std::fs::remove_file(UART_SOCK);

    // Pane 2 (right): socat listens on the Unix socket so it's ready
    // BEFORE QEMU starts.  QEMU connects as a client, so no data is lost.
    let st = Command::new("tmux")
        .args([
            "new-session", "-d", "-s", TMUX_SESSION, "-c", &root_str,
            "socat", "-", &format!("UNIX-LISTEN:{UART_SOCK},reuseaddr,fork"),
        ])
        .status()
        .expect("tmux not found. Install with: brew install tmux");
    assert!(st.success(), "tmux new-session (socat) failed");

    // Pane 1 (left): QEMU with UART1 connecting to socat's socket.
    let argv = qemu_argv(
        &qemu_bin,
        machine,
        &img.dtb,
        &opensbi_fw,
        &starryos_bin,
        &img.app_bin,
        &img.rtasync_dtb,
        rtasync_base,
        rtasync_dtb_base,
        &rootfs,
        "server=off",
        true,
    );
    let qemu_cmd = argv.join(" ");

    let st = Command::new("tmux")
        .args([
            "split-window", "-h", "-t", TMUX_SESSION, "-c", &root_str,
            "sh", "-c", &qemu_cmd,
        ])
        .status()
        .expect("tmux not found");
    assert!(st.success(), "tmux split-window (QEMU) failed");

    let _ = Command::new("tmux")
        .args(["attach", "-t", TMUX_SESSION])
        .status();
}

pub fn log(root: &Path) {
    let uart_log = root.join("build/rt-async-uart.log");

    std::fs::create_dir_all(root.join("build")).ok();
    if !uart_log.exists() {
        std::fs::write(&uart_log, []).ok();
    }

    let mut child = Command::new("tail")
        .args(["-n", "+1", "-f", &uart_log.to_string_lossy()])
        .stdout(Stdio::piped())
        .spawn()
        .expect("tail not found");

    let stdout = child.stdout.take().unwrap();
    let reader = std::io::BufReader::new(stdout);
    let prefix = "\x1b[36m[rt-async]\x1b[0m";

    for line in reader.lines() {
        match line {
            Ok(l) => println!("{prefix} {l}"),
            Err(_) => break,
        }
    }

    let _ = child.wait();
}
