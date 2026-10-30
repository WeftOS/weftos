//! `weaver workload place | explain | status | stop | logs | unload | revoke`
//! (ADR-099 sections 3 and 7, card mesh-placement-12).
//!
//! Each verb calls the daemon's placement control plane (the
//! `workload.*` RPC family in `workload_place_rpc`). The daemon decides,
//! chains and dispatches; this module only builds params and renders.

use std::path::{Path, PathBuf};

use clap::{Args, Subcommand};
use serde_json::{Value, json};

/// Placement subcommands of `weaver workload` (flattened into it).
#[derive(Subcommand, Debug)]
pub enum WorkloadPlaceCmd {
    /// Place a signed package on the best node (fetch, admit, load, start).
    Place(PlaceArgs),
    /// Decide and explain a placement without dispatching it.
    Explain(PlaceArgs),
    /// Place an operator-pinned store cog on a Cognitum Seed, addressed by
    /// its operator-assigned node id (from the daemon's workload-seeds.json).
    PlaceSeed {
        /// Operator-assigned Seed node id.
        node_id: String,
        /// Store cog as `<id>@<version>` (must be pinned for that Seed).
        cog: String,
        /// Expected SHA-256 of the store binary.
        #[arg(long)]
        sha256: Option<String>,
        /// Run mode: once, interval, listener.
        #[arg(long, default_value = "interval")]
        mode: String,
        /// Install and load without starting.
        #[arg(long)]
        no_start: bool,
    },
    /// Placed instances and their live status (or one instance).
    Status {
        /// Instance id (omit for all placements).
        instance_id: Option<String>,
        /// Print raw JSON.
        #[arg(long)]
        json: bool,
    },
    /// Stop a placed instance (gated and chained on both nodes).
    Stop {
        /// Instance id.
        instance_id: String,
    },
    /// Captured output of a stopped instance.
    Logs {
        /// Instance id.
        instance_id: String,
    },
    /// Revoke a package, signer key or artifact hash: this node stops and
    /// unloads what it runs from it, drops its bytes, and (with a pinned
    /// operator key) tells every peer to do the same. Admin only.
    Revoke(RevokeArgs),
    /// Unload a placed instance, or with `--catalog` a catalogued workload by name.
    Unload {
        /// Instance id (or catalog name with `--catalog`).
        target: String,
        /// Remove a node-local catalog entry instead of a placed instance.
        #[arg(long)]
        catalog: bool,
    },
}

/// What `weaver workload revoke` revokes: exactly one of the three.
#[derive(Args, Debug, Clone)]
#[command(group(
    clap::ArgGroup::new("subject")
        .required(true)
        .multiple(false)
        .args(["package", "signer", "hash"])
))]
pub struct RevokeArgs {
    /// Package id (the signed statement's BLAKE3, shown by `weaver
    /// workload verify`, or a catalog name for `workload install`).
    #[arg(long)]
    pub package: Option<String>,
    /// Signer public key (64 hex): revokes every package that key signed.
    #[arg(long)]
    pub signer: Option<String>,
    /// Artifact BLAKE3 hash (64 hex): a manifest or payload file.
    #[arg(long)]
    pub hash: Option<String>,
    /// Why (kept in the chain and the revocation list).
    #[arg(long)]
    pub reason: Option<String>,
    /// Print raw JSON.
    #[arg(long)]
    pub json: bool,
}

/// Options shared by `place` and `explain`.
#[derive(Args, Debug, Clone)]
pub struct PlaceArgs {
    /// Unpacked signed package directory (`weaver workload pack --out`).
    pub package: PathBuf,
    /// `workload-host` address (`host:port`) to also consider (repeat). A
    /// peer named here is only `discovered`; assign trust tiers in the
    /// daemon's workload-peers.json.
    #[arg(long = "peer")]
    pub peers: Vec<String>,
    /// Place on this node id or fail naming the constraint.
    #[arg(long)]
    pub pin: Option<String>,
    /// Prefer these node ids among equally tiered candidates (repeat).
    #[arg(long)]
    pub prefer: Vec<String>,
    /// Avoid these node ids unless nothing else fits (repeat).
    #[arg(long)]
    pub avoid: Vec<String>,
    /// Allow an emulated route (operator opt-in; recorded as emulated).
    #[arg(long)]
    pub allow_emulated: bool,
    /// Never reschedule this instance onto another node when its node is
    /// lost (an alert is chained instead). `--pin` has the same effect.
    #[arg(long)]
    pub no_migrate: bool,
    /// Run mode: once, interval, listener.
    #[arg(long, default_value = "interval")]
    pub mode: String,
    /// Seconds between cycles for `--mode interval`.
    #[arg(long, default_value_t = 10)]
    pub interval: u32,
    /// UDP port the cog binds for the sensor feed.
    #[arg(long, default_value_t = 5006)]
    pub csi_port: u16,
    /// Extra cog argument (repeat, e.g. `--arg=--state-dir --arg=/data/x`). The host checks
    /// each against the cog's `[config]` cli_args; `--once`/`--interval` come from `--mode`.
    #[arg(long = "arg", value_name = "ARG", allow_hyphen_values = true)]
    pub args: Vec<String>,
    /// Load without starting.
    #[arg(long)]
    pub no_start: bool,
    /// Print the full explanation (always printed for `explain`).
    #[arg(long)]
    pub explain: bool,
    /// Print raw JSON.
    #[arg(long)]
    pub json: bool,
}

