//! Draws a header and three buttons, then prints every object event.
//!
//! Windowed (mouse = touch, space bar = push-to-talk, close the window
//! to exit):
//!
//! ```bash
//! cd crates/weftos-display-client
//! cargo run --example tap_demo --features window
//! ```
//!
//! Headless (default build, or `--headless` / `WEFTOS_SIM_HEADLESS=1`):
//! the same scene on the in-process simulator with a scripted tap on
//! each button, a drag, and a push-to-talk press.

use weftos_display_client::scene::{
    px, BuiltinFont, CursorHint, FontFace, HitShape, InputRegion, KerningHint, Layer, Node, NodeId,
    Primitive, Rgba, SceneOp, Style, Transform,
};
use weftos_display_client::{DisplayGrant, DisplaySession, DisplayTarget, LeafSim, ObjectEvent};

const HEADER: u32 = 1;
const TITLE: u32 = 2;
const STATUS: u32 = 3;
const BUTTONS: [(u32, &str); 3] = [(10, "Focus"), (11, "Approve"), (12, "Note")];
const BUTTON_Y: i32 = 120;
const BUTTON_W: i32 = 200;
const BUTTON_H: i32 = 64;

fn text(id: u32, x: i32, y: i32, s: &str, color: Rgba) -> Node {
    let mut n = Node::new(
        NodeId::from_raw(id),
        Layer::Text,
        Primitive::Text {
            content: s.to_string(),
            face: FontFace::Builtin(BuiltinFont::Mono10x20),
            size_q8: 20 << 8,
            weight: 400,
            kerning: KerningHint::Auto,
        },
    );
    n.transform = Transform::translate(px(x), px(y));
    n.style = Style::filled(color);
    n
}

fn rect(id: u32, x: i32, y: i32, w: i32, h: i32, color: Rgba, tappable: bool) -> Node {
    let mut n = Node::new(
        NodeId::from_raw(id),
        Layer::Widget,
        Primitive::Rect {
            w: px(w),
            h: px(h),
            radius_q8: 0,
        },
    );
    n.transform = Transform::translate(px(x), px(y));
    n.style = Style::filled(color);
    if tappable {
        n.input = Some(InputRegion {
            shape: HitShape::Aabb { w: px(w), h: px(h) },
            cursor_hint: CursorHint::Pointer,
            capture: true,
        });
    }
    n
}

fn button_x(i: usize) -> i32 {
    40 + i as i32 * (BUTTON_W + 40)
}

fn scene() -> Vec<SceneOp> {
    let mut ops = vec![
        SceneOp::Insert(rect(HEADER, 0, 0, 800, 48, Rgba::opaque(0x22, 0x55, 0x99), false)),
        SceneOp::Insert(text(TITLE, 16, 14, "WeftOS display - tap demo", Rgba::WHITE)),
        SceneOp::Insert(text(STATUS, 16, 420, "waiting for input", Rgba::opaque(0xAA, 0xAA, 0xAA))),
    ];
    for (i, (id, label)) in BUTTONS.iter().enumerate() {
        let x = button_x(i);
        ops.push(SceneOp::Insert(rect(*id, x, BUTTON_Y, BUTTON_W, BUTTON_H, Rgba::opaque(0x33, 0x66, 0x44), true)));
        ops.push(SceneOp::Insert(text(*id + 100, x + 16, BUTTON_Y + 22, label, Rgba::WHITE)));
    }
    ops
}

fn label_of(node: NodeId) -> &'static str {
    BUTTONS
        .iter()
        .find(|(id, _)| *id == node.raw())
        .map_or("?", |(_, l)| l)
}

fn script(sim: &LeafSim) {
    for (i, _) in BUTTONS.iter().enumerate() {
        sim.tap(button_x(i) + BUTTON_W / 2, BUTTON_Y + BUTTON_H / 2);
    }
    let (x, y) = (button_x(1) + 20, BUTTON_Y + 20);
    sim.touch_down(x, y);
    sim.touch_move(x + 30, y);
    sim.touch_move(x + 60, y + 10);
    sim.touch_up(x + 60, y + 10);
    sim.ptt(true);
    sim.ptt(false);
    sim.tap(400, 300); // empty space: no event
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let headless = std::env::var_os("WEFTOS_SIM_HEADLESS").is_some()
        || std::env::args().any(|a| a == "--headless")
        || !cfg!(feature = "window");
    let (target, sim) = if headless {
        let (t, s) = DisplayTarget::sim();
        (t, Some(s))
    } else {
        (DisplayTarget::window(), None)
    };

    let mut session = DisplaySession::open(target, DisplayGrant::dev("tap_demo"))?;
    let size = session.size();
    println!("display {}x{} caps {:?}", size.width, size.height, session.capabilities().render);
    session.apply(&scene())?;

    if let Some(sim) = &sim {
        script(sim);
    } else {
        println!("click a button, drag on one, hold space for ptt; close the window to exit");
    }

    loop {
        match session.poll_event() {
            Some(ev) => {
                let line = match &ev {
                    ObjectEvent::Tap { node } => format!("tap {} (node {})", label_of(*node), node.raw()),
                    ObjectEvent::Drag { node, dx, dy } => format!("drag {} by ({dx}, {dy})", label_of(*node)),
                    ObjectEvent::Sensor { name, value } => format!("sensor {name} = {value:?}"),
                };
                println!("{line}");
                if !session.is_closed() {
                    session.apply(&[SceneOp::Update(text(STATUS, 16, 420, &line, Rgba::opaque(0xAA, 0xAA, 0xAA)))])?;
                }
            }
            // Headless: the script is spent once the queue is empty.
            None if session.is_closed() || sim.is_some() => break,
            None => std::thread::sleep(std::time::Duration::from_millis(16)),
        }
    }
    session.close()?;
    println!("session closed; {} nodes left on the leaf", sim.map_or(0, |s| s.node_count()));
    Ok(())
}
