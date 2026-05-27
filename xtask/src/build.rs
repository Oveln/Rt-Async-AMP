use std::fs;
use std::path::Path;
use std::process::Command;

use crate::config::Config;
use crate::util;

pub fn rt_async(root: &Path, _cfg: &Config) {
    let target = "riscv64imac-unknown-none-elf";
    util::run(
        &root.join("apps/rt-async-app"),
        "cargo",
        &["build", "--target", target, "--release", "-p", "rt-async-app"],
    );

    let build_dir = root.join("build");
    fs::create_dir_all(&build_dir).unwrap();

    let elf = root
        .join("target")
        .join(target)
        .join("release")
        .join("demo");
    let bin = build_dir.join("rt-async.bin");
    util::run(
        root,
        "riscv64-elf-objcopy",
        &["-O", "binary", &elf.to_string_lossy(), &bin.to_string_lossy()],
    );
    eprintln!("rt-async → {}", bin.display());
}

pub fn opensbi(root: &Path, _cfg: &Config) {
    let dir = root.join("opensbi");
    assert!(
        dir.join(".patched").exists(),
        "opensbi not ready. Run 'cargo xtask setup' first."
    );

    let nproc = std::thread::available_parallelism()
        .map(|n| n.to_string())
        .unwrap_or_else(|_| "4".into());

    util::run(
        &dir,
        "make",
        &[
            &format!("-j{nproc}"),
            "PLATFORM=generic",
            "CROSS_COMPILE=riscv64-elf-",
            "O=build",
            "FW_TEXT_START=0x80000000",
        ],
    );

    let build_dir = root.join("build");
    fs::create_dir_all(&build_dir).unwrap();

    let src = dir.join("build/platform/generic/firmware/fw_dynamic.bin");
    let dst = build_dir.join("fw_dynamic.bin");
    fs::copy(&src, &dst).unwrap();
    eprintln!("OpenSBI → {}", dst.display());
}

pub fn starryos(root: &Path, _cfg: &Config) {
    let dir = root.join("StarryOS");
    assert!(
        dir.is_dir(),
        "StarryOS not found. Run 'git submodule update --init StarryOS'."
    );

    let target = "riscv64gc-unknown-none-elf";
    let features = "axfeat/myplat axfeat/bus-pci axfeat/display axfeat/fs-ng-times starry-kernel/input starry-kernel/vsock starry-kernel/dev-log qemu";
    let axconfig = dir.join(".axconfig.toml");

    let plat_config = root
        .join("modules/axplat-riscv64-qemu-virt/axconfig.toml");
    let defconfig = dir.join("make/defconfig.toml");
    if !axconfig.exists()
        || fs::metadata(&plat_config)
            .ok()
            .map_or(true, |m| fs::metadata(&axconfig).ok().map_or(true, |a| m.modified().unwrap() > a.modified().unwrap()))
    {
        util::run(
            &dir,
            "axconfig-gen",
            &[
                &defconfig.to_string_lossy(),
                &plat_config.to_string_lossy(),
                "-w",
                "arch=\"riscv64\"",
                "-w",
                "platform=\"riscv64-qemu-virt\"",
                "-o",
                &axconfig.to_string_lossy(),
            ],
        );
    }

    let rustflags = format!(
        "-C link-arg=-Ttarget/{target}/release/linker_riscv64-qemu-virt.lds -C link-arg=-no-pie -C link-arg=-znostart-stop-gc"
    );

    let toolchain = read_toolchain(&dir.join("rust-toolchain.toml"));

    eprintln!(
        "cd {} && AX_CONFIG_PATH={} RUSTFLAGS='{}' RUSTUP_TOOLCHAIN={toolchain} cargo build -Z unstable-options --target {target} --target-dir target --release --features '{features}'",
        dir.display(),
        axconfig.display(),
        rustflags,
    );

    let st = Command::new("cargo")
        .args([
            "build",
            "-Z",
            "unstable-options",
            "--target",
            target,
            "--target-dir",
            "target",
            "--release",
            "--features",
            features,
        ])
        .env("AX_CONFIG_PATH", axconfig.to_string_lossy().to_string())
        .env("RUSTFLAGS", &rustflags)
        .env("RUSTUP_TOOLCHAIN", &toolchain)
        .current_dir(&dir)
        .status()
        .expect("cargo not found");
    assert!(st.success(), "StarryOS build failed");

    let build_dir = root.join("build");
    fs::create_dir_all(&build_dir).unwrap();

    let elf = dir
        .join("target")
        .join(target)
        .join("release")
        .join("starryos");
    let bin = build_dir.join("starryos.bin");
    util::run(
        root,
        "riscv64-elf-objcopy",
        &["-O", "binary", &elf.to_string_lossy(), &bin.to_string_lossy()],
    );
    eprintln!("StarryOS → {}", bin.display());
}

pub fn user_test(root: &Path, _cfg: &Config) {
    let target = "riscv64gc-unknown-linux-musl";
    util::run(
        &root.join("user-apps/user-test-ipc"),
        "cargo",
        &["build", "--target", target, "--release"],
    );

    let build_dir = root.join("build");
    fs::create_dir_all(&build_dir).unwrap();

    let src = root
        .join("user-apps/user-test-ipc/target")
        .join(target)
        .join("release")
        .join("user-test-ipc");
    let dst = build_dir.join("user-test-ipc");
    fs::copy(&src, &dst).unwrap();
    eprintln!("user-test-ipc → {}", dst.display());
}

pub fn user_test_rpc(root: &Path, _cfg: &Config) {
    let target = "riscv64gc-unknown-linux-musl";
    util::run(
        &root.join("user-apps/user-test-rpc"),
        "cargo",
        &["build", "--target", target, "--release"],
    );

    let build_dir = root.join("build");
    fs::create_dir_all(&build_dir).unwrap();

    let src = root
        .join("target")
        .join(target)
        .join("release")
        .join("user-test-rpc");
    let dst = build_dir.join("user-test-rpc");
    fs::copy(&src, &dst).unwrap();
    eprintln!("user-test-rpc → {}", dst.display());
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

fn read_toolchain(path: &Path) -> String {
    let content = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("failed to read {}: {}", path.display(), e));
    let doc: toml::Value = content
        .parse()
        .unwrap_or_else(|e| panic!("failed to parse {}: {}", path.display(), e));
    doc.get("toolchain")
        .and_then(|t| t.get("channel"))
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("no toolchain.channel found in {}", path.display()))
        .to_string()
}
