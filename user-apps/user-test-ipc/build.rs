use std::path::Path;

fn main() {
    let ws = xtask::config::workspace_dir_from_manifest();
    let config = xtask::config::load_amp_toml(&ws);
    let out_dir_str = std::env::var("OUT_DIR").unwrap();
    let out_dir = Path::new(&out_dir_str);
    xtask::config::generate_amp_rs(&config, out_dir);
    println!("cargo:rerun-if-changed={}/amp.toml", ws.display());
}
