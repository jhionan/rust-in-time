use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// Tracking settings share their YAML document with invoice settings. Unknown
/// keys deliberately remain accepted so either subsystem can read that file.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub interval: String,
    pub idle_threshold: String,
    pub agent_cpu_threshold: u64,
    pub agent_cpu_threshold_idle: u64,
    pub agent_busy_rise_ticks: u32,
    pub agent_busy_fall_ticks: u32,
    pub transcript_tracking: bool,
    pub transcript_grace: String,
    pub transcript_root: PathBuf,
    pub codex_root: PathBuf,
    pub opencode_db: PathBuf,
    pub db_path: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        let home = dirs::home_dir();
        let home_path = |tail: &str| home.as_ref().map(|h| h.join(tail)).unwrap_or_default();
        Self {
            interval: "5s".into(),
            idle_threshold: "2m".into(),
            agent_cpu_threshold: 15,
            agent_cpu_threshold_idle: 100,
            agent_busy_rise_ticks: 2,
            agent_busy_fall_ticks: 3,
            transcript_tracking: true,
            transcript_grace: "10m".into(),
            transcript_root: std::env::var_os("CLAUDE_CONFIG_DIR")
                .filter(|v| !v.is_empty())
                .map(|v| PathBuf::from(v).join("projects"))
                .unwrap_or_else(|| home_path(".claude/projects")),
            codex_root: std::env::var_os("CODEX_HOME")
                .filter(|v| !v.is_empty())
                .map(|v| PathBuf::from(v).join("sessions"))
                .unwrap_or_else(|| home_path(".codex/sessions")),
            opencode_db: std::env::var_os("XDG_DATA_HOME")
                .filter(|v| !v.is_empty())
                .map(|v| PathBuf::from(v).join("opencode/opencode.db"))
                .unwrap_or_else(|| home_path(".local/share/opencode/opencode.db")),
            db_path: None,
        }
    }
}

impl Config {
    /// An absent optional file uses defaults; malformed or unreadable files fail.
    pub fn load(path: &Path) -> Result<Self> {
        let mut config: Self = match fs::read_to_string(path) {
            Ok(text) => {
                serde_yaml::from_str(&text).with_context(|| format!("parse {}", path.display()))?
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(err) => return Err(err).with_context(|| format!("read {}", path.display())),
        };
        config.validate()?;
        // The existing database bills each distinct timestamp as five seconds.
        // Accept old custom intervals, but never silently change that accounting.
        config.interval = "5s".into();
        let absolute = std::path::absolute(path).context("resolve configuration file path")?;
        let base = absolute
            .parent()
            .context("configuration file has no parent directory")?;
        config.transcript_root = resolve_path(base, &config.transcript_root)?;
        config.codex_root = resolve_path(base, &config.codex_root)?;
        config.opencode_db = resolve_path(base, &config.opencode_db)?;
        config.db_path = config
            .db_path
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| resolve_path(base, &p))
            .transpose()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        positive_duration("interval", &self.interval)?;
        positive_duration("idle_threshold", &self.idle_threshold)?;
        positive_duration("transcript_grace", &self.transcript_grace)?;
        if self.agent_cpu_threshold == 0 || self.agent_cpu_threshold_idle == 0 {
            bail!("CPU thresholds must be positive");
        }
        if self.agent_cpu_threshold_idle < self.agent_cpu_threshold {
            bail!("agent_cpu_threshold_idle must be at least agent_cpu_threshold");
        }
        if self.agent_busy_rise_ticks == 0 || self.agent_busy_fall_ticks == 0 {
            bail!("agent_busy_rise_ticks and agent_busy_fall_ticks must be positive");
        }
        Ok(())
    }

    pub fn interval_duration(&self) -> Result<Duration> {
        positive_duration("interval", &self.interval)?;
        Ok(Duration::from_secs(5))
    }

    pub fn idle_duration(&self) -> Result<Duration> {
        positive_duration("idle_threshold", &self.idle_threshold)
    }

    pub fn grace_duration(&self) -> Result<Duration> {
        positive_duration("transcript_grace", &self.transcript_grace)
    }
}

fn positive_duration(name: &str, value: &str) -> Result<Duration> {
    let duration = humantime::parse_duration(value)
        .with_context(|| format!("{name}: invalid duration {value:?}"))?;
    if duration.is_zero() || duration.as_secs() > i64::MAX as u64 {
        bail!("{name}: duration must be positive and fit in signed seconds");
    }
    Ok(duration)
}

fn expand_home(path: &Path) -> Result<PathBuf> {
    let text = path.to_string_lossy();
    if text == "~" || text.starts_with("~/") || text.starts_with("~\\") {
        let home = dirs::home_dir().context("cannot expand ~ without a user home directory")?;
        return Ok(home.join(text.trim_start_matches('~').trim_start_matches(['/', '\\'])));
    }
    Ok(path.to_owned())
}

fn resolve_path(base: &Path, path: &Path) -> Result<PathBuf> {
    if !source_enabled(path) {
        return Ok(path.to_owned());
    }
    let path = expand_home(path)?;
    let text = path.to_string_lossy();
    let windows_absolute = text.starts_with("\\\\")
        || (text.as_bytes().get(1) == Some(&b':')
            && text
                .as_bytes()
                .get(2)
                .is_some_and(|b| matches!(b, b'/' | b'\\')));
    if path.is_absolute() || windows_absolute {
        Ok(path)
    } else {
        Ok(base.join(path))
    }
}

pub fn source_enabled(path: &Path) -> bool {
    !path.as_os_str().is_empty() && path != Path::new("off")
}
