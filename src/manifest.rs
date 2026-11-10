//! The manifest: one TOML file per app (see DESIGN.md, "Manifest").

use crate::state::Paths;
use serde::de::DeserializeOwned;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::File;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

/// `$FAWKES_HOME`, else `~/.fawkes`: default state directories and the `run/` links `watch` discovers.
pub fn fawkes_home() -> Result<PathBuf, String> {
    fawkes_home_from(std::env::var_os("FAWKES_HOME"), std::env::var_os("HOME"))
}

/// `fawkes_home` for given values of the two variables, so a test need not change the process's environment. A
/// relative directory would resolve against whichever directory the daemon or client runs in: worktrees would be
/// created inside the repository, and the CLI would look for the socket in another place than the daemon.
fn fawkes_home_from(
    fawkes_home: Option<OsString>,
    home: Option<OsString>,
) -> Result<PathBuf, String> {
    let (variable, dir) = match (fawkes_home, home) {
        (Some(dir), _) => ("FAWKES_HOME", PathBuf::from(dir)),
        (None, Some(home)) => ("HOME", Path::new(&home).join(".fawkes")),
        (None, None) => return Err("neither FAWKES_HOME nor HOME is set".into()),
    };
    if dir.is_absolute() {
        Ok(dir)
    } else {
        Err(format!(
            "{variable} must be an absolute path, but it gives {}; set it to an absolute path",
            dir.display()
        ))
    }
}

/// A manifest in `home` must be `<name>.toml`: `--manifest <name>` finds an app there by its file name, and
/// everything else (`watch`, `status`, the state directory, the service label) shows its `name`, so the two must agree.
fn named_after_its_file(path: &Path, name: &str, home: &Path) -> Result<(), String> {
    let (Some(dir), Ok(home)) = (path.parent(), home.canonicalize()) else {
        return Ok(());
    };
    if dir.canonicalize().is_ok_and(|dir| dir == home)
        && path.file_name() != Some(format!("{name}.toml").as_ref())
    {
        return Err(format!(
            "the manifest {} names the app {name}, but its file name says otherwise; rename it to {}, since \
             `--manifest <name>` finds an app in {} by its file name",
            path.display(),
            home.join(format!("{name}.toml")).display(),
            home.display()
        ));
    }
    Ok(())
}

/// `<fawkes_home>/run`, where every daemon links its socket as `<name>.sock` for `fawkes watch` to find.
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
    pub hooks: Hooks,
    pub port: u16,
    pub spare_ports: [u16; 2],
    pub install: Option<String>,
    pub install_timeout: Duration,
    pub reuse: Vec<String>,
    /// How many generations `prune` keeps; without it nothing is pruned.
    pub keep: Option<u32>,
    pub start: String,
    pub deadline: u64,
    pub stop_timeout: Duration,
    pub min_uptime: Duration,
    pub max_failed_starts: u32,
    /// Roll a pending generation back as soon as its health check gives up, instead of at the deadline.
    pub rollback_on_unhealthy: bool,
    pub health: Health,
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Health {
    pub check: Check,
    pub interval: Duration,
    pub timeout: Duration,
}

/// The commands that carry the app's storage through a switch, a rollback and a test (DESIGN.md → Hooks). Without
/// them a rollback is code-only and a test starts on an empty home.
#[derive(Debug, Clone, PartialEq)]
pub struct Hooks {
    pub snapshot: Option<String>,
    pub restore: Option<String>,
    pub test_copy: Option<String>,
    pub timeout: Duration,
}

// ------------------------------------------------ reading the file

/// One TOML table whose keys are taken out one by one: what is left when the reading is done is what the manifest
/// does not know. A key that is missing or of the wrong type is recorded as a problem and reads as nothing, so one
/// pass finds every problem instead of stopping at the first.
struct Section {
    /// `""` for the top level, else the section's name (`health`).
    path: &'static str,
    table: toml::Table,
    /// The keys the section asked for, in the order of the manifest's documentation: the valid ones.
    keys: Vec<&'static str>,
    problems: Vec<String>,
}

impl Section {
    fn new(path: &'static str, table: toml::Table) -> Section {
        Section {
            path,
            table,
            keys: Vec::new(),
            problems: Vec::new(),
        }
    }

    /// `key` as the manifest names it: `health.url` inside `[health]`.
    fn full(&self, key: &str) -> String {
        if self.path.is_empty() {
            key.to_string()
        } else {
            format!("{}.{key}", self.path)
        }
    }

