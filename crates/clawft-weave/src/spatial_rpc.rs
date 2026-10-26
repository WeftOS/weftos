//! Daemon RPC handlers for `ecc.spatial.*` (WEFT-720 / ADR-056), over the kernel's
//! [`SpatialService`](clawft_kernel::spatial_service::SpatialService) BVH backend.
//!
//! Every mutation goes through the BVH store's chain sink (ExoChain when attached, and the
//! backend's in-process event log). Spatial is opt-in (`[kernel.spatial].enabled`); without it
//! every method answers "spatial is not enabled". Payloads are opaque bytes in the store; this
//! surface accepts and returns them as JSON (`payload`) so a leaf can carry provenance
//! (ADR-079: generative content is labelled, never metric truth).

use std::sync::Arc;

use clawft_bvh::{Aabb, BranchId, BranchMeta, BvhStore, IdentityKind, Leaf, LeafId, Ray, Vec3, tags};
use clawft_kernel::boot::Kernel;
use clawft_platform::NativePlatform;
use serde_json::{Value, json};

use crate::protocol::Response;

type KernelRef = Arc<tokio::sync::RwLock<Kernel<NativePlatform>>>;

const TAGS: &[(&str, u32)] = &[
    ("splat_scene", tags::SPLAT_SCENE),
    ("splat_camera", tags::SPLAT_CAMERA),
    ("splat_yardstick", tags::SPLAT_YARDSTICK),
    ("wm_object", tags::WM_OBJECT),
    ("wm_surface", tags::WM_SURFACE),
    ("wm_volume", tags::WM_VOLUME),
    ("wm_segment", tags::WM_SEGMENT),
    ("wm_sensor_fov", tags::WM_SENSOR_FOV),
    ("wm_affordance", tags::WM_AFFORDANCE),
];

/// Tag name (or decimal / `0x` hex number) to its registry value.
pub fn parse_tag(s: &str) -> Result<u32, String> {
    let s = s.trim();
    if let Some(&(_, v)) = TAGS.iter().find(|(n, _)| n.eq_ignore_ascii_case(s)) {
        return Ok(v);
    }
    let n = match s.strip_prefix("0x") {
        Some(h) => u32::from_str_radix(h, 16),
        None => s.parse::<u32>(),
    };
    n.map_err(|_| format!("unknown tag {s:?} (known: {})", TAGS.iter().map(|(n, _)| *n).collect::<Vec<_>>().join(", ")))
}

fn tag_name(v: u32) -> String {
    TAGS.iter().find(|(_, t)| *t == v).map_or_else(|| format!("0x{v:08x}"), |(n, _)| (*n).to_owned())
}

fn floats(s: &str, n: usize, what: &str) -> Result<Vec<f32>, String> {
    let v: Result<Vec<f32>, _> = s.split(',').map(|x| x.trim().parse::<f32>()).collect();
    match v {
        Ok(v) if v.len() == n && v.iter().all(|x| x.is_finite()) => Ok(v),
        _ => Err(format!("{what}: expected {n} comma-separated finite numbers, got {s:?}")),
    }
}

/// `x,y,z`.
pub fn parse_vec3(s: &str) -> Result<Vec3, String> {
    let v = floats(s, 3, "point")?;
    Ok(Vec3::new(v[0], v[1], v[2]))
}

/// `minx,miny,minz,maxx,maxy,maxz` (min <= max on every axis).
pub fn parse_aabb6(s: &str) -> Result<Aabb, String> {
    let v = floats(s, 6, "aabb")?;
    if v[0] > v[3] || v[1] > v[4] || v[2] > v[5] {
        return Err(format!("aabb: min exceeds max in {s:?}"));
    }
    Ok(Aabb::from_min_max(Vec3::new(v[0], v[1], v[2]), Vec3::new(v[3], v[4], v[5])))
}

fn str_param<'a>(p: &'a Value, k: &str) -> Option<&'a str> {
    p.get(k).and_then(Value::as_str)
}

fn branch(p: &Value, k: &str) -> BranchId {
    BranchId(p.get(k).and_then(Value::as_u64).unwrap_or(0))
}

fn aabb_json(b: &Aabb) -> Value {
    json!([b.min.x, b.min.y, b.min.z, b.max.x, b.max.y, b.max.z])
}

