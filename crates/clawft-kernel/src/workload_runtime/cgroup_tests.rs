//! Card 59b07953. The fake-tree tests run anywhere; the live ones need a
//! delegated cgroup v2 subtree (`systemd-run --user --scope`) and skip
//! with a note when the host has none.

use super::*;
use crate::workload_runtime::supervise::{LaunchSpec, ProcLimits, Supervised};
use std::time::Duration;

fn sh(script: &str, limits: ProcLimits) -> LaunchSpec {
    LaunchSpec {
        program: "/bin/sh".into(),
        args: vec!["-c".into(), script.into()],
        env: vec![],
        cwd: "/".into(),
        limits,
        output_limit: 4096,
        run_as: None,
        groups: vec![],
    }
}

#[test]
fn parses_the_unified_hierarchy_path() {
    assert_eq!(parse_self_cgroup("0::/user.slice/a.scope\n"), Some("/user.slice/a.scope"));
    assert_eq!(parse_self_cgroup("12:memory:/x\n0::/y\n"), Some("/y"));
    assert_eq!(parse_self_cgroup("12:memory:/x\n"), None);
}

#[test]
fn memory_events_key_is_read() {
    let b = "low 0\nhigh 0\nmax 3\noom 2\noom_kill 1\n";
    assert_eq!(flat_key(b, "oom_kill"), Some(1));
    assert_eq!(flat_key(b, "oom"), Some(2));
    assert_eq!(flat_key(b, "nope"), None);
}

#[test]
fn a_host_without_cgroup_v2_gives_a_reason() {
    let dir = tempfile::tempdir().unwrap();
    let host = CgroupHost {
        root: dir.path().into(),
        self_cgroup: dir.path().join("self"),
    };
    let e = Cgroup::create(&host, 1 << 20, None).unwrap_err();
    assert!(e.contains("not a cgroup v2 mount"), "{e}");
}

#[test]
fn an_undelegated_cgroup_gives_a_reason() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("cgroup.controllers"), "memory\n").unwrap();
    std::fs::write(dir.path().join("self"), "0::/nowhere\n").unwrap();
    let host = CgroupHost {
        root: dir.path().into(),
        self_cgroup: dir.path().join("self"),
    };
    // The parent directory does not exist, so the child cannot be created.
    let e = Cgroup::create(&host, 1 << 20, None).unwrap_err();
    assert!(e.contains("not delegated"), "{e}");
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn without_delegation_the_watchdog_still_enforces_ram_mb() {
    let Some(py) = ["/usr/bin/python3", "/usr/local/bin/python3"]
        .into_iter()
        .find(|p| std::path::Path::new(p).exists())
    else {
        eprintln!("skip: python3 not available");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let host = CgroupHost {
        root: dir.path().into(),
        self_cgroup: dir.path().join("self"),
    };
    let mut l = ProcLimits::for_ram_mb(128);
    l.rss_poll = Duration::from_millis(50);
    let mut spec = sh("", l);
    spec.program = py.into();
    spec.args = vec![
        "-c".into(),
        "import time\nkeep=[]\nfor i in range(40):\n    keep.append(bytearray(b'y'*(16<<20)))\n    time.sleep(0.1)\nprint('finished')".into(),
    ];
    let s = Supervised::spawn_on(spec, &host).unwrap();
    assert_eq!(s.memory_enforcement(), "watchdog");
    let ev = s.wait_for(Duration::from_secs(30)).await;
    assert!(ev.killed_for_memory, "{ev:?}");
}

/// A live host test needs a writable cgroup under this process's own
/// cgroup; returns `None` (skip) when there is none.
#[cfg(target_os = "linux")]
fn live_host() -> Option<CgroupHost> {
    let host = CgroupHost::system();
    match Cgroup::create(&host, 64 << 20, None) {
        Ok((cg, _)) => {
            cg.remove_best_effort();
            Some(host)
        }
        Err(e) => {
            eprintln!("skip: no delegated cgroup v2 subtree: {e}");
            None
        }
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_native_instance_gets_its_own_cgroup_with_memory_max_and_no_swap() {
    let Some(host) = live_host() else { return };
    let l = ProcLimits::for_ram_mb(64);
    let s = Supervised::spawn_on(
        sh("cat /sys/fs/cgroup$(sed -n 's/^0:://p' /proc/self/cgroup)/memory.max; \
            cat /sys/fs/cgroup$(sed -n 's/^0:://p' /proc/self/cgroup)/memory.swap.max; \
            sed -n 's/^0:://p' /proc/self/cgroup",
           l),
        &host,
    )
    .unwrap();
    assert_eq!(s.memory_enforcement(), "cgroup");
    let ev = s.wait_for(Duration::from_secs(10)).await;
    let lines: Vec<&str> = ev.stdout.lines().collect();
    assert_eq!(lines[0], (64u64 << 20).to_string(), "{ev:?}");
    assert_eq!(lines[1], "0", "{ev:?}");
    assert!(lines[2].contains("/weft-"), "{ev:?}");
    // The instance cgroup is removed after the run.
    let dir = std::path::Path::new("/sys/fs/cgroup").join(lines[2].trim_start_matches('/'));
    assert!(!dir.exists(), "cgroup left behind: {}", dir.display());
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn an_oom_inside_the_cgroup_is_reported_as_killed_for_memory() {
    let Some(host) = live_host() else { return };
    let Some(py) = ["/usr/bin/python3", "/usr/local/bin/python3"]
        .into_iter()
        .find(|p| std::path::Path::new(p).exists())
    else {
        eprintln!("skip: python3 not available");
        return;
    };
    let mut spec = sh("", ProcLimits::for_ram_mb(64));
    spec.program = py.into();
    spec.args = vec![
        "-c".into(),
        "keep=[]\nfor i in range(60):\n    keep.append(bytearray(b'y'*(8<<20)))\nprint('finished')".into(),
    ];
    // Watchdog polling is irrelevant in cgroup mode; the kernel kills it.
    let s = Supervised::spawn_on(spec, &host).unwrap();
    assert_eq!(s.memory_enforcement(), "cgroup");
    let ev = s.wait_for(Duration::from_secs(30)).await;
    assert!(ev.killed_for_memory, "{ev:?}");
    assert!(!ev.succeeded());
    assert!(!ev.stdout.contains("finished"), "{ev:?}");
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_cog_within_ram_mb_is_not_reported_killed_in_a_cgroup() {
    let Some(host) = live_host() else { return };
    let s = Supervised::spawn_on(sh("echo ok", ProcLimits::for_ram_mb(64)), &host).unwrap();
    let ev = s.wait_for(Duration::from_secs(10)).await;
    assert!(ev.succeeded(), "{ev:?}");
    assert!(!ev.killed_for_memory);
}
