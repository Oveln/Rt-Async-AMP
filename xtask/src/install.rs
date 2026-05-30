use std::path::{Path, PathBuf};
use std::process::Command;

use crate::util;

const DEBUGFS: &str = "/opt/homebrew/opt/e2fsprogs/sbin/debugfs";

pub fn run(root: &Path, file: &str, dst: &str) {
    let file_path = if Path::new(file).is_absolute() {
        PathBuf::from(file)
    } else {
        root.join(file)
    };
    let rootfs = root.join("StarryOS/rootfs-riscv64.img");

    assert!(file_path.exists(), "File not found: {}", file_path.display());
    assert!(rootfs.exists(), "Rootfs not found: {}", rootfs.display());

    let _ = Command::new("pkill")
        .args(["-9", "qemu-system-riscv64"])
        .status();
    std::thread::sleep(std::time::Duration::from_millis(500));

    let rootfs_str = rootfs.to_string_lossy();
    let _ = util::try_run(
        root,
        DEBUGFS,
        &["-w", "-R", &format!("rm {dst}"), &rootfs_str],
    );

    util::run(
        root,
        DEBUGFS,
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
