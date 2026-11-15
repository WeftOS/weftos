//! Memory is enforced as resident memory, not address space (card
//! 7306a144): a cog that maps a lot but touches little runs; one that
//! touches too much is stopped and reported.

use super::*;

fn python() -> Option<&'static str> {
    ["/usr/bin/python3", "/usr/local/bin/python3"]
        .into_iter()
        .find(|p| std::path::Path::new(p).exists())
}

fn spec_sh(script: &str, limits: ProcLimits) -> LaunchSpec {
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

fn spec_py(py: &str, code: &str, limits: ProcLimits) -> LaunchSpec {
    LaunchSpec {
        program: py.into(),
        args: vec!["-c".into(), code.into()],
        ..spec_sh("", limits)
    }
}

#[test]
fn proc_parsers_read_vmrss_and_the_process_group() {
    assert_eq!(parse_vmrss("Name:\tx\nVmRSS:\t   2048 kB\n"), Some(2048 * 1024));
    assert_eq!(parse_vmrss("Name:\tkthread\n"), None);
    // The command name can hold spaces and parentheses.
    assert_eq!(parse_pgrp("42 (a (b) c) S 1 777 5 0 0"), Some(777));
    assert_eq!(parse_pgrp("garbage"), None);
}

#[test]
fn default_limits_keep_address_space_off() {
    let l = ProcLimits::for_ram_mb(256);
    assert_eq!(l.address_space_bytes, None);
    assert_eq!(l.mem_bytes, 256 << 20);
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn rlimit_data_backstop_is_set_and_address_space_stays_unlimited() {
    let ev = Supervised::spawn(spec_sh(
        "echo \"data=$(ulimit -d) vmem=$(ulimit -v)\"",
        ProcLimits::for_ram_mb(256),
    ))
    .unwrap()
    .wait_for(Duration::from_secs(10))
    .await;
    // ulimit reports KiB: 2 x 256 MiB.
    assert!(ev.stdout.contains("data=524288 vmem=unlimited"), "{ev:?}");
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn address_space_cap_is_opt_in() {
    let mut l = ProcLimits::for_ram_mb(256);
    l.address_space_bytes = Some(1 << 30);
    let ev = Supervised::spawn(spec_sh("echo vmem=$(ulimit -v)", l))
        .unwrap()
        .wait_for(Duration::from_secs(10))
        .await;
    assert!(ev.stdout.contains("vmem=1048576"), "{ev:?}");
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_cog_that_maps_4_gib_but_touches_50_mib_runs_under_ram_mb_256() {
    let Some(py) = python() else {
        eprintln!("skip: python3 not available");
        return;
    };
    let code = "import mmap,time\n\
        m=mmap.mmap(-1,4<<30,prot=mmap.PROT_NONE)\n\
        b=bytearray(b'x'*(50<<20))\n\
        time.sleep(1.5)\n\
        print('ok',len(b))";
    let mut l = ProcLimits::for_ram_mb(256);
    l.rss_poll = Duration::from_millis(100);
    let ev = Supervised::spawn(spec_py(py, code, l))
        .unwrap()
        .wait_for(Duration::from_secs(20))
        .await;
    assert!(ev.succeeded(), "{ev:?}");
    assert!(!ev.killed_for_memory);
    assert!(ev.stdout.contains("ok"), "{ev:?}");
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_resident_memory_hog_is_stopped_and_reported() {
    let Some(py) = python() else {
        eprintln!("skip: python3 not available");
        return;
    };
    // Touches 16 MiB every 100 ms up to 640 MiB; ram_mb = 128 must stop it
    // well before the RLIMIT_DATA backstop (256 MiB) or the end.
    let code = "import time\n\
        keep=[]\n\
        for i in range(40):\n\
        \x20   keep.append(bytearray(b'y'*(16<<20)))\n\
        \x20   time.sleep(0.1)\n\
        print('finished')";
    let mut l = ProcLimits::for_ram_mb(128);
    l.rss_poll = Duration::from_millis(50);
    let ev = Supervised::spawn(spec_py(py, code, l))
        .unwrap()
        .wait_for(Duration::from_secs(30))
        .await;
    assert!(ev.killed_for_memory, "{ev:?}");
    assert!(!ev.succeeded());
    assert!(!ev.stdout.contains("finished"), "{ev:?}");
    assert_eq!(ev.audit()["killed_for_memory"], true);
}
