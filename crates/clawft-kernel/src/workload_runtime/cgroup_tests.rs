//! Card 59b07953. The fake-tree tests run anywhere; the live ones need a
//! delegated cgroup v2 subtree (`systemd-run --user --scope -p Delegate=yes`).
//! A live test skips only when the host plainly has no delegation (no mount,
//! EACCES on mkdir, ...); any other creation error fails it. With
//! `WEFT_REQUIRE_CGROUP=1` every skip is a failure too (the PG/CI lane).

use super::*;
use std::path::PathBuf;
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
        delegated: true,
    };
    let e = Cgroup::create(&host, 1 << 20, None).unwrap_err();
    assert!(matches!(e, CgroupError::NotDelegated(_)), "{e}");
    assert!(e.to_string().contains("not a cgroup v2 mount"), "{e}");
}

#[test]
fn an_undelegated_cgroup_gives_a_reason() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("cgroup.controllers"), "memory\n").unwrap();
    std::fs::write(dir.path().join("self"), "0::/nowhere\n").unwrap();
    let host = CgroupHost {
        root: dir.path().into(),
        self_cgroup: dir.path().join("self"),
        delegated: true,
    };
    // The parent directory does not exist, so the child cannot be created.
    let e = Cgroup::create(&host, 1 << 20, None).unwrap_err();
    assert!(matches!(e, CgroupError::NotDelegated(_)), "{e}");
    assert!(!e.to_string().is_empty());
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
        delegated: false,
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

/// A live host test needs a writable, delegated cgroup under this process's
/// own cgroup. `None` (skip) only for a host that plainly has none; any other
/// failure, or any skip under `WEFT_REQUIRE_CGROUP=1`, panics.
#[cfg(target_os = "linux")]
fn live_host() -> Option<CgroupHost> {
    let mut host = CgroupHost::system();
    host.delegated = true; // the test lane is the delegated scope
    match Cgroup::create(&host, 64 << 20, None) {
        Ok((cg, _)) => {
            cg.remove_best_effort();
            Some(host)
        }
        Err(CgroupError::NotDelegated(e)) => {
            assert!(
                std::env::var("WEFT_REQUIRE_CGROUP").as_deref() != Ok("1"),
                "WEFT_REQUIRE_CGROUP=1 but no delegated cgroup: {e}"
            );
            eprintln!("skip: no delegated cgroup v2 subtree: {e}");
            None
        }
        Err(e @ CgroupError::Failed(_)) => panic!("cgroup creation failed (not a skip): {e}"),
    }
}

/// Fake tree whose base advertises controllers; the process sits in `base`.
fn fake(controllers: &str, subtree: Option<&str>) -> (tempfile::TempDir, CgroupHost, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("cgroup.controllers"), "memory\n").unwrap();
    let base = dir.path().join("svc");
    std::fs::create_dir(&base).unwrap();
    std::fs::write(base.join("cgroup.controllers"), controllers).unwrap();
    if let Some(s) = subtree {
        std::fs::write(base.join("cgroup.subtree_control"), s).unwrap();
    }
    std::fs::write(dir.path().join("self"), "0::/svc\n").unwrap();
    let host = CgroupHost {
        root: dir.path().into(),
        self_cgroup: dir.path().join("self"),
        delegated: true,
    };
    (dir, host, base)
}

#[test]
fn without_the_delegation_opt_in_the_cgroup_path_is_not_used() {
    // Writable and a controller present, but the unit never said Delegate=yes.
    let (_d, mut host, base) = fake("memory\n", Some("memory\n"));
    host.delegated = false;
    let e = Cgroup::create(&host, 1 << 20, None).unwrap_err();
    assert!(matches!(e, CgroupError::NotDelegated(_)), "{e}");
    assert!(e.to_string().contains(DELEGATED_ENV), "{e}");
    assert!(std::fs::read_dir(&base).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().starts_with("weft-")));
}

#[test]
fn the_move_is_skipped_when_the_parent_hands_out_no_memory() {
    let (_d, host, base) = fake("cpu io\n", None);
    let e = prepare(&base).unwrap_err();
    assert!(e.to_string().contains("not available in"), "{e}");
    assert!(!base.join("supervisor").exists(), "moved for nothing");
    // The failure is cached with its reason; create reports it, no retry.
    let again = Cgroup::create(&host, 1 << 20, None).unwrap_err();
    assert!(again.to_string().contains("not available in"), "{again}");
}

#[test]
fn a_failed_enable_is_cached_and_does_not_sleep() {
    // base has memory available but cgroup.subtree_control is a directory, so
    // the "+memory" write fails; the failure must be immediate and cached.
    let (_d, _h, base) = fake("memory\n", None);
    std::fs::create_dir(base.join("cgroup.subtree_control")).unwrap();
    let t = std::time::Instant::now();
    let first = prepare(&base).unwrap_err().to_string();
    let second = prepare(&base).unwrap_err().to_string();
    assert_eq!(first, second);
    assert!(t.elapsed() < Duration::from_millis(500), "{:?}", t.elapsed());
}

