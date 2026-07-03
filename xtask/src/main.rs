use std::path::{Path, PathBuf};
use std::{env, fs};

use clap::{ArgGroup, CommandFactory, Parser, Subcommand};
use clap_complete::{Shell, generate};

mod build;
mod install;
mod run;
mod setup;
mod util;

fn project_root() -> PathBuf {
    Path::new(&env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(1)
        .unwrap()
        .to_path_buf()
}

#[derive(Parser)]
#[command(
    name = "xtask",
    about = "rt-async-amp build orchestrator",
    after_help = "Tip: use 'cargo xtask <cmd> --help' for details on each subcommand."
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    #[command(about = "Clone & patch upstream repos (OpenSBI, QEMU)")]
    Setup,
    #[command(about = "Build one or all targets")]
    Build {
        #[arg(default_value = "all", help = "Target to build", value_name = "TARGET")]
        target: String,
    },
    #[command(about = "Launch QEMU with dual-core AMP image")]
    Run {
        #[arg(long, help = "Run inside a tmux split (left: QEMU, right: UART1)")]
        tmux: bool,
        #[arg(long, default_value = "demo", help = "rt-async binary to load")]
        bin: String,
    },
    #[command(about = "Tail the rt-async UART1 log with colored prefix")]
    Log,
    #[command(about = "Install a file into the StarryOS rootfs")]
    #[command(group = ArgGroup::new("install_target").args(["file", "all"]).required(true))]
    Install {
        #[arg(help = "Path to the file to install (e.g. build/user-test-ipc)")]
        file: Option<String>,
        #[arg(
            short,
            long,
            help = "Destination path inside rootfs (default: /<filename>)"
        )]
        dst: Option<String>,
        #[arg(long, help = "Install all user-apps")]
        all: bool,
    },
    #[command(about = "Remove build artifacts")]
    Clean {
        #[arg(long, help = "Also remove cloned opensbi/ and qemu/ directories")]
        dist: bool,
    },
    #[command(about = "Build the patched QEMU from source")]
    Qemu,
    #[command(about = "Generate shell tab-completion scripts")]
    Completions {
        #[arg(value_enum, help = "Target shell")]
        shell: Shell,
        #[arg(short, long, help = "Write to file instead of stdout")]
        output: Option<String>,
    },
}

fn main() {
    let cli = Cli::parse();
    let root = project_root();
    let cfg = xtask::config::Config::load(&root);

    match cli.cmd {
        Cmd::Setup => setup::run(&root, &cfg),
        Cmd::Build { target } => {
            match target.as_str() {
                "all" => {
                    build::opensbi(&root, &cfg);
                    build::starryos(&root, &cfg);
                    for bin in build::RTASYNC_BINS {
                        build::build_rt_async(&root, bin);
                    }
                    build::user_test(&root, &cfg);
                    build::user_test_rpc(&root, &cfg);
                    build::user_test_sched(&root, &cfg);
                    eprintln!("Build complete. Run 'cargo xtask run' to start QEMU.");
                }
                "opensbi" => build::opensbi(&root, &cfg),
                "starryos" => build::starryos(&root, &cfg),
                "user-test-ipc" => build::user_test(&root, &cfg),
                "user-test-rpc" => build::user_test_rpc(&root, &cfg),
                "user-test-sched" => build::user_test_sched(&root, &cfg),
                // 平台聚合：构建该平台所有 rt-async bin
                "qemu" => {
                    for bin in build::RTASYNC_BINS
                        .iter()
                        .filter(|b| b.platform == "qemu")
                    {
                        build::build_rt_async(&root, bin);
                    }
                }
                "k3" => {
                    for bin in build::RTASYNC_BINS.iter().filter(|b| b.platform == "k3") {
                        build::build_rt_async(&root, bin);
                    }
                }
                // 单个 rt-async bin：按 target_name（带平台前缀，如 qemu-demo/k3-minimal）
                name => {
                    let bin = build::find_by_target(name).unwrap_or_else(|| {
                        eprintln!("unknown target: {name}");
                        eprintln!("\nrt-async bins (target name):");
                        for b in build::RTASYNC_BINS {
                            eprintln!("  {}", b.target_name);
                        }
                        eprintln!("\nplatform aggregates: qemu, k3");
                        eprintln!("\nother targets: all, opensbi, starryos, user-test-ipc, user-test-rpc, user-test-sched");
                        std::process::exit(1);
                    });
                    build::build_rt_async(&root, bin);
                }
            }
        }
        Cmd::Run { tmux, bin } => {
            // run 仅服务 QEMU；--bin 用 cargo 短名（demo/console/...），不带平台前缀
            let bin_def = build::find_by_name(&bin).unwrap_or_else(|| {
                eprintln!("unknown rt-async bin: {bin}");
                eprintln!("\navailable bins (run uses short name):");
                for b in build::RTASYNC_BINS.iter().filter(|b| b.platform == "qemu") {
                    eprintln!("  {}", b.name);
                }
                std::process::exit(1);
            });
            if tmux {
                run::run_tmux_bin(&root, &cfg, bin_def);
            } else {
                run::run_bin(&root, &cfg, bin_def);
            }
        }
        Cmd::Log => run::log(&root),
        Cmd::Install { file, dst, all } => {
            if all {
                for name in ["user-test-ipc", "user-test-rpc", "user-test-sched"] {
                    let src = root.join("build").join(name);
                    if src.exists() {
                        install::run(&root, &src.to_string_lossy(), &format!("/{name}"));
                    } else {
                        eprintln!(
                            "{} not found in build/. Run 'cargo xtask build' first.",
                            name
                        );
                    }
                }
            } else {
                let file = match file {
                    Some(f) => f,
                    None => {
                        eprintln!("error: FILE is required when --all is not set");
                        std::process::exit(1);
                    }
                };
                let dst = dst.unwrap_or_else(|| {
                    format!(
                        "/{}",
                        Path::new(&file).file_name().unwrap().to_string_lossy()
                    )
                });
                install::run(&root, &file, &dst);
            }
        }
        Cmd::Clean { dist } => {
            let build_dir = root.join("build");
            if build_dir.exists() {
                fs::remove_dir_all(&build_dir).unwrap();
            }
            util::run(&root, "cargo", &["clean"]);
            if dist {
                for name in ["opensbi", "qemu"] {
                    let d = root.join(name);
                    if d.exists() {
                        fs::remove_dir_all(&d).unwrap();
                    }
                }
                eprintln!("Removed opensbi/ and qemu/. Run 'cargo xtask setup' to re-clone.");
            }
        }
        Cmd::Qemu => build::qemu(&root, &cfg),
        Cmd::Completions { shell, output } => {
            let mut cmd = Cli::command();
            let name = "xtask".to_string();
            match output {
                Some(path) => {
                    let mut f = fs::File::create(&path)
                        .unwrap_or_else(|e| panic!("failed to create {path}: {e}"));
                    generate(shell, &mut cmd, &name, &mut f);
                    eprintln!("Completions written to {path}");
                }
                None => {
                    generate(shell, &mut cmd, &name, &mut std::io::stdout());
                }
            }
        }
    }
}