    /// The value of an optional key. None when it is absent or not a `T`, the second case recorded.
    fn take<T: DeserializeOwned>(&mut self, key: &'static str) -> Option<T> {
        self.keys.push(key);
        let value = self.table.remove(key)?;
        value
            .try_into()
            .map_err(|e: toml::de::Error| {
                let problem = format!("{}: {}", self.full(key), e.message());
                self.problems.push(problem);
            })
            .ok()
    }

    /// A duration in seconds, `default` when absent. One that is not above 0 and at most `MAX_SECONDS` is a problem:
    /// `Duration::from_secs_f64` and `Instant + Duration` panic on huge values, so the bound keeps every later use of
    /// it safe.
    fn seconds(&mut self, key: &'static str, default: f64) -> Duration {
        let seconds = self.take(key).unwrap_or(default);
        if seconds.is_finite() && seconds > 0.0 && seconds <= MAX_SECONDS {
            Duration::from_secs_f64(seconds)
        } else {
            self.problems.push(format!(
                "{} {seconds} must be a number of seconds above 0 and at most {MAX_SECONDS}",
                self.full(key)
            ));
            Duration::ZERO
        }
    }

    /// The value of a key the manifest cannot do without: absent is a problem too.
    fn require<T: DeserializeOwned>(&mut self, key: &'static str) -> Option<T> {
        if !self.table.contains_key(key) {
            self.keys.push(key);
            self.problems
                .push(format!("missing required key `{}`", self.full(key)));
            return None;
        }
        self.take(key)
    }

    /// The table under `key`, empty when absent or of another type (recorded). Its problems join this section's
    /// once it is `finish`ed.
    fn section(&mut self, key: &'static str) -> Section {
        let table = self.take(key).unwrap_or_default();
        Section::new(key, table)
    }

    /// The problems so far, and one for every key nobody asked for.
    fn finish(mut self) -> Vec<String> {
        let keys: Vec<String> = self.keys.iter().map(|key| format!("`{key}`")).collect();
        let within = if self.path.is_empty() {
            String::new()
        } else {
            format!(" in [{}]", self.path)
        };
        for key in self.table.keys() {
            self.problems.push(format!(
                "unknown key `{}`, the valid keys{within} are {}",
                self.full(key),
                keys.join(", ")
            ));
        }
        self.problems
    }
}

/// The manifest as written in the TOML file, before `load` has validated and resolved it. What could not be read is
/// None (and has been recorded as a problem), so the checks of `problems` run on what could.
struct Written {
    name: Option<String>,
    repo: Option<PathBuf>,
    state_dir: Option<PathBuf>,
    home: Option<PathBuf>,
    hooks: Hooks,
    port: Option<u16>,
    spare_ports: Option<[u16; 2]>,
    install: Option<String>,
    install_timeout: Duration,
    reuse: Vec<String>,
    keep: Option<u32>,
    start: Option<String>,
    deadline: u64,
    stop_timeout: Duration,
    min_uptime: Duration,
    max_failed_starts: u32,
    rollback_on_unhealthy: bool,
    /// Without a `url` or a `command` in `[health]`, the default url; None when both are given.
    check: Option<Check>,
    health_interval: Duration,
    health_timeout: Duration,
    env: BTreeMap<String, String>,
    /// Whether the file has `storage`, which `[hooks]` replaced.
    has_storage: bool,
}

