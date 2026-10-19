use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default = "default_listen")]
    pub listen: String,
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,
    #[serde(default = "default_instance")]
    pub instance_id: String,
    #[serde(default)]
    pub simulate_enabled: bool,
    #[serde(default = "default_canary")]
    pub simulate_canary: String,
    #[serde(default)]
    pub artifacts_enabled: bool,
    /// Trusted public origin for discovery documents. Never taken from Host.
    #[serde(default = "default_public_base")]
    pub public_base_url: String,
}

fn default_listen() -> String {
    "127.0.0.1:8787".into()
}
fn default_data_dir() -> PathBuf {
    PathBuf::from("./data")
}
fn default_instance() -> String {
    "aegis-dev".into()
}
fn default_canary() -> String {
    "AEGIS-SIM-ROOT-CANARY".into()
}
fn default_public_base() -> String {
    "https://aegis.sunny99.win".into()
}

/// https://host[:port] only. No path, query, fragment, or userinfo.
pub fn normalize_public_base_url(raw: &str) -> Result<String, String> {
    let s = raw.trim();
    let s = s.strip_suffix('/').unwrap_or(s);
    if !s.starts_with("https://") {
        return Err(
            "public_base_url must be an https:// origin with no path (example: https://aegis.sunny99.win)"
                .into(),
        );
    }
    let rest = &s["https://".len()..];
    if rest.is_empty()
        || rest.contains('/')
        || rest.contains('?')
        || rest.contains('#')
        || rest.contains('@')
        || rest.contains('\\')
        || rest.contains(' ')
    {
        return Err(
            "public_base_url must be an https:// origin with no path, query, userinfo, or fragment"
                .into(),
        );
    }
    let host = rest.split(':').next().unwrap_or(rest);
    if host.parse::<std::net::IpAddr>().is_ok()
        || host.eq_ignore_ascii_case("localhost")
        || !host.contains('.')
    {
        return Err("public_base_url host must be a public DNS name".into());
    }
    Ok(format!("https://{rest}"))
}

impl Config {
    pub fn load(path: Option<&Path>) -> Result<Self, String> {
        if let Some(p) = path {
            let raw = std::fs::read_to_string(p)
                .map_err(|e| format!("read config {}: {e}", p.display()))?;
            let parsed: toml::Value =
                toml::from_str(&raw).map_err(|e| format!("parse config: {e}"))?;
            if let Some(table) = parsed.as_table() {
                for k in table.keys() {
                    let lk = k.to_ascii_lowercase();
                    if lk == "skip_disk" || lk == "aegis_skip_disk_check" || lk.contains("skip_disk")
                    {
                        return Err(
                            "disk skip is not a config key and is forbidden in deploy files; use --dev only in local tests"
                                .into(),
                        );
                    }
                }
            }
            let mut cfg: Self = parsed.try_into().map_err(|e| format!("parse config: {e}"))?;
            cfg.public_base_url = normalize_public_base_url(&cfg.public_base_url)?;
            return Ok(cfg);
        }
        Ok(Self {
            listen: default_listen(),
            data_dir: default_data_dir(),
            instance_id: default_instance(),
            simulate_enabled: false,
            simulate_canary: default_canary(),
            artifacts_enabled: false,
            public_base_url: default_public_base(),
        })
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("aegis.sqlite")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_base_url_https_origin_only() {
        assert_eq!(
            normalize_public_base_url("https://aegis.sunny99.win").unwrap(),
            "https://aegis.sunny99.win"
        );
        assert_eq!(
            normalize_public_base_url("https://aegis.sunny99.win/").unwrap(),
            "https://aegis.sunny99.win"
        );
        assert!(normalize_public_base_url("http://aegis.sunny99.win").is_err());
        assert!(normalize_public_base_url("https://aegis.sunny99.win/api").is_err());
        assert!(normalize_public_base_url("https://127.0.0.1").is_err());
        assert!(normalize_public_base_url("https://localhost").is_err());
    }
}
