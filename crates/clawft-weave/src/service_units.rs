//! Service-unit generators for the per-user daemon (ADR-103 Phase 1, package H).
//!
//! Pure text builders: nothing here touches the filesystem, `launchctl` or
//! `systemctl`. `weaver service unit` prints the result and the commands to
//! install it; the operator runs them.
//!
//! The unit runs `<exe> kernel start --foreground --profile user`, where
//! `exe` is the canonical path of the `weaver` that generated the unit. No
//! install prefix is hard-coded (the flaw in `scripts/com.clawft.wake.plist`).
//! After `weaver update` replaces that binary in place the unit keeps
//! pointing at the same path, so a restart picks up the new build.
//!
//! Per-project child kernels (Phase 2) are started by the daemon and must
//! survive its restart so `weaver update` does not take every project down:
//! launchd `AbandonProcessGroup` and systemd `KillMode=process` stop the
//! service manager from killing the rest of the daemon's process group or
//! cgroup. The restarted daemon adopts the survivors.

use std::path::{Path, PathBuf};

/// launchd label for the per-user daemon.
pub const LAUNCHD_LABEL: &str = "ai.weftos.user";

/// Set by the generated launchd plist. Only a daemon the service manager
/// started (it sees this) lifts the `REFUSED` sentinel on boot; a manual
/// `kernel start` must not, or launchd (watching the sentinel) would launch its
/// own, refused, instance every throttle interval beside it.
pub const SERVICE_MANAGER_ENV: &str = "WEFTOS_SERVICE_MANAGER";
/// Value [`SERVICE_MANAGER_ENV`] carries in the plist.
pub const SERVICE_MANAGER_LAUNCHD: &str = "launchd";

/// systemd user unit name (without the `.service` suffix).
pub const SYSTEMD_UNIT: &str = "weftos";

/// Exit code the daemon uses for a refused boot (EX_CONFIG). The systemd unit
/// lists it in `RestartPreventExitStatus`; launchd cannot filter on codes, so
/// its plist watches the `REFUSED` sentinel instead (see `boot_refusal`).
pub const REFUSED_EXIT: i32 = 78;

/// systemd `WatchdogSec=` for the generated units (card 1a919ae6). The
/// process feeds it from its liveness loop (`crate::liveness`) every third of
/// this, and only after a round where its runtime, locks and RPC answered; a
/// wedged daemon is killed and restarted (`Restart=`) after it lapses.
pub const WATCHDOG_SECS: u32 = 60;

/// launchd `ThrottleInterval`, seconds between restarts.
const LAUNCHD_THROTTLE_SECS: u32 = 30;

/// Arguments after the executable.
const ARGS: [&str; 5] = ["kernel", "start", "--foreground", "--profile", "user"];

/// `<home>/.weftos/run/kernel.log`, the launchd stdout/stderr target.
pub fn log_path(home: &Path) -> PathBuf {
    home.join(".weftos").join("run").join("kernel.log")
}

/// `<home>/.weftos/run/REFUSED`, the sentinel launchd's `KeepAlive` watches.
pub fn refused_path(home: &Path) -> PathBuf {
    clawft_types::runtime_paths::user_runtime_root(home).join(clawft_types::runtime_paths::REFUSED_FILE_NAME)
}

/// Canonical path of the running binary, as a unit would reference it.
///
/// A replaced-in-place binary reads back from `/proc/<pid>/exe` with a
/// ` (deleted)` suffix; that suffix is dropped before canonicalising.
pub fn canonical_exe(exe: &Path) -> PathBuf {
    let stripped = strip_deleted(exe);
    stripped.canonicalize().unwrap_or(stripped)
}

/// Drop the Linux ` (deleted)` marker from an exe path.
pub fn strip_deleted(exe: &Path) -> PathBuf {
    let s = exe.to_string_lossy();
    match s.strip_suffix(" (deleted)") {
        Some(p) => PathBuf::from(p),
        None => exe.to_path_buf(),
    }
}

fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

