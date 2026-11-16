//! `fawkes service`: run the supervisor (`fawkes up --all`), and with it every app's daemon in `$FAWKES_HOME`, as the
//! launchd user agent `dev.fawkes` (see DESIGN.md, "Service"). A client-side convenience that only writes a plist and
//! calls `launchctl`: the supervisor cannot tell it was started this way.

use clap::Subcommand;

/// What `fawkes service` does.
#[derive(Subcommand, Clone, Copy)]
pub enum Action {
    /// Install (or reinstall) the supervisor as the launchd agent dev.fawkes, run now, at login and after it fails
    /// (macOS only)
    Install,
    /// Stop the supervisor and the daemons it runs, and remove its plist; the state directories stay
    Uninstall,
    /// Stop the supervisor and every app's daemon it runs gracefully and start them again from the plist; each daemon's
    /// start rolls a pending switch back, as at any start. `fawkes reexec` picks up a new binary without stopping anything
    Restart,
}

/// The agent's label.
pub const LABEL: &str = "dev.fawkes";

/// Do `action` for the supervisor of the apps in `home`: the JSON result, and its text for a person.
#[cfg(not(target_os = "macos"))]
pub fn run(
    _action: Action,
    _home: &std::path::Path,
) -> crate::protocol::R<(serde_json::Value, String)> {
    Err(crate::protocol::err(
        crate::protocol::ErrorCode::Usage,
        "fawkes service needs launchd, so it runs on macOS only; elsewhere run `fawkes up --all` under your init \
         system (systemd user units are Later)",
    ))
}

#[cfg(target_os = "macos")]
pub use launchd::run;

#[cfg(target_os = "macos")]
mod launchd {
    use super::{Action, LABEL};
    use crate::discovery;
    use crate::fleet;
    use crate::manifest::Manifest;
    use crate::process::{Finished, run_bounded};
    use crate::protocol::{Error, ErrorCode, R, describe_status, err, invalid_state};
    use serde_json::{Value, json};
    use std::ffi::OsStr;
    use std::fs::DirBuilder;
    use std::os::unix::fs::DirBuilderExt;
    use std::os::unix::net::UnixStream;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    /// Slack on top of the supervisor's `ExitTimeOut` for `launchctl`, which `bootout` makes wait until it is gone.
    const LAUNCHCTL_SLACK: u64 = 30;
    /// How long a daemon may take to stop beyond its own bound (`Manifest::daemon_stop_wait`) before launchd sends the
    /// supervisor SIGKILL: its 2 s wait for the answers it owes (`ANSWER_WAIT`), the supervisor's wait after it.
    const EXIT_MARGIN: u64 = 10;

    pub fn run(action: Action, home: &Path) -> R<(Value, String)> {
        let service = Service::new(home)?;
        match action {
            Action::Install => service.install(),
            Action::Uninstall => service.uninstall(),
            Action::Restart => service.restart(),
        }
    }

    /// The agent: the supervisor of the apps in `home`, its plist and launchd domain.
    struct Service {
        home: PathBuf,
        plist: PathBuf,
        /// `gui/<uid>`, where a user agent lives.
        domain: String,
    }

    impl Service {
        fn new(home: &Path) -> R<Service> {
            let user = std::env::var_os("HOME")
                .map(PathBuf::from)
                .filter(|home| home.is_absolute())
                .ok_or_else(|| {
                    err(
                        ErrorCode::Usage,
                        "HOME must be set to an absolute path: the plist goes to $HOME/Library/LaunchAgents",
                    )
                })?;
            Ok(Service {
                home: home.to_path_buf(),
                plist: user.join(format!("Library/LaunchAgents/{LABEL}.plist")),
                domain: format!("gui/{}", rustix::process::getuid().as_raw()),
            })
        }

        // ------------------------------------------------ actions

