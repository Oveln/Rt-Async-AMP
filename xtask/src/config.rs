use std::collections::HashMap;
use std::path::Path;

pub struct RepoSource {
    pub repo: String,
    pub commit: String,
}

pub struct Config {
    vars: HashMap<String, String>,
    pub opensbi: RepoSource,
    pub qemu_src: RepoSource,
}

impl Config {
    pub fn load(root: &Path) -> Self {
        let path = root.join("amp.toml");
        let content = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("failed to read {}: {}", path.display(), e));
        let doc: toml::Value = content
            .parse()
            .unwrap_or_else(|e| panic!("failed to parse {}: {}", path.display(), e));
        let table = doc.as_table().expect("amp.toml root must be a table");

        let vars = flatten_toplevel(table);

        let opensbi = {
            let t = table
                .get("opensbi")
                .and_then(|v| v.as_table())
                .expect("missing [opensbi] section in amp.toml");
            RepoSource {
                repo: t["repo"].as_str().expect("opensbi.repo").into(),
                commit: t["commit"].as_str().expect("opensbi.commit").into(),
            }
        };

        let qemu_src = {
            let t = table
                .get("qemu_src")
                .and_then(|v| v.as_table())
                .expect("missing [qemu_src] section in amp.toml");
            RepoSource {
                repo: t["repo"].as_str().expect("qemu_src.repo").into(),
                commit: t["commit"].as_str().expect("qemu_src.commit").into(),
            }
        };

        Self {
            vars,
            opensbi,
            qemu_src,
        }
    }

    pub fn get(&self, key: &str) -> &str {
        self.vars
            .get(key)
            .unwrap_or_else(|| panic!("missing config key: {key}"))
    }

    pub fn template_vars(&self) -> &HashMap<String, String> {
        &self.vars
    }
}

fn flatten_toplevel(table: &toml::map::Map<String, toml::Value>) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for (key, value) in table {
        if value.is_table() {
            continue;
        }
        let s = match value {
            toml::Value::String(s) => s.clone(),
            toml::Value::Integer(i) => i.to_string(),
            other => other.to_string(),
        };
        map.insert(key.clone(), s);
    }
    map
}

pub fn load_amp_toml(ws: &Path) -> HashMap<String, String> {
    let path = ws.join("amp.toml");
    let content = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read {}: {}", path.display(), e));
    let doc: toml::Value = content
        .parse()
        .unwrap_or_else(|e| panic!("failed to parse {}: {}", path.display(), e));
    flatten_toplevel(doc.as_table().expect("amp.toml root must be a table"))
}

pub fn generate_amp_rs(config: &HashMap<String, String>, out_dir: &Path) {
    let mut keys: Vec<&String> = config.keys().collect();
    keys.sort();

    let mut buf = String::from("// Auto-generated from amp.toml. Do not edit.\n\n");
    for key in keys {
        let value = &config[key];
        let const_name = key.to_uppercase();
        if value.starts_with("0x") || value.starts_with("0X") {
            if let Ok(addr) = u64::from_str_radix(&value[2..], 16) {
                buf.push_str(&format!("pub const {const_name}: usize = 0x{addr:x};\n"));
            }
        } else if let Ok(int) = value.parse::<u64>() {
            buf.push_str(&format!("pub const {const_name}: usize = {int};\n"));
        } else {
            buf.push_str(&format!("pub const {const_name}: &str = \"{value}\";\n"));
        }
    }
    let out_path = out_dir.join("amp_gen.rs");
    std::fs::write(&out_path, &buf)
        .unwrap_or_else(|e| panic!("failed to write {}: {}", out_path.display(), e));
}

pub fn parse_size(s: &str) -> u64 {
    let s = s.trim();
    if s.is_empty() {
        panic!("empty size string");
    }
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        return u64::from_str_radix(hex, 16).expect("invalid hex size");
    }
    let (num, unit) = s.split_at(s.len() - 1);
    let n: u64 = num.parse().expect("invalid size number");
    match unit {
        "K" | "k" => n * 1024,
        "M" | "m" => n * 1024 * 1024,
        "G" | "g" => n * 1024 * 1024 * 1024,
        _ => panic!("unknown size unit: {unit}"),
    }
}

pub fn workspace_dir_from_manifest() -> std::path::PathBuf {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let mut dir = std::path::PathBuf::from(manifest_dir);
    loop {
        if dir.join("amp.toml").exists() {
            return dir;
        }
        dir = dir
            .parent()
            .expect("reached filesystem root without finding amp.toml")
            .to_path_buf();
    }
}