impl Written {
    /// Read every key of `table`, with the problems found on the way: unknown keys, missing keys, wrong types.
    fn read(table: toml::Table) -> (Written, Vec<String>) {
        let mut top = Section::new("", table);
        let name = top.require("name");
        let repo = top.require("repo");
        let state_dir = top.take("state_dir");
        let home = top.require("home");
        let port: Option<u16> = top.require("port");
        let spare_ports = Self::read_spare_ports(&mut top, port);
        let install = top.take("install");
        let install_timeout = top.seconds("install_timeout", 1800.0);
        let reuse = top.take("reuse");
        let keep = top.take("keep");
        let start = top.require("start");
        let deadline = top.take("deadline");
        let stop_timeout = top.seconds("stop_timeout", 10.0);
        let min_uptime = top.seconds("min_uptime", 10.0);
        let max_failed_starts = top.take("max_failed_starts");
        let rollback_on_unhealthy = top.take("rollback_on_unhealthy");

        let mut health = top.section("health");
        let url = health.take("url");
        let command = health.take("command");
        let interval = health.seconds("interval", 0.5);
        let health_timeout = health.seconds("timeout", 60.0);
        let check = match (url, command) {
            (Some(url), None) => Some(Check::Url(url)),
            (None, Some(command)) => Some(Check::Command(command)),
            (None, None) => Some(Check::Url(DEFAULT_HEALTH_URL.into())),
            (Some(_), Some(_)) => None,
        };

        let mut hooks = top.section("hooks");
        let hooks_written = Hooks {
            snapshot: hooks.take("snapshot"),
            restore: hooks.take("restore"),
            test_copy: hooks.take("test_copy"),
            timeout: hooks.seconds("timeout", 600.0),
        };

        let mut env = BTreeMap::new();
        let env_table: toml::Table = top.take("env").unwrap_or_default();
        for (key, value) in env_table {
            match value {
                toml::Value::String(value) => {
                    env.insert(key, value);
                }
                other => top.problems.push(format!(
                    "env.{key} must be a string, not {}",
                    other.type_str()
                )),
            }
        }

        // `storage` is no key of the manifest, so it is not among the valid keys the unknown ones are listed with.
        let has_storage = top.table.remove("storage").is_some();
        let mut problems = health.finish();
        problems.extend(hooks.finish());
        problems.extend(top.finish());
        let written = Written {
            name,
            repo,
            state_dir,
            home,
            hooks: hooks_written,
            port,
            spare_ports,
            install,
            install_timeout,
            reuse: reuse.unwrap_or_default(),
            keep,
            start,
            deadline: deadline.unwrap_or_else(default_deadline),
            stop_timeout,
            min_uptime,
            max_failed_starts: max_failed_starts.unwrap_or_else(default_max_failed_starts),
            rollback_on_unhealthy: rollback_on_unhealthy.unwrap_or(false),
            check,
            health_interval: interval,
            health_timeout,
            env,
            has_storage,
        };
        (written, problems)
    }

    /// `spare_ports`: the range as written, else the ten ports after `port`.
    fn read_spare_ports(top: &mut Section, port: Option<u16>) -> Option<[u16; 2]> {
        let written = top.table.contains_key("spare_ports");
        match (top.take::<Vec<u16>>("spare_ports"), port) {
            (Some(ports), _) => <[u16; 2]>::try_from(&ports[..])
                .inspect_err(|_| {
                    top.problems.push(format!(
                        "spare_ports is a range [low, high], not a list: it has {} ports; give the first and the \
                         last port of the range",
                        ports.len()
                    ));
                })
                .ok(),
            // Written wrongly: recorded by `take`.
            (None, _) if written => None,
            // A missing port is a problem of its own.
            (None, None) => None,
            (None, Some(port)) if port > u16::MAX - SPARE_PORTS => {
                top.problems.push(format!(
                    "port {port} leaves no {SPARE_PORTS} ports after it for the default spare_ports: set \
                     spare_ports = [low, high], a range outside port"
                ));
                None
            }
            (None, Some(port)) => Some([port + 1, port + SPARE_PORTS]),
        }
    }
}

/// How many spare ports follow `port` when `spare_ports` is left out.
const SPARE_PORTS: u16 = 10;

/// The health check when `[health]` gives neither a `url` nor a `command`.
const DEFAULT_HEALTH_URL: &str = "http://127.0.0.1:{port}/";

/// The longest confirm window, in seconds. A `deadline` in the manifest and `switch --deadline` are both held to it,
/// so a typo cannot leave a generation unconfirmed (and its predecessor not rolled back to) for weeks.
pub const MAX_DEADLINE: u64 = 7 * 86_400;

/// The longest any manifest duration may be, in seconds (`Section::seconds`).
const MAX_SECONDS: f64 = 86_400.0;

fn default_deadline() -> u64 {
    120
}
fn default_max_failed_starts() -> u32 {
    3
}

/// What a health check does: the URL to fetch or the command to run, as written in the manifest (templates are
/// expanded by `health_check`).
#[derive(Debug, Clone, PartialEq)]
pub enum Check {
    Url(String),
    Command(String),
}

