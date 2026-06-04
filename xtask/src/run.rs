use std::io::BufRead;
use std::path::Path;
use std::process::{Command, Stdio};

use xtask::config::Config;

const TMUX_SESSION: &str = "rt-async-amp";

pub fn run(root: &Path, cfg: &Config) {
    let build = root.join("build");
    let opensbi_fw = build.join("fw_dynamic.bin");
    let app_bin = build.join("rt-async.bin");
    let starryos_bin = build.join("starryos.bin");
    let uart_log = build.join("rt-async-uart.log");
    let qemu_bin = root.join("qemu/build/qemu-system-riscv64-unsigned");
    let rootfs = root.join("StarryOS/rootfs-riscv64.img");

    assert!(
        opensbi_fw.exists(),
        "Run 'cargo xtask build opensbi' first."
    );
    assert!(app_bin.exists(), "Run 'cargo xtask build rt-async' first.");
    if !starryos_bin.exists() {
        eprintln!("Warning: no StarryOS binary.");
    }

    let rtasync_base = cfg.get("RTASYNCBASE");
    let smp = cfg.get("QEMUSMP");
    let ram = cfg.get("QEMURAM");

    eprintln!("Starting QEMU ({smp} cores, {ram} RAM)...");
    eprintln!("  UART0 → stdio (OpenSBI/StarryOS)");
    eprintln!("  UART1 → {} (rt-async)", uart_log.display());

    let st = Command::new(&qemu_bin)
        .args([
            "-machine",
            "virt",
            "-display",
            "none",
            "-serial",
            "mon:stdio",
            "-serial",
            &format!("file:{}", uart_log.display()),
            "-smp",
            smp,
            "-m",
            ram,
            "-bios",
            &opensbi_fw.to_string_lossy(),
            "-kernel",
            &starryos_bin.to_string_lossy(),
            "-device",
            &format!("loader,addr={rtasync_base},file={}", app_bin.display()),
            "-drive",
            &format!("file={},format=raw,if=none,id=hd0", rootfs.display()),
            "-device",
            "virtio-blk-pci,drive=hd0",
        ])
        .status()
        .expect("qemu not found");

    if !st.success() {
        eprintln!("QEMU exited with {st}");
    }
}

pub fn run_tmux(root: &Path, _cfg: &Config) {
    let _ = Command::new("tmux")
        .args(["kill-session", "-t", TMUX_SESSION])
        .status();

    let root_str = root.to_string_lossy().to_string();

    let st = Command::new("tmux")
        .args(["new-session", "-d", "-s", TMUX_SESSION, "-c", &root_str])
        .args(["cargo", "xtask", "run"])
        .status()
        .expect("tmux not found. Install with: brew install tmux");
    assert!(st.success(), "tmux new-session failed");

    let st = Command::new("tmux")
        .args(["split-window", "-h", "-t", TMUX_SESSION, "-c", &root_str])
        .args(["cargo", "xtask", "log"])
        .status()
        .expect("tmux not found");
    assert!(st.success(), "tmux split-window failed");

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
