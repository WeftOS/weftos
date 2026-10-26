//! The privileged path with real syscalls: setgroups/setgid/setuid in the
//! child and the socket's chown to the `cogbus` group. Needs a root test
//! process and an existing `cogbus` group (`scripts/cogbus-uid-setup.sh`);
//! otherwise each test says why it skipped and passes.

use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::time::Duration;

use clawft_kernel::workload_ctl::host_cogbus::{CogbusRegistrar, CogbusRegistration, Isolation};
use clawft_kernel::workload_runtime::supervise::{LaunchSpec, ProcLimits, Supervised, running_as_root};
use super::*;

const TOKEN: &str = "root-path-token-root-path-token-000";
const PROBE: &str = r#"
import os, socket, struct, json, sys
who = "uid=%d gid=%d groups=%s" % (os.getuid(), os.getgid(), sorted(os.getgroups()))
try:
    s = socket.socket(socket.AF_UNIX)
    s.connect(os.environ["WEFTOS_COGBUS"])
except Exception as e:
    print(who, "connect-failed=" + type(e).__name__); sys.exit(0)
b = json.dumps({"op": "hello", "id": 1, "token": os.environ["WEFTOS_COGBUS_TOKEN"]}).encode()
s.sendall(struct.pack(">I", len(b)) + b)
n = struct.unpack(">I", s.recv(4))[0]
print(who, "reply=" + s.recv(n).decode())
"#;

fn prerequisites() -> Option<u32> {
    if !running_as_root() {
        eprintln!("skipped: needs a root test process");
        return None;
    }
    let gid = group_gid(DEFAULT_GROUP);
    if gid.is_none() {
        eprintln!("skipped: no {DEFAULT_GROUP} group (run scripts/cogbus-uid-setup.sh apply --yes)");
    }
    gid
}

async fn probe_as(dir: &Path, socket: &Path, run_as: (u32, u32), groups: Vec<u32>) -> String {
    let script = dir.join("probe.py");
    std::fs::write(&script, PROBE).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o644)).unwrap();
    let p = Supervised::spawn(LaunchSpec {
        program: "/usr/bin/python3".into(),
        args: vec![script.to_string_lossy().into_owned()],
        env: vec![
            ("WEFTOS_COGBUS".into(), socket.to_string_lossy().into_owned()),
            ("WEFTOS_COGBUS_TOKEN".into(), TOKEN.into()),
        ],
        cwd: "/".into(),
        limits: ProcLimits::for_ram_mb(1024),
        output_limit: 8192,
        run_as: Some(run_as),
        groups,
    })
    .expect("spawn under the range uid");
    let ev = p.wait_for(Duration::from_secs(20)).await;
    eprintln!("probe as {run_as:?}: {}{}", ev.stdout.trim(), ev.stderr.trim());
    ev.stdout
}

#[tokio::test]
async fn a_native_cog_under_a_range_uid_is_accepted_and_any_other_uid_is_refused() {
    let Some(gid) = prerequisites() else { return };
    let tmp = tempfile::tempdir().unwrap();
    std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    let iso = Arc::new(Isolator::new(
        IsolationConfig { require_cog_uid: true, uid_range: Some([61000, 61009]), group: DEFAULT_GROUP.into(), ..Default::default() },
        can_switch_uid(),
        Some(gid),
    ));
    let rt = crate::cogbus_serve::start_with(crate::cogbus_serve::broker("n"), tmp.path(), iso, Some(gid))
        .await
        .unwrap();

    // The socket: root:cogbus 0660.
    let sr = rt.socket_report();
    assert_eq!((sr.mode, sr.uid, sr.gid, sr.group_applied), (0o660, 0, gid, true), "{sr:?}");

    let reg = crate::cogbus_serve::Registrar::for_runtime(&rt);
    let a = reg.attach("native").unwrap();
    let id = a.identity.clone().expect("a privileged daemon allocates an identity");
    assert_eq!((a.isolation, id.uid, id.gid, id.groups.clone()), (Isolation::PerCogUid, 61000, gid, vec![gid]));
    reg.register(&CogbusRegistration {
        instance_id: "root-1".into(),
        cog_id: "probe".into(),
        project_id: None,
        controller_node: "ctl".into(),
        token: clawft_types::secret::SecretString::new(TOKEN.to_string()),
        uid: a.uid,
        isolation: a.isolation,
        outputs: vec![],
        inputs: vec![],
    })
    .unwrap();

    // The cog, launched as its range uid with only the cogbus group.
    let ok = probe_as(tmp.path(), rt.socket(), (id.uid, id.gid), id.groups.clone()).await;
    assert!(ok.contains("uid=61000") && ok.contains(&format!("groups=[{gid}]")), "{ok}");
    assert!(ok.contains(r#""instance_id":"root-1""#), "accepted: {ok}");

    // Same token, another uid in the cogbus group: the token is not enough.
    let stolen = probe_as(tmp.path(), rt.socket(), (61001, gid), vec![gid]).await;
    assert!(stolen.contains("uid=61001") && stolen.contains("uid_mismatch"), "{stolen}");
    let seen = rt.broker.recent_refusals();
    assert!(
        seen.iter().any(|r| matches!(&r.outcome, weftos_cogbus::Outcome::Refused { code, .. } if code == "uid_mismatch") && r.instance == "root-1"),
        "the refusal is in the activity log: {seen:?}"
    );

    // A uid outside the cogbus group cannot even open the socket.
    let outside = probe_as(tmp.path(), rt.socket(), (61002, 61002), vec![]).await;
    assert!(outside.contains("connect-failed=PermissionError"), "{outside}");

    // The listing marks the instance and the pool keeps its uid.
    let doc = crate::cogbus_serve::list_json(Some(&rt));
    assert_eq!(doc["isolation"]["instances"][0]["isolation"], "per-cog-uid");
    assert_eq!(doc["isolation"]["instances"][0]["uid"], 61000);
    reg.revoke("root-1");
    reg.release("root-1");
    assert_eq!(reg.attach("native").unwrap().uid, Some(61000), "unload frees the uid");
}