/// Values for the `{port}`, `{home}`, `{code}`, `{generation}` and `{state}` templates, and for the hooks'
/// `{snapshot}` and `{test_home}`, which stay as written where they are None.
pub struct Vars {
    pub port: u16,
    pub home: PathBuf,
    pub code: PathBuf,
    pub generation: String,
    pub state: PathBuf,
    pub snapshot: Option<PathBuf>,
    pub test_home: Option<PathBuf>,
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
                    "snapshot" => self.snapshot.as_ref()?.to_string_lossy().into_owned(),
                    "test_home" => self.test_home.as_ref()?.to_string_lossy().into_owned(),
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

/// Whether a `reuse` entry stays inside the directory it is joined onto.
fn is_inside(entry: &str) -> bool {
    !entry.is_empty()
        && Path::new(entry)
            .components()
            .all(|part| matches!(part, Component::Normal(_) | Component::CurDir))
        && Path::new(entry)
            .components()
            .any(|part| matches!(part, Component::Normal(_)))
}

/// Refuse a manifest whose owner is neither `owner` (the euid) nor root, or that other users can write: its commands
/// run as `owner`, so whoever can change it can run code as that user. Group-writable passes, as under a umask of
/// `002` with a group of one's own.
fn trusted(path: &Path, metadata: &std::fs::Metadata, owner: u32) -> Result<(), String> {
    let found = metadata.uid();
    if found != owner && found != 0 {
        return Err(format!(
            "manifest {} belongs to uid {found}, not to this user (uid {owner}) or root, and its commands would run as \
             this user; use a manifest of this user's",
            path.display()
        ));
    }
    if metadata.mode() & 0o002 != 0 {
        return Err(format!(
            "manifest {} can be changed by other users (mode {:o}), and its commands would run as this user; \
             run `chmod o-w {}`",
            path.display(),
            metadata.mode() & 0o7777,
            path.display()
        ));
    }
    Ok(())
}

/// Whether a raw `health.url` template is `http://` with an explicit port after the host: `{port}`, or a number
/// from 1 to 65535.
fn has_port(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("http://") else {
        return false;
    };
    let authority = rest.split('/').next().unwrap_or("");
    match authority.rsplit_once(':') {
        Some((host, port)) => {
            !host.is_empty()
                && (port == "{port}" || port.parse::<u16>().is_ok_and(|port| port != 0))
        }
        None => false,
    }
}

impl Manifest {
    /// Read and validate a manifest; relative paths resolve against its directory.
    pub fn load(path: &Path) -> Result<Manifest, String> {
        let cannot_read =
            |e: std::io::Error| format!("cannot read manifest {}: {e}", path.display());
        // One open file for the check and the read, so the text read is the file checked.
        let mut file = File::open(path).map_err(cannot_read)?;
        trusted(
            path,
            &file.metadata().map_err(cannot_read)?,
            rustix::process::geteuid().as_raw(),
        )?;
        let mut text = String::new();
        file.read_to_string(&mut text).map_err(cannot_read)?;
        let table: toml::Table = text
            .parse()
            .map_err(|e| format!("invalid manifest {}: {e}", path.display()))?;
        let (written, mut problems) = Written::read(table);
        problems.extend(written.problems());
        let invalid = |problems: &[String]| {
            format!(
                "invalid manifest {}: {}",
                path.display(),
                problems.join("; ")
            )
        };
        // What is missing or unreadable is among the problems, so there is something to say whenever this fails.
        let (
            Some(name),
            Some(repo),
            Some(home),
            Some(port),
            Some(spare_ports),
            Some(start),
            Some(check),
        ) = (
            written.name,
            written.repo,
            written.home,
            written.port,
            written.spare_ports,
            written.start,
            written.check,
        )
        else {
            return Err(invalid(&problems));
        };
        if !problems.is_empty() {
            return Err(invalid(&problems));
        }
        let absolute = std::path::absolute(path).map_err(|e| e.to_string())?;
        // Without a fawkes_home there is no app name to look up, so no name to disagree with.
        if let Ok(fawkes_home) = fawkes_home() {
            named_after_its_file(&absolute, &name, &fawkes_home)?;
        }
        let base = absolute.parent().unwrap_or(Path::new("/")).to_path_buf();
        let state_dir = match &written.state_dir {
            Some(dir) => base.join(dir),
            None => fawkes_home()?.join(&name),
        };
        Ok(Manifest {
            name,
            repo: base.join(&repo),
            state_dir,
            home: base.join(&home),
            hooks: written.hooks,
            port,
            spare_ports,
            install: written.install,
            install_timeout: written.install_timeout,
            reuse: written.reuse,
            keep: written.keep,
            start,
            deadline: written.deadline,
            stop_timeout: written.stop_timeout,
            min_uptime: written.min_uptime,
            max_failed_starts: written.max_failed_starts,
            rollback_on_unhealthy: written.rollback_on_unhealthy,
            health: Health {
                check,
                interval: written.health_interval,
                timeout: written.health_timeout,
            },
            env: written.env,
        })
    }