fn leaf_json(id: LeafId, leaf: &Leaf) -> Value {
    let payload = if leaf.payload.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice::<Value>(&leaf.payload).unwrap_or_else(|_| json!({ "bytes": leaf.payload.len() }))
    };
    json!({
        "leaf_id": id.0,
        "tag": tag_name(leaf.tag),
        "identity": match leaf.identity_kind { IdentityKind::Object => "object", IdentityKind::Event => "event" },
        "aabb": aabb_json(&leaf.bound),
        "payload": payload,
    })
}

/// Tag allow-list from `filter_tags` (comma-separated); `None` = no filter.
fn filter_tags(p: &Value) -> Result<Option<Vec<u32>>, String> {
    match str_param(p, "filter_tags").map(str::trim) {
        None | Some("") => Ok(None),
        Some(s) => s.split(',').filter(|x| !x.trim().is_empty()).map(parse_tag).collect::<Result<Vec<_>, _>>().map(Some),
    }
}

/// Dispatch one `ecc.spatial.*` method.
pub async fn handle(method: &str, params: &Value, kernel: &KernelRef) -> Response {
    let k = kernel.read().await;
    let Some(svc) = k.ecc_spatial() else {
        return Response::error("spatial is not enabled on this daemon ([kernel.spatial].enabled)");
    };
    let Some(bvh) = svc.bvh() else {
        return Response::error("spatial backend is not a BVH store");
    };
    match method {
        "ecc.spatial.status" => {
            let (events, truncated) = bvh.events();
            bvh.with_store(|s| {
                Response::success(json!({
                    "backend": "bvh-memory",
                    "epoch": s.epoch(),
                    "len": s.len(),
                    "branches": s.branch_count(),
                    "events": events.len(),
                    "events_truncated": truncated,
                }))
            })
        }
        "ecc.spatial.insert" => {
            let leaf = match build_leaf(params) {
                Ok(l) => l,
                Err(e) => return Response::error(e),
            };
            let b = branch(params, "branch");
            bvh.with_store(|s| match s.insert_on(b, leaf) {
                Ok(id) => Response::success(json!({ "leaf_id": id.0, "branch": b.0, "epoch": s.epoch() })),
                Err(e) => Response::error(e.to_string()),
            })
        }
        "ecc.spatial.get" => {
            let Some(id) = params.get("leaf_id").and_then(Value::as_u64) else {
                return Response::error("missing leaf_id");
            };
            let b = branch(params, "branch");
            bvh.with_store(|s| match s.get_on(b, LeafId(id)) {
                Some(leaf) => Response::success(json!({ "branch": b.0, "leaf": leaf_json(LeafId(id), leaf) })),
                None => Response::error(format!("no leaf {id} on branch {}", b.0)),
            })
        }
        "ecc.spatial.remove" => {
            let Some(id) = params.get("leaf_id").and_then(Value::as_u64) else {
                return Response::error("missing leaf_id");
            };
            let b = branch(params, "branch");
            bvh.with_store(|s| {
                let removed = s.remove_on(b, LeafId(id));
                Response::success(json!({ "removed": removed, "leaf_id": id, "branch": b.0 }))
            })
        }
        "ecc.spatial.query" => query(params, bvh),
        "ecc.spatial.region.publish" => {
            let (leaf, region_id) = match region_leaf(params) {
                Ok(v) => v,
                Err(e) => return Response::error(e),
            };
            let b = branch(params, "branch");
            bvh.with_store(|s| {
                match find_region(s, b, &region_id) {
                    Ok(Some(id)) => return Response::error(format!("region {region_id} already published as leaf {}", id.0)),
                    Ok(None) => {}
                    Err(e) => return Response::error(e),
                }
                match s.insert_on(b, leaf) {
                    Ok(id) => Response::success(json!({ "region_id": region_id, "leaf_id": id.0, "branch": b.0, "epoch": s.epoch() })),
                    Err(e) => Response::error(e.to_string()),
                }
            })
        }
        "ecc.spatial.region.get" => {
            let Some(region_id) = str_param(params, "region_id") else {
                return Response::error("missing region_id");
            };
            let b = branch(params, "branch");
            bvh.with_store(|s| match find_region(s, b, region_id) {
                Ok(Some(id)) => match s.get_on(b, id) {
                    Some(leaf) => Response::success(json!({ "branch": b.0, "leaf": leaf_json(id, leaf) })),
                    None => Response::error(format!("no region {region_id} on branch {}", b.0)),
                },
                Ok(None) => Response::error(format!("no region {region_id} on branch {}", b.0)),
                Err(e) => Response::error(e),
            })
        }
        "ecc.spatial.branch.derive" => {
            let parent = branch(params, "parent");
            let meta = BranchMeta {
                name: str_param(params, "name").unwrap_or("branch").to_owned(),
                priority_tier: params.get("priority_tier").and_then(Value::as_u64).unwrap_or(0).min(255) as u8,
            };
            bvh.with_store(|s| match s.derive_from(parent, meta) {
                Ok(child) => Response::success(json!({ "branch_id": child.0, "parent": parent.0, "epoch": s.epoch() })),
                Err(e) => Response::error(e.to_string()),
            })
        }
        "ecc.spatial.diff" => {
            let (a, b) = (branch(params, "a"), branch(params, "b"));
            let region = match str_param(params, "region") {
                Some(r) => match parse_aabb6(r) {
                    Ok(bb) => bb,
                    Err(e) => return Response::error(e),
                },
                None => Aabb::from_min_max(Vec3::new(f32::MIN, f32::MIN, f32::MIN), Vec3::new(f32::MAX, f32::MAX, f32::MAX)),
            };
            bvh.with_store(|s| match s.branch_diff(a, b, region) {
                Ok(entries) => {
                    // Relative to a -> b: only in a = removed, only in b = added.
                    let entries: Vec<Value> = entries
                        .into_iter()
                        .map(|e| json!({ "leaf_id": e.leaf_id.0, "kind": if e.only_in == a { "removed" } else { "added" }, "aabb": aabb_json(&e.bound) }))
                        .collect();
                    Response::success(json!({ "a": a.0, "b": b.0, "entries": entries }))
                }
                Err(e) => Response::error(e.to_string()),
            })
        }
        "ecc.spatial.events" => {
            let (events, truncated) = bvh.events();
            let limit = params.get("limit").and_then(Value::as_u64).unwrap_or(0) as usize;
            let tail = if limit == 0 || limit >= events.len() { &events[..] } else { &events[events.len() - limit..] };
            Response::success(json!({
                "count": events.len(),
                "truncated": truncated,
                "events": tail.iter().map(event_json).collect::<Vec<_>>(),
            }))
        }
        "ecc.spatial.replay" => {
            let r = bvh.verify_replay();
            Response::success(json!({ "ok": r.ok, "events_replayed": r.events, "truncated": r.truncated, "branches": r.branches }))
        }
        other => Response::error(format!("unknown spatial method {other}")),
    }
}

