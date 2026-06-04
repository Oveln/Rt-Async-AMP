use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use xtask::config::{self as amp_config, Config};

use crate::util;

pub fn rt_async(root: &Path, _cfg: &Config) {
    let target = "riscv64imac-unknown-none-elf";
    util::run(
        &root.join("apps/rt-async-app"),
        "cargo",
        &[
            "build",
            "--target",
            target,
            "--release",
            "-p",
            "rt-async-app",
        ],
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
        &[
            "-O",
            "binary",
            &elf.to_string_lossy(),
            &bin.to_string_lossy(),
        ],
    );
    eprintln!("rt-async → {}", bin.display());
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
    let dir = root.join("StarryOS");
    assert!(
        dir.is_dir(),
        "StarryOS not found. Run 'git submodule update --init StarryOS'."
    );

    let target = "riscv64gc-unknown-none-elf";
    let features = "axfeat/myplat axfeat/bus-pci axfeat/display axfeat/fs-ng-times starry-kernel/input starry-kernel/vsock starry-kernel/dev-log qemu";
    let axconfig = dir.join(".axconfig.toml");

    let plat_config = generate_axconfig(root, cfg);
    let defconfig = dir.join("make/defconfig.toml");
    if !axconfig.exists()
        || fs::metadata(&plat_config).ok().map_or(true, |m| {
            fs::metadata(&axconfig).ok().map_or(true, |a| {
                m.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH)
                    > a.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH)
            })
        })
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
        &[
            "-O",
            "binary",
            &elf.to_string_lossy(),
            &bin.to_string_lossy(),
        ],
    );
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

fn generate_axconfig(root: &Path, cfg: &Config) -> PathBuf {
    let template_path = root.join("modules/axplat-riscv64-qemu-virt/axconfig.toml.in");
    let output_path = root.join("modules/axplat-riscv64-qemu-virt/axconfig.toml");

    let template = std::fs::read_to_string(&template_path)
        .unwrap_or_else(|e| panic!("failed to read {}: {}", template_path.display(), e));

    let mut vars = cfg.template_vars().clone();

    let qemu_ram = amp_config::parse_size(cfg.get("QEMURAM"));
    vars.insert("QEMURAM_HEX".into(), format!("0x{qemu_ram:x}"));

    let starryos_base = u64::from_str_radix(cfg.get("STARRYOSBASE").trim_start_matches("0x"), 16)
        .expect("invalid STARRYOSBASE");
    let phys_virt_offset: u64 = 0xffff_ffc0_0000_0000;
    let kernel_base_vaddr = phys_virt_offset + starryos_base;
    vars.insert(
        "KERNEL_BASE_VADDR".into(),
        format!("0x{kernel_base_vaddr:x}"),
    );

    let rendered = substitute(&template, &vars);
    std::fs::write(&output_path, &rendered)
        .unwrap_or_else(|e| panic!("failed to write {}: {}", output_path.display(), e));

    output_path
}

fn substitute(content: &str, vars: &std::collections::HashMap<String, String>) -> String {
    let mut out = content.to_string();
    for (key, value) in vars {
        out = out.replace(&format!("{{{key}}}"), value);
    }
    out
}
