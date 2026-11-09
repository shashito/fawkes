//! `fawkes service`: run the daemon as a launchd user agent (see DESIGN.md, "Service"). A client-side convenience
//! that only writes a plist and calls `launchctl`: the daemon cannot tell it was started this way.

use clap::Subcommand;

/// What `fawkes service` does.
#[derive(Subcommand, Clone, Copy)]
pub enum Action {
    /// Install (or reinstall) this app's daemon as a launchd agent, run now, at login and after it fails (macOS only)
    Install,
    /// Stop the agent's daemon and remove its plist; the state directory stays
    Uninstall,
    /// Stop the agent's daemon gracefully and start it again from the plist; a daemon start rolls a pending switch
    /// back, as at any start
    Restart,
}

/// Do `action` for the app of the manifest at `manifest_path`: the JSON result, and its text for a person.
#[cfg(not(target_os = "macos"))]
pub fn run(
    _action: Action,
    _manifest_path: &std::path::Path,
) -> crate::protocol::R<(serde_json::Value, String)> {
    Err(crate::protocol::err(
        crate::protocol::ErrorCode::Usage,
        "fawkes service needs launchd, so it runs on macOS only; elsewhere run `fawkes daemon --manifest …` under \
         your init system (systemd user units are Later)",
    ))
}

#[cfg(target_os = "macos")]
pub use launchd::run;

#[cfg(target_os = "macos")]
mod launchd {
    use super::Action;
    use crate::manifest::Manifest;
    use crate::process::{Finished, run_bounded};
    use crate::protocol::{Error, ErrorCode, R, err, invalid_state};
    use serde_json::{Value, json};
    use std::ffi::OsStr;
    use std::fs::DirBuilder;
    use std::os::unix::fs::DirBuilderExt;
    use std::os::unix::net::UnixStream;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::time::Duration;

    /// Slack on top of the daemon's `ExitTimeOut` for `launchctl`, which `bootout` makes wait until the daemon is gone.
    const LAUNCHCTL_SLACK: u64 = 30;
    /// How long the daemon may take to stop beyond the manifest's `stop_timeout` (its own steps around stopping the
    /// app) before launchd sends SIGKILL.
    const EXIT_MARGIN: u64 = 10;

    pub fn run(action: Action, manifest_path: &Path) -> R<(Value, String)> {
        let service = Service::new(manifest_path)?;
        match action {
            Action::Install => service.install(manifest_path),
            Action::Uninstall => service.uninstall(),
            Action::Restart => service.restart(),
        }
    }

    /// The agent of one manifest: its label, plist and launchd domain.
    struct Service {
        manifest: Manifest,
        label: String,
        plist: PathBuf,
        /// `gui/<uid>`, where a user agent lives.
        domain: String,
    }

    impl Service {
        fn new(manifest_path: &Path) -> R<Service> {
            let manifest = Manifest::load(manifest_path).map_err(|e| err(ErrorCode::Usage, e))?;
            let home = std::env::var("HOME")
                .ok()
                .filter(|home| Path::new(home).is_absolute())
                .ok_or_else(|| {
                    err(
                        ErrorCode::Usage,
                        "HOME must be set to an absolute path: the plist goes to $HOME/Library/LaunchAgents",
                    )
                })?;
            let label = format!("dev.fawkes.{}", manifest.name);
            let plist = Path::new(&home)
                .join("Library/LaunchAgents")
                .join(format!("{label}.plist"));
            let domain = format!("gui/{}", rustix::process::getuid().as_raw());
            Ok(Service {
                manifest,
                label,
                plist,
                domain,
            })
        }

        // ------------------------------------------------ actions