/// The package directory as an absolute, canonical path (`cwd` resolves a
/// relative one): the daemon runs elsewhere and must see the same dir.
pub fn resolve_package(package: &Path, cwd: &Path) -> Result<PathBuf, String> {
    let p = if package.is_absolute() {
        package.to_path_buf()
    } else {
        cwd.join(package)
    };
    let c = p
        .canonicalize()
        .map_err(|e| format!("package {}: {e}", package.display()))?;
    if !c.is_dir() {
        return Err(format!("package {} is not a directory", package.display()));
    }
    Ok(c)
}

/// RPC method and params for a placement verb (`cwd` resolves a relative
/// package path).
pub fn request(cmd: &WorkloadPlaceCmd, cwd: &Path) -> Result<(&'static str, Value), String> {
    Ok(match cmd {
        WorkloadPlaceCmd::Place(a) | WorkloadPlaceCmd::Explain(a) => {
            let m = if matches!(cmd, WorkloadPlaceCmd::Place(_)) {
                "workload.place"
            } else {
                "workload.explain"
            };
            let mut p = json!({
                "package_dir": resolve_package(&a.package, cwd)?, "peers": a.peers, "prefer": a.prefer, "avoid": a.avoid,
                "allow_emulated": a.allow_emulated, "mode": a.mode, "interval": a.interval,
                "csi_port": a.csi_port, "start": !a.no_start,
            });
            if !a.args.is_empty() {
                p["args"] = json!(a.args);
            }
            if a.no_migrate {
                p["migratable"] = json!(false);
            }
            if let Some(pin) = &a.pin {
                p["pin"] = json!(pin);
            }
            (m, p)
        }
        WorkloadPlaceCmd::PlaceSeed {
            node_id,
            cog,
            sha256,
            mode,
            no_start,
        } => {
            let (id, version) = cog
                .split_once('@')
                .filter(|(i, v)| !i.is_empty() && !v.is_empty())
                .ok_or_else(|| format!("store cog {cog:?} must be <id>@<version>"))?;
            let mut pin = json!({ "node_id": node_id, "id": id, "version": version,
                                  "mode": mode, "start": !no_start });
            if let Some(h) = sha256 {
                pin["sha256"] = json!(h);
            }
            ("workload.place", json!({ "store_pin": pin }))
        }
        WorkloadPlaceCmd::Revoke(a) => {
            let mut p = json!({});
            for (k, v) in [("package", &a.package), ("signer", &a.signer), ("hash", &a.hash), ("reason", &a.reason)] {
                if let Some(v) = v {
                    p[k] = json!(v);
                }
            }
            ("workload.revoke", p)
        }
        WorkloadPlaceCmd::Status { instance_id, .. } => (
            "workload.status",
            instance_id
                .as_ref()
                .map_or(json!({}), |i| json!({ "instance_id": i })),
        ),
        WorkloadPlaceCmd::Stop { instance_id } => {
            ("workload.stop", json!({ "instance_id": instance_id }))
        }
        WorkloadPlaceCmd::Logs { instance_id } => {
            ("workload.logs", json!({ "instance_id": instance_id }))
        }
        WorkloadPlaceCmd::Unload {
            target,
            catalog: true,
        } => ("workload.unload", json!({ "name": target })),
        WorkloadPlaceCmd::Unload {
            target,
            catalog: false,
        } => ("workload.unload", json!({ "instance_id": target })),
    })
}

/// Why a verb's result is a failure for the exit code (`place` that
/// placed nothing), or `None`.
pub fn failure(cmd: &WorkloadPlaceCmd, result: &Value) -> Option<String> {
    match cmd {
        WorkloadPlaceCmd::Place(_) if result.get("placed").is_none_or(Value::is_null) => {
            Some(if result["decision"]["placement"].is_null() {
                "unplaceable: no eligible node".to_string()
            } else {
                "not placed: no candidate accepted it".to_string()
            })
        }
        _ => None,
    }
}