fn event_json(e: &clawft_bvh::BvhChainKind) -> Value {
    use clawft_bvh::BvhChainKind as K;
    match e {
        K::Insert { leaf_id, leaf, branch } => json!({ "kind": "insert", "leaf_id": leaf_id.0, "branch": branch.0, "tag": tag_name(leaf.tag) }),
        K::Remove { leaf_id, branch } => json!({ "kind": "remove", "leaf_id": leaf_id.0, "branch": branch.0 }),
        K::Derive { parent, child, meta } => json!({ "kind": "derive", "parent": parent.0, "child": child.0, "name": meta.name }),
        K::RebalanceSeal { branch, leaf_count, epoch } => json!({ "kind": "seal", "branch": branch.0, "leaf_count": leaf_count, "epoch": epoch }),
    }
}

fn build_leaf(p: &Value) -> Result<Leaf, String> {
    let tag = parse_tag(str_param(p, "tag").unwrap_or("wm_object"))?;
    let bound = parse_aabb6(str_param(p, "aabb").ok_or("missing aabb")?)?;
    let identity = match str_param(p, "identity").unwrap_or("object") {
        "object" => IdentityKind::Object,
        "event" => IdentityKind::Event,
        other => return Err(format!("identity must be object or event, got {other:?}")),
    };
    let payload = match p.get("payload") {
        None | Some(Value::Null) => Vec::new(),
        Some(v) => serde_json::to_vec(v).map_err(|e| format!("payload: {e}"))?,
    };
    if payload.len() > 64 * 1024 {
        return Err("payload over 64 KiB".into());
    }
    Ok(Leaf::new(bound, identity, tag, payload))
}

