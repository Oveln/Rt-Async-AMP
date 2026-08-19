use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

/// 解析 QEMU 用的 StarryOS rootfs 镜像路径。
///
/// 优先 tgoskits 正统流程产物（`cd tgoskits && cargo xtask starry rootfs
/// --arch riscv64`，下载解压到 tmp/axbuild/rootfs/ 下）；回退 legacy
/// Makefile 下载位置（os/StarryOS/rootfs-riscv64.img）。两处都无则 None。
pub fn resolve_rootfs(root: &Path) -> Option<PathBuf> {
    let candidates = [
        // tg-xtask 托管目录：rootfs-riscv64-alpine.img 是目录，镜像同名文件在其中
        root.join("tgoskits/tmp/axbuild/rootfs/rootfs-riscv64-alpine.img/rootfs-riscv64-alpine.img"),
        root.join("tgoskits/os/StarryOS/rootfs-riscv64.img"),
    ];
    candidates.into_iter().find(|p| p.exists())
}

/// S 为 AsRef<OsStr>：调用方传 &[&str] 或 &[String]（自定义 target 的
/// --target 参数是运行期拼出的 JSON 绝对路径）。
pub fn run<S: AsRef<OsStr>>(cwd: &Path, program: &str, args: &[S]) {
    let st = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .status()
        .unwrap_or_else(|e| panic!("{program}: {e}"));
    assert!(st.success(), "{program} exited with {st}");
}

pub fn try_run(cwd: &Path, program: &str, args: &[&str]) -> bool {
    Command::new(program)
        .args(args)
        .current_dir(cwd)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}
