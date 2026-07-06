use std::path::{Path, PathBuf};
use std::process::Command;

use crate::util;

pub fn run(root: &Path, file: &str, dst: &str) {
    let file_path = if Path::new(file).is_absolute() {
        PathBuf::from(file)
    } else {
        root.join(file)
    };
    let rootfs = root.join("tgoskits/os/StarryOS/rootfs-riscv64.img");

    assert!(
        file_path.exists(),
        "File not found: {}",
        file_path.display()
    );
    assert!(rootfs.exists(), "Rootfs not found: {}", rootfs.display());

    if is_file_in_use(&rootfs) {
        eprintln!(
            "Error: {} is in use (QEMU may be running). Stop QEMU first.",
            rootfs.display()
        );
        std::process::exit(1);
    }

    let rootfs_str = rootfs.to_string_lossy();
    let debugfs = find_debugfs();
    let _ = util::try_run(
        root,
        &debugfs,
        &["-w", "-R", &format!("rm {dst}"), &rootfs_str],
    );

    util::run(
        root,
        &debugfs,
        &[
            "-w",
            "-R",
            &format!("write {} {dst}", file_path.display()),
            &rootfs_str,
        ],
    );
    eprintln!(
        "Installed {} → {dst} in {}",
        file_path.display(),
        rootfs.display()
    );
}

fn is_file_in_use(path: &Path) -> bool {
    // Try fuser first (reliable on both Linux and macOS), fall back to lsof
    let fuser = Command::new("fuser")
        .arg("-s")
        .arg(path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    if let Ok(s) = fuser {
        return s.success();
    }
    Command::new("lsof")
        .arg(path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn find_debugfs() -> &'static str {
    let candidates = [
        "debugfs",
        "/usr/sbin/debugfs",
        "/opt/homebrew/opt/e2fsprogs/sbin/debugfs",
        "/usr/local/opt/e2fsprogs/sbin/debugfs",
    ];
    for path in &candidates {
        if Command::new(path)
            .arg("-V")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
        {
            return path;
        }
    }
    panic!(
        "debugfs not found. Install e2fsprogs:\n  \
         macOS:  brew install e2fsprogs\n  \
         Ubuntu: sudo apt install e2fsprogs\n  \
         Arch:   sudo pacman -S e2fsprogs\n  \
         Tried: {}",
        candidates.join(", ")
    );
}
