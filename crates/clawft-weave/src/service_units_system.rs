//! System-level unit generators for the machine mesh service (ADR-103
//! Phase 3, package H).
//!
//! Pure text builders, like [`crate::service_units`]: nothing here touches
//! the filesystem, `launchctl`, `systemctl` or `dscl`. `weaver mesh
//! install-service` prints these inside a reviewed shell script; a human runs
//! it as an administrator.
//!
//! The service never runs as root and never from a user-writable path: the
//! unit points at [`SERVICE_EXE`], a root-owned copy. The runtime directory
//! (`/var/run/weftos`, holding `mesh.sock` and `service.json`) is owned by the
//! service account and its group with mode 0750, so a user reaches the socket
//! only when they are in that group ([`macos_group`] / [`linux_group`]).

use std::path::Path;

use crate::service_units::{WATCHDOG_SECS, sh_quote};

/// launchd label of the machine mesh service.
pub const MESH_LAUNCHD_LABEL: &str = "ai.weftos.mesh";
/// launchd label of the helper that recreates `/var/run/weftos` at boot
/// (macOS clears `/var/run`, and the service account cannot create it).
pub const MESH_RUNDIR_LABEL: &str = "ai.weftos.mesh-rundir";
/// systemd unit name of the machine mesh service.
pub const MESH_SYSTEMD_UNIT: &str = "weftos-mesh";

/// Root-owned copy of the binary the service runs.
pub const SERVICE_EXE: &str = "/usr/local/libexec/weftos/weaver";
/// Production `mesh.toml`.
pub const MESH_TOML: &str = "/etc/weftos/mesh.toml";
/// Service state (node.key, journal, bindings).
pub const STATE_DIR: &str = "/var/lib/weftos/mesh";
/// Directory of `mesh.sock` and `service.json`.
pub const RUN_DIR: &str = "/var/run/weftos";
/// The mesh-local socket.
pub const SOCKET: &str = "/var/run/weftos/mesh.sock";
/// Log directory (launchd; systemd logs to the journal).
pub const LOG_DIR: &str = "/var/log/weftos";
/// macOS service account and group.
pub const MACOS_ACCOUNT: &str = "_weftos";
/// Linux service account.
pub const LINUX_ACCOUNT: &str = "weftos";
/// Where the launchd plist is installed.
pub const MESH_PLIST_PATH: &str = "/Library/LaunchDaemons/ai.weftos.mesh.plist";
/// Where the rundir helper plist is installed.
pub const RUNDIR_PLIST_PATH: &str = "/Library/LaunchDaemons/ai.weftos.mesh-rundir.plist";
/// Where the systemd unit is installed.
pub const MESH_UNIT_PATH: &str = "/etc/systemd/system/weftos-mesh.service";
/// Where the sysusers fragment is installed.
pub const SYSUSERS_PATH: &str = "/etc/sysusers.d/weftos-mesh.conf";
/// Old rotation snippet; never installed (it breaks launchd logging), removed on uninstall if present.
pub const NEWSYSLOG_PATH: &str = "/etc/newsyslog.d/weftos-mesh.conf";

/// Group that owns `/var/run/weftos` on macOS; users must be members.
pub fn macos_group() -> &'static str {
    MACOS_ACCOUNT
}

/// Group that owns the runtime directory on Linux; users must be members.
pub fn linux_group() -> &'static str {
    LINUX_ACCOUNT
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

/// One word of a systemd `ExecStart=` line (double quotes; `\ " % $` escaped).
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

/// The launchd system plist for `/Library/LaunchDaemons/ai.weftos.mesh.plist`.
///
/// Runs `<exe> mesh serve --config /etc/weftos/mesh.toml` as `_weftos:_weftos`
/// with `KeepAlive` and `RunAtLoad`. State and socket paths come from
/// `mesh.toml` only (no environment overrides, so the file stays the single
/// source); `state` is the working directory.
pub fn launchd_system_plist(exe: &Path, state: &Path) -> String {
    let exe = xml_escape(&exe.to_string_lossy());
    let state = xml_escape(&state.to_string_lossy());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{MESH_LAUNCHD_LABEL}</string>
    <key>UserName</key>
    <string>{MACOS_ACCOUNT}</string>
    <key>GroupName</key>
    <string>{MACOS_ACCOUNT}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
        <string>mesh</string>
        <string>serve</string>
        <string>--config</string>
        <string>{MESH_TOML}</string>
    </array>
    <key>WorkingDirectory</key>
    <string>{state}</string>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>ThrottleInterval</key>
    <integer>10</integer>
    <key>StandardOutPath</key>
    <string>{LOG_DIR}/mesh.log</string>
    <key>StandardErrorPath</key>
    <string>{LOG_DIR}/mesh.log</string>
</dict>
</plist>
"#
    )
}

