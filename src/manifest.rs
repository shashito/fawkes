//! The manifest: one TOML file per app (see DESIGN.md, "Manifest").

use crate::state::Paths;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

/// `$FAWKES_HOME`, else `~/.fawkes`: default state directories and the `run/` links the TUI discovers.
pub fn fawkes_home() -> Result<PathBuf, String> {
    if let Some(dir) = std::env::var_os("FAWKES_HOME") {
        return Ok(dir.into());
    }
    let home = std::env::var_os("HOME").ok_or("neither FAWKES_HOME nor HOME is set")?;
    Ok(Path::new(&home).join(".fawkes"))
}

/// `<fawkes_home>/run`, where every daemon links its socket as `<name>.sock` for `fawkes tui` to find.
pub fn run_dir() -> Result<PathBuf, String> {
    Ok(fawkes_home()?.join("run"))
}

/// The manifest as validated and resolved by `load`: paths absolute, `state_dir` decided, the health check one of
/// its two kinds.
#[derive(Debug, Clone, PartialEq)]
pub struct Manifest {
    pub name: String,
    pub repo: PathBuf,
    state_dir: PathBuf,
    pub home: PathBuf,
    pub storage: Vec<String>,
    pub port: u16,
    pub spare_ports: [u16; 2],
    pub install: Option<String>,
    pub install_timeout: f64,
    pub reuse: Vec<String>,
    /// How many generations `prune` keeps; without it nothing is pruned.
    pub keep: Option<u32>,
    pub start: String,
    pub deadline: u64,
    pub stop_timeout: f64,
    pub min_uptime: f64,
    pub max_failed_starts: u32,
    /// Roll a pending generation back as soon as its health check gives up, instead of at the deadline.
    pub rollback_on_unhealthy: bool,
    pub health: Health,
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Health {
    pub check: Check,
    pub interval: f64,
    pub timeout: f64,
}

/// The manifest as written in the TOML file, before `load` has validated and resolved it.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Written {
    name: String,
    repo: PathBuf,
    state_dir: Option<PathBuf>,
    home: PathBuf,
    #[serde(default)]
    storage: Vec<String>,
    port: u16,
    spare_ports: [u16; 2],
    install: Option<String>,
    #[serde(default = "default_install_timeout")]
    install_timeout: f64,
    #[serde(default)]
    reuse: Vec<String>,
    keep: Option<u32>,
    start: String,
    #[serde(default = "default_deadline")]
    deadline: u64,
    #[serde(default = "default_stop_timeout")]
    stop_timeout: f64,
    #[serde(default = "default_min_uptime")]
    min_uptime: f64,
    #[serde(default = "default_max_failed_starts")]
    max_failed_starts: u32,
    #[serde(default)]
    rollback_on_unhealthy: bool,
    health: WrittenHealth,
    #[serde(default)]
    env: BTreeMap<String, String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WrittenHealth {
    url: Option<String>,
    command: Option<String>,
    #[serde(default = "default_interval")]
    interval: f64,
    #[serde(default = "default_health_timeout")]
    timeout: f64,
}

/// The longest confirm window, in seconds. A `deadline` in the manifest and `switch --deadline` are both held to it,
/// so a typo cannot leave a generation unconfirmed (and its predecessor not rolled back to) for weeks.
pub const MAX_DEADLINE: u64 = 7 * 86_400;

/// The longest any manifest duration may be, in seconds. `Duration::from_secs_f64` and `Instant + Duration` panic on
/// huge values, so the bound is what keeps those calls safe.
const MAX_SECONDS: f64 = 86_400.0;

fn default_deadline() -> u64 {
    120
}
fn default_install_timeout() -> f64 {
    1800.0
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

/// What a health check does: the URL to fetch or the command to run, as written in the manifest (templates are
/// expanded by `health_check`).
#[derive(Debug, Clone, PartialEq)]
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
    /// Substitute the templates as they are, for `[env]` values and `health.url`.
    pub fn expand(&self, text: &str) -> String {
        self.substitute(text, |value| value)
    }

    /// Substitute the templates with each value shell-quoted, for text that goes to `/bin/sh -c`.
    pub fn expand_command(&self, text: &str) -> String {
        self.substitute(text, shell_quote)
    }

    /// One pass over `text`: substituted values are never scanned again, so a path that contains `{port}` stays
    /// as it is. An unknown `{...}` stays as written.
    fn substitute(&self, text: &str, finish: impl Fn(String) -> String) -> String {
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(open) = rest.find('{') {
            out.push_str(&rest[..open]);
            rest = &rest[open..];
            let value = rest.find('}').and_then(|close| {
                let value = match &rest[1..close] {
                    "port" => self.port.to_string(),
                    "home" => self.home.to_string_lossy().into_owned(),
                    "code" => self.code.to_string_lossy().into_owned(),
                    "generation" => self.generation.clone(),
                    "state" => self.state.to_string_lossy().into_owned(),
                    _ => return None,
                };
                Some((close, value))
            });
            if let Some((close, value)) = value {
                out.push_str(&finish(value));
                rest = &rest[close + 1..];
            } else {
                out.push('{');
                rest = &rest[1..];
            }
        }
        out.push_str(rest);
        out
    }
}

/// `value` as one shell word: bare when it only has shell-safe characters, else in single quotes.
fn shell_quote(value: String) -> String {
    let safe = |c: char| c.is_ascii_alphanumeric() || "_-./:,+=@%".contains(c);
    if !value.is_empty() && value.chars().all(safe) {
        value
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

/// Whether a `storage` or `reuse` entry stays inside the directory it is joined onto.
fn is_inside(entry: &str) -> bool {
    !entry.is_empty()
        && Path::new(entry)
            .components()
            .all(|part| matches!(part, Component::Normal(_) | Component::CurDir))
        && Path::new(entry)
            .components()
            .any(|part| matches!(part, Component::Normal(_)))
}

/// Whether a raw `health.url` template is `http://` with an explicit port (digits or `{port}`) after the host.
fn has_port(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("http://") else {
        return false;
    };
    let authority = rest.split('/').next().unwrap_or("");
    match authority.rsplit_once(':') {
        Some((host, port)) => {
            !host.is_empty()
                && (port == "{port}"
                    || (!port.is_empty() && port.bytes().all(|b| b.is_ascii_digit())))
        }
        None => false,
    }
}

impl Manifest {
    /// Read and validate a manifest; relative paths resolve against its directory.
    pub fn load(path: &Path) -> Result<Manifest, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read manifest {}: {e}", path.display()))?;
        let written: Written = toml::from_str(&text)
            .map_err(|e| format!("invalid manifest {}: {e}", path.display()))?;
        // Without exactly one of url and command there is no check, and `problems` says so among the rest.
        let problems = written.problems();
        let check = match (&written.health.url, &written.health.command) {
            (Some(url), None) => Some(Check::Url(url.clone())),
            (None, Some(command)) => Some(Check::Command(command.clone())),
            _ => None,
        };
        let (true, Some(check)) = (problems.is_empty(), check) else {
            return Err(format!(
                "invalid manifest {}: {}",
                path.display(),
                problems.join("; ")
            ));
        };
        let absolute = std::path::absolute(path).map_err(|e| e.to_string())?;
        let base = absolute.parent().unwrap_or(Path::new("/")).to_path_buf();
        let state_dir = match &written.state_dir {
            Some(dir) => base.join(dir),
            None => fawkes_home()?.join(&written.name),
        };
        Ok(Manifest {
            name: written.name,
            repo: base.join(&written.repo),
            state_dir,
            home: base.join(&written.home),
            storage: written.storage,
            port: written.port,
            spare_ports: written.spare_ports,
            install: written.install,
            install_timeout: written.install_timeout,
            reuse: written.reuse,
            keep: written.keep,
            start: written.start,
            deadline: written.deadline,
            stop_timeout: written.stop_timeout,
            min_uptime: written.min_uptime,
            max_failed_starts: written.max_failed_starts,
            rollback_on_unhealthy: written.rollback_on_unhealthy,
            health: Health {
                check,
                interval: written.health.interval,
                timeout: written.health.timeout,
            },
            env: written.env,
        })
    }

    /// Decide whether `new` may replace `self` while the daemon runs. Ok: the TOML names of the changed fields,
    /// possibly none. Err: a message naming every refused field and what to do instead. Both manifests are the loaded
    /// ones, so paths are compared after resolution.
    #[expect(
        clippy::float_cmp,
        reason = "the timeouts are copied from the manifest text, never computed: any difference is a change the user wrote"
    )]
    pub fn reload(&self, new: &Manifest, app_running: bool) -> Result<Vec<&'static str>, String> {
        // The daemon's identity, its state and the files a restore overwrites are fixed for its lifetime: a changed
        // `home` would restore a snapshot into a different directory than the one it was taken from.
        let fixed = [
            ("name", self.name != new.name),
            ("repo", self.repo != new.repo),
            ("state_dir", self.state_dir != new.state_dir),
            ("home", self.home != new.home),
            ("storage", self.storage != new.storage),
        ];
        let refused = differing(&fixed);
        let mut problems = Vec::new();
        if !refused.is_empty() {
            problems.push(format!(
                "{} can only change with a daemon restart: stop the daemon and start it again with the new manifest",
                refused.join(", ")
            ));
        }
        if app_running && self.port != new.port {
            problems.push(
                "port cannot change while the app is running: `fawkes stop` the app first, then reload again".into(),
            );
        }
        if !problems.is_empty() {
            return Err(problems.join("; "));
        }
        let reloadable = [
            ("port", self.port != new.port),
            ("spare_ports", self.spare_ports != new.spare_ports),
            ("install", self.install != new.install),
            (
                "install_timeout",
                self.install_timeout != new.install_timeout,
            ),
            ("keep", self.keep != new.keep),
            ("reuse", self.reuse != new.reuse),
            ("start", self.start != new.start),
            ("deadline", self.deadline != new.deadline),
            ("stop_timeout", self.stop_timeout != new.stop_timeout),
            ("min_uptime", self.min_uptime != new.min_uptime),
            (
                "max_failed_starts",
                self.max_failed_starts != new.max_failed_starts,
            ),
            (
                "rollback_on_unhealthy",
                self.rollback_on_unhealthy != new.rollback_on_unhealthy,
            ),
            ("health", self.health != new.health),
            ("env", self.env != new.env),
        ];
        Ok(differing(&reloadable))
    }

    /// The files of `state_dir`.
    pub fn paths(&self) -> Paths {
        Paths {
            root: self.state_dir.clone(),
        }
    }

    pub fn health_check(&self, vars: &Vars) -> Check {
        match &self.health.check {
            Check::Url(url) => Check::Url(vars.expand(url)),
            Check::Command(command) => Check::Command(vars.expand_command(command)),
        }
    }
}

