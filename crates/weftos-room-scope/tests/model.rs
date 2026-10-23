//! Model, geometry and loading tests against the synthetic fixture in tests/fixture (no real site data).

use std::path::PathBuf;
use weftos_room_scope::Opts;
use weftos_room_scope::model::{self, Csi, FramesIndex, Meta, Track};

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixture")
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-6
}

#[test]
fn inverse_homography_round_trips() {
    let h = [
        [0.002, 0.0005, 0.1],
        [-0.0004, 0.003, 0.2],
        [0.0001, 0.0002, 1.0],
    ];
    let hi = model::inv3(&h);
    let (x, y, _) = model::apply(&h, 640.0, 360.0);
    let (u, v, w) = model::apply(&hi, x, y);
    assert!(w > 0.0 && close(u, 640.0) && close(v, 360.0), "{u} {v}");
}

#[test]
fn clip_keeps_inner_part_of_segment() {
    let quad = [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];
    let (a, b) = model::clip_to_quad([-1.0, 0.5], [2.0, 0.5], &quad).expect("crosses the quad");
    assert!(close(a[0], 0.0) && close(b[0], 1.0));
    // reversed winding gives the same result
    let rev = [[0.0, 0.0], [0.0, 1.0], [1.0, 1.0], [1.0, 0.0]];
    assert!(model::clip_to_quad([-1.0, 0.5], [2.0, 0.5], &rev).is_some());
    assert!(model::clip_to_quad([2.0, 2.0], [3.0, 3.0], &quad).is_none());
}

#[test]
fn segment_distance_and_colours() {
    assert!(close(
        model::seg_dist([1.0, 1.0], [0.0, 0.0], [2.0, 0.0]),
        1.0
    ));
    assert!(close(
        model::seg_dist([3.0, 0.0], [0.0, 0.0], [2.0, 0.0]),
        1.0
    ));
    assert_eq!(model::z_rgb(None), [120, 120, 120]);
    assert_eq!(model::z_rgb(Some(-1.0)), [90, 130, 170]);
    assert_eq!(model::z_rgb(Some(10.0))[0], 255);
}

#[test]
fn loads_fixture_run() {
    let d = fixture();
    assert_eq!(model::list_runs(&d), vec!["demo".to_string()]);
    let meta = Meta::from_json(&model::read_json(&d.join("data/demo_meta.json")).unwrap()).unwrap();
    assert_eq!(meta.markers.len(), 2);
    assert_eq!(meta.spots()[0].2, vec!["1".to_string(), "2".to_string()]);
    let h = meta.homographies["corrected"];
    let (x, y, _) = model::apply(&h, 320.0, 180.0);
    assert!((x - 1.0).abs() < 1e-3 && (y - 1.0).abs() < 1e-3);
    let backends = model::list_backends(&d, "demo");
    assert_eq!(backends.len(), 1);
    assert_eq!(backends[0].0, "cpu-blob");
    let tr: Track = serde_json::from_value(model::read_json(&backends[0].1).unwrap()).unwrap();
    assert_eq!(tr.backend_id(), "cpu-blob");
    assert!(tr.at(0.12, 0.05).is_some_and(|(i, f)| i == 1 && f.present));
    assert!(tr.at(5.0, 0.25).is_none());
}

#[test]
fn csi_gating_and_index() {
    let csi: Csi =
        serde_json::from_value(model::read_json(&fixture().join("data/demo_csi.json")).unwrap())
            .unwrap();
    assert_eq!(csi.node_ids(), vec!["1".to_string(), "2".to_string()]);
    assert_eq!(csi.index(1000.6), Some(1));
    assert_eq!(csi.index(999.0), None);
    assert_eq!(csi.live_z("2", 1), Some(4.0));
    assert_eq!(csi.live_z("2", 2), None, "gated window must not count");
}

#[test]
fn frames_index_and_jpeg_decode() {
    let dir = fixture().join("cache/frames/demo");
    let ix: FramesIndex =
        serde_json::from_value(model::read_json(&dir.join("index.json")).unwrap()).unwrap();
    assert_eq!(ix.index_at(0.04), 0);
    assert_eq!(ix.index_at(99.0), 0, "clamped to the last frame");
    assert_eq!(ix.file_name(12), "f00012.jpg");
    let img = weftos_room_scope::frames::decode_jpeg(&dir.join(ix.file_name(0))).unwrap();
    assert_eq!(img.size, [16, 9]);
}

#[test]
fn parses_cli() {
    let o = Opts::parse(["--data", "/x", "--run", "r", "--t", "3.5", "--heat"].map(String::from))
        .unwrap();
    assert_eq!(o.run.as_deref(), Some("r"));
    assert_eq!(o.t, Some(3.5));
    assert!(o.heat);
    assert!(Opts::parse(["--bogus".to_string()]).is_err());
    assert!(Opts::parse(["--t".to_string()]).is_err());
}
