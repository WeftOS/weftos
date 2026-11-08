//! The IAM policy the daemon attaches to a principal's access key (ADR-126 section 3):
//! RustFS enforces what governance decided.

use serde_json::{Value, json};

use crate::access::{Access, Grant};
use crate::name::StoreName;

/// Two owner decisions about what the direct (S3) keys of the `append` and `sweep` roles may do.
/// Both default to off, the safer choice; each widens only the key's own power at RustFS, never what
/// the service lets a principal do. See the ADR-126 section "Owner decisions on the release roles".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PolicyOpts {
    /// The sweeper's key may also `s3:PutObject` (so it can overwrite or copy over an artifact).
    pub sweeper_put: bool,
    /// CI's key may `s3:PutObject` directly (uploads need no trip through the daemon) but can then
    /// overwrite or copy over an artifact; only a locked store's retention protects existing versions.
    pub ci_direct_put: bool,
}

const PUT: [&str; 3] = ["s3:PutObject", "s3:AbortMultipartUpload", "s3:ListMultipartUploadParts"];

fn actions(a: Access, o: &PolicyOpts) -> Vec<&'static str> {
    let mut v = vec!["s3:GetObject", "s3:ListBucket", "s3:ListBucketVersions", "s3:GetBucketLocation"];
    match a {
        Access::Read => {}
        // CI: read only at RustFS. Write-once cannot be stated in RustFS IAM (it rejects the
        // `s3:if-none-match` condition key, 1.0.4), and a plain PutObject allow would let the key
        // overwrite or copy over an artifact. So CI's puts go through the daemon, which makes them
        // `If-None-Match: *` with its own key; the key itself can read and list, nothing else.
        Access::Append => {
            if o.ci_direct_put {
                v.extend(PUT);
            }
        }
        // The sweeper: delete, never put.
        Access::Sweep => {
            v.push("s3:DeleteObject");
            if o.sweeper_put {
                v.extend(PUT);
            }
        }
        Access::Write | Access::Admin => {
            v.extend(PUT);
            v.push("s3:DeleteObject");
        }
    }
    v
}

/// A policy allowing exactly the enabled grants, nothing else. No enabled grants gives a
/// policy with no statements (everything denied).
pub fn for_grants(grants: &[Grant]) -> Value {
    for_grants_with(grants, &PolicyOpts::default())
}

