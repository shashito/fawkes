//! The manifest: one TOML file per app (see DESIGN.md, "Manifest").

use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// `$FAWKES_HOME`, else `~/.fawkes`: default state directories and the `run/` links the TUI discovers.
pub fn fawkes_home() -> Result<PathBuf, String> {
    if let Some(dir) = std::env::var_os("FAWKES_HOME") {
        return Ok(dir.into());
    }
    let home = std::env::var_os("HOME").ok_or("neither FAWKES_HOME nor HOME is set")?;
    Ok(Path::new(&home).join(".fawkes"))
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub name: String,
    pub repo: PathBuf,
    pub state_dir: Option<PathBuf>,
    pub home: PathBuf,
    #[serde(default)]
    pub storage: Vec<String>,
    pub port: u16,
    pub spare_ports: [u16; 2],
    pub install: Option<String>,
    #[serde(default)]
    pub reuse: Vec<String>,
    pub start: String,
    #[serde(default = "default_deadline")]
    pub deadline: u64,
    #[serde(default = "default_stop_timeout")]
    pub stop_timeout: f64,
    #[serde(default = "default_min_uptime")]
    pub min_uptime: f64,
    #[serde(default = "default_max_failed_starts")]
    pub max_failed_starts: u32,
    pub health: Health,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Health {
    pub url: Option<String>,
    pub command: Option<String>,
    #[serde(default = "default_interval")]
    pub interval: f64,
    #[serde(default = "default_health_timeout")]
    pub timeout: f64,
}

fn default_deadline() -> u64 {
    120
}
fn default_stop_timeout() -> f64 {
    10.0
}
fn default_min_uptime() -> f64 {
    10.0
}
fn default_max_failed_starts() -> u32 {
    3
}
fn default_interval() -> f64 {
    0.5
}
fn default_health_timeout() -> f64 {
    60.0
}

/// What a health check does, with templates already expanded.
pub enum Check {
    Url(String),
    Command(String),
}

/// Values for the `{port}`, `{home}`, `{code}`, `{generation}` and `{state}` templates.
pub struct Vars {
    pub port: u16,
    pub home: PathBuf,
    pub code: PathBuf,
    pub generation: String,
    pub state: PathBuf,
}

impl Vars {
    pub fn expand(&self, text: &str) -> String {
        text.replace("{port}", &self.port.to_string())
            .replace("{home}", &self.home.to_string_lossy())
            .replace("{code}", &self.code.to_string_lossy())
            .replace("{generation}", &self.generation)
            .replace("{state}", &self.state.to_string_lossy())
    }
}

impl Manifest {
    /// Read and validate a manifest; relative paths resolve against its directory.
    pub fn load(path: &Path) -> Result<Manifest, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read manifest {}: {e}", path.display()))?;
        let mut m: Manifest = toml::from_str(&text)
            .map_err(|e| format!("invalid manifest {}: {e}", path.display()))?;
        if m.health.url.is_some() == m.health.command.is_some() {
            return Err(format!(
                "invalid manifest {}: [health] needs exactly one of url or command",
                path.display()
            ));
        }
        if m.spare_ports[0] > m.spare_ports[1] {
            return Err(format!(
                "invalid manifest {}: spare_ports must be [low, high]",
                path.display()
            ));
        }
        let absolute = std::path::absolute(path).map_err(|e| e.to_string())?;
        let base = absolute.parent().unwrap_or(Path::new("/")).to_path_buf();
        m.repo = base.join(&m.repo);
        m.home = base.join(&m.home);
        m.state_dir = Some(match &m.state_dir {
            Some(dir) => base.join(dir),
            None => fawkes_home()?.join(&m.name),
        });
        Ok(m)
    }

    pub fn state_dir(&self) -> &Path {
        self.state_dir
            .as_deref()
            .expect("state_dir is resolved by load")
    }

    pub fn health_check(&self, vars: &Vars) -> Check {
        match (&self.health.url, &self.health.command) {
            (Some(url), _) => Check::Url(vars.expand(url)),
            (_, Some(command)) => Check::Command(vars.expand(command)),
            _ => unreachable!("load requires exactly one health check"),
        }
    }
}