    /// Decide whether `new` may replace `self` while the daemon runs. Ok: the TOML names of the changed fields,
    /// possibly none. Err: a message naming every refused field and what to do instead. Both manifests are the loaded
    /// ones, so paths are compared after resolution.
    pub fn reload(&self, new: &Manifest, app_running: bool) -> Result<Vec<&'static str>, String> {
        // Every field is named, so a new one fails to compile until it is named here, and is an unused variable until
        // one of the two lists below compares it.
        let Manifest {
            name,
            repo,
            state_dir,
            home,
            hooks,
            port,
            spare_ports,
            install,
            install_timeout,
            reuse,
            keep,
            start,
            deadline,
            stop_timeout,
            min_uptime,
            max_failed_starts,
            rollback_on_unhealthy,
            health,
            env,
        } = self;
        // The daemon's identity, its state and the home a restore writes to are fixed for its lifetime: a changed
        // `home` would restore a snapshot into a different directory than the one it was taken from.
        let fixed = [
            ("name", *name != new.name),
            ("repo", *repo != new.repo),
            ("state_dir", *state_dir != new.state_dir),
            ("home", *home != new.home),
        ];
        let refused = differing(&fixed);
        let mut problems = Vec::new();
        if !refused.is_empty() {
            problems.push(format!(
                "{} can only change with a daemon restart: stop the daemon and start it again with the new manifest",
                refused.join(", ")
            ));
        }
        if app_running && *port != new.port {
            problems.push(
                "port cannot change while the app is running: `fawkes stop` the app first, then reload again".into(),
            );
        }
        if !problems.is_empty() {
            return Err(problems.join("; "));
        }
        let reloadable = [
            ("port", *port != new.port),
            ("spare_ports", *spare_ports != new.spare_ports),
            ("install", *install != new.install),
            ("install_timeout", *install_timeout != new.install_timeout),
            ("keep", *keep != new.keep),
            ("reuse", *reuse != new.reuse),
            ("start", *start != new.start),
            ("deadline", *deadline != new.deadline),
            ("stop_timeout", *stop_timeout != new.stop_timeout),
            ("min_uptime", *min_uptime != new.min_uptime),
            (
                "max_failed_starts",
                *max_failed_starts != new.max_failed_starts,
            ),
            (
                "rollback_on_unhealthy",
                *rollback_on_unhealthy != new.rollback_on_unhealthy,
            ),
            ("health", *health != new.health),
            ("hooks", *hooks != new.hooks),
            ("env", *env != new.env),
        ];
        Ok(differing(&reloadable))
    }

