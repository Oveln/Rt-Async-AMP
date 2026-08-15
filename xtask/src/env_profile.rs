//! 环境 profile（envs/&lt;name&gt;.toml）。
//!
//! 一个"环境"= 一套可复现的运行组合，是一等公民：
//!   qemu-plic   QEMU virt 双核 AMP（PLIC）
//!   qemu-aia    QEMU virt 双核 AMP（APLIC+IMSIC，K3 中断架构的仿真对应物）
//!   k3-com260   K3 COM260 真板（X100@StarryOS + RT24 rcpu1@rt-async）
//!
//! profile 声明：StarryOS 板级配置（tgoskits 内路径）、QEMU 机器参数、
//! AP 侧 DTB 来源、k3 的 itb 打包 bin。amp.toml 仍管地址布局与上游 pin，
//! 环境差异全部收敛到本目录。

use std::path::{Path, PathBuf};

/// QEMU 机器参数（仅 qemu 环境有）。
pub struct QemuMachine {
    /// `-machine` 参数整体，如 "virt" / "virt,aia=aplic-imsic"。
    pub machine: String,
    pub smp: String,
    pub ram: String,
}

/// AP 侧 DTB 来源。
pub enum DtbSource {
    /// 从 its/ 手写 dts 编译（cc -E → dtc），如 its/qemu-virt-amp.dts。
    Dts(String),
    /// 从本环境 QEMU 机器 dumpdtb 导出基线，再 fdtoverlay 叠加共享窗
    /// overlay。中断拓扑（PLIC 还是 IMSIC/APLIC）自动跟随机器参数，
    /// 无需手写 imsic 节点。
    DumpdtbOverlay { overlay: String },
}

pub struct EnvProfile {
    pub name: String,
    /// "qemu" / "k3"，用于过滤 RTASYNC_BINS 与选择默认环境。
    pub platform: String,
    pub desc: String,
    /// 是否为该平台的默认环境（单 bin 构建的产物落 build/<默认环境>/）。
    pub is_default: bool,
    /// StarryOS 板级配置（tgoskits 内相对路径）。
    pub starry_config: String,
    /// tg-xtask --smp 覆盖（qemu AMP 必须传 1：hart1 归 rt-async）。
    pub starry_smp: Option<u32>,
    /// tg-xtask 期望产物名（qemu：starryos.bin；k3：starryos.uimg）。
    pub starry_artifact: String,
    pub qemu: Option<QemuMachine>,
    pub dtb: Option<DtbSource>,
    /// RP 侧 dts（缺省 its/rt-async-qemu-virt-amp.dts）。
    pub rp_dts: Option<String>,
    /// RP dts 额外 cpp 宏（如 qemu-aia 的 OV_NO_PLIC：裁掉 plic 节点）。
    pub rp_defines: Vec<String>,
    /// k3：打包进 esos.itb rcpu1-fw 节点的默认 bin（RTASYNC_BINS target_name）。
    pub pack_bin: Option<String>,
}

impl EnvProfile {
    /// 该环境的产物目录：build/<env-name>/。
    pub fn env_build_dir(&self, root: &Path) -> PathBuf {
        root.join("build").join(&self.name)
    }
}

/// 扫描 envs/*.toml 加载全部环境。
pub fn load_all(root: &Path) -> Vec<EnvProfile> {
    let dir = root.join("envs");
    let mut out = Vec::new();
    let entries = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("failed to read {}: {e} (envs/ 目录缺失)", dir.display()));
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string();
        out.push(parse(&name, &path));
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    assert!(!out.is_empty(), "envs/ 下没有任何环境 profile");
    out
}