        fn install(&self, manifest_path: &Path) -> R<(Value, String)> {
            let manifest_file = std::path::absolute(manifest_path).map_err(|e| {
                err(
                    ErrorCode::Usage,
                    format!("cannot resolve {}: {e}", manifest_path.display()),
                )
            })?;
            let binary = std::env::current_exe()
                .and_then(|path| path.canonicalize())
                .map_err(|e| {
                    err(
                        ErrorCode::ServiceFailed,
                        format!("cannot find the running fawkes binary: {e}"),
                    )
                })?;
            if let Some(build) = binary
                .ancestors()
                .skip(1)
                .find(|dir| dir.join("CACHEDIR.TAG").is_file())
            {
                return Err(err(
                    ErrorCode::Usage,
                    format!(
                        "{} is in the build directory {} (it holds a CACHEDIR.TAG), which the next `cargo build` \
                         replaces under the running daemon; install a copy with `cargo install --path . --locked` and \
                         run `fawkes service install` from it",
                        binary.display(),
                        build.display()
                    ),
                ));
            }
            let log = self.manifest.paths().root.join("daemon.log");
            let mut variables = Vec::new();
            for name in ["PATH", "FAWKES_HOME"] {
                if let Some(value) = std::env::var_os(name) {
                    let value = value.into_string().map_err(|_| {
                        err(
                            ErrorCode::Usage,
                            format!("{name} is not valid UTF-8, which the plist needs"),
                        )
                    })?;
                    variables.push((name, value));
                }
            }
            let environment: Vec<(&str, &str)> = variables
                .iter()
                .map(|(name, value)| (*name, value.as_str()))
                .collect();
            let plist = render_plist(
                &self.label,
                &[
                    utf8(&binary, "the fawkes binary")?,
                    "daemon",
                    "--manifest",
                    utf8(&manifest_file, "the manifest path")?,
                ],
                &environment,
                self.exit_timeout(),
                utf8(&log, "the state directory")?,
            );

            let replaced = self.plist.exists();
            // With a plist, an answering daemon is the service's own, and the bootout below stops it.
            let socket = self.manifest.paths().socket();
            if !replaced && UnixStream::connect(&socket).is_ok() {
                return Err(invalid_state(format!(
                    "a daemon for {} already runs on {} (started from a terminal?); stop it, then install the service",
                    self.manifest.name,
                    socket.display()
                )));
            }
            if let Some(dir) = self.plist.parent() {
                std::fs::create_dir_all(dir).map_err(|e| failed_to("create", dir, &e))?;
            }
            if replaced {
                self.bootout()?;
            }
            // launchd opens `StandardErrorPath` before the daemon exists to create the directory.
            let state_dir = &self.manifest.paths().root;
            DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(state_dir)
                .map_err(|e| failed_to("create", state_dir, &e))?;
            let staged = self.plist.with_extension("plist.tmp");
            std::fs::write(&staged, plist)
                .and_then(|()| std::fs::rename(&staged, &self.plist))
                .map_err(|e| failed_to("write", &self.plist, &e))?;
            self.bootstrap()?;

            let (label, plist) = (&self.label, self.plist.display());
            let text = format!(
                "{} {label} ({plist}); launchd runs the daemon at login and after it fails; its log is {}",
                if replaced { "reinstalled" } else { "installed" },
                log.display()
            );
            let result = json!({
                "label": label,
                "plist": self.plist,
                "log": log,
                "replaced": replaced,
            });
            Ok((result, text))
        }

        fn uninstall(&self) -> R<(Value, String)> {
            let loaded = self.bootout()?;
            let existed = self.plist.exists();
            if existed {
                std::fs::remove_file(&self.plist)
                    .map_err(|e| failed_to("remove", &self.plist, &e))?;
            }
            let removed = existed || loaded;
            let text = if removed {
                format!("uninstalled {}", self.label)
            } else {
                format!("{} was not installed", self.label)
            };
            let result = json!({"label": self.label, "plist": self.plist, "removed": removed});
            Ok((result, text))
        }

        fn restart(&self) -> R<(Value, String)> {
            if !self.plist.exists() {
                return Err(err(
                    ErrorCode::NotFound,
                    format!(
                        "{} is not installed; `fawkes service install --manifest <path>` installs it",
                        self.label
                    ),
                ));
            }
            // A bootout stops the daemon with a SIGTERM and waits for it; the bootstrap reads the plist afresh.
            self.bootout()?;
            self.bootstrap()?;
            let result = json!({"label": self.label, "plist": self.plist});
            Ok((result, format!("restarted {}", self.label)))
        }

        // ------------------------------------------------ launchctl

        /// launchd's `ExitTimeOut`: its default of 20 s would SIGKILL a daemon that is still stopping an app with a
        /// longer `stop_timeout`.
        fn exit_timeout(&self) -> u64 {
            let stop = Duration::from_secs_f64(self.manifest.stop_timeout);
            stop.as_secs() + u64::from(stop.subsec_nanos() > 0) + EXIT_MARGIN
        }

        /// Unload the agent: whether it was loaded.
        fn bootout(&self) -> R<bool> {
            let target = format!("{}/{}", self.domain, self.label);
            let done = self.launchctl(["bootout", target.as_str()])?;
            if done.status.success() {
                return Ok(true);
            }
            if done.status.code() == Some(3) && output_of(&done).contains("No such process") {
                return Ok(false);
            }
            Err(launchctl_failed("bootout", &done))
        }

        /// Load the agent, which starts the daemon (`RunAtLoad`).
        fn bootstrap(&self) -> R<()> {
            let done = self.launchctl([
                OsStr::new("bootstrap"),
                OsStr::new(&self.domain),
                self.plist.as_os_str(),
            ])?;
            if done.status.success() {
                return Ok(());
            }
            let error = launchctl_failed("bootstrap", &done);
            let hint = format!(
                "the plist stays at {}: `fawkes service install` again retries, `fawkes service uninstall` removes it",
                self.plist.display()
            );
            Err(error.with_hint(hint))
        }

