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

        let mut vars = HashMap::new();
        for (key, value) in table {
            if value.is_table() {
                continue;
            }
            vars.insert(
                key.clone(),
                match value {
                    toml::Value::String(s) => s.clone(),
                    toml::Value::Integer(i) => i.to_string(),
                    other => other.to_string(),
                },
            );
        }

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