fn parse(name: &str, path: &Path) -> EnvProfile {
    let content = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
    let doc: toml::Value = content
        .parse()
        .unwrap_or_else(|e| panic!("failed to parse {}: {e}", path.display()));

    let env_t = table(&doc, "env", path);
    let platform = str_of(env_t, "platform", path);
    let desc = env_t
        .get("desc")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let is_default = env_t
        .get("default")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let starry = table(&doc, "starry", path);
    let starry_config = str_of(starry, "config", path);
    let starry_smp = starry.get("smp").and_then(|v| v.as_integer()).map(|v| v as u32);
    let starry_artifact = str_of(starry, "artifact", path);

    let qemu = doc.get("qemu").and_then(|v| v.as_table()).map(|t| QemuMachine {
        machine: t
            .get("machine")
            .and_then(|v| v.as_str())
            .unwrap_or("virt")
            .to_string(),
        smp: t
            .get("smp")
            .and_then(|v| v.as_str().map(String::from).or_else(|| v.as_integer().map(|i| i.to_string())))
            .unwrap_or_else(|| "2".into()),
        ram: t
            .get("ram")
            .and_then(|v| v.as_str())
            .unwrap_or("256M")
            .to_string(),
    });

    let dtb_table = doc.get("dtb").and_then(|v| v.as_table());
    let dtb = dtb_table.map(|t| {
        let source = t.get("source").and_then(|v| v.as_str()).unwrap_or("dts");
        match source {
            "dts" => DtbSource::Dts(str_of(t, "dts", path)),
            "dumpdtb" => DtbSource::DumpdtbOverlay {
                overlay: str_of(t, "overlay", path),
            },
            other => panic!("{}: dtb.source 未知取值 {other}（dts | dumpdtb）", path.display()),
        }
    });
    let rp_dts = dtb_table
        .and_then(|t| t.get("rp_dts"))
        .and_then(|v| v.as_str())
        .map(String::from);
    let rp_defines = dtb_table
        .and_then(|t| t.get("rp_defines"))
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .map(|v| v.as_str().expect("dtb.rp_defines 必须是字符串数组").to_string())
                .collect()
        })
        .unwrap_or_default();

    let pack_bin = doc
        .get("pack")
        .and_then(|v| v.as_table())
        .and_then(|t| t.get("bin"))
        .and_then(|v| v.as_str())
        .map(String::from);

    EnvProfile {
        name: name.to_string(),
        platform,
        desc,
        is_default,
        starry_config,
        starry_smp,
        starry_artifact,
        qemu,
        dtb,
        rp_dts,
        rp_defines,
        pack_bin,
    }
}

fn table<'a>(doc: &'a toml::Value, key: &str, path: &Path) -> &'a toml::map::Map<String, toml::Value> {
    doc.get(key)
        .and_then(|v| v.as_table())
        .unwrap_or_else(|| panic!("{}: 缺少 [{key}] 节", path.display()))
}

fn str_of(t: &toml::map::Map<String, toml::Value>, key: &str, path: &Path) -> String {
    t.get(key)
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("{}: 缺少 {key}", path.display()))
        .to_string()
}

/// 按名查找环境；找不到时列出全部可用环境后退出。
pub fn find_or_die<'a>(envs: &'a [EnvProfile], name: &str) -> &'a EnvProfile {
    envs
        .iter()
        .find(|e| e.name == name)
        .unwrap_or_else(|| {
            eprintln!("unknown env: {name}");
            eprintln!("\navailable envs:");
            for e in envs {
                eprintln!("  {:<12} {}", e.name, e.desc);
            }
            std::process::exit(1);
        })
}

/// 平台的默认环境（env.default = true 的那个；无标记则取该平台第一个）。
pub fn default_for_platform<'a>(envs: &'a [EnvProfile], platform: &str) -> &'a EnvProfile {
    let mut first: Option<&EnvProfile> = None;
    for e in envs.iter().filter(|e| e.platform == platform) {
        if e.is_default {
            return e;
        }
        if first.is_none() {
            first = Some(e);
        }
    }
    first.unwrap_or_else(|| panic!("platform {platform} 没有任何环境 profile"))
}