/// Root LaunchDaemon that recreates [`RUN_DIR`] (group-owned, 0750) at every
/// boot, because macOS clears `/var/run` and `_weftos` cannot create it.
pub fn launchd_rundir_plist() -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{MESH_RUNDIR_LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>/bin/sh</string>
        <string>-c</string>
        <string>/usr/bin/install -d -m 0750 -o {MACOS_ACCOUNT} -g {MACOS_ACCOUNT} {RUN_DIR}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
</dict>
</plist>
"#
    )
}

/// The systemd system unit for `/etc/systemd/system/weftos-mesh.service`.
///
/// `RuntimeDirectory=weftos` is created owned by `weftos:weftos`; mode 0750
/// makes group membership the access rule for the socket.
pub fn systemd_system_unit(exe: &Path) -> String {
    let exec = systemd_quote(&exe.to_string_lossy());
    format!(
        "[Unit]\n\
Description=WeftOS machine mesh service\n\
Documentation=https://github.com/WeftOS/weftos\n\
After=network.target\n\
StartLimitIntervalSec=60\n\
StartLimitBurst=5\n\
\n\
[Service]\n\
Type=simple\n\
User={LINUX_ACCOUNT}\n\
Group={LINUX_ACCOUNT}\n\
ExecStart={exec} mesh serve --config {MESH_TOML}\n\
Restart=always\n\
RestartSec=3\n\
NotifyAccess=main\n\
WatchdogSec={WATCHDOG_SECS}\n\
StateDirectory=weftos/mesh\n\
StateDirectoryMode=0700\n\
RuntimeDirectory=weftos\n\
RuntimeDirectoryMode=0750\n\
NoNewPrivileges=yes\n\
ProtectSystem=strict\n\
ProtectHome=yes\n\
PrivateTmp=yes\n\
CapabilityBoundingSet=\n\
RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6\n\
\n\
[Install]\n\
WantedBy=multi-user.target\n"
    )
}

/// `/etc/sysusers.d/weftos-mesh.conf`: the `weftos` account and its group.
pub fn sysusers_conf() -> String {
    format!(
        "# WeftOS machine mesh service account (never root, no login shell).\n\
u {LINUX_ACCOUNT} - \"WeftOS mesh service\" {STATE_DIR} /usr/sbin/nologin\n"
    )
}

/// POSIX shell that creates the `_weftos` group and account with `dscl`
/// (idempotent, picks the first free id from 300 up). macOS only.
pub fn macos_account_snippet() -> String {
    let a = MACOS_ACCOUNT;
    format!(
        r#"if ! dscl . -read /Groups/{a} >/dev/null 2>&1; then
  ID=300
  while dscl . -list /Groups PrimaryGroupID | awk '{{print $2}}' | grep -qx "$ID" ||
        dscl . -list /Users UniqueID | awk '{{print $2}}' | grep -qx "$ID"; do
    ID=$((ID + 1))
  done
  dscl . -create /Groups/{a}
  dscl . -create /Groups/{a} PrimaryGroupID "$ID"
  dscl . -create /Groups/{a} RealName "WeftOS mesh service"
  dscl . -create /Groups/{a} Password '*'
fi
if ! dscl . -read /Users/{a} >/dev/null 2>&1; then
  GID_=$(dscl . -read /Groups/{a} PrimaryGroupID | awk '{{print $2}}')
  UID_="$GID_"
  while dscl . -list /Users UniqueID | awk '{{print $2}}' | grep -qx "$UID_"; do
    UID_=$((UID_ + 1))
  done
  dscl . -create /Users/{a}
  dscl . -create /Users/{a} UniqueID "$UID_"
  dscl . -create /Users/{a} PrimaryGroupID "$GID_"
  dscl . -create /Users/{a} RealName "WeftOS mesh service"
  dscl . -create /Users/{a} UserShell /usr/bin/false
  dscl . -create /Users/{a} NFSHomeDirectory /var/empty
  dscl . -create /Users/{a} Password '*'
  dscl . -create /Users/{a} IsHidden 1
fi
"#
    )
}

