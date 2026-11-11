//! Which app a command means (DESIGN.md, "CLI" → "Finding the app"): its daemon's socket or its manifest, from the
//! flags, the environment, the current directory and the manifests in `<fawkes_home>`. Pure over what it is given, so
//! the order is tested without touching the process's environment or a real `~/.fawkes`.

use serde::Deserialize;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

/// The manifest a project carries, found by walking up from the current directory.
const MARKER: &str = "fawkes.toml";

/// What the command line, the environment and the current directory say about the app.
pub struct Given {
    /// `--socket`.
    pub socket: Option<PathBuf>,
    /// `--manifest`: a path, or an app name in `<fawkes_home>`.
    pub manifest: Option<OsString>,
    /// `FAWKES_SOCKET`, which Fawkes sets for the processes it starts.
    pub socket_env: Option<OsString>,
    /// `FAWKES_MANIFEST`, read like `--manifest`.
    pub manifest_env: Option<OsString>,
    pub cwd: Result<PathBuf, String>,
    pub fawkes_home: Result<PathBuf, String>,
}

/// Where a command reaches its daemon.
#[derive(Debug, PartialEq, Eq)]
pub enum Target {
    /// A socket named outright. `inherited`: it is `FAWKES_SOCKET`, so the daemon is the one that started the caller.
    Socket { path: PathBuf, inherited: bool },
    /// The socket of this manifest.
    Manifest(PathBuf),
}

impl Given {
    /// The flags as parsed, the rest from the process: an empty variable counts as unset.
    pub fn new(socket: Option<PathBuf>, manifest: Option<OsString>) -> Given {
        let variable = |name| std::env::var_os(name).filter(|value| !value.is_empty());
        Given {
            socket,
            manifest,
            socket_env: variable("FAWKES_SOCKET"),
            manifest_env: variable("FAWKES_MANIFEST"),
            cwd: std::env::current_dir()
                .map_err(|e| format!("cannot read the current directory: {e}")),
            fawkes_home: crate::manifest::fawkes_home(),
        }
    }

    /// Whether a flag or a variable names the app, so `watch` follows that one daemon instead of every daemon.
    pub fn names_one(&self) -> bool {
        self.socket.is_some()
            || self.manifest.is_some()
            || self.socket_env.is_some()
            || self.manifest_env.is_some()
    }

    /// The daemon a client command talks to: `--socket`, `--manifest`, `FAWKES_SOCKET`, `FAWKES_MANIFEST`, the
    /// nearest `fawkes.toml`, then the manifest in `<fawkes_home>` whose project or repo holds the current directory.
    /// The flags come before the environment, so an app Fawkes runs (which inherits `FAWKES_SOCKET`) reaches another
    /// app's daemon by naming it, and its own without a word.
    pub fn target(&self) -> Result<Target, String> {
        if let Some(path) = &self.socket {
            return Ok(Target::Socket {
                path: path.clone(),
                inherited: false,
            });
        }
        if let Some(name) = &self.manifest {
            return self.named(name, "--manifest").map(Target::Manifest);
        }
        if let Some(path) = &self.socket_env {
            return Ok(Target::Socket {
                path: path.into(),
                inherited: true,
            });
        }
        self.unflagged_manifest().map(Target::Manifest)
    }

    /// The manifest of a command that needs one (`daemon`, `service`): the same order without the two sockets.
    pub fn manifest(&self) -> Result<PathBuf, String> {
        match &self.manifest {
            Some(name) => self.named(name, "--manifest"),
            None => self.unflagged_manifest(),
        }
    }