    /// How long a daemon shutting down waits for a switch, rollback or restart in flight to finish (`Daemon::shutdown`):
    /// one that stops the app and starts another takes up to twice `stop_timeout`, and 5 s for the rest of its steps.
    /// launchd's `ExitTimeOut` is derived from it, so the two cannot drift apart.
    pub fn shutdown_wait(&self) -> Duration {
        self.stop_timeout * 2 + Duration::from_secs(5)
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
    /// What is wrong with the values that could be read, each saying what is allowed. Empty when valid.
    fn problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        if let Some(name) = &self.name
            && (name.is_empty()
                || !name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'))
        {
            problems.push(format!(
                "name {name:?} must be non-empty and use only letters, digits, '_' and '-' (it becomes a file name)"
            ));
        }
        if self.check.is_none() {
            problems.push("[health] needs exactly one of url or command".into());
        }
        if self.port == Some(0) {
            problems.push("port must be between 1 and 65535".into());
        }
        if let Some([low, high]) = self.spare_ports {
            if low == 0 {
                problems.push("spare_ports must start at 1 or higher".into());
            }
            if low > high {
                problems.push("spare_ports must be [low, high]".into());
            } else if let Some(port) = self.port
                && (low..=high).contains(&port)
            {
                problems.push(format!(
                    "port {port} must lie outside spare_ports {:?}: the spare range is for staged generations",
                    [low, high]
                ));
            }
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
        for entry in &self.reuse {
            if !is_inside(entry) {
                problems.push(format!(
                    "reuse entry {entry:?} must be a non-empty relative path without '..' (it is joined onto the worktree)"
                ));
            }
        }
        problems.extend(self.hook_problems());
        if let Some(Check::Url(url)) = &self.check
            && url != DEFAULT_HEALTH_URL
            && !has_port(url)
        {
            problems.push(format!(
                "health.url {url:?} must start with http:// and end its host with :<port> (1 to 65535) or :{{port}}, for example http://127.0.0.1:{{port}}/"
            ));
        }
        problems
    }

    /// What is wrong with `[hooks]`, and with `storage`, which it replaced.
    fn hook_problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        if self.has_storage {
            problems.push(
                "storage is no longer read: Fawkes runs the app's own commands instead, `snapshot`, `restore` and `test_copy` in [hooks] (DESIGN.md → Hooks)"
                    .into(),
            );
        }
        let hooks = &self.hooks;
        // A snapshot nothing restores is useless, a restore without a snapshot has nothing to read, and an app whose
        // storage is worth a snapshot must not be tested on an empty home.
        for (hook, given, needs, needed) in [
            ("snapshot", &hooks.snapshot, "restore", &hooks.restore),
            ("restore", &hooks.restore, "snapshot", &hooks.snapshot),
            ("snapshot", &hooks.snapshot, "test_copy", &hooks.test_copy),
        ] {
            if given.is_some() && needed.is_none() {
                problems.push(format!("hooks.{hook} needs hooks.{needs} too"));
            }
        }
        // A template only the hooks get would stay as written anywhere else, and name a path that does not exist.
        let mut fields: Vec<(String, &str, &[&str])> = vec![
            ("start".into(), self.start.as_deref().unwrap_or(""), &[]),
            (
                "hooks.snapshot".into(),
                hooks.snapshot.as_deref().unwrap_or(""),
                &["{snapshot}"],
            ),
            (
                "hooks.restore".into(),
                hooks.restore.as_deref().unwrap_or(""),
                &["{snapshot}"],
            ),
            (
                "hooks.test_copy".into(),
                hooks.test_copy.as_deref().unwrap_or(""),
                &["{test_home}"],
            ),
            ("install".into(), self.install.as_deref().unwrap_or(""), &[]),
        ];
        match &self.check {
            Some(Check::Url(url)) => fields.push(("health.url".into(), url, &[])),
            Some(Check::Command(command)) => fields.push(("health.command".into(), command, &[])),
            None => {}
        }
        for (name, value) in &self.env {
            fields.push((format!("env.{name}"), value, &[]));
        }
        for (field, text, allowed) in fields {
            for template in ["{snapshot}", "{test_home}"] {
                if text.contains(template) && !allowed.contains(&template) {
                    problems.push(format!(
                        "{field} uses {template}, which only {} gets",
                        if template == "{snapshot}" {
                            "hooks.snapshot and hooks.restore"
                        } else {
                            "hooks.test_copy"
                        }
                    ));
                }
            }
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
            snapshot: None,
            test_home: None,
        }
    }

    #[test]
    fn templates_expand_in_one_pass() {
        let mut v = vars("/tmp/{port}");
        assert_eq!(
            v.expand("{home}:{port}:{nope}:{"),
            "/tmp/{port}:4790:{nope}:{"
        );
        // A hook's own template stays as written without its value.
        assert_eq!(v.expand("{snapshot}"), "{snapshot}");
        v.snapshot = Some(PathBuf::from("/s/g3/snapshot"));
        assert_eq!(v.expand("{snapshot}"), "/s/g3/snapshot");
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
port = 4790
spare_ports = [4791, 4795]
reuse = ["node_modules"]
start = "node app.js"
[health]
url = "http://127.0.0.1:{port}/"
[hooks]
snapshot = "store save {home} {snapshot}"
restore = "store load {snapshot} {home}"
test_copy = "store save {home} {test_home}"
"#;

    /// A temporary directory for manifests, removed on drop. Manifests loaded from one scratch resolve their paths
    /// against the same directory, so they can be compared by `reload`.
    struct Scratch(tempfile::TempDir);

    impl Scratch {
        fn new() -> Scratch {
            Scratch(tempfile::tempdir().unwrap())
        }

        fn load(&self, text: &str) -> Result<Manifest, String> {
            let path = self.0.path().join("fawkes.toml");
            std::fs::write(&path, text).unwrap();
            Manifest::load(&path)
        }
    }

    /// `VALID` with one line replaced: `from` must occur in it.
    fn with(from: &str, to: &str) -> String {
        assert!(VALID.contains(from), "{from:?} is not in VALID");
        VALID.replacen(from, to, 1)
    }

    #[test]
    fn a_valid_manifest_loads() {
        let m = Scratch::new().load(VALID).unwrap();
        assert_eq!(m.name, "app");
        assert!(m.home.is_absolute());
    }

    #[test]
    fn a_manifest_others_may_write_or_another_user_owns_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let scratch = Scratch::new();
        let path = scratch.0.path().join("fawkes.toml");
        std::fs::write(&path, VALID).unwrap();
        let me = rustix::process::geteuid().as_raw();
        let metadata = || std::fs::metadata(&path).unwrap();
        let set =
            |mode| std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();

        set(0o664);
        trusted(&path, &metadata(), me).unwrap();
        if me == 0 {
            // Root's manifests are trusted by every user, so as root the refusal is tried on another user's file.
            trusted(&path, &metadata(), 1).unwrap();
            std::os::unix::fs::chown(&path, Some(1), None).unwrap();
        }
        let refused = trusted(&path, &metadata(), metadata().uid() + 1).unwrap_err();
        assert!(refused.contains("belongs to uid"), "{refused}");
        if me == 0 {
            std::os::unix::fs::chown(&path, Some(0), None).unwrap();
        }
        set(0o646);
        let refused = trusted(&path, &metadata(), me).unwrap_err();
        assert!(refused.contains("changed by other users"), "{refused}");
        assert!(Manifest::load(&path).unwrap_err().contains("chmod o-w"));
    }

    #[test]
    fn invalid_manifests_are_refused_naming_the_field() {
        let scratch = Scratch::new();
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
            (
                "storage is no longer read",
                with("port = 4790", "storage = [\"app.db\"]\nport = 4790"),
            ),
            (
                "hooks.snapshot needs hooks.restore too",
                with("restore = \"store load {snapshot} {home}\"\n", ""),
            ),
            (
                "hooks.restore needs hooks.snapshot too",
                with("snapshot = \"store save {home} {snapshot}\"\n", ""),
            ),
            (
                "hooks.snapshot needs hooks.test_copy too",
                with("test_copy = \"store save {home} {test_home}\"\n", ""),
            ),
            ("hooks.timeout", with("[hooks]", "[hooks]\ntimeout = 0.0")),
            ("snapshoot", with("[hooks]", "[hooks]\nsnapshoot = \"x\"")),
            (
                "start uses {snapshot}",
                with("node app.js", "node app.js {snapshot}"),
            ),
            (
                "hooks.test_copy uses {snapshot}",
                with("{home} {test_home}", "{home} {snapshot}"),
            ),
            (
                "hooks.restore uses {test_home}",
                with("{snapshot} {home}", "{test_home} {home}"),
            ),
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
            ("port", with("port = 4790", "port = 0")),
            (
                "spare_ports",
                with("port = 4790", "port = 5000").replacen("[4791, 4795]", "[0, 4795]", 1),
            ),
            (
                "health.url",
                with("http://127.0.0.1:{port}/", "http://127.0.0.1:0/"),
            ),
            (
                "health.url",
                with("http://127.0.0.1:{port}/", "http://127.0.0.1:65536/"),
            ),
            (
                "health.url",
                with("http://127.0.0.1:{port}/", "http://127.0.0.1:99999999999/"),
            ),
            (
                "[health] needs exactly one of url or command",
                with("[health]", "[health]\ncommand = \"true\""),
            ),
            (
                "spare_ports must be [low, high]",
                with("[4791, 4795]", "[4795, 4791]"),
            ),
            // `deny_unknown_fields`: a misspelt key is an error, never ignored.
            ("stop_timout", with("start =", "stop_timout = 5\nstart =")),
            ("intervall", with("[health]", "[health]\nintervall = 1.0")),
            ("`start`", with("start = \"node app.js\"\n", "")),
        ];
        for (field, text) in cases {
            let err = scratch.load(&text).expect_err(&text);
            assert!(err.starts_with("invalid manifest "), "{err}");
            assert!(err.contains(field), "{field} not named in {err}");
        }
    }