/// Short human summary of a place / explain result.
pub fn summary(r: &Value, explain: bool) -> String {
    let mut out = String::new();
    if explain {
        out.push_str(r["explain"].as_str().unwrap_or(""));
    }
    match r.get("placed").filter(|p| !p.is_null()) {
        Some(p) => out.push_str(&format!(
            "placed {} on {} via {} (instance {})\n",
            p["workload"].as_str().unwrap_or("?"),
            p["node_id"].as_str().unwrap_or("?"),
            p["variant"].as_str().unwrap_or("?"),
            p["instance_id"].as_str().unwrap_or("?"),
        )),
        None if r["decision"]["placement"].is_null() => {
            out.push_str("unplaceable: no eligible node (see --explain)\n")
        }
        None if r["attempts"].as_array().is_some_and(|a| !a.is_empty()) => {
            out.push_str("not placed: every candidate refused (see --explain)\n")
        }
        None => out.push_str(&format!(
            "decision: {} (not dispatched)\n",
            r["decision"]["placement"]["node_id"]
                .as_str()
                .unwrap_or("?")
        )),
    }
    out.push_str(&format!(
        "decision_id {}\n",
        r["decision_id"].as_str().unwrap_or("?")
    ));
    out
}

/// Human summary of a `workload.revoke` result.
pub fn revoke_summary(r: &Value) -> String {
    let mut out = format!(
        "{} {} {}{}\n",
        if r["newly_revoked"] == true { "revoked" } else { "already revoked:" },
        r["revoked"]["kind"].as_str().unwrap_or("?"),
        r["revoked"]["id"].as_str().unwrap_or("?"),
        if r["persisted"] == false {
            format!(" (NOT saved to disk: {})", r["persist_error"].as_str().unwrap_or("?"))
        } else {
            String::new()
        },
    );
    for f in r["forced"].as_array().into_iter().flatten() {
        out.push_str(&format!(
            "  {} {} ({})\n",
            if f["unloaded"] == true { "stopped and unloaded" } else { "COULD NOT unload" },
            f["instance_id"].as_str().unwrap_or("?"),
            f["error"].as_str().unwrap_or(f["workload"].as_str().unwrap_or("?")),
        ));
    }
    out.push_str(&format!(
        "artifacts swept: {}\nnotice: {}\n",
        r["artifacts_swept"], r["notice"].as_str().unwrap_or("?")
    ));
    out
}

/// Placements table for `weaver workload list` / `status`.
pub fn render_placements(status: &Value) -> String {
    let rows = status["instances"].as_array().cloned().unwrap_or_default();
    if rows.is_empty() {
        return "No placed instances\n".to_owned();
    }
    let mut out = format!(
        "{:<40} {:<16} {:<18} {:<10}\n",
        "INSTANCE", "NODE", "VARIANT", "STATE"
    );
    for r in rows {
        let p = &r["placement"];
        let state = r["status"]["Ok"]["status"]["state"]
            .as_str()
            .unwrap_or("unreachable");
        out.push_str(&format!(
            "{:<40} {:<16} {:<18} {:<10}\n",
            p["instance_id"].as_str().unwrap_or("?"),
            p["node_id"].as_str().unwrap_or("?"),
            p["variant"].as_str().unwrap_or("?"),
            state,
        ));
    }
    out
}