/// [`for_grants`] with the owner's switches for the `append` and `sweep` roles.
pub fn for_grants_with(grants: &[Grant], opts: &PolicyOpts) -> Value {
    let mut st = Vec::new();
    for g in grants.iter().filter(|g| g.enabled) {
        let Ok(name) = StoreName::parse(&g.store) else { continue };
        let b = name.bucket();
        st.push(json!({
            "Effect": "Allow",
            "Action": actions(g.access, opts),
            "Resource": [format!("arn:aws:s3:::{b}"), format!("arn:aws:s3:::{b}/*")],
        }));
    }
    // Control state is the daemon's alone (review F4); an explicit Deny beats any Allow above.
    let mut deny = Vec::new();
    for g in grants.iter().filter(|g| g.enabled) {
        let Ok(name) = StoreName::parse(&g.store) else { continue };
        let prefix = match name {
            StoreName::Worktree { .. } => ".wt/",
            StoreName::Project { .. } => "worktrees/",
            _ => continue,
        };
        deny.push(format!("arn:aws:s3:::{}/{prefix}*", name.bucket()));
    }
    // Deterministic: the same grants give the same document whatever order they were read in.
    deny.sort();
    deny.dedup();
    if !deny.is_empty() {
        st.push(json!({
            "Effect": "Deny",
            "Action": ["s3:PutObject", "s3:DeleteObject", "s3:AbortMultipartUpload"],
            "Resource": deny,
        }));
    }
    json!({"Version": "2012-10-17", "Statement": st})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::access::Principal;

    fn g(store: &str, a: Access, enabled: bool) -> Grant {
        Grant { principal: Principal::parse("cog:c").unwrap(), store: store.into(), access: a, enabled, by: "t".into(), ts_ms: 0 }
    }

    #[test]
    fn read_grant_has_no_write_actions_and_disabled_grants_are_omitted() {
        let s = StoreName::node("m", "n1").unwrap().to_string();
        let t = StoreName::node("m", "n2").unwrap().to_string();
        let p = for_grants(&[g(&s, Access::Read, true), g(&t, Access::Write, false)]);
        let st = p["Statement"].as_array().unwrap();
        assert_eq!(st.len(), 1);
        let acts: Vec<&str> = st[0]["Action"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
        assert!(acts.contains(&"s3:GetObject") && !acts.contains(&"s3:PutObject"));
        assert_eq!(st[0]["Resource"][0], "arn:aws:s3:::n-n1");
        assert!(for_grants(&[g(&s, Access::Write, false)])["Statement"].as_array().unwrap().is_empty());
    }

    #[test]
    fn ci_appends_the_sweeper_deletes_and_neither_does_the_others_job() {
        let s = StoreName::release("m", "01HZX3K9Q5M2N7P8R4T6V0WABC", "stable").unwrap().to_string();
        let acts = |a| {
            let p = for_grants(&[g(&s, a, true)]);
            p["Statement"][0]["Action"].as_array().unwrap().iter().filter_map(Value::as_str).map(str::to_owned).collect::<Vec<_>>()
        };
        let ci = acts(Access::Append);
        assert!(ci.contains(&"s3:GetObject".into()) && !ci.contains(&"s3:PutObject".into()) && !ci.contains(&"s3:DeleteObject".into()), "append is read-only at RustFS; the daemon does the write-once puts");
        let sw = acts(Access::Sweep);
        assert!(sw.contains(&"s3:DeleteObject".into()) && !sw.contains(&"s3:PutObject".into()) && sw.contains(&"s3:GetObject".into()));
        let w = acts(Access::Write);
        assert!(w.contains(&"s3:PutObject".into()) && w.contains(&"s3:DeleteObject".into()));
    }

    #[test]
    fn the_owner_switches_widen_only_the_ci_and_sweeper_keys_and_default_off() {
        let s = StoreName::release("m", "01HZX3K9Q5M2N7P8R4T6V0WABC", "stable").unwrap().to_string();
        let acts = |a, o: PolicyOpts| {
            let p = for_grants_with(&[g(&s, a, true)], &o);
            p["Statement"][0]["Action"].as_array().unwrap().iter().filter_map(Value::as_str).map(str::to_owned).collect::<Vec<_>>()
        };
        let put = "s3:PutObject".to_owned();
        assert_eq!(PolicyOpts::default(), PolicyOpts { sweeper_put: false, ci_direct_put: false });
        // Off (the default): neither key can put, and `for_grants` is the off policy.
        assert!(!acts(Access::Append, PolicyOpts::default()).contains(&put));
        assert!(!acts(Access::Sweep, PolicyOpts::default()).contains(&put));
        assert_eq!(for_grants(&[g(&s, Access::Sweep, true)]), for_grants_with(&[g(&s, Access::Sweep, true)], &PolicyOpts::default()));
        // Each switch moves only its own role.
        let sw_on = PolicyOpts { sweeper_put: true, ..Default::default() };
        assert!(acts(Access::Sweep, sw_on).contains(&put) && acts(Access::Sweep, sw_on).contains(&"s3:DeleteObject".to_owned()));
        assert!(!acts(Access::Append, sw_on).contains(&put) && !acts(Access::Read, sw_on).contains(&put));
        let ci_on = PolicyOpts { ci_direct_put: true, ..Default::default() };
        assert!(acts(Access::Append, ci_on).contains(&put) && !acts(Access::Append, ci_on).contains(&"s3:DeleteObject".to_owned()));
        assert!(!acts(Access::Sweep, ci_on).contains(&put) && !acts(Access::Read, ci_on).contains(&put));
    }

    #[test]
    fn worktree_and_project_grants_deny_writes_to_control_state() {
        let w = StoreName::worktree("m", "01JBXK2M3N4P5Q6R7S8T9V0W1X", "feat-x").unwrap();
        let p = StoreName::project("m", "01JBXK2M3N4P5Q6R7S8T9V0W1X").unwrap();
        let pol = for_grants(&[g(&w.to_string(), Access::Write, true), g(&p.to_string(), Access::Write, true)]);
        let st = pol["Statement"].as_array().unwrap();
        let deny = st.iter().find(|s| s["Effect"] == "Deny").expect("a deny statement");
        let res: Vec<&str> = deny["Resource"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
        assert!(res.contains(&format!("arn:aws:s3:::{}/.wt/*", w.bucket()).as_str()));
        assert!(res.contains(&format!("arn:aws:s3:::{}/worktrees/*", p.bucket()).as_str()));
    }
}