    #[test]
    fn a_list_of_spare_ports_is_refused() {
        let text = with("[4791, 4795]", "[4791, 4792, 4793]");
        let err = Scratch::new().load(&text).unwrap_err();
        assert!(
            err.contains("spare_ports is a range [low, high], not a list"),
            "{err}"
        );
    }

    #[test]
    fn every_problem_of_a_manifest_is_named_at_once() {
        let text = r#"
name = "app"
repo = "repo"
port = "x"
stop_timout = 5
deadline = 0
start = "node app.js"
[health]
intervall = 1.0
[hooks]
snapshoot = "x"
[env]
A = 1
"#;
        let err = Scratch::new().load(text).unwrap_err();
        assert_eq!(err.lines().count(), 1, "{err}");
        // `storage` is refused with its own message, never offered as a valid key.
        assert!(!err.contains("`storage`"), "{err}");
        for problem in [
            "unknown key `stop_timout`",
            "unknown key `health.intervall`",
            "unknown key `hooks.snapshoot`",
            "missing required key `home`",
            "port: invalid type: string \"x\"",
            "deadline 0 must be between",
            "env.A must be a string",
        ] {
            assert!(err.contains(problem), "{problem} not named in {err}");
        }
    }

    #[test]
    fn a_minimal_manifest_gets_the_default_spare_ports_and_health_url() {
        let text = "name = \"app\"\nrepo = \"repo\"\nhome = \"home\"\nport = 4790\nstart = \"node app.js\"\n";
        let m = Scratch::new().load(text).unwrap();
        assert_eq!(m.spare_ports, [4791, 4800]);
        assert_eq!(
            m.health.check,
            Check::Url("http://127.0.0.1:{port}/".to_string())
        );
        assert_eq!(
            (m.health.interval, m.health.timeout),
            (Duration::from_millis(500), Duration::from_secs(60))
        );
    }

