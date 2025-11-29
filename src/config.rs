use std::{fs, path::Path, time::Duration};

use serde::de::Visitor;
use serde::{Deserialize, Deserializer};
use serde_yaml as yaml;

use anyhow::Context;
use tracing::info;

#[derive(Clone, Debug, Deserialize)]
pub struct Config {
    #[allow(dead_code)]
    pub version: i32,
    pub generator: GeneratorConfig,
    pub limits: LimitsConfig,
    #[serde(rename = "http_check")]
    pub http_check: HTTPCheckConfig,
    pub run: RunConfig,
    pub storage: StorageConfig,
}

#[derive(Clone, Debug, Deserialize)]
pub struct GeneratorConfig {
    #[serde(default)]
    pub tlds: Vec<String>,
    #[serde(default)]
    pub tlds_file: String,
    pub min_length: i32,
    pub max_length: i32,
    #[serde(default)]
    pub alphabet: String,
    #[serde(default)]
    pub allow_hyphen: bool,
    #[serde(default)]
    pub forbid_leading_hyphen: bool,
    #[serde(default)]
    pub forbid_trailing_hyphen: bool,
    #[serde(default)]
    pub forbid_double_hyphen: bool,
}

#[derive(Clone, Debug, Deserialize)]
pub struct LimitsConfig {
    pub concurrency: i32,
    pub rate_per_second: i32,
    pub max_candidates: i32,
}

#[derive(Clone, Debug, Deserialize)]
pub struct HTTPCheckConfig {
    #[serde(deserialize_with = "de_duration")]
    pub timeout: Duration,
    #[serde(default)]
    pub retry: u32,
    #[serde(default)]
    pub method: String,
    pub accept_status_min: i32,
    pub accept_status_max: i32,
    #[serde(default)]
    pub try_https_first: bool,
}

#[derive(Clone, Debug, Deserialize)]
pub struct RunConfig {
    #[serde(default)]
    pub loop_: bool,
}

#[derive(Clone, Debug, Deserialize)]
pub struct StorageConfig {
    pub dir: String,
    #[serde(default)]
    pub resume: bool,
    #[serde(default)]
    pub state_file: String,
}


// -------- Duration "3s" etc --------
fn de_duration<'de, D>(deserializer: D) -> Result<Duration, D::Error>
where
    D: Deserializer<'de>,
{
    struct DVisitor;
    impl<'de> Visitor<'de> for DVisitor {
        type Value = Duration;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("duration like 3s, 500ms, 2m, 1h")
        }
        fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            parse_duration(v).map_err(E::custom)
        }
    }
    deserializer.deserialize_any(DVisitor)
}

fn parse_duration(s: &str) -> Result<Duration, String> {
    let st = s.trim().to_lowercase();
    let unit = if st.ends_with("ms") {
        "ms"
    } else if st.ends_with('s') {
        "s"
    } else if st.ends_with('m') {
        "m"
    } else if st.ends_with('h') {
        "h"
    } else {
        "s"
    };
    let num_str = if unit == "ms" {
        &st[..st.len() - 2]
    } else if ["s", "m", "h"].contains(&unit) && st.len() >= 1 && st.ends_with(unit) {
        &st[..st.len() - 1]
    } else {
        &st
    };
    let val = num_str
        .trim()
        .parse::<f64>()
        .map_err(|e| format!("invalid duration {s}: {e}"))?;
    let dur = match unit {
        "ms" => Duration::from_millis(val as u64),
        "s" => Duration::from_secs_f64(val),
        "m" => Duration::from_secs_f64(val * 60.0),
        "h" => Duration::from_secs_f64(val * 3600.0),
        _ => Duration::from_secs_f64(val),
    };
    Ok(dur)
}

// -------- TLD loading --------

pub async fn load_config(path: &str) -> anyhow::Result<Config> {
    info!("loading config from {}", path);
    let data = fs::read(path).with_context(|| format!("read config {path}"))?;
    let mut cfg: Config = yaml::from_slice(&data)?;
    validate_config(&cfg)?;
    info!(
        "config validated: storage.dir={}, limits.concurrency={}, rps={}, len={}..{}, inline_tlds={}",
        cfg.storage.dir,
        cfg.limits.concurrency,
        cfg.limits.rate_per_second,
        cfg.generator.min_length,
        cfg.generator.max_length,
        cfg.generator.tlds.len()
    );
    // maybe load TLDs
    if !cfg.generator.tlds_file.trim().is_empty() {
        let src = cfg.generator.tlds_file.trim();
        info!("loading TLDs from {}", src);
        let tlds = if src.starts_with("http://") || src.starts_with("https://") {
            load_tlds_from_url(src).await?
        } else {
            load_tlds_from_file(src)?
        };
        if tlds.is_empty() {
            anyhow::bail!("no TLDs parsed from {src}");
        }
        info!("loaded {} TLDs from {}", tlds.len(), src);
        cfg.generator.tlds = tlds;
    }
    if cfg.storage.state_file.trim().is_empty() {
        cfg.storage.state_file = Path::new(&cfg.storage.dir).join("state.json").to_string_lossy().to_string();
        info!("storage.state_file not set, computed default: {}", cfg.storage.state_file);
    }
    Ok(cfg)
}

pub fn validate_config(cfg: &Config) -> anyhow::Result<()> {
    if cfg.generator.tlds.is_empty() && cfg.generator.tlds_file.trim().is_empty() {
        anyhow::bail!("generator.tlds must not be empty (or provide generator.tlds_file)");
    }
    if cfg.generator.min_length < 1 || cfg.generator.max_length < cfg.generator.min_length {
        anyhow::bail!(
            "invalid lengths: {}..{}",
            cfg.generator.min_length,
            cfg.generator.max_length
        );
    }
    if cfg.limits.concurrency <= 0 {
        anyhow::bail!("limits.concurrency must be > 0");
    }
    if cfg.limits.rate_per_second <= 0 {
        anyhow::bail!("limits.rate_per_second must be > 0");
    }
    if cfg.http_check.accept_status_min <= 0 || cfg.http_check.accept_status_max < cfg.http_check.accept_status_min {
        anyhow::bail!("invalid http_check accept status range");
    }
    if cfg.storage.dir.trim().is_empty() {
        anyhow::bail!("storage.dir must not be empty");
    }
    Ok(())
}

pub fn load_tlds_from_file(path: &str) -> anyhow::Result<Vec<String>> {
    let txt = fs::read_to_string(path)?;
    let mut uniq = std::collections::BTreeSet::<String>::new();
    for line in txt.lines() {
        let mut t = line.trim().to_string();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        if t.starts_with('.') {
            t = t[1..].to_string();
        }
        t.make_ascii_lowercase();
        if t.is_empty() {
            continue;
        }
        uniq.insert(format!(".{}", t));
    }
    Ok(uniq.into_iter().collect())
}

pub async fn load_tlds_from_url(url: &str) -> anyhow::Result<Vec<String>> {
    info!("fetching TLDs from URL: {}", url);
    let body = reqwest::get(url).await?.text().await?;
    let total_lines = body.lines().count();
    let mut uniq = std::collections::BTreeSet::<String>::new();
    for line in body.lines() {
        let mut t = line.trim().to_string();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        if t.starts_with('.') {
            t = t[1..].to_string();
        }
        t.make_ascii_lowercase();
        if t.is_empty() {
            continue;
        }
        uniq.insert(format!(".{}", t));
    }
    info!("parsed {} TLDs from {} (input lines={})", uniq.len(), url, total_lines);
    Ok(uniq.into_iter().collect())
}