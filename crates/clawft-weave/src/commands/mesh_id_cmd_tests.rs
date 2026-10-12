//! `weaver mesh create | id` over explicit inputs. Temp dirs only.

use super::*;

fn key(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}

fn out(f: impl FnOnce(&mut Vec<u8>) -> Result<()>) -> (String, Result<()>) {
    let mut b = Vec::new();
    let r = f(&mut b);
    (String::from_utf8(b).unwrap(), r)
}

#[test]
fn create_prints_the_id_and_a_second_create_refuses() {
    let d = tempfile::tempdir().unwrap();
    let (text, r) = out(|w| create_with(w, d.path(), &key(1), &key(2), None, "2026-10-12T00:00:00Z"));
    r.unwrap();
    let id = text.lines().next().unwrap().strip_prefix("mesh id: ").unwrap().to_owned();
    assert_eq!(id.len(), 64);
    let (_, r) = out(|w| create_with(w, d.path(), &key(1), &key(2), None, "2026-10-12T00:00:00Z"));
    assert!(r.unwrap_err().to_string().contains(&id));
}

#[test]
fn id_names_the_source_for_genesis_and_licence_and_refuses_when_there_is_none() {
    let d = tempfile::tempdir().unwrap();
    let (text, r) = out(|w| show(w, d.path(), None, false));
    assert!(text.is_empty());
    let e = r.unwrap_err().to_string();
    assert!(e.contains("no MeshId") && e.contains("weaver mesh create"), "{e}");
    let (j, r) = out(|w| show(w, d.path(), None, true));
    assert!(r.is_err() && j.contains("\"mesh_id\":null"), "{j}");

    out(|w| create_with(w, d.path(), &key(1), &key(2), None, "2026-10-12T00:00:00Z")).1.unwrap();
    let (text, r) = out(|w| show(w, d.path(), None, false));
    r.unwrap();
    let mut lines = text.lines();
    assert_eq!(lines.next().unwrap().len(), 64);
    assert_eq!(lines.next().unwrap(), "source: genesis");

    let licence = MeshId::derive(&[7; 32], &[8; 32]);
    let (text, r) = out(|w| show(w, d.path(), Some(licence), true));
    r.unwrap();
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["mesh_id"], licence.to_hex());
    assert_eq!(v["source"], "licence");
    assert!(v["conflicting_record"].is_string());
}