        /// `launchctl` from `PATH`, bounded: a `bootout` waits for the daemon to stop.
        fn launchctl<S: AsRef<OsStr>>(&self, args: impl IntoIterator<Item = S>) -> R<Finished> {
            let mut command = Command::new("launchctl");
            command
                .args(args)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let bound = Duration::from_secs(self.exit_timeout() + LAUNCHCTL_SLACK);
            run_bounded(&mut command, bound, 2.0, "launchctl", |_| {})
                .map_err(|e| err(ErrorCode::ServiceFailed, e))
        }
    }

    // ------------------------------------------------ helpers

    /// What `launchctl` printed, stderr first.
    fn output_of(done: &Finished) -> String {
        format!("{}{}", done.stderr, done.stdout)
    }

    fn launchctl_failed(what: &str, done: &Finished) -> Error {
        let printed = output_of(done);
        let printed = printed.trim();
        err(
            ErrorCode::ServiceFailed,
            format!(
                "launchctl {what} failed ({}){}{printed}",
                done.status,
                if printed.is_empty() { "" } else { ": " }
            ),
        )
    }

    fn failed_to(verb: &str, path: &Path, error: &std::io::Error) -> Error {
        err(
            ErrorCode::ServiceFailed,
            format!("cannot {verb} {}: {error}", path.display()),
        )
    }

    /// `path` as text, which the plist (XML) needs.
    fn utf8<'a>(path: &'a Path, what: &str) -> R<&'a str> {
        path.to_str().ok_or_else(|| {
            err(
                ErrorCode::Usage,
                format!(
                    "{what} ({}) is not valid UTF-8, which the plist needs",
                    path.display()
                ),
            )
        })
    }

    // ------------------------------------------------ the plist

    /// The plist of an agent that runs `arguments`, restarts only after a failed exit, and gets `exit_timeout` seconds
    /// to stop before launchd kills it. Without an environment the key is left out.
    fn render_plist(
        label: &str,
        arguments: &[&str],
        environment: &[(&str, &str)],
        exit_timeout: u64,
        log: &str,
    ) -> String {
        let string = |depth: usize, text: &str| {
            format!("{}<string>{}</string>", "\t".repeat(depth), escape(text))
        };
        let mut lines = vec![
            r#"<?xml version="1.0" encoding="UTF-8"?>"#.to_string(),
            r#"<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">"#.to_string(),
            r#"<plist version="1.0">"#.to_string(),
            "<dict>".to_string(),
            "\t<key>Label</key>".to_string(),
            string(1, label),
            "\t<key>ProgramArguments</key>".to_string(),
            "\t<array>".to_string(),
        ];
        lines.extend(arguments.iter().map(|argument| string(2, argument)));
        lines.push("\t</array>".to_string());
        if !environment.is_empty() {
            lines.push("\t<key>EnvironmentVariables</key>".to_string());
            lines.push("\t<dict>".to_string());
            for (name, value) in environment {
                lines.push(format!("\t\t<key>{}</key>", escape(name)));
                lines.push(string(2, value));
            }
            lines.push("\t</dict>".to_string());
        }
        lines.extend(
            [
                "\t<key>RunAtLoad</key>",
                "\t<true/>",
                "\t<key>KeepAlive</key>",
                "\t<dict>",
                "\t\t<key>SuccessfulExit</key>",
                "\t\t<false/>",
                "\t</dict>",
                "\t<key>ExitTimeOut</key>",
            ]
            .map(String::from),
        );
        lines.push(format!("\t<integer>{exit_timeout}</integer>"));
        lines.push("\t<key>StandardErrorPath</key>".to_string());
        lines.push(string(1, log));
        lines.extend(["</dict>", "</plist>"].map(String::from));
        lines.join("\n") + "\n"
    }

    /// `text` for the inside of an XML element.
    fn escape(text: &str) -> String {
        text.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn escape_covers_the_three_characters_xml_text_cannot_hold() {
            assert_eq!(escape("a&b<c>d"), "a&amp;b&lt;c&gt;d");
            assert_eq!(escape("&lt;"), "&amp;lt;");
            assert_eq!(escape("/plain path-1"), "/plain path-1");
        }

        #[test]
        fn a_plist_without_an_environment_has_no_environment_key() {
            let text = render_plist("dev.fawkes.x", &["/bin/fawkes"], &[], 15, "/s/daemon.log");
            assert!(!text.contains("EnvironmentVariables"));
            assert!(text.contains("\t<integer>15</integer>\n"));
        }
    }
}
