use std::path::{Path, PathBuf};
use std::{env, fs};

use clap::{ArgGroup, CommandFactory, Parser, Subcommand, ValueEnum};
use clap_complete::{generate, Shell};

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
        #[arg(
            default_value = "all",
            help = "Target to build",
            value_name = "TARGET"
        )]
        target: BuildTarget,
    },
    #[command(about = "Launch QEMU with dual-core AMP image")]
    Run {
        #[arg(long, help = "Run inside a tmux split (left: QEMU, right: UART1 log)")]
        tmux: bool,
    },
    #[command(about = "Tail the rt-async UART1 log with colored prefix")]
    Log,
    #[command(about = "Install a file into the StarryOS rootfs")]
    #[command(group = ArgGroup::new("install_target").args(["file", "all"]).required(true))]
    Install {
        #[arg(help = "Path to the file to install (e.g. build/user-test-ipc)")]
        file: Option<String>,
        #[arg(short, long, help = "Destination path inside rootfs (default: /<filename>)")]
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

#[derive(Clone, ValueEnum)]
enum BuildTarget {
    All,
    RtAsync,
    Opensbi,
    Starryos,
    #[value(name = "user-test-ipc")]
    UserTest,
    #[value(name = "user-test-rpc")]
    UserTestRpc,
    #[value(name = "user-test-sched")]
    UserTestSched,
}

fn main() {
    let cli = Cli::parse();
    let root = project_root();
    let cfg = xtask::config::Config::load(&root);

    match cli.cmd {
        Cmd::Setup => setup::run(&root, &cfg),
        Cmd::Build { target } => match target {
            BuildTarget::All => {
                build::rt_async(&root, &cfg);
                build::opensbi(&root, &cfg);
                build::starryos(&root, &cfg);
                build::user_test(&root, &cfg);
                build::user_test_rpc(&root, &cfg);
                build::user_test_sched(&root, &cfg);
                eprintln!("Build complete. Run 'cargo xtask run' to start QEMU.");
            }
            BuildTarget::RtAsync => build::rt_async(&root, &cfg),
            BuildTarget::Opensbi => build::opensbi(&root, &cfg),
            BuildTarget::Starryos => build::starryos(&root, &cfg),
            BuildTarget::UserTest => build::user_test(&root, &cfg),
            BuildTarget::UserTestRpc => build::user_test_rpc(&root, &cfg),
            BuildTarget::UserTestSched => build::user_test_sched(&root, &cfg),
        },
        Cmd::Run { tmux } => {
            if tmux {
                run::run_tmux(&root, &cfg);
            } else {
                run::run(&root, &cfg);
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
                        eprintln!("{} not found in build/. Run 'cargo xtask build' first.", name);
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
                        Path::new(&file)
                            .file_name()
                            .unwrap()
                            .to_string_lossy()
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
