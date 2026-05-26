use std::collections::HashMap;
use std::path::Path;

fn load_amp_toml(ws: &Path) -> HashMap<String, String> {
    let path = ws.join("amp.toml");
    let content = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read {}: {}", path.display(), e));

    let doc: toml::Value = content
        .parse()
        .unwrap_or_else(|e| panic!("failed to parse {}: {}", path.display(), e));
    let table = doc.as_table().expect("amp.toml root must be a table");

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

fn generate_rs(config: &HashMap<String, String>, out_dir: &Path) {
    let mut buf = String::from("// Auto-generated from amp.toml. Do not edit.\n\n");
    for (key, value) in config {
        let const_name = key.to_uppercase();
        if let Ok(addr) = u64::from_str_radix(value.trim_start_matches("0x"), 16) {
            buf.push_str(&format!("pub const {const_name}: usize = 0x{addr:x};\n"));
        } else if let Ok(int) = value.parse::<u64>() {
            buf.push_str(&format!("pub const {const_name}: usize = {int};\n"));
        } else {
            buf.push_str(&format!("pub const {const_name}: &str = \"{value}\";\n"));
        }
    }
    let out_path = out_dir.join("amp_gen.rs");
    std::fs::write(&out_path, &buf)
        .unwrap_or_else(|e| panic!("failed to write {}: {}", out_path.display(), e));
    println!("cargo:rerun-if-changed=amp.toml");
}

fn main() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let ws = Path::new(&manifest_dir).join("../..");
    let config = load_amp_toml(&ws);
    let out_dir_str = std::env::var("OUT_DIR").unwrap();
    let out_dir = Path::new(&out_dir_str);
    generate_rs(&config, out_dir);
}