/// Region id shape: `region/urth/...`, the ids the evidence contract allows (ADR-111 §1.1).
fn valid_region_id(id: &str) -> bool {
    id.starts_with("region/urth/")
        && id.len() <= 128
        && id.bytes().all(|c| c.is_ascii_alphanumeric() || b"._:/-@".contains(&c))
}

fn finite_vec3(v: &Value, what: &str) -> Result<[f64; 3], String> {
    let a = v.as_array().filter(|a| a.len() == 3).ok_or_else(|| format!("{what}: expected [x,y,z]"))?;
    let mut out = [0.0; 3];
    for (o, x) in out.iter_mut().zip(a) {
        let n = x.as_f64().filter(|n| n.is_finite() && n.abs() <= 1000.0).ok_or_else(|| format!("{what}: need finite numbers within 1000 m"))?;
        *o = n;
    }
    Ok(out)
}

/// Build the region leaf from a `spatial.evidence.v1` `shell_measure` record (ADR-111).
///
/// Only the measured interior box is claimed. The payload says in so many words that everything
/// else (contents of the room, anything outside it) is unobserved, that the region has no site
/// anchor yet, and that appearance is never metric truth (ADR-079 §2).
fn region_leaf(p: &Value) -> Result<(Leaf, String), String> {
    let ev = p.get("evidence").ok_or("missing evidence (a shell_measure record)")?;
    let text = |k: &str| ev.get(k).and_then(Value::as_str).ok_or_else(|| format!("evidence.{k} missing"));
    if text("schema")? != "spatial.evidence.v1" || text("type")? != "shell_measure" {
        return Err("evidence must be a spatial.evidence.v1 shell_measure".into());
    }
    if text("frame")? != "room_enu" {
        return Err("evidence.frame must be room_enu; refusing to guess another frame".into());
    }
    let region_id = text("region")?.to_owned();
    if !valid_region_id(&region_id) {
        return Err(format!("evidence.region {region_id:?} is not a region/urth/... id"));
    }
    let unc = ev.get("uncertainty_m").and_then(Value::as_f64).filter(|u| *u > 0.0 && *u <= 100.0);
    let unc = unc.ok_or("evidence.uncertainty_m must be in (0, 100]")?;
    let prov = ev.get("provenance").ok_or("evidence.provenance missing")?;
    let proof = prov.get("proof").and_then(Value::as_str).unwrap_or("");
    if !matches!(proof, "MEASURED" | "CODE" | "SYNTHETIC") {
        return Err(format!("provenance.proof must be MEASURED, CODE or SYNTHETIC, got {proof:?}"));
    }
    let (lo, hi) = (finite_vec3(ev.get("min").unwrap_or(&Value::Null), "min")?, finite_vec3(ev.get("max").unwrap_or(&Value::Null), "max")?);
    if (0..3).any(|i| lo[i] >= hi[i]) {
        return Err("shell min must be below max on every axis".into());
    }
    let bound = Aabb::from_min_max(Vec3::new(lo[0] as f32, lo[1] as f32, lo[2] as f32), Vec3::new(hi[0] as f32, hi[1] as f32, hi[2] as f32));
    let payload = json!({
        "kind": "urth.region.v1",
        "region_id": region_id,
        "frame": "room_enu",
        "frame_def": "right-handed ENU, origin at the south-west floor corner, x east, y north, z up, metres",
        "extent": { "min": lo, "max": hi, "what": "measured interior box of the room shell" },
        "observed": ["room_shell_interior_box"],
        "unobserved": "everything outside the box, and every object, surface and occupancy inside it",
        "anchor": { "status": "unanchored", "t_ecef_local": null },
        "metric_truth": proof == "MEASURED",
        "appearance_is_metric": false,
        "provenance": {
            "schema": "spatial.evidence.v1",
            "type": "shell_measure",
            "proof": proof,
            "uncertainty_m": unc,
            "receipt": prov.get("receipt"),
            "producer": prov.get("producer"),
            "source_id": ev.get("source_id"),
            "t_ns": ev.get("t_ns"),
        },
    });
    let bytes = serde_json::to_vec(&payload).map_err(|e| format!("payload: {e}"))?;
    Ok((Leaf::new(bound, IdentityKind::Object, tags::WM_VOLUME, bytes), region_id))
}