#[test]
fn stale_instance_cgroups_of_dead_supervisors_are_swept() {
    let (_d, _h, base) = fake("memory\n", Some("memory\n"));
    let dead = base.join("weft-999999999-0");
    let mine = base.join(format!("weft-{}-77", std::process::id()));
    let other = base.join("not-ours");
    for d in [&dead, &mine, &other] {
        std::fs::create_dir(d).unwrap();
    }
    prepare(&base).unwrap();
    assert!(!dead.exists(), "stale cgroup kept");
    assert!(mine.exists() && other.exists(), "swept something it must not");
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

#[cfg(target_os = "linux")]
fn python() -> Option<&'static str> {
    ["/usr/bin/python3", "/usr/local/bin/python3"]
        .into_iter()
        .find(|p| std::path::Path::new(p).exists())
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_failed_cgroup_join_retries_under_the_watchdog() {
    let Some(host) = live_host() else { return };
    // Writes to /dev/full fail with ENOSPC in the child: the join fails.
    crate::workload_runtime::supervise::tests_join_override::set("/dev/full");
    let s = Supervised::spawn_on(sh("echo ok", ProcLimits::for_ram_mb(64)), &host).unwrap();
    assert_eq!(s.memory_enforcement(), "watchdog");
    let ev = s.wait_for(Duration::from_secs(10)).await;
    assert!(ev.succeeded() && ev.stdout.contains("ok"), "{ev:?}");
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_spawn_error_unrelated_to_the_join_is_not_retried() {
    let Some(host) = live_host() else { return };
    let mut spec = sh("", ProcLimits::for_ram_mb(64));
    spec.program = "/nonexistent/weft-no-such-program".into();
    let e = Supervised::spawn_on(spec, &host).err().expect("must fail");
    assert!(e.to_string().contains("spawn"), "{e}");
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn dropping_a_running_instance_removes_its_cgroup() {
    let Some(host) = live_host() else { return };
    let s = Supervised::spawn_on(sh("sleep 30", ProcLimits::for_ram_mb(64)), &host).unwrap();
    let dir = s.cgroup_dir().expect("cgroup mode");
    assert!(dir.exists());
    drop(s);
    for _ in 0..100 {
        if !dir.exists() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("cgroup left behind after Drop: {}", dir.display());
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_launches_each_get_their_own_cgroup() {
    let Some(host) = live_host() else { return };
    let mut tasks = vec![];
    for i in 0..6 {
        let host = host.clone();
        tasks.push(tokio::spawn(async move {
            let s = Supervised::spawn_on(sh(&format!("echo n{i}"), ProcLimits::for_ram_mb(64)), &host)
                .unwrap();
            let enf = s.memory_enforcement();
            let dir = s.cgroup_dir();
            let ev = s.wait_for(Duration::from_secs(10)).await;
            (enf, dir, ev)
        }));
    }
    let mut dirs = std::collections::HashSet::new();
    for t in tasks {
        let (enf, dir, ev) = t.await.unwrap();
        assert_eq!(enf, "cgroup");
        assert!(ev.succeeded(), "{ev:?}");
        assert!(dirs.insert(dir.unwrap()), "two launches shared a cgroup");
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn an_oom_kills_the_whole_cgroup_including_grandchildren() {
    let Some(host) = live_host() else { return };
    let Some(py) = python() else {
        assert!(std::env::var("WEFT_REQUIRE_CGROUP").as_deref() != Ok("1"), "python3 required");
        eprintln!("skip: python3 not available");
        return;
    };
    // A shell whose background grandchild allocates past memory.max, then a
    // long sleep: with memory.oom.group every task dies together, so the run
    // ends well before the sleep would.
    let script = format!(
        "{py} -c \"k=[]\nfor i in range(60): k.append(bytearray(b'y'*(8<<20)))\" & sleep 60; wait"
    );
    let t = std::time::Instant::now();
    let s = Supervised::spawn_on(sh(&script, ProcLimits::for_ram_mb(64)), &host).unwrap();
    let ev = s.wait_for(Duration::from_secs(30)).await;
    assert!(ev.killed_for_memory, "{ev:?}");
    assert!(t.elapsed() < Duration::from_secs(30), "sleep survived the OOM: {:?}", t.elapsed());
}

#[test]
fn the_opt_in_name_matches_the_generated_unit() {
    // service_units.rs and its golden spell the variable literally.
    assert_eq!(DELEGATED_ENV, "WEFTOS_CGROUP_DELEGATED");
}

/// Manual probe for the unit-reload scenario (run with `--ignored` inside a
/// transient unit, then `systemctl --user daemon-reload` about 2 s in): the
/// instance holds still for 6 s, then allocates well past `ram_mb`. Prints
/// the enforcement mode, `memory.max` before and after, and the evidence.
#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "manual: needs a transient unit and a daemon-reload"]
async fn h1_reload_probe() {
    let Some(py) = python() else { return };
    let mut spec = sh("", ProcLimits::for_ram_mb(64));
    spec.program = py.into();
    spec.args = vec![
        "-c".into(),
        "import time\ntime.sleep(6)\nk=[]\nfor i in range(30):\n    k.append(bytearray(b'y'*(8<<20)))\n    time.sleep(0.05)\ntime.sleep(10)\nprint('finished')".into(),
    ];
    let s = Supervised::spawn(spec).unwrap();
    let max = |d: &Option<PathBuf>| {
        d.as_ref()
            .map(|d| std::fs::read_to_string(d.join("memory.max")).unwrap_or_else(|e| format!("GONE ({e})")))
            .unwrap_or_else(|| "n/a".into())
    };
    let dir = s.cgroup_dir();
    println!("PROBE mode={} memory.max(before)={}", s.memory_enforcement(), max(&dir).trim());
    tokio::time::sleep(Duration::from_secs(4)).await;
    println!("PROBE memory.max(after reload)={}", max(&dir).trim());
    let ev = s.wait_for(Duration::from_secs(40)).await;
    println!("PROBE killed_for_memory={} finished={}", ev.killed_for_memory, ev.stdout.contains("finished"));
}