/// The names of the fields that differ, in the order given.
fn differing(fields: &[(&'static str, bool)]) -> Vec<&'static str> {
    fields
        .iter()
        .filter(|(_, differs)| *differs)
        .map(|(field, _)| *field)
        .collect()
}

impl Written {
    /// Everything wrong with the manifest as written, each saying what is allowed. Empty when valid.
    fn problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        if self.name.is_empty()
            || !self
                .name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            problems.push(format!(
                "name {:?} must be non-empty and use only letters, digits, '_' and '-' (it becomes a file name)",
                self.name
            ));
        }
        if self.health.url.is_some() == self.health.command.is_some() {
            problems.push("[health] needs exactly one of url or command".into());
        }
        if self.spare_ports[0] > self.spare_ports[1] {
            problems.push("spare_ports must be [low, high]".into());
        } else if (self.spare_ports[0]..=self.spare_ports[1]).contains(&self.port) {
            problems.push(format!(
                "port {} must lie outside spare_ports {:?}: the spare range is for staged generations",
                self.port, self.spare_ports
            ));
        }
        if !(1..=MAX_DEADLINE).contains(&self.deadline) {
            problems.push(format!(
                "deadline {} must be between 1 and {MAX_DEADLINE} seconds",
                self.deadline
            ));
        }
        if self.max_failed_starts < 1 {
            problems.push("max_failed_starts must be at least 1".into());
        }
        if self.keep == Some(0) {
            problems.push("keep must be at least 1 (leave it out to keep every generation)".into());
        }
        for (field, seconds) in [
            ("install_timeout", self.install_timeout),
            ("stop_timeout", self.stop_timeout),
            ("min_uptime", self.min_uptime),
            ("health.interval", self.health.interval),
            ("health.timeout", self.health.timeout),
        ] {
            if !(seconds.is_finite() && seconds > 0.0 && seconds <= MAX_SECONDS) {
                problems.push(format!(
                    "{field} {seconds} must be a number of seconds above 0 and at most {MAX_SECONDS}"
                ));
            }
        }
        for (field, entries) in [("storage", &self.storage), ("reuse", &self.reuse)] {
            for entry in entries {
                if !is_inside(entry) {
                    problems.push(format!(
                        "{field} entry {entry:?} must be a non-empty relative path without '..' (it is joined onto a directory that a restore may delete from)"
                    ));
                }
            }
        }
        if let Some(url) = &self.health.url
            && !has_port(url)
        {
            problems.push(format!(
                "health.url {url:?} must start with http:// and end its host with :<port> or :{{port}}, for example http://127.0.0.1:{{port}}/"
            ));
        }
        problems
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(home: &str) -> Vars {
        Vars {
            port: 4790,
            home: PathBuf::from(home),
            code: PathBuf::from("/srv/code"),
            generation: "g3".to_string(),
            state: PathBuf::from("/srv/state"),
        }
    }

    #[test]
    fn templates_expand_in_one_pass() {
        let v = vars("/tmp/{port}");
        assert_eq!(
            v.expand("{home}:{port}:{nope}:{"),
            "/tmp/{port}:4790:{nope}:{"
        );
        assert_eq!(v.expand_command("{home} {port}"), "'/tmp/{port}' 4790");
    }

    #[test]
    fn commands_quote_values_and_env_does_not() {
        let tricky = "/tmp/it's a dir";
        let v = vars(tricky);
        assert_eq!(v.expand("{home}"), tricky);
        assert_eq!(v.expand_command("{code} {generation}"), "/srv/code g3");
        assert_eq!(vars("").expand_command("{home}"), "''");
        let out = std::process::Command::new("/bin/sh")
            .args(["-c", &v.expand_command("printf %s {home}")])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8(out.stdout).unwrap(), tricky);
    }

    const VALID: &str = r#"
name = "app"
repo = "repo"
state_dir = "state"
home = "home"
storage = ["data/app.db"]
port = 4790
spare_ports = [4791, 4795]
reuse = ["node_modules"]
start = "node app.js"
[health]
url = "http://127.0.0.1:{port}/"
"#;

    /// A temporary directory for manifests, removed on drop. Manifests loaded from one scratch resolve their paths
    /// against the same directory, so they can be compared by `reload`.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(label: &str) -> Scratch {
            let dir =
                std::env::temp_dir().join(format!("fk-manifest-{}-{label}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            Scratch(dir)
        }

        fn load(&self, text: &str) -> Result<Manifest, String> {
            let path = self.0.join("fawkes.toml");
            std::fs::write(&path, text).unwrap();
            Manifest::load(&path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }

    /// `VALID` with one line replaced: `from` must occur in it.
    fn with(from: &str, to: &str) -> String {
        assert!(VALID.contains(from), "{from:?} is not in VALID");
        VALID.replacen(from, to, 1)
    }

    #[test]
    fn a_valid_manifest_loads() {
        let m = Scratch::new("valid").load(VALID).unwrap();
        assert_eq!(m.name, "app");
        assert!(m.home.is_absolute());
    }

    #[test]
    fn invalid_manifests_are_refused_naming_the_field() {
        let scratch = Scratch::new("invalid_manifests_ar");
        let cases = [
            (
                "stop_timeout",
                with("start =", "stop_timeout = -1.0\nstart ="),
            ),
            (
                "stop_timeout",
                with("start =", "stop_timeout = 0.0\nstart ="),
            ),
            (
                "stop_timeout",
                with("start =", "stop_timeout = 1e300\nstart ="),
            ),
            (
                "stop_timeout",
                with("start =", "stop_timeout = nan\nstart ="),
            ),
            (
                "min_uptime",
                with("start =", "min_uptime = 86401.0\nstart ="),
            ),
            (
                "health.interval",
                with("[health]", "[health]\ninterval = 0.0"),
            ),
            (
                "health.timeout",
                with("[health]", "[health]\ntimeout = inf"),
            ),
            ("deadline", with("start =", "deadline = 0\nstart =")),
            ("keep", with("start =", "keep = 0\nstart =")),
            (
                "install_timeout",
                with("start =", "install_timeout = 0.0\nstart ="),
            ),
            ("deadline", with("start =", "deadline = 604801\nstart =")),
            (
                "max_failed_starts",
                with("start =", "max_failed_starts = 0\nstart ="),
            ),
            ("name", with("\"app\"", "\"../x\"")),
            ("name", with("\"app\"", "\"\"")),
            ("name", with("\"app\"", "\"a b\"")),
            ("storage", with("data/app.db", "/abs")),
            ("storage", with("data/app.db", "../escape")),
            ("storage", with("data/app.db", "a/../../b")),
            ("storage", with("[\"data/app.db\"]", "[\"\"]")),
            ("reuse", with("node_modules", "/abs")),
            ("reuse", with("node_modules", "..")),
            ("port", with("port = 4790", "port = 4793")),
            (
                "health.url",
                with("http://127.0.0.1:{port}/", "https://127.0.0.1:{port}/"),
            ),
            (
                "health.url",
                with("http://127.0.0.1:{port}/", "http://localhost/"),
            ),
            (
                "health.url",
                with("http://127.0.0.1:{port}/", "http://[::1]/health"),
            ),
            (
                "health.url",
                with("http://127.0.0.1:{port}/", "http://host:80x/"),
            ),
        ];
        for (field, text) in cases {
            let err = scratch.load(&text).expect_err(&text);
            assert!(err.starts_with("invalid manifest "), "{err}");
            assert!(err.contains(field), "{field} not named in {err}");
        }
    }

    #[test]
    fn health_urls_with_a_port_are_accepted() {
        let scratch = Scratch::new("health_urls_with_a_p");
        for url in [
            "http://127.0.0.1:{port}/",
            "http://[::1]:4790/health",
            "http://localhost:8080",
        ] {
            scratch
                .load(&with("http://127.0.0.1:{port}/", url))
                .expect(url);
        }
    }

    #[test]
    fn reload_reports_changed_fields() {
        let scratch = Scratch::new("reload_reports_chang");
        let old = scratch.load(VALID).unwrap();
        assert_eq!(old.reload(&old, true), Ok(vec![]));

        let changed = with(
            "start = \"node app.js\"",
            "start = \"node b.js\"\n[env]\nA = \"1\"",
        );
        let new = scratch.load(&changed).unwrap();
        assert_eq!(old.reload(&new, true), Ok(vec!["start", "env"]));
    }

    #[test]
    fn reload_refuses_fields_fixed_for_the_daemons_lifetime() {
        let scratch = Scratch::new("reload_refuses_field");
        let old = scratch.load(VALID).unwrap();
        let new = scratch
            .load(&with("home = \"home\"", "home = \"elsewhere\""))
            .unwrap();
        let err = old.reload(&new, false).unwrap_err();
        assert!(err.contains("home") && err.contains("restart"), "{err}");
    }

    #[test]
    fn reload_refuses_a_new_port_only_while_the_app_runs() {
        let scratch = Scratch::new("reload_refuses_a_new");
        let old = scratch.load(VALID).unwrap();
        let new = scratch.load(&with("port = 4790", "port = 4780")).unwrap();
        let err = old.reload(&new, true).unwrap_err();
        assert!(err.contains("port") && err.contains("fawkes stop"), "{err}");
        assert_eq!(old.reload(&new, false), Ok(vec!["port"]));
    }
}