/// Leaf id of the region record with this id on `b`, if any.
fn find_region(s: &BvhStore, b: BranchId, region_id: &str) -> Result<Option<LeafId>, String> {
    let leaves = s.snapshot_leaves(b).map_err(|e| e.to_string())?;
    Ok(leaves.into_iter().find_map(|(id, l)| {
        let v: Value = serde_json::from_slice(&l.payload).ok()?;
        (v.get("kind")?.as_str()? == "urth.region.v1" && v.get("region_id")?.as_str()? == region_id).then_some(id)
    }))
}

fn query(p: &Value, bvh: &clawft_kernel::spatial_bvh::BvhBackend) -> Response {
    let kind = str_param(p, "kind").unwrap_or("aabb");
    let b = branch(p, "branch");
    let filter = match filter_tags(p) {
        Ok(f) => f,
        Err(e) => return Response::error(e),
    };
    let limit = p.get("limit").and_then(Value::as_u64).unwrap_or(0) as usize;
    let with_payload = p.get("with_payload").and_then(Value::as_bool).unwrap_or(false);
    let want = |k: &str| str_param(p, k).ok_or_else(|| format!("missing {k}"));
    let point = |k: &str| want(k).and_then(parse_vec3);
    bvh.with_store(|s: &mut BvhStore| {
        let ids: Result<Vec<(LeafId, Option<f32>)>, String> = match kind {
            "point" => point("at").map(|at| s.query_point_on(b, at).into_iter().map(|i| (i, None)).collect()),
            "aabb" => want("region").and_then(parse_aabb6).map(|r| s.query_aabb_on(b, r).into_iter().map(|i| (i, None)).collect()),
            "sphere" => point("center").map(|c| {
                let r = p.get("radius").and_then(Value::as_f64).unwrap_or(1.0) as f32;
                s.query_sphere_on(b, c, r).into_iter().map(|i| (i, None)).collect()
            }),
            "knn" => point("at").map(|at| {
                let k = p.get("k").and_then(Value::as_u64).unwrap_or(5) as usize;
                s.query_knn_on(b, at, k).into_iter().map(|i| (i, None)).collect()
            }),
            "ray" => point("origin").and_then(|o| point("dir").map(|d| (o, d))).map(|(o, d)| {
                let max_t = p.get("max_t").and_then(Value::as_f64).unwrap_or(1.0e6) as f32;
                s.query_ray_on(b, Ray { origin: o, dir: d }, max_t).into_iter().map(|h| (h.id, Some(h.t))).collect()
            }),
            other => Err(format!("unknown query kind {other:?}")),
        };
        let mut ids = match ids {
            Ok(v) => v,
            Err(e) => return Response::error(e),
        };
        if let Some(ref allowed) = filter {
            ids.retain(|(id, _)| s.get_on(b, *id).is_some_and(|l| allowed.contains(&l.tag)));
        }
        if limit > 0 {
            ids.truncate(limit);
        }
        let hits: Vec<Value> = ids
            .into_iter()
            .map(|(id, t)| {
                let mut h = json!({ "leaf_id": id.0 });
                if let Some(t) = t {
                    h["t"] = json!(t);
                }
                if let Some(l) = s.get_on(b, id) {
                    h["tag"] = json!(tag_name(l.tag));
                    if with_payload {
                        h["leaf"] = leaf_json(id, l);
                    }
                }
                h
            })
            .collect();
        Response::success(json!({ "kind": kind, "branch": b.0, "hits": hits }))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parsers_accept_valid_input_and_refuse_the_rest() {
        assert_eq!(parse_tag("wm_object"), Ok(tags::WM_OBJECT));
        assert_eq!(parse_tag("WM_SURFACE"), Ok(tags::WM_SURFACE));
        assert_eq!(parse_tag("0x53500010"), Ok(tags::WM_OBJECT));
        assert!(parse_tag("nope").is_err());
        assert!(parse_vec3("1,2,3").is_ok());
        assert!(parse_vec3("1,2").is_err() && parse_vec3("1,2,NaN").is_err());
        assert!(parse_aabb6("0,0,0,1,1,1").is_ok());
        assert!(parse_aabb6("2,0,0,1,1,1").is_err(), "min > max");
        assert_eq!(tag_name(tags::WM_VOLUME), "wm_volume");
        assert_eq!(tag_name(7), "0x00000007");
    }
}
