//! Argument plumbing for `weaver store view <verb>` (ADR-126 E-1). The CLI turns flags into the
//! `store.view.<verb>` params; the daemon validates them.
//!
//! ```text
//! weaver store view open    --name <store> --root <abs> (--spec <file.json> | --include P[=AT].. [--work P [--work-start clone|empty]]
//!                           [--deny PAT].. [--mount-point DIR].. [--lease-ttl-ms N] [--device D]) [--read-only]
//! weaver store view status|refresh|renew --root <abs> [--name <store>]
//! weaver store view publish --name <store> --root <abs>
//! weaver store view resolve --name <store> --root <abs> --path <rel> --keep local|remote
//! weaver store view close   --name <store> --root <abs> (--publish | --discard)
//! ```
//!
//! `--spec` is read here, by the CLI, and sent as the `spec` param: the daemon never opens a path the
//! caller names for the spec. The file is a `ViewSpec` JSON without `bucket` (the bucket is the one
//! `--name` binds to). `--spec` and the spec flags are alternatives; mixing them is refused.

use serde_json::{Map, Value, json};

const FLAGS: &[&str] = &["json", "read_only", "publish", "discard"];
const REPEAT: &[&str] = &["include", "deny", "mount_point"];
const SPEC_FLAGS: &[&str] = &["include", "work", "work_start", "deny", "mount_point", "lease_ttl_ms", "device"];

/// `--key value` pairs to the daemon's params. `agent` and `host` come from the caller.
pub fn view_params(rest: &[String], agent: &str, host: &str) -> anyhow::Result<Value> {
    let mut m = Map::new();
    let mut rep: Map<String, Value> = Map::new();
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        let Some(k) = a.strip_prefix("--").filter(|k| !k.is_empty()) else {
            anyhow::bail!("unexpected argument '{a}' (expected --key value)");
        };
        let k = k.replace('-', "_");
        if FLAGS.contains(&k.as_str()) {
            m.insert(k, Value::Bool(true));
            continue;
        }
        let v = it.next().ok_or_else(|| anyhow::anyhow!("--{k} needs a value"))?.clone();
        if REPEAT.contains(&k.as_str()) {
            rep.entry(k).or_insert_with(|| json!([])).as_array_mut().expect("array").push(Value::String(v));
        } else {
            m.insert(k, Value::String(v));
        }
    }
    let flagged = SPEC_FLAGS.iter().any(|f| m.contains_key(*f) || rep.contains_key(*f));
    if let Some(file) = m.remove("spec") {
        if flagged {
            anyhow::bail!("--spec and the spec flags (--include, --work, ...) are alternatives; give one");
        }
        let file = file.as_str().unwrap_or_default().to_owned();
        let text = std::fs::read_to_string(&file).map_err(|e| anyhow::anyhow!("--spec {file}: {e}"))?;
        m.insert("spec".into(), serde_json::from_str(&text).map_err(|e| anyhow::anyhow!("--spec {file}: not JSON: {e}"))?);
    } else if flagged {
        let spec = flag_spec(&mut m, &rep)?;
        m.insert("spec".into(), spec);
    }
    m.entry("agent").or_insert_with(|| Value::String(agent.into()));
    m.entry("host").or_insert_with(|| Value::String(host.into()));
    Ok(Value::Object(m))
}

/// The `ViewSpec` the flags describe; the flags are consumed from `m`.
fn flag_spec(m: &mut Map<String, Value>, rep: &Map<String, Value>) -> anyhow::Result<Value> {
    let mut spec = Map::new();
    let list = |k: &str| rep.get(k).and_then(Value::as_array).cloned().unwrap_or_default();
    let base: Vec<Value> = list("include")
        .iter()
        .filter_map(Value::as_str)
        .map(|s| {
            let (prefix, at) = s.split_once('=').unwrap_or((s, ""));
            json!({"prefix": prefix, "at": at})
        })
        .collect();
    spec.insert("base".into(), json!(base));
    let start = m.remove("work_start");
    match (m.remove("work"), start) {
        (Some(p), s) => {
            let mut w = json!({"prefix": p});
            if let Some(s) = s {
                w["start"] = s;
            }
            spec.insert("work".into(), w);
        }
        (None, Some(_)) => anyhow::bail!("--work-start needs --work"),
        (None, None) => {}
    }
    spec.insert("deny".into(), json!(list("deny")));
    spec.insert("mount_points".into(), json!(list("mount_point")));
    if let Some(t) = m.remove("lease_ttl_ms") {
        let n: u64 = t.as_str().unwrap_or_default().parse().map_err(|_| anyhow::anyhow!("--lease-ttl-ms must be a whole number of milliseconds"))?;
        spec.insert("lease_ttl_ms".into(), json!(n));
    }
    if let Some(d) = m.remove("device") {
        spec.insert("device".into(), d);
    }
    Ok(Value::Object(spec))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn flags_become_a_spec_and_repeats_accumulate() {
        let p = view_params(&a(&["--name", "n", "--root", "/r", "--include", "docs=docs", "--include", "lib", "--work", "proj/", "--work-start", "empty", "--deny", "secret*", "--mount-point", "tmp", "--lease-ttl-ms", "60000", "--read-only"]), "ag", "h1").unwrap();
        assert_eq!((p["name"].as_str(), p["root"].as_str(), p["read_only"].as_bool(), p["agent"].as_str(), p["host"].as_str()), (Some("n"), Some("/r"), Some(true), Some("ag"), Some("h1")));
        assert_eq!(p["spec"]["base"], json!([{"prefix": "docs", "at": "docs"}, {"prefix": "lib", "at": ""}]));
        assert_eq!(p["spec"]["work"], json!({"prefix": "proj/", "start": "empty"}));
        assert_eq!((p["spec"]["deny"].clone(), p["spec"]["mount_points"].clone(), p["spec"]["lease_ttl_ms"].clone()), (json!(["secret*"]), json!(["tmp"]), json!(60000)));
    }

    #[test]
    fn a_spec_file_is_read_by_the_cli_and_cannot_be_mixed_with_flags() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("spec.json");
        std::fs::write(&f, r#"{"base":[{"prefix":"docs"}],"work":{"prefix":"w/"}}"#).unwrap();
        let fs = f.to_str().unwrap();
        let p = view_params(&a(&["--name", "n", "--root", "/r", "--spec", fs]), "ag", "h").unwrap();
        assert_eq!(p["spec"]["work"]["prefix"], "w/");
        assert!(view_params(&a(&["--spec", fs, "--include", "x"]), "ag", "h").unwrap_err().to_string().contains("alternatives"));
        assert!(view_params(&a(&["--spec", "/no/such/spec.json"]), "ag", "h").is_err());
        std::fs::write(&f, "not json").unwrap();
        assert!(view_params(&a(&["--spec", fs]), "ag", "h").unwrap_err().to_string().contains("not JSON"));
    }

    #[test]
    fn stray_arguments_missing_values_and_bad_numbers_are_refused() {
        assert!(view_params(&a(&["stray"]), "a", "h").is_err());
        assert!(view_params(&a(&["--root"]), "a", "h").is_err());
        assert!(view_params(&a(&["--work-start", "empty"]), "a", "h").is_err(), "needs --work");
        assert!(view_params(&a(&["--include", "x", "--lease-ttl-ms", "soon"]), "a", "h").is_err());
        let p = view_params(&a(&["--root", "/r", "--discard"]), "a", "h").unwrap();
        assert_eq!((p["discard"].as_bool(), p.get("spec")), (Some(true), None));
    }
}
