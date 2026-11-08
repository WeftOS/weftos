//! The IAM policy the daemon attaches to a principal's access key (ADR-126 section 3):
//! RustFS enforces what governance decided.

use serde_json::{Value, json};

use crate::access::{Access, Grant};
use crate::name::StoreName;

fn actions(a: Access) -> Vec<&'static str> {
    let mut v = vec!["s3:GetObject", "s3:ListBucket", "s3:ListBucketVersions", "s3:GetBucketLocation"];
    let put = ["s3:PutObject", "s3:AbortMultipartUpload", "s3:ListMultipartUploadParts"];
    match a {
        Access::Read => {}
        // CI: read only at RustFS. Write-once cannot be stated in RustFS IAM (it rejects the
        // `s3:if-none-match` condition key, 1.0.4), and a plain PutObject allow would let the key
        // overwrite or copy over an artifact. So CI's puts go through the daemon, which makes them
        // `If-None-Match: *` with its own key; the key itself can read and list, nothing else.
        Access::Append => {}
        // The sweeper: delete, never put.
        Access::Sweep => v.push("s3:DeleteObject"),
        Access::Write | Access::Admin => {
            v.extend(put);
            v.push("s3:DeleteObject");
        }
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
}