/// Executable named by an installed unit (the systemd `ExecStart=` or the
/// first launchd `ProgramArguments` string); `None` when it cannot be parsed.
pub fn unit_exe(text: &str) -> Option<String> {
    if let Some(l) = text.lines().find_map(|l| l.strip_prefix("ExecStart=")) {
        let l = l.trim_start_matches(['-', '@', '+', '!', ' ']);
        if let Some(rest) = l.strip_prefix('"') {
            let mut out = String::new();
            let mut it = rest.chars();
            while let Some(c) = it.next() {
                match c {
                    '"' => return Some(out),
                    '\\' => out.extend(it.next()),
                    '%' if it.clone().next() == Some('%') => {
                        it.next();
                        out.push('%');
                    }
                    '$' if it.clone().next() == Some('$') => {
                        it.next();
                        out.push('$');
                    }
                    c => out.push(c),
                }
            }
            return None;
        }
        return l.split_whitespace().next().map(str::to_owned);
    }
    let after = text.split("<key>ProgramArguments</key>").nth(1)?;
    let after = after.split("<string>").nth(1)?;
    let raw = after.split("</string>").next()?;
    Some(
        raw.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&"),
    )
}

/// Quote a path for the printed script.
pub fn q(p: &Path) -> String {
    sh_quote(&p.to_string_lossy())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Compare with `tests/golden/<name>`; `UPDATE_GOLDEN=1` rewrites it.
    fn check_golden(name: &str, got: &str) {
        let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden").join(name);
        if std::env::var_os("UPDATE_GOLDEN").is_some() {
            std::fs::write(&p, got).unwrap();
        }
        let want = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
        assert_eq!(got, want, "{name}");
    }

    #[test]
    fn plist_matches_golden() {
        let got = launchd_system_plist(Path::new(SERVICE_EXE), Path::new(STATE_DIR));
        check_golden("weftos-mesh.plist", &got);
    }

    #[test]
    fn rundir_plist_matches_golden() {
        check_golden("weftos-mesh-rundir.plist", &launchd_rundir_plist());
    }

    #[test]
    fn unit_matches_golden() {
        let got = systemd_system_unit(Path::new(SERVICE_EXE));
        check_golden("weftos-mesh.service", &got);
    }

    #[test]
    fn sysusers_matches_golden() {
        check_golden("weftos-mesh-sysusers.conf", &sysusers_conf());
    }

    #[test]
    fn dscl_snippet_matches_golden() {
        check_golden("weftos-mesh-dscl.sh", &macos_account_snippet());
    }

    #[test]
    fn unit_is_hardened_and_group_gated() {
        let t = systemd_system_unit(Path::new(SERVICE_EXE));
        for k in [
            "User=weftos\n", "StateDirectory=weftos/mesh\n", "StateDirectoryMode=0700\n",
            "RuntimeDirectory=weftos\n", "RuntimeDirectoryMode=0750\n", "NoNewPrivileges=yes\n",
            "ProtectSystem=strict\n", "ProtectHome=yes\n", "PrivateTmp=yes\n", "CapabilityBoundingSet=\n",
            "RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6\n", "Restart=always\n",
        ] {
            assert!(t.contains(k), "{k}");
        }
        assert!(!t.contains("User=root"));
    }

    #[test]
    fn unit_exe_round_trips_both_formats() {
        for exe in ["/usr/local/libexec/weftos/weaver", "/a b/we\"av%er$", "/Users/a/.cargo/bin/weaver"] {
            let u = systemd_system_unit(Path::new(exe));
            assert_eq!(unit_exe(&u).as_deref(), Some(exe), "systemd {exe}");
        }
        let p = launchd_system_plist(Path::new("/a b/&<w>"), Path::new("/s"));
        assert_eq!(unit_exe(&p).as_deref(), Some("/a b/&<w>"));
        assert_eq!(unit_exe("nothing"), None);
    }

    #[test]
    fn plist_runs_as_the_service_account_never_root() {
        let t = launchd_system_plist(Path::new(SERVICE_EXE), Path::new(STATE_DIR));
        assert!(t.contains("<key>UserName</key>\n    <string>_weftos</string>"));
        assert!(t.contains("<key>GroupName</key>\n    <string>_weftos</string>"));
        assert!(!t.contains("<string>root</string>"));
    }

    #[test]
    fn paths_are_escaped() {
        let t = launchd_system_plist(Path::new("/a b/&<w>"), Path::new("/s"));
        assert!(t.contains("<string>/a b/&amp;&lt;w&gt;</string>"));
        let u = systemd_system_unit(Path::new("/a b/we\"av%er$"));
        assert!(u.contains("ExecStart=\"/a b/we\\\"av%%er$$\" mesh serve"));
        assert!(!u.contains("Environment="), "paths come from mesh.toml only");
    }
}