    /// `FAWKES_MANIFEST`, the nearest `fawkes.toml`, then the registry.
    fn unflagged_manifest(&self) -> Result<PathBuf, String> {
        if let Some(name) = &self.manifest_env {
            return self.named(name, "FAWKES_MANIFEST");
        }
        let cwd = self.cwd.as_ref()?;
        let (found, top) = walk_up(cwd, self.fawkes_home.as_deref().ok());
        if let Some(path) = found {
            return Ok(path);
        }
        let home = self.fawkes_home.as_ref()?;
        let mut matches = registry(home, cwd);
        let walked = top.map_or_else(
            || {
                format!(
                    "the search for a {MARKER} does not enter {}",
                    home.display()
                )
            },
            |top| {
                format!(
                    "there is no {MARKER} in it or above it up to {}",
                    top.display()
                )
            },
        );
        match matches.len() {
            1 => Ok(matches.remove(0).1),
            0 => Err(format!(
                "no app found for {}: {walked}, and no manifest in {} has its project or repo here; pass --manifest \
                 with an app name or a path{}",
                cwd.display(),
                home.display(),
                listed(home)
            )),
            _ => Err(format!(
                "{} is in the project or repo of more than one app in {}: {}; pass --manifest with the name of one",
                cwd.display(),
                home.display(),
                matches
                    .iter()
                    .map(|(name, _)| name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }

    /// A `--manifest` or `FAWKES_MANIFEST` value: a path when it has a `/` or ends in `.toml`, else the name of
    /// `<fawkes_home>/<name>.toml`, which must exist.
    fn named(&self, value: &OsStr, source: &str) -> Result<PathBuf, String> {
        let text = value.to_string_lossy();
        if text.contains('/') || text.ends_with(".toml") {
            return Ok(value.into());
        }
        let home = self.fawkes_home.as_ref()?;
        let path = home.join(format!("{text}.toml"));
        if path.is_file() {
            return Ok(path);
        }
        Err(format!(
            "{source} names the app {text}, but there is no {}; pass a path to its manifest, or an app name{}",
            path.display(),
            listed(home)
        ))
    }
}

/// The nearest `fawkes.toml` from `cwd` up, stopping at the first directory with a `.git` (the repository root), so
/// a project inside another never takes the outer one's manifest; and the last directory looked in, none when `cwd`
/// is `fawkes_home`. `fawkes_home` ends the walk before it is read: its `fawkes.toml` is the app named `fawkes`, not
/// the manifest of the state directories below it.
fn walk_up<'a>(cwd: &'a Path, fawkes_home: Option<&Path>) -> (Option<PathBuf>, Option<&'a Path>) {
    let fawkes_home = fawkes_home.and_then(|home| home.canonicalize().ok());
    let mut top = None;
    for dir in cwd.ancestors() {
        if fawkes_home.is_some() && dir.canonicalize().ok() == fawkes_home {
            break;
        }
        top = Some(dir);
        let file = dir.join(MARKER);
        if file.is_file() {
            return (Some(file), top);
        }
        if dir.join(".git").exists() {
            break;
        }
    }
    (None, top)
}

/// The part of a manifest that matching reads: the loading that follows checks the rest.
#[derive(Deserialize)]
struct Place {
    repo: PathBuf,
    project: Option<PathBuf>,
}

/// The apps of `<fawkes_home>/*.toml` whose `project`, or `repo` when it names no project, is `cwd` or one of its
/// parents, by name, sorted. A manifest with a `project` never matches by its `repo`: apps that share one repository
/// (every Ultimate host runs from the Ultimate clone) are told apart by their project. A manifest that cannot be read
/// or names no directory that exists matches nothing.
fn registry(home: &Path, cwd: &Path) -> Vec<(String, PathBuf)> {
    let Ok(cwd) = cwd.canonicalize() else {
        return Vec::new();
    };
    manifests(home)
        .into_iter()
        .filter(|(_, path)| {
            let Ok(text) = std::fs::read_to_string(path) else {
                return false;
            };
            let Ok(Place { repo, project }) = toml::from_str(&text) else {
                return false;
            };
            // Either resolves as `Manifest::load` resolves `repo`: against the manifest's directory.
            home.join(project.unwrap_or(repo))
                .canonicalize()
                .is_ok_and(|dir| cwd.starts_with(dir))
        })
        .collect()
}

/// Every `<fawkes_home>/<name>.toml`, sorted by name.
fn manifests(home: &Path) -> Vec<(String, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(home) else {
        return Vec::new();
    };
    let mut found: Vec<(String, PathBuf)> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension() == Some(OsStr::new("toml")) && path.is_file())
        .filter_map(|path| {
            let name = path.file_stem()?.to_str()?.to_string();
            Some((name, path))
        })
        .collect();
    found.sort();
    found
}

/// ` (the apps in <home>: a, b)`, or nothing when it has none, for an error that asks for a name.
fn listed(home: &Path) -> String {
    let names: Vec<String> = manifests(home).into_iter().map(|(name, _)| name).collect();
    if names.is_empty() {
        String::new()
    } else {
        format!(" (the apps in {}: {})", home.display(), names.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A world: `home` is the `fawkes_home`, `work/` a repository (with `.git`) holding `deep/er`, `outer/` a directory
    /// with a `fawkes.toml` above the repository.
    struct World {
        _temp: tempfile::TempDir,
        root: PathBuf,
        home: PathBuf,
    }

    impl World {
        fn new() -> World {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().canonicalize().unwrap();
            let home = root.join("home");
            std::fs::create_dir_all(&home).unwrap();
            std::fs::create_dir_all(root.join("outer/work/.git")).unwrap();
            std::fs::create_dir_all(root.join("outer/work/deep/er")).unwrap();
            std::fs::write(root.join("outer").join(MARKER), "").unwrap();
            World {
                _temp: temp,
                root,
                home,
            }
        }

        /// `<home>/<name>.toml` with the given `repo` line.
        fn app(&self, name: &str, repo: &Path) -> PathBuf {
            let path = self.home.join(format!("{name}.toml"));
            std::fs::write(&path, format!("name = \"{name}\"\nrepo = {repo:?}\n")).unwrap();
            path
        }

        fn given(&self, cwd: &str) -> Given {
            Given {
                socket: None,
                manifest: None,
                socket_env: None,
                manifest_env: None,
                cwd: Ok(self.root.join(cwd)),
                fawkes_home: Ok(self.home.clone()),
            }
        }
    }

    #[expect(
        clippy::unnecessary_wraps,
        reason = "compared with what `target` returns"
    )]
    fn manifest(path: impl Into<PathBuf>) -> Result<Target, String> {
        Ok(Target::Manifest(path.into()))
    }

    #[test]
    fn each_source_beats_the_ones_after_it() {
        let w = World::new();
        let work = w.root.join("outer/work");
        let xana = w.app("xana", &work);
        let blog = w.app("blog", &w.root.join("elsewhere"));
        std::fs::write(work.join(MARKER), "").unwrap();

        let mut g = w.given("outer/work/deep");
        g.socket = Some("/s/flag.sock".into());
        g.manifest = Some("blog".into());
        g.socket_env = Some("/s/env.sock".into());
        g.manifest_env = Some("xana".into());
        let flag = Target::Socket {
            path: "/s/flag.sock".into(),
            inherited: false,
        };
        assert_eq!(g.target(), Ok(flag));
        g.socket = None;
        // An explicit --manifest beats an inherited FAWKES_SOCKET.
        assert_eq!(g.target(), manifest(&blog));
        g.manifest = None;
        let inherited = Target::Socket {
            path: "/s/env.sock".into(),
            inherited: true,
        };
        assert_eq!(g.target(), Ok(inherited));
        g.socket_env = None;
        assert_eq!(g.target(), manifest(&xana));
        g.manifest_env = None;
        // The nearest fawkes.toml beats the registry, which matches here too.
        assert_eq!(g.target(), manifest(work.join(MARKER)));
        std::fs::remove_file(work.join(MARKER)).unwrap();
        assert_eq!(g.target(), manifest(&xana));
    }

    #[test]
    fn a_command_that_needs_a_manifest_skips_the_sockets() {
        let w = World::new();
        let mut g = w.given("outer");
        g.socket = Some("/s/flag.sock".into());
        g.socket_env = Some("/s/env.sock".into());
        assert_eq!(g.manifest(), Ok(w.root.join("outer").join(MARKER)));
        g.manifest_env = Some("x.toml".into());
        assert_eq!(g.manifest(), Ok("x.toml".into()));
        g.manifest = Some("dir/y".into());
        assert_eq!(g.manifest(), Ok("dir/y".into()));
    }

    #[test]
    fn the_walk_up_stops_at_the_repository_root() {
        let w = World::new();
        // From below `outer/`, without a repository between: found.
        std::fs::create_dir_all(w.root.join("outer/plain/sub")).unwrap();
        assert_eq!(
            w.given("outer/plain/sub").target(),
            manifest(w.root.join("outer").join(MARKER))
        );
        // From inside `work/`, whose `.git` ends the walk: the outer manifest is never taken.
        let err = w.given("outer/work/deep/er").target().unwrap_err();
        assert!(
            err.contains(&format!("up to {}", w.root.join("outer/work").display())),
            "{err}"
        );
        // A `.git` file, as in a git worktree, ends it too; its own fawkes.toml is still found.
        std::fs::create_dir_all(w.root.join("outer/tree/sub")).unwrap();
        std::fs::write(w.root.join("outer/tree/.git"), "gitdir: x").unwrap();
        assert!(w.given("outer/tree/sub").target().is_err());
        std::fs::write(w.root.join("outer/tree").join(MARKER), "").unwrap();
        assert_eq!(
            w.given("outer/tree/sub").target(),
            manifest(w.root.join("outer/tree").join(MARKER))
        );
        // fawkes_home ends it too: its fawkes.toml is the app `fawkes`, not that of a state directory below it.
        w.app("fawkes", &w.root.join("elsewhere"));
        std::fs::create_dir_all(w.home.join("xana/snapshots")).unwrap();
        let err = w.given("home/xana/snapshots").target().unwrap_err();
        assert!(
            err.contains(&format!("up to {}", w.home.join("xana").display())),
            "{err}"
        );
    }

    #[test]
    fn the_registry_matches_the_repo_and_refuses_when_several_do() {
        let w = World::new();
        let work = w.root.join("outer/work");
        // Relative to the manifest's directory, as Manifest::load resolves it.
        let fawkes = w.app("fawkes", Path::new("../outer/work"));
        w.app("blog", &w.root.join("elsewhere"));
        std::fs::write(w.home.join("broken.toml"), "repo = [").unwrap();
        assert_eq!(w.given("outer/work/deep/er").target(), manifest(&fawkes));
        assert_eq!(w.given("outer/work").target(), manifest(&fawkes));

        w.app("xana", &work);
        let err = w.given("outer/work/deep").target().unwrap_err();
        assert!(
            err.contains("more than one app") && err.contains(": fawkes, xana; pass --manifest"),
            "{err}"
        );

        // Nothing matches: the error names the apps there are.
        std::fs::create_dir_all(w.root.join("lone/.git")).unwrap();
        let err = w.given("lone").target().unwrap_err();
        assert!(err.starts_with("no app found for"), "{err}");
        assert!(err.contains(": blog, broken, fawkes, xana)"), "{err}");
    }

    #[test]
    fn a_manifest_with_a_project_matches_by_it_and_never_by_its_repo() {
        let w = World::new();
        let work = w.root.join("outer/work");
        std::fs::create_dir_all(w.root.join("xana/.git")).unwrap();
        std::fs::create_dir_all(w.root.join("xana/deep")).unwrap();
        std::fs::create_dir_all(w.root.join("blog/.git")).unwrap();
        // Two hosts of one repository, told apart by their projects; `xana`'s is relative to the manifest's directory.
        let project = |name: &str, project: &str| {
            let path = w.home.join(format!("{name}.toml"));
            std::fs::write(
                &path,
                format!("name = \"{name}\"\nrepo = {work:?}\nproject = {project:?}\n"),
            )
            .unwrap();
            path
        };
        let xana = project("xana", "../xana");
        let blog = project("blog", &w.root.join("blog").to_string_lossy());
        let fawkes = w.app("fawkes", &work);
        assert_eq!(w.given("xana/deep").target(), manifest(&xana));
        assert_eq!(w.given("blog").target(), manifest(&blog));
        // In the shared repository only the app without a project matches.
        assert_eq!(w.given("outer/work/deep").target(), manifest(&fawkes));
        std::fs::remove_file(&fawkes).unwrap();
        let err = w.given("outer/work/deep").target().unwrap_err();
        assert!(err.contains("has its project or repo here"), "{err}");
    }

    #[test]
    fn a_manifest_value_is_a_path_or_an_app_name() {
        let w = World::new();
        let xana = w.app("xana", &w.root);
        let mut g = w.given("outer/work");
        for path in ["fawkes.toml", "./x", "/abs/app", "dir/app.toml"] {
            g.manifest = Some(path.into());
            assert_eq!(g.target(), manifest(path), "{path}");
        }
        g.manifest = Some("xana".into());
        assert_eq!(g.target(), manifest(&xana));
        g.manifest = Some("nope".into());
        let err = g.target().unwrap_err();
        assert!(
            err.starts_with("--manifest names the app nope, but there is no ")
                && err.ends_with(": xana)"),
            "{err}"
        );
        g.manifest = None;
        g.manifest_env = Some("nope".into());
        assert!(g.target().unwrap_err().starts_with("FAWKES_MANIFEST names"));
    }

    #[test]
    fn only_a_flag_or_a_variable_names_one_daemon() {
        let w = World::new();
        let mut g = w.given("outer");
        assert!(!g.names_one());
        g.manifest_env = Some("x".into());
        assert!(g.names_one());
    }
}
