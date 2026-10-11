//! `weaver cluster nodes --facts`: print signed node facts with provenance
//! (card mesh-placement-03).
//!
//! Each entry's signed envelope is re-verified here, independently of the
//! daemon, before it is printed as verified.

use clawft_kernel::node_facts::TierSource;
use clawft_kernel::node_facts_advert::verify_node_facts;
use clawft_types::placement::{AttrValue, Capability, NodeFacts, TrustTier};
use comfy_table::{Table, presets};

use crate::client::DaemonClient;
use crate::node_facts_rpc::FactsEntry;
use crate::protocol;

const BYTE_ATTRS: &[&str] = &["total", "free", "mem_bytes", "mem_free_bytes"];

fn gib(n: i64) -> String {
    format!("{:.1} GiB", n as f64 / (1u64 << 30) as f64)
}

/// Render one attribute value; byte-count attributes also get GiB.
pub fn fmt_attr(name: &str, v: &AttrValue) -> String {
    match v {
        AttrValue::Int(i) if BYTE_ATTRS.contains(&name) => format!("{name}={i} ({})", gib(*i)),
        AttrValue::Str(s) => format!("{name}={s:?}"),
        AttrValue::List(l) => {
            let items: Vec<String> = l
                .iter()
                .map(|x| match x {
                    AttrValue::Str(s) => s.clone(),
                    other => fmt_attr("", other).trim_start_matches('=').to_string(),
                })
                .collect();
            format!("{name}=[{}]", items.join(","))
        }
        AttrValue::Int(i) => format!("{name}={i}"),
        AttrValue::Float(f) => format!("{name}={f}"),
        AttrValue::Bool(b) => format!("{name}={b}"),
    }
}

fn row(c: &Capability) -> Vec<String> {
    let attrs: Vec<String> = c.attrs.iter().map(|(k, v)| fmt_attr(k, v)).collect();
    vec![
        c.id.to_string(),
        format!("{:?}", c.provenance).to_lowercase(),
        format!("{:?}", c.state).to_lowercase() + if c.exclusive { " excl" } else { "" },
        attrs.join(" "),
    ]
}

/// Why the daemon-supplied `shown` facts are not what the signed `signed`
/// facts say, or `None` if they are. Two differences are expected and allowed:
/// the receiver caps a peer's provenance (never raises it), and, once deltas
/// are applied (`deltas`), capability state and free memory move.
pub fn mismatch(signed: &NodeFacts, shown: &NodeFacts, deltas: bool) -> Option<String> {
    if shown.node_id != signed.node_id {
        return Some(format!("it names node {}, the signature covers {}", shown.node_id, signed.node_id));
    }
    if (shown.version, shown.issued_at, shown.ttl_secs, shown.seq)
        != (signed.version, signed.issued_at, signed.ttl_secs, signed.seq)
        || shown.notes != signed.notes
        || shown.capabilities.len() != signed.capabilities.len()
    {
        return Some("the facts differ from the signed envelope".into());
    }
    for (s, d) in signed.capabilities.iter().zip(&shown.capabilities) {
        let live = deltas && s.id.as_str().starts_with("mem.");
        // A delta sets `free` on the memory capabilities, even where the base had none.
        let keep = |k: &&String| !(live && k.as_str() == "free");
        let attrs_ok = s.attrs.keys().filter(keep).eq(d.attrs.keys().filter(keep))
            && s.attrs
                .iter()
                .filter(|(k, _)| keep(k))
                .all(|(k, v)| d.attrs.get(k) == Some(v));
        if s.id != d.id
            || s.exclusive != d.exclusive
            || d.provenance > s.provenance
            || !attrs_ok
            || (!deltas && s.state != d.state)
        {
            return Some(format!("capability {} differs from the signed envelope", s.id));
        }
    }
    None
}

/// The trust column: what the receiver holds, and what it means.
fn trust_label(tier: TrustTier, source: Option<TierSource>) -> String {
    match (tier, source) {
        (TrustTier::Discovered, _) => "discovered".into(),
        (t, Some(TierSource::Mesh)) => format!(
            "mesh-verified (node id checked at admission, held as {t:?}; not operator-paired)"
        ),
        (t, _) => format!("{t:?}").to_lowercase(),
    }
}