        fn install(&self) -> R<(Value, String)> {
            // The plist names the binary as it was run, not its resolved path: a symlinked install (Homebrew's
            // `bin/fawkes` into a versioned `Cellar/`) keeps working after an upgrade removes the old version. The
            // build directory check looks at where it really is.
            let found = |e: std::io::Error| {
                err(
                    ErrorCode::ServiceFailed,
                    format!("cannot find the running fawkes binary: {e}"),
                )
            };
            let binary = std::env::current_exe()
                .and_then(std::path::absolute)
                .map_err(found)?;
            if let Some(build) = binary
                .canonicalize()
                .map_err(found)?
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
            let log = self.home.join("supervisor.log");
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
            let plist = agent_plist(
                utf8(&binary, "the fawkes binary")?,
                &variables,
                self.exit_timeout(),
                utf8(&log, "$FAWKES_HOME")?,
            );

            let replaced = self.plist.exists();
            // A loaded agent's supervisor is the one that answered, and the bootout has stopped it. Any other that
            // answers (one started from a terminal) holds `supervisor.lock`, so the agent's would fail at every start
            // and launchd would retry it for ever.
            let loaded = replaced && self.bootout()?;
            let socket = fleet::socket(&self.home);
            if !loaded && UnixStream::connect(&socket).is_ok() {
                return Err(invalid_state(format!(
                    "a supervisor already runs on {} (`fawkes up --all` from a terminal?); stop it (Ctrl-C or \
                     SIGTERM), then install the service",
                    socket.display()
                )));
            }
            if let Some(dir) = self.plist.parent() {
                std::fs::create_dir_all(dir).map_err(|e| failed_to("create", dir, &e))?;
            }
            // launchd opens `StandardErrorPath` before the supervisor exists to create the directory; 0700, as the
            // supervisor makes it.
            DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&self.home)
                .map_err(|e| failed_to("create", &self.home, &e))?;
            let staged = self.plist.with_extension("plist.tmp");
            std::fs::write(&staged, plist)
                .and_then(|()| std::fs::rename(&staged, &self.plist))
                .map_err(|e| failed_to("write", &self.plist, &e))?;
            self.bootstrap()?;

            let text = format!(
                "{} {LABEL} ({}); launchd runs the supervisor, and with it every app's daemon in {}, at login and \
                 after it fails; its log is {}",
                if replaced { "reinstalled" } else { "installed" },
                self.plist.display(),
                self.home.display(),
                log.display()
            );
            let result = json!({
                "label": LABEL,
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
                format!("uninstalled {LABEL}")
            } else {
                format!("{LABEL} was not installed")
            };
            let result = json!({"label": LABEL, "plist": self.plist, "removed": removed});
            Ok((result, text))
        }

        fn restart(&self) -> R<(Value, String)> {
            if !self.plist.exists() {
                return Err(err(
                    ErrorCode::NotFound,
                    format!("{LABEL} is not installed; `fawkes service install` installs it"),
                ));
            }
            // A bootout stops the supervisor with a SIGTERM, which stops every daemon it runs, and waits for it; the
            // bootstrap reads the plist afresh.
            self.bootout()?;
            self.bootstrap()?;
            let result = json!({"label": LABEL, "plist": self.plist});
            Ok((result, format!("restarted {LABEL}")))
        }

        // ------------------------------------------------ launchctl

        /// launchd's `ExitTimeOut`, the longest the supervisor's shutdown takes: it stops every daemon at once, so the
        /// longest of their shutdowns (`Daemon::shutdown`: its wait for a switch or rollback in flight, then
        /// `stop_timeout` of grace for the app), plus a margin. launchd's default of 20 s would SIGKILL it partway, and
        /// the next start would have to recover. Read from the manifests in `home` at install; an invalid one counts
        /// for nothing, since its daemon does not run.
        fn exit_timeout(&self) -> u64 {
            let stop = discovery::manifests(&self.home)
                .iter()
                .filter_map(|(_, path)| Manifest::load(path).ok())
                .map(|manifest| manifest.daemon_stop_wait())
                .max()
                .unwrap_or_default();
            stop.as_secs() + u64::from(stop.subsec_nanos() > 0) + EXIT_MARGIN
        }