    #[test]
    fn a_manifest_with_two_problems_names_both_in_one_error() {
        let text = with("start =", "stop_timeout = 0.0\nkeep = 0\nstart =");
        let err = Scratch::new().load(&text).unwrap_err();
        assert!(err.starts_with("invalid manifest "), "{err}");
        assert!(
            err.contains("stop_timeout") && err.contains("keep"),
            "{err}"
        );
        assert_eq!(err.lines().count(), 1, "{err}");
    }

    /// The manifests the documentation shows: the example files, and every TOML sample in DESIGN.md. A copy goes
    /// into a scratch directory, because `load` refuses a file that others can write.
    #[test]
    fn the_documented_example_manifests_load() {
        let design = include_str!("../DESIGN.md");
        let mut documented = vec![
            (
                "examples/sample/fawkes.toml".to_string(),
                include_str!("../examples/sample/fawkes.toml").to_string(),
            ),
            (
                "examples/ultimate.toml".to_string(),
                include_str!("../examples/ultimate.toml").to_string(),
            ),
        ];
        let blocks: Vec<&str> = design
            .split("```toml\n")
            .skip(1)
            .filter_map(|rest| rest.split_once("```").map(|(block, _)| block))
            .collect();
        assert!(
            blocks.len() >= 2,
            "DESIGN.md has {} toml blocks",
            blocks.len()
        );
        documented.extend(
            blocks
                .into_iter()
                .enumerate()
                .map(|(i, block)| (format!("DESIGN.md toml block {}", i + 1), block.to_string())),
        );
        for (source, text) in documented {
            let manifest = Scratch::new()
                .load(&text)
                .unwrap_or_else(|e| panic!("{source}: {e}"));
            assert!(manifest.home.is_absolute(), "{source}");
        }
    }

    #[test]
    fn a_manifest_in_fawkes_home_is_named_after_its_file() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        std::fs::create_dir(&home).unwrap();
        let file = home.join("xana.toml");
        assert_eq!(named_after_its_file(&file, "xana", &home), Ok(()));
        let err = named_after_its_file(&file, "ultimate-xana", &home).unwrap_err();
        assert!(
            err.contains("names the app ultimate-xana") && err.contains("rename it to "),
            "{err}"
        );
        assert!(err.contains("ultimate-xana.toml, since"), "{err}");
        // Elsewhere a manifest's file name is its own business.
        let elsewhere = temp.path().join("fawkes.toml");
        assert_eq!(named_after_its_file(&elsewhere, "xana", &home), Ok(()));
    }

    #[test]
    fn a_relative_fawkes_home_or_home_is_refused() {
        let os = |s: &str| Some(OsString::from(s));
        for (fawkes_home, home) in [
            (os(".fawkes"), os("/home/me")),
            (os(""), os("/home/me")),
            (None, os("relative/home")),
        ] {
            let err = fawkes_home_from(fawkes_home, home).unwrap_err();
            assert!(err.contains("absolute path"), "{err}");
        }
        assert_eq!(
            fawkes_home_from(os("/srv/fawkes"), os("relative")),
            Ok(PathBuf::from("/srv/fawkes"))
        );
        assert_eq!(
            fawkes_home_from(None, os("/home/me")),
            Ok(PathBuf::from("/home/me/.fawkes"))
        );
        assert!(fawkes_home_from(None, None).is_err());
    }

    #[test]
    fn health_urls_with_a_port_are_accepted() {
        let scratch = Scratch::new();
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
        let scratch = Scratch::new();
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
        let scratch = Scratch::new();
        let old = scratch.load(VALID).unwrap();
        let new = scratch
            .load(&with("home = \"home\"", "home = \"elsewhere\""))
            .unwrap();
        let err = old.reload(&new, false).unwrap_err();
        assert!(err.contains("home") && err.contains("restart"), "{err}");
    }

    #[test]
    fn reload_refuses_a_new_port_only_while_the_app_runs() {
        let scratch = Scratch::new();
        let old = scratch.load(VALID).unwrap();
        let new = scratch.load(&with("port = 4790", "port = 4780")).unwrap();
        let err = old.reload(&new, true).unwrap_err();
        assert!(err.contains("port") && err.contains("fawkes stop"), "{err}");
        assert_eq!(old.reload(&new, false), Ok(vec!["port"]));
    }
}
