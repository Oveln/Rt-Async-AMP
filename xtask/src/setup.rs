use std::collections::HashMap;
use std::fs;
use std::path::Path;

use crate::config::Config;
use crate::util;

pub fn run(root: &Path, cfg: &Config) {
    clone_and_patch(
        root,
        "opensbi",
        &cfg.opensbi,
        "opensbi-amp.patch",
        cfg.template_vars(),
    );
    clone_and_patch(
        root,
        "qemu",
        &cfg.qemu_src,
        "qemu-uart1.patch",
        cfg.template_vars(),
    );
    eprintln!("Setup complete.");
}

fn clone_and_patch(
    root: &Path,
    dir_name: &str,
    source: &crate::config::RepoSource,
    patch_file: &str,
    vars: &HashMap<String, String>,
) {
    let dir = root.join(dir_name);
    let stamp = dir.join(".patched");

    if stamp.exists() {
        eprintln!("{dir_name} already patched, skipping.");
        return;
    }

    util::run(
        root,
        "git",
        &["clone", "--filter=blob:none", &source.repo, dir_name],
    );
    util::run(
        root,
        "git",
        &["-C", dir_name, "checkout", "-f", &source.commit],
    );

    let patch_path = root.join("patches").join(patch_file);
    let raw = fs::read_to_string(&patch_path)
        .unwrap_or_else(|e| panic!("read {}: {}", patch_path.display(), e));
    let rendered = substitute(&raw, vars);

    let tmp = dir.join(".xtask-patch");
    fs::write(&tmp, rendered).unwrap();

    util::run(
        root,
        "git",
        &[
            "-C",
            dir_name,
            "apply",
            "--whitespace=nowarn",
            ".xtask-patch",
        ],
    );

    let _ = fs::remove_file(&tmp);
    fs::write(stamp, "").unwrap();
    eprintln!("{dir_name}: cloned, templated, patched.");
}

fn substitute(content: &str, vars: &HashMap<String, String>) -> String {
    let mut out = content.to_string();
    for (key, value) in vars {
        out = out.replace(&format!("{{{key}}}"), value);
    }
    out
}
