use std::path::Path;
use std::process::Command;

pub fn run(cwd: &Path, program: &str, args: &[&str]) {
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