        /// Unload the agent: whether it was loaded. launchd answers a `bootout` before it has let go of the service,
        /// and a `bootstrap` until then fails with `5: Input/output error`, so it waits until `print` no longer finds
        /// the service.
        fn bootout(&self) -> R<bool> {
            let target = format!("{}/{LABEL}", self.domain);
            let done = self.launchctl(["bootout", target.as_str()])?;
            if done.status.success() {
                let until = Instant::now() + Duration::from_secs(self.exit_timeout());
                while self.launchctl(["print", target.as_str()])?.status.success() {
                    if Instant::now() >= until {
                        return Err(err(
                            ErrorCode::ServiceFailed,
                            format!(
                                "launchd still has {target} after its bootout; try again in a moment"
                            ),
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                return Ok(true);
            }
            if done.status.code() == Some(3) && output_of(&done).contains("No such process") {
                return Ok(false);
            }
            Err(launchctl_failed("bootout", &done))
        }

        /// Load the agent, which starts the supervisor (`RunAtLoad`).
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

        /// `launchctl` from `PATH`, bounded: a `bootout` waits for the supervisor to stop.
        fn launchctl<S: AsRef<OsStr>>(&self, args: impl IntoIterator<Item = S>) -> R<Finished> {
            let mut command = Command::new("launchctl");
            command
                .args(args)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let bound = Duration::from_secs(self.exit_timeout() + LAUNCHCTL_SLACK);
            run_bounded(
                &mut command,
                bound,
                Duration::from_secs(2),
                "launchctl",
                |_| {},
            )
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
                describe_status(done.status),
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

    /// The plist of the agent: `binary up --all`, with `environment`, its stderr appended to `log`.
    fn agent_plist(
        binary: &str,
        environment: &[(&str, String)],
        exit_timeout: u64,
        log: &str,
    ) -> String {
        render_plist(
            LABEL,
            &[binary, "up", "--all"],
            environment,
            exit_timeout,
            log,
        )
    }

    /// The plist of an agent that runs `arguments`, restarts only after a failed exit, and gets `exit_timeout` seconds
    /// to stop before launchd kills it. Without an environment the key is left out.
    fn render_plist(
        label: &str,
        arguments: &[&str],
        environment: &[(&str, String)],
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
        fn the_agent_runs_the_supervisor_and_logs_to_fawkes_home() {
            let env = [("FAWKES_HOME", "/h/fh".to_string())];
            let text = agent_plist("/bin/fawkes", &env, 45, "/h/fh/supervisor.log");
            let has = |lines: &[&str]| {
                let block = lines.join("\n");
                assert!(text.contains(&block), "{block}\nnot in\n{text}");
            };
            has(&["\t<key>Label</key>", "\t<string>dev.fawkes</string>"]);
            has(&[
                "\t<key>ProgramArguments</key>",
                "\t<array>",
                "\t\t<string>/bin/fawkes</string>",
                "\t\t<string>up</string>",
                "\t\t<string>--all</string>",
                "\t</array>",
            ]);
            has(&["\t\t<key>FAWKES_HOME</key>", "\t\t<string>/h/fh</string>"]);
            has(&["\t<key>ExitTimeOut</key>", "\t<integer>45</integer>"]);
            has(&[
                "\t<key>StandardErrorPath</key>",
                "\t<string>/h/fh/supervisor.log</string>",
            ]);
        }

        #[test]
        fn a_plist_without_an_environment_has_no_environment_key() {
            let text = render_plist(LABEL, &["/bin/fawkes"], &[], 15, "/h/supervisor.log");
            assert!(!text.contains("EnvironmentVariables"));
            assert!(text.contains("\t<integer>15</integer>\n"));
        }
    }
}