/// Quote one word for a systemd `ExecStart=` line: double quotes, with
/// `\`, `"`, `%` (specifier) and `$` (variable) escaped.
fn systemd_quote(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '%' => out.push_str("%%"),
            '$' => out.push_str("$$"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// `WorkingDirectory=` takes a path verbatim; only `%` needs escaping.
fn systemd_path(s: &str) -> String {
    s.replace('%', "%%")
}

const PLIST_TEMPLATE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>@LABEL@</string>
    <key>ProgramArguments</key>
    <array>
        <string>@EXE@</string>
@ARGS@    </array>
    <key>WorkingDirectory</key>
    <string>@HOME@</string>
    <key>EnvironmentVariables</key>
    <dict>
        <key>@MANAGER_ENV@</key>
        <string>@MANAGER@</string>
    </dict>
    <key>RunAtLoad</key>
    <true/>
    <key>ThrottleInterval</key>
    <integer>@THROTTLE@</integer>
    <key>KeepAlive</key>
    <dict>
        <key>PathState</key>
        <dict>
            <key>@REFUSED@</key>
            <false/>
        </dict>
    </dict>
    <key>AbandonProcessGroup</key>
    <true/>
    <key>StandardOutPath</key>
    <string>@LOG@</string>
    <key>StandardErrorPath</key>
    <string>@LOG@</string>
</dict>
</plist>
"#;

/// The launchd property list for `~/Library/LaunchAgents/ai.weftos.user.plist`.
pub fn launchd_plist(exe: &Path, home: &Path) -> String {
    let exe = xml_escape(&exe.to_string_lossy());
    let log = xml_escape(&log_path(home).to_string_lossy());
    let refused = xml_escape(&refused_path(home).to_string_lossy());
    let home = xml_escape(&home.to_string_lossy());
    let args: String = ARGS
        .iter()
        .map(|a| format!("        <string>{a}</string>\n"))
        .collect();
    PLIST_TEMPLATE
        .replace("@LABEL@", LAUNCHD_LABEL)
        .replace("@EXE@", &exe)
        .replace("@ARGS@", &args)
        .replace("@HOME@", &home)
        .replace("@LOG@", &log)
        .replace("@REFUSED@", &refused)
        .replace("@MANAGER_ENV@", SERVICE_MANAGER_ENV)
        .replace("@MANAGER@", SERVICE_MANAGER_LAUNCHD)
        .replace("@THROTTLE@", &LAUNCHD_THROTTLE_SECS.to_string())
}

/// The systemd user unit for `~/.config/systemd/user/weftos.service`.
pub fn systemd_user_unit(exe: &Path, home: &Path) -> String {
    let mut exec = systemd_quote(&exe.to_string_lossy());
    for a in ARGS {
        exec.push(' ');
        exec.push_str(a);
    }
    let home = systemd_path(&home.to_string_lossy());
    format!(
        "[Unit]\n\
Description=WeftOS per-user daemon\n\
Documentation=https://github.com/WeftOS/weftos\n\
StartLimitIntervalSec=60\n\
StartLimitBurst=3\n\
\n\
[Service]\n\
Type=simple\n\
WorkingDirectory={home}\n\
ExecStart={exec}\n\
KillMode=process\n\
Restart=on-failure\n\
RestartSec=3\n\
RestartPreventExitStatus={REFUSED_EXIT}\n\
NotifyAccess=main\n\
WatchdogSec={WATCHDOG_SECS}\n\
\n\
[Install]\n\
WantedBy=default.target\n"
    )
}

/// Which service manager a unit is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnitKind {
    Launchd,
    Systemd,
}

/// Refuse paths that would corrupt a unit: any control character (a newline
/// would start a new directive).
pub fn check_paths(exe: &Path, home: &Path) -> Result<(), String> {
    for (what, p) in [("executable", exe), ("home directory", home)] {
        if p.to_string_lossy().chars().any(char::is_control) {
            return Err(format!("the {what} path contains a control character; cannot write a unit for it"));
        }
    }
    Ok(())
}

/// Single-quote `s` for a POSIX shell (`'` becomes `'\''`).
pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// The path a unit should run: the PATH-visible spelling of `exe` when it
/// names the same file (a stable symlink such as `/usr/local/bin/weaver`
/// survives Homebrew or package version churn), else the canonical path.
pub fn stable_exe(exe: &Path, path_var: Option<&std::ffi::OsStr>) -> PathBuf {
    let canon = canonical_exe(exe);
    let Some(name) = canon.file_name() else { return canon };
    let Some(path_var) = path_var else { return canon };
    for dir in std::env::split_paths(path_var).filter(|d| d.is_absolute()) {
        let cand = dir.join(name);
        if cand.canonicalize().ok().as_deref() == Some(canon.as_path()) {
            return cand;
        }
    }
    canon
}

impl UnitKind {
    /// Unit text for this manager, or why the paths cannot be used.
    pub fn render(self, exe: &Path, home: &Path) -> Result<String, String> {
        check_paths(exe, home)?;
        Ok(match self {
            UnitKind::Launchd => launchd_plist(exe, home),
            UnitKind::Systemd => systemd_user_unit(exe, home),
        })
    }

    /// Conventional install location of the unit file.
    pub fn default_path(self, home: &Path) -> PathBuf {
        match self {
            UnitKind::Launchd => home
                .join("Library/LaunchAgents")
                .join(format!("{LAUNCHD_LABEL}.plist")),
            UnitKind::Systemd => home
                .join(".config/systemd/user")
                .join(format!("{SYSTEMD_UNIT}.service")),
        }
    }

    /// Install commands as text for a unit written to `file`. Never run.
    /// systemd only reads units from its own directories, so a `--out`
    /// elsewhere gets an `install` step first.
    pub fn install_commands(self, file: &Path, home: &Path) -> Vec<String> {
        let f = sh_quote(&file.to_string_lossy());
        match self {
            UnitKind::Launchd => vec![format!("launchctl bootstrap gui/$UID {f}")],
            UnitKind::Systemd => {
                let mut v = Vec::new();
                let dest = self.default_path(home);
                if file != dest {
                    v.push(format!("install -D -m 0644 {f} {}", sh_quote(&dest.to_string_lossy())));
                }
                v.push("systemctl --user daemon-reload".to_owned());
                v.push(format!("systemctl --user enable --now {SYSTEMD_UNIT}"));
                v
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn golden(name: &str) -> String {
        let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden").join(name);
        std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
    }

    #[test]
    fn launchd_matches_golden() {
        let got = launchd_plist(Path::new("/opt/weftos/bin/weaver"), Path::new("/Users/alice"));
        assert_eq!(got, golden("weftos-user.plist"));
    }

    #[test]
    fn systemd_matches_golden() {
        let got = systemd_user_unit(Path::new("/opt/weftos/bin/weaver"), Path::new("/home/alice"));
        assert_eq!(got, golden("weftos.service"));
    }

    #[test]
    fn no_hardcoded_install_prefix() {
        let exe = Path::new("/somewhere/else/weaver");
        for t in [
            launchd_plist(exe, Path::new("/h")),
            systemd_user_unit(exe, Path::new("/h")),
        ] {
            assert!(!t.contains("/usr/local/bin"));
            assert!(t.contains("/somewhere/else/weaver"));
        }
    }

    #[test]
    fn launchd_escapes_xml_in_paths() {
        let t = launchd_plist(Path::new("/a b/&<weaver>"), Path::new("/Users/jos\u{e9} \"x\""));
        assert!(t.contains("<string>/a b/&amp;&lt;weaver&gt;</string>"));
        assert!(t.contains("jos\u{e9} &quot;x&quot;/.weftos/run/kernel.log"));
        assert!(!t.contains("&<"));
    }

    #[test]
    fn systemd_quotes_exec_start() {
        let t = systemd_user_unit(Path::new("/a b/we\"av%er$"), Path::new("/h/100%"));
        assert!(t.contains("ExecStart=\"/a b/we\\\"av%%er$$\" kernel start --foreground --profile user\n"));
        assert!(t.contains("WorkingDirectory=/h/100%%\n"));
    }

    #[test]
    fn deleted_suffix_is_stripped() {
        assert_eq!(
            strip_deleted(Path::new("/opt/bin/weaver (deleted)")),
            PathBuf::from("/opt/bin/weaver")
        );
        assert_eq!(strip_deleted(Path::new("/opt/bin/weaver")), PathBuf::from("/opt/bin/weaver"));
    }

    #[test]
    fn install_commands_are_text_only() {
        let f = Path::new("/tmp/x.plist");
        let h = Path::new("/h");
        assert_eq!(
            UnitKind::Launchd.install_commands(f, h),
            vec!["launchctl bootstrap gui/$UID '/tmp/x.plist'".to_owned()]
        );
        let sd = UnitKind::Systemd.install_commands(f, h);
        assert_eq!(sd[0], "install -D -m 0644 '/tmp/x.plist' '/h/.config/systemd/user/weftos.service'");
        assert!(sd[2].ends_with("enable --now weftos"));
        // At the default path no install step is needed.
        let dflt = UnitKind::Systemd.default_path(h);
        assert_eq!(UnitKind::Systemd.install_commands(&dflt, h).len(), 2);
    }

    #[test]
    fn shell_quoting_survives_a_single_quote() {
        assert_eq!(sh_quote("/a/b'c"), "'/a/b'\\''c'");
        let c = UnitKind::Launchd.install_commands(Path::new("/x/it's.plist"), Path::new("/h"));
        assert_eq!(c[0], "launchctl bootstrap gui/$UID '/x/it'\\''s.plist'");
    }

    #[test]
    fn control_characters_in_paths_are_refused_and_escaped() {
        assert!(UnitKind::Systemd.render(Path::new("/a\nb/weaver"), Path::new("/h")).is_err());
        assert!(UnitKind::Launchd.render(Path::new("/a/weaver"), Path::new("/h\r")).is_err());
        assert!(UnitKind::Systemd.render(Path::new("/a/weaver"), Path::new("/h")).is_ok());
        // Defence in depth: even called directly, a newline never starts a directive.
        let t = systemd_user_unit(Path::new("/a\nExecStartPre=evil"), Path::new("/h"));
        assert!(!t.contains("\nExecStartPre"));
    }

    #[test]
    fn sentinel_matches_what_the_plist_watches_unless_the_runtime_dir_is_overridden() {
        use clawft_types::runtime_paths::RuntimePaths;
        let home = Path::new("/Users/alice");
        // No override: the daemon writes exactly the path the plist watches.
        let p = RuntimePaths::user_with(None, Some(home));
        assert_eq!(p.refused(), refused_path(home));
        assert!(launchd_plist(Path::new("/x/weaver"), home).contains(&p.refused().to_string_lossy().into_owned()));
        // WEFTOS_RUNTIME_DIR moves the daemon's root; the plist (which sets only
        // the service-manager marker) keeps watching ~/.weftos/run, so an override is not supervised.
        let o = RuntimePaths::user_with(Some("/tmp/iso"), Some(home));
        assert_eq!(o.refused(), Path::new("/tmp/iso/REFUSED"));
        assert_ne!(o.refused(), refused_path(home));
    }

    #[test]
    fn crash_loop_guards_are_in_the_units() {
        let sd = systemd_user_unit(Path::new("/x/weaver"), Path::new("/h"));
        for k in ["RestartPreventExitStatus=78", "StartLimitIntervalSec=60", "StartLimitBurst=3"] {
            assert!(sd.contains(k), "{k}");
        }
        let pl = launchd_plist(Path::new("/x/weaver"), Path::new("/h"));
        assert!(pl.contains("<key>ThrottleInterval</key>\n    <integer>30</integer>"));
        // A permanent refusal stops the cycle: KeepAlive is the sentinel's
        // absence, not "any non-zero exit".
        assert!(pl.contains("<key>PathState</key>"));
        assert!(pl.contains("<key>/h/.weftos/run/REFUSED</key>\n            <false/>"));
        assert!(!pl.contains("SuccessfulExit"));
        // Only the launchd-started instance may lift the sentinel.
        assert!(pl.contains("<key>EnvironmentVariables</key>\n    <dict>\n        <key>WEFTOS_SERVICE_MANAGER</key>\n        <string>launchd</string>"));
    }

    #[cfg(unix)]
    #[test]
    fn stable_exe_prefers_the_path_visible_symlink() {
        let d = tempfile::tempdir().unwrap();
        let real = d.path().join("Cellar/1.0/bin");
        let bin = d.path().join("bin");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(real.join("weaver"), "x").unwrap();
        std::os::unix::fs::symlink(real.join("weaver"), bin.join("weaver")).unwrap();
        let canon_real = real.join("weaver").canonicalize().unwrap();
        let path = std::env::join_paths([Path::new("relative"), bin.as_path()]).unwrap();
        assert_eq!(stable_exe(&canon_real, Some(&path)), bin.join("weaver"));
        // A PATH entry naming a different file is ignored.
        let other = d.path().join("other");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join("weaver"), "y").unwrap();
        let path = std::env::join_paths([other.as_path()]).unwrap();
        assert_eq!(stable_exe(&canon_real, Some(&path)), canon_real);
        assert_eq!(stable_exe(&canon_real, None), canon_real);
    }
}
