use std::io::BufRead;
use std::path::Path;
use std::process::{Command, Stdio};

use xtask::config::Config;

use crate::build::RtAsyncBin;

const TMUX_SESSION: &str = "rt-async-amp";
const UART_SOCK: &str = "/tmp/rt-async-uart.sock";

/// Path to the DTS source and the compiled DTB inside build/.
const QEMU_DTS: &str = "its/qemu-virt-amp.dts";
const QEMU_DTB: &str = "qemu-virt-amp.dtb";

/// Compile the QEMU AMP device-tree source to DTB (uses `dtc`).
fn ensure_dtb(root: &Path) -> std::path::PathBuf {
    let dts = root.join(QEMU_DTS);
    let dtb = root.join("build").join(QEMU_DTB);

    let dts_mtime = std::fs::metadata(&dts).ok().and_then(|m| m.modified().ok());
    let dtb_mtime = std::fs::metadata(&dtb).ok().and_then(|m| m.modified().ok());

    let stale = match (dts_mtime, dtb_mtime) {
        (_, None) => true,
        (Some(dts_ts), Some(dtb_ts)) => dts_ts > dtb_ts,
        (None, Some(_)) => false,
    };

    if stale {
        eprintln!("DTB: compiling {} -> {}", dts.display(), dtb.display());
        std::fs::create_dir_all(root.join("build")).unwrap();
        let out = Command::new("dtc")
            .args(["-I", "dts", "-O", "dtb", "-o", &dtb.to_string_lossy(), &dts.to_string_lossy()])
            .output()
            .expect("dtc not found. Install device-tree-compiler (dtc) via your system package manager");
        if !out.status.success() {
            eprintln!("dtc failed:\n{}", String::from_utf8_lossy(&out.stderr));
            panic!("DTB compilation failed");
        }
    }
    dtb
}

pub fn run_bin(root: &Path, cfg: &Config, bin: &RtAsyncBin) {
    let build = root.join("build");
    let opensbi_fw = build.join("fw_dynamic.bin");
    let app_bin = build.join(bin.out);
    let starryos_bin = build.join("starryos.bin");
    let qemu_bin = root.join("qemu/build/qemu-system-riscv64-unsigned");
    let rootfs = root.join("tgoskits/os/StarryOS/rootfs-riscv64.img");

    assert!(
        opensbi_fw.exists(),
        "Run 'cargo xtask build opensbi' first."
    );
    assert!(
        app_bin.exists(),
        "Run 'cargo xtask build {}' first.",
        bin.name
    );
    if !starryos_bin.exists() {
        eprintln!("Warning: no StarryOS binary.");
    }

    let rtasync_base = cfg.get("RTASYNCBASE");
    let smp = cfg.get("QEMUSMP");
    let ram = cfg.get("QEMURAM");
    let dtb = ensure_dtb(root);

    let _ = std::fs::remove_file(UART_SOCK);

    eprintln!("Starting QEMU ({smp} cores, {ram} RAM) [bin={}]...", bin.name);
    eprintln!("  UART0 → stdio (OpenSBI/StarryOS)");
    eprintln!(
        "  UART1 → unix socket {} (rt-async, bidirectional)",
        UART_SOCK
    );
    eprintln!("  Connect with: socat - UNIX-CONNECT:{}", UART_SOCK);

    let st = Command::new(&qemu_bin)
        .args([
            "-machine",
            "virt",
            "-display",
            "none",
            "-serial",
            "mon:stdio",
            "-chardev",
            &format!("socket,id=uart1,path={UART_SOCK},server=on,wait=off"),
            "-serial",
            "chardev:uart1",
            "-smp",
            smp,
            "-m",
            ram,
            "-dtb",
            &dtb.to_string_lossy(),
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

pub fn run_tmux_bin(root: &Path, cfg: &Config, bin: &RtAsyncBin) {
    let _ = Command::new("tmux")
        .args(["kill-session", "-t", TMUX_SESSION])
        .status();

    let _ = std::fs::remove_file(UART_SOCK);

    let build = root.join("build");
    let opensbi_fw = build.join("fw_dynamic.bin");
    let app_bin = build.join(bin.out);
    let starryos_bin = build.join("starryos.bin");
    let qemu_bin = root.join("qemu/build/qemu-system-riscv64-unsigned");
    let rootfs = root.join("tgoskits/os/StarryOS/rootfs-riscv64.img");

    assert!(opensbi_fw.exists(), "Run 'cargo xtask build opensbi' first.");
    assert!(app_bin.exists(), "Run 'cargo xtask build {}' first.", bin.name);
    if !starryos_bin.exists() {
        eprintln!("Warning: no StarryOS binary.");
    }

    let rtasync_base = cfg.get("RTASYNCBASE");
    let smp = cfg.get("QEMUSMP");
    let ram = cfg.get("QEMURAM");
    let dtb = ensure_dtb(root);
    let root_str = root.to_string_lossy().to_string();

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
    let qemu_cmd = format!(
        "{} -machine virt -display none \
         -serial mon:stdio \
         -chardev socket,id=uart1,path={},server=off \
         -serial chardev:uart1 \
         -smp {} -m {} \
         -dtb {} \
         -bios {} -kernel {} \
         -device loader,addr={},file={} \
         -drive file={},format=raw,if=none,id=hd0 \
         -device virtio-blk-pci,drive=hd0 \
         -nographic",
        qemu_bin.display(),
        UART_SOCK, smp, ram,
        dtb.display(),
        opensbi_fw.display(), starryos_bin.display(),
        rtasync_base, app_bin.display(),
        rootfs.display(),
    );

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