/// Render a verb's result.
pub fn render(cmd: &WorkloadPlaceCmd, result: &Value) -> String {
    match cmd {
        WorkloadPlaceCmd::Place(a) if !a.json => summary(result, a.explain),
        WorkloadPlaceCmd::Explain(a) if !a.json => summary(result, true),
        WorkloadPlaceCmd::Status {
            instance_id: None,
            json: false,
        } => render_placements(result),
        WorkloadPlaceCmd::Revoke(a) if !a.json => revoke_summary(result),
        WorkloadPlaceCmd::Logs { .. } => format!(
            "{}{}",
            result["stdout"]
                .as_str()
                .unwrap_or(result["note"].as_str().unwrap_or("")),
            result["stderr"].as_str().unwrap_or("")
        ),
        _ => format!(
            "{}\n",
            serde_json::to_string_pretty(result).unwrap_or_default()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(pkg: &str) -> PlaceArgs {
        PlaceArgs {
            package: pkg.into(),
            peers: vec!["pi5:9471".into()],
            pin: None,
            prefer: vec![],
            avoid: vec![],
            allow_emulated: false,
            no_migrate: false,
            mode: "interval".into(),
            interval: 1,
            csi_port: 5006,
            args: vec![],
            no_start: false,
            explain: true,
            json: false,
        }
    }

    fn req(cmd: &WorkloadPlaceCmd) -> (&'static str, Value) {
        request(cmd, Path::new("/")).unwrap()
    }

    #[test]
    fn a_relative_package_is_resolved_against_the_callers_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("work/pkg")).unwrap();
        let cwd = tmp.path().join("work");
        let (_, p) = request(&WorkloadPlaceCmd::Place(args("./pkg")), &cwd).unwrap();
        let sent = PathBuf::from(p["package_dir"].as_str().unwrap());
        assert!(sent.is_absolute());
        assert_eq!(sent, cwd.join("pkg").canonicalize().unwrap());
        let err = request(&WorkloadPlaceCmd::Place(args("./missing")), &cwd).unwrap_err();
        assert!(err.contains("missing"), "{err}");
    }

    #[derive(clap::Parser, Debug)]
    struct Cli {
        #[command(subcommand)]
        cmd: WorkloadPlaceCmd,
    }

    fn parse(argv: &[&str]) -> Result<WorkloadPlaceCmd, clap::Error> {
        use clap::Parser;
        Cli::try_parse_from(std::iter::once("weaver").chain(argv.iter().copied())).map(|c| c.cmd)
    }

    #[test]
    fn place_sends_repeated_arg_flags_as_args_and_omits_them_when_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let pkg = tmp.path().to_str().unwrap();
        let cmd = parse(&["place", pkg, "--arg=--state-dir", "--arg", "/data/s", "--arg=--watch"]).unwrap();
        let (m, p) = req(&cmd);
        assert_eq!(m, "workload.place");
        assert_eq!(p["args"], json!(["--state-dir", "/data/s", "--watch"]));
        let (_, p) = req(&parse(&["place", pkg]).unwrap());
        assert!(p.get("args").is_none(), "{p}");
    }

    #[test]
    fn revoke_names_exactly_one_subject_and_sends_it_as_workload_revoke() {
        let key = "ab".repeat(32);
        for (flag, val, field) in [
            ("--package", "cog.x", "package"),
            ("--signer", key.as_str(), "signer"),
            ("--hash", key.as_str(), "hash"),
        ] {
            let cmd = parse(&["revoke", flag, val, "--reason", "leaked"]).unwrap();
            let (m, p) = req(&cmd);
            assert_eq!(m, "workload.revoke");
            assert_eq!(p, json!({ field: val, "reason": "leaked" }));
        }
        let (_, p) = req(&parse(&["revoke", "--package", "cog.x"]).unwrap());
        assert_eq!(p, json!({ "package": "cog.x" }), "no reason sent unless given");
        // None, or more than one: refused before anything is sent.
        assert!(parse(&["revoke"]).is_err());
        assert!(parse(&["revoke", "--package", "a", "--signer", &key]).is_err());
        assert!(parse(&["revoke", "--hash", &key, "--package", "a"]).is_err());
    }

    #[test]
    fn revoke_summary_says_what_was_stopped_and_what_was_not_reached() {
        let r = json!({
            "revoked": { "kind": "signer_key", "id": "ab", "reason": "leaked" },
            "newly_revoked": true, "persisted": true, "artifacts_swept": 3,
            "forced": [
                { "instance_id": "i-1", "workload": "w", "unloaded": true },
                { "instance_id": "i-2", "workload": "w", "unloaded": false, "error": "adapter busy" },
            ],
            "notice": "not issued: this node's key is not a pinned operator key",
        });
        let out = summary_for_revoke(&r);
        assert!(out.contains("revoked signer_key ab"), "{out}");
        assert!(out.contains("stopped and unloaded i-1"), "{out}");
        assert!(out.contains("COULD NOT unload i-2 (adapter busy)"), "{out}");
        assert!(out.contains("artifacts swept: 3"), "{out}");
        assert!(out.contains("not issued"), "{out}");
        let unsaved = json!({ "revoked": {"kind": "package", "id": "p"}, "newly_revoked": true,
            "persisted": false, "persist_error": "disk full", "forced": [], "artifacts_swept": 0, "notice": "x" });
        assert!(summary_for_revoke(&unsaved).contains("NOT saved to disk: disk full"));
    }

    fn summary_for_revoke(r: &Value) -> String {
        render(
            &WorkloadPlaceCmd::Revoke(RevokeArgs {
                package: Some("x".into()),
                signer: None,
                hash: None,
                reason: None,
                json: false,
            }),
            r,
        )
    }

    #[test]
    fn place_that_placed_nothing_is_a_failure() {
        let place = WorkloadPlaceCmd::Place(args("/"));
        let refused = json!({"decision": {"placement": {"node_id": "n"}}, "placed": null});
        assert!(failure(&place, &refused).unwrap().contains("not placed"));
        let none = json!({"decision": {"placement": null}});
        assert!(failure(&place, &none).unwrap().contains("unplaceable"));
        let ok = json!({"decision": {"placement": {"node_id": "n"}}, "placed": {"node_id": "n"}});
        assert!(failure(&place, &ok).is_none());
        assert!(failure(&WorkloadPlaceCmd::Explain(args("/")), &refused).is_none());
    }

    #[test]
    fn verbs_map_to_the_workload_rpc_family() {
        let (m, p) = req(&WorkloadPlaceCmd::Place(args("/")));
        assert_eq!(m, "workload.place");
        assert_eq!(p["peers"][0], "pi5:9471");
        assert_eq!(p["start"], true);
        assert!(p.get("pin").is_none());
        let mut pinned = args("/");
        pinned.pin = Some("n-1".into());
        let (m, p) = req(&WorkloadPlaceCmd::Explain(pinned));
        assert_eq!((m, p["pin"].as_str()), ("workload.explain", Some("n-1")));
        let unload = WorkloadPlaceCmd::Unload {
            target: "i-1".into(),
            catalog: false,
        };
        assert_eq!(req(&unload).1, json!({"instance_id": "i-1"}));
        let seed = WorkloadPlaceCmd::PlaceSeed {
            node_id: "seed-lab".into(),
            cog: "fall-detect@1.0.0".into(),
            sha256: None,
            mode: "once".into(),
            no_start: false,
        };
        let (m, p) = req(&seed);
        assert_eq!(m, "workload.place");
        assert_eq!(p["store_pin"]["node_id"], "seed-lab");
        assert_eq!(p["store_pin"]["id"], "fall-detect");
        assert_eq!(p["store_pin"]["version"], "1.0.0");
        let bad = WorkloadPlaceCmd::PlaceSeed {
            node_id: "s".into(),
            cog: "fall-detect".into(),
            sha256: None,
            mode: "once".into(),
            no_start: false,
        };
        assert!(request(&bad, Path::new("/")).is_err());
        let cat = WorkloadPlaceCmd::Unload {
            target: "w".into(),
            catalog: true,
        };
        assert_eq!(req(&cat).1, json!({"name": "w"}));
        assert_eq!(
            req(&WorkloadPlaceCmd::Status {
                instance_id: None,
                json: false
            })
            .1,
            json!({})
        );
    }

    #[test]
    fn summary_names_the_node_or_why_not() {
        let placed = json!({"explain": "EXPLAIN\n", "decision_id": "d1", "decision": {"placement": {"node_id": "n-pi"}},
            "attempts": [{}], "placed": {"workload": "anomaly-detect", "node_id": "n-pi", "variant": "aarch64-native", "instance_id": "i"}});
        let s = summary(&placed, true);
        assert!(s.starts_with("EXPLAIN"));
        assert!(s.contains("placed anomaly-detect on n-pi via aarch64-native"));
        let refused = json!({"decision_id": "d2", "decision": {"placement": {"node_id": "n-mac"}}, "attempts": [{}], "placed": null});
        assert!(summary(&refused, false).contains("every candidate refused"));
        let none = json!({"decision_id": "d3", "decision": {"placement": null}, "attempts": []});
        assert!(summary(&none, false).contains("unplaceable"));
        let dry = json!({"decision_id": "d4", "decision": {"placement": {"node_id": "n-pi"}}, "attempts": []});
        assert!(summary(&dry, false).contains("n-pi (not dispatched)"));
    }

    #[test]
    fn placements_table_shows_state_or_unreachable() {
        let st = json!({"instances": [
            {"placement": {"instance_id": "a-1", "node_id": "n-pi", "variant": "aarch64-native"},
             "status": {"Ok": {"status": {"state": "running"}}}},
            {"placement": {"instance_id": "b-2", "node_id": "n-gone", "variant": "aarch64-native"},
             "status": {"Err": "unreachable"}}]});
        let t = render_placements(&st);
        assert!(t.contains("running"));
        assert!(
            t.lines()
                .any(|l| l.starts_with("b-2") && l.contains("unreachable"))
        );
        assert_eq!(render_placements(&json!({})), "No placed instances\n");
    }
}
