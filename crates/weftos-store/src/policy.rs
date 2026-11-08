//! The IAM policy the daemon attaches to a principal's access key (ADR-126 section 3):
//! RustFS enforces what governance decided.

use serde_json::{Value, json};

use crate::access::{Access, Grant};
use crate::name::StoreName;

fn actions(a: Access) -> Vec<&'static str> {
    let mut v = vec!["s3:GetObject", "s3:ListBucket", "s3:GetBucketLocation"];
    if a >= Access::Write {
        v.extend(["s3:PutObject", "s3:DeleteObject", "s3:AbortMultipartUpload", "s3:ListMultipartUploadParts"]);
    }
    v
}

/// A policy allowing exactly the enabled grants, nothing else. No enabled grants gives a
/// policy with no statements (everything denied).
pub fn for_grants(grants: &[Grant]) -> Value {
    let mut st = Vec::new();
    for g in grants.iter().filter(|g| g.enabled) {
        let Ok(name) = StoreName::parse(&g.store) else { continue };
        let b = name.bucket();
        st.push(json!({
            "Effect": "Allow",
            "Action": actions(g.access),
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