/// Render entries as text (pure; tested).
///
/// The signature is checked here, and what is printed is checked against it:
/// capabilities come from the signed envelope unless the daemon's copy
/// matches it (allowing only the receiver's provenance cap and live deltas).
pub fn render(entries: &[FactsEntry], now: u64) -> String {
    if entries.is_empty() {
        return "No node facts cached.\n".into();
    }
    let mut out = String::new();
    for e in entries {
        let checked = verify_node_facts(&e.signed, now);
        let (verified, shown) = match &checked {
            Err(err) => (
                format!("NOT VERIFIED: {err}; the contents below are unverified"),
                &e.facts,
            ),
            Ok(signed) if signed.node_id != e.node_id => (
                format!(
                    "NOT VERIFIED: entry is for node {}, the signature covers {}; showing the signed facts",
                    e.node_id, signed.node_id
                ),
                signed,
            ),
            Ok(signed) => match mismatch(signed, &e.facts, e.delta_seq > 0) {
                Some(why) => (
                    format!("NOT VERIFIED: {why}; showing the signed facts"),
                    signed,
                ),
                None if e.delta_seq > 0 => (
                    format!(
                        "id-bound signature (ed25519); live state from update #{} is daemon-reported, not re-verified here",
                        e.delta_seq
                    ),
                    &e.facts,
                ),
                None => ("id-bound signature (ed25519)".to_string(), &e.facts),
            },
        };
        out.push_str(&format!(
            "Node {}{}  trust={}  seq={}  issued_at={}  expires_in={}s\nSignature: {verified}\n",
            e.node_id,
            if e.local { " (local)" } else { "" },
            trust_label(e.trust_tier, e.tier_source),
            shown.seq,
            shown.issued_at,
            e.expires_at.saturating_sub(now),
        ));
        let mut table = Table::new();
        table.load_preset(presets::UTF8_FULL_CONDENSED);
        table.set_header(vec!["Capability", "Provenance", "State", "Attributes"]);
        for c in shown.capabilities() {
            table.add_row(row(c));
        }
        out.push_str(&format!("{table}\n"));
        if !shown.notes.is_empty() {
            out.push_str("Provenance notes:\n");
            for n in &shown.notes {
                out.push_str(&format!("  {}: {}\n", n.probe, n.note));
            }
        }
        out.push('\n');
    }
    out
}

/// The entries of a `cluster.facts` result, and, when `--refresh` was skipped
/// by the daemon's minimum gap, when the cached facts were last probed. Reads
/// both result shapes: a bare list, or the refresh object.
pub fn parse_result(value: serde_json::Value) -> anyhow::Result<(Vec<FactsEntry>, Option<u64>)> {
    if value.is_array() {
        return Ok((serde_json::from_value(value)?, None));
    }
    let r: crate::node_facts_rpc::RefreshedFacts = serde_json::from_value(value)?;
    Ok((r.entries, (!r.refreshed).then_some(r.cached_as_of)))
}

/// The line printed when the daemon skipped a requested refresh.
pub fn skipped_note(cached_as_of: u64, now: u64) -> String {
    if cached_as_of == 0 {
        "refresh skipped: one ran moments ago; showing the cached facts".to_string()
    } else {
        format!(
            "refresh skipped (at most one every {}s): showing facts cached as of {}s ago",
            crate::node_facts_rpc::MIN_FORCED_REFRESH_SECS,
            now.saturating_sub(cached_as_of)
        )
    }
}

