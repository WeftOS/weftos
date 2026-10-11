//! Raw leaf input becomes Tap / Drag / Sensor, in order.

mod common;

use common::*;
use weftos_display_client::scene::NodeId;
use weftos_display_client::{ObjectEvent, SensorValue, SENSOR_PTT_DOWN, SENSOR_PTT_UP};

#[test]
fn drag_reports_incremental_pixel_deltas_and_no_tap() {
    let (mut s, sim) = open_sim();
    s.apply(&[insert(button(1, 100, 100, 200, 200))]).unwrap();
    sim.touch_down(150, 150);
    sim.touch_move(152, 150); // under the drag threshold: nothing yet
    sim.touch_move(170, 150);
    sim.touch_move(170, 180);
    sim.touch_up(170, 180);
    let node = NodeId::from_raw(1);
    assert_eq!(
        drain(&mut s),
        vec![ObjectEvent::Drag { node, dx: 20, dy: 0 }, ObjectEvent::Drag { node, dx: 0, dy: 30 }]
    );
}

#[test]
fn drag_keeps_the_node_it_started_on() {
    let (mut s, sim) = open_sim();
    s.apply(&[insert(button(1, 100, 100, 50, 50)), insert(button(2, 300, 100, 50, 50))]).unwrap();
    sim.touch_down(120, 120);
    sim.touch_move(320, 120); // now over node 2
    sim.touch_up(320, 120);
    let evs = drain(&mut s);
    assert_eq!(evs.len(), 1);
    assert_eq!(evs[0].node(), Some(NodeId::from_raw(1)));
    assert!(matches!(evs[0], ObjectEvent::Drag { .. }));
}

#[test]
fn ptt_arrives_as_sensor_events() {
    let (mut s, sim) = open_sim();
    sim.ptt(true);
    sim.ptt(false);
    assert_eq!(
        drain(&mut s),
        vec![
            ObjectEvent::Sensor { name: SENSOR_PTT_DOWN.into(), value: SensorValue::Bool(true) },
            ObjectEvent::Sensor { name: SENSOR_PTT_UP.into(), value: SensorValue::Bool(false) },
        ]
    );
    assert_eq!(ObjectEvent::ptt(true), ObjectEvent::Sensor { name: "ptt.down".into(), value: SensorValue::Bool(true) });
}

#[test]
fn events_keep_leaf_order_across_kinds() {
    let (mut s, sim) = open_sim();
    s.apply(&[insert(button(1, 0, 0, 100, 100))]).unwrap();
    sim.ptt(true);
    sim.tap(10, 10);
    sim.sensor("light.lux", SensorValue::Float(120.5));
    sim.ptt(false);
    let kinds: Vec<&str> = drain(&mut s)
        .iter()
        .map(|e| match e {
            ObjectEvent::Tap { .. } => "tap",
            ObjectEvent::Drag { .. } => "drag",
            ObjectEvent::Sensor { name, .. } if name == "light.lux" => "lux",
            ObjectEvent::Sensor { .. } => "ptt",
        })
        .collect();
    assert_eq!(kinds, vec!["ptt", "tap", "lux", "ptt"]);
}

#[test]
fn poll_is_empty_when_nothing_happened() {
    let (mut s, _sim) = open_sim();
    assert_eq!(s.poll_event(), None);
    assert!(!s.is_closed());
}