/// Fetch `cluster.facts` and print it.
pub async fn run(
    client: &mut DaemonClient,
    refresh: bool,
    node: Option<String>,
    json: bool,
) -> anyhow::Result<()> {
    let params = serde_json::json!({ "refresh": refresh, "node_id": node });
    let resp = client
        .call(protocol::Request::with_params("cluster.facts", params))
        .await?;
    if !resp.ok {
        anyhow::bail!(resp.error.unwrap_or_else(|| "unknown error".into()));
    }
    let value = resp.result.unwrap_or_default();
    if json {
        println!("{}", serde_json::to_string_pretty(&value)?);
        return Ok(());
    }
    let (entries, skipped) = parse_result(value)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if let Some(at) = skipped {
        println!("{}", skipped_note(at, now));
    }
    print!("{}", render(&entries, now));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clawft_kernel::node_facts_advert::sign_node_facts;
    use clawft_types::placement::{CapabilityId, NodeFacts, ProbeNote, Provenance, TrustTier};
    use ed25519_dalek::SigningKey;

    fn entry(tamper: bool) -> FactsEntry {
        let key = SigningKey::from_bytes(&[7u8; 32]);
        let id = clawft_kernel::node_id_from_pubkey(&key.verifying_key().to_bytes());
        let mut f = NodeFacts::new(id.clone(), 1_000, 600, 3);
        f.capabilities = vec![
            Capability::new(
                CapabilityId::new("mem.unified").unwrap(),
                Provenance::Probed,
            )
            .with_attr("total", 137_438_953_472i64),
            Capability::new(
                CapabilityId::new("accel.npu.ane").unwrap(),
                Provenance::Claimed,
            ),
        ];
        f.notes = vec![ProbeNote::new(
            "accel.npu.ane",
            "inferred from chip + CoreML",
        )];
        let mut signed = sign_node_facts(&f, &key).unwrap();
        if tamper {
            signed.payload = signed.payload.replace("claimed", "measured");
        }
        FactsEntry {
            node_id: id,
            local: true,
            trust_tier: TrustTier::Pinned,
            tier_source: Some(TierSource::Operator),
            received_at: 1_000,
            expires_at: 1_600,
            delta_seq: 0,
            facts: f,
            signed,
        }
    }

    #[test]
    fn renders_provenance_memory_and_verification() {
        let text = render(&[entry(false)], 1_100);
        assert!(text.contains("(local)"));
        assert!(text.contains("Signature: id-bound signature"));
        assert!(text.contains("total=137438953472 (128.0 GiB)"));
        assert!(text.contains("claimed"));
        assert!(text.contains("accel.npu.ane: inferred from chip + CoreML"));
    }

    #[test]
    fn tampered_envelope_is_flagged() {
        let text = render(&[entry(true)], 1_100);
        assert!(text.contains("NOT VERIFIED"), "{text}");
    }

    #[test]
    fn what_is_printed_is_what_was_signed() {
        // The daemon's copy claims a higher provenance than the signature covers.
        let mut e = entry(false);
        e.facts.capabilities[1].provenance = Provenance::Measured;
        let text = render(&[e], 1_100);
        assert!(text.contains("NOT VERIFIED"), "{text}");
        assert!(text.contains("accel.npu.ane"), "{text}");
        assert!(!text.contains("measured"), "daemon-supplied value must not be shown: {text}");
        assert!(text.contains("claimed"), "the signed value is shown: {text}");
    }

    #[test]
    fn an_entry_for_a_different_node_than_the_signature_is_flagged() {
        let mut e = entry(false);
        e.node_id = "n-someone-else".into();
        let text = render(&[e], 1_100);
        assert!(text.contains("NOT VERIFIED: entry is for node n-someone-else"), "{text}");
    }

    #[test]
    fn the_receivers_provenance_cap_is_not_a_mismatch_but_a_raise_is() {
        let signed_facts = entry(false).facts;
        let mut capped = signed_facts.clone();
        capped.capabilities[0].provenance = Provenance::Claimed; // signed: probed
        assert_eq!(mismatch(&signed_facts, &capped, false), None);
        let mut raised = signed_facts.clone();
        raised.capabilities[1].provenance = Provenance::Probed; // signed: claimed
        raised.capabilities[0].provenance = Provenance::Measured; // signed: probed
        assert!(mismatch(&signed_facts, &raised, false).is_some());
    }

    #[test]
    fn live_state_may_differ_only_once_deltas_were_applied() {
        let signed_facts = entry(false).facts;
        let mut live = signed_facts.clone();
        live.capabilities[1].state = clawft_types::placement::CapabilityState::Busy;
        live.capabilities[0].attrs.insert("free".into(), AttrValue::Int(1));
        assert!(mismatch(&signed_facts, &live, false).is_some());
        assert_eq!(mismatch(&signed_facts, &live, true), None);
        // ... but not a different capability set, even with deltas.
        let mut other = signed_facts.clone();
        other.capabilities.pop();
        assert!(mismatch(&signed_facts, &other, true).is_some());
        let mut e = entry(false);
        e.delta_seq = 2;
        e.facts = live;
        let text = render(&[e], 1_100);
        assert!(text.contains("id-bound signature (ed25519); live state from update #2"), "{text}");
    }

    #[test]
    fn a_mesh_derived_paired_tier_is_not_called_paired() {
        let mut e = entry(false);
        e.local = false;
        e.trust_tier = TrustTier::Paired;
        e.tier_source = Some(TierSource::Mesh);
        let text = render(&[e.clone()], 1_100);
        assert!(text.contains("trust=mesh-verified"), "{text}");
        assert!(text.contains("not operator-paired"), "{text}");
        e.tier_source = Some(TierSource::Operator);
        assert!(render(&[e.clone()], 1_100).contains("trust=paired "));
        e.tier_source = Some(TierSource::Mesh);
        e.trust_tier = TrustTier::Discovered;
        assert!(render(&[e], 1_100).contains("trust=discovered "));
    }

    #[test]
    fn a_skipped_refresh_is_reported_with_the_cache_age() {
        use crate::node_facts_rpc::RefreshedFacts;
        let e = entry(false);
        // A bare list (no refresh asked): nothing skipped.
        let (got, skipped) = parse_result(serde_json::to_value(vec![e.clone()]).unwrap()).unwrap();
        assert_eq!((got.len(), skipped), (1, None));
        // A refresh that ran.
        let ran = RefreshedFacts { refreshed: true, cached_as_of: 1_050, entries: vec![e.clone()] };
        assert_eq!(parse_result(serde_json::to_value(ran).unwrap()).unwrap().1, None);
        // A refresh the daemon skipped.
        let skip = RefreshedFacts { refreshed: false, cached_as_of: 1_050, entries: vec![e] };
        let (got, skipped) = parse_result(serde_json::to_value(skip).unwrap()).unwrap();
        assert_eq!((got.len(), skipped), (1, Some(1_050)));
        let note = skipped_note(1_050, 1_062);
        assert!(note.contains("refresh skipped") && note.contains("12s ago"), "{note}");
        assert!(skipped_note(0, 5).contains("moments ago"));
    }
}
