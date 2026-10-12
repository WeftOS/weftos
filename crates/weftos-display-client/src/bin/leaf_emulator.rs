//! `leaf-emulator`: an emulated edge pad on the real certified-leaf channel.
//!
//! ```bash
//! leaf-emulator --seed leaf.seed --cert leaf.cbor --parent discover:255.255.255.255:9490
//! leaf-emulator --seed leaf.seed --cert leaf.cbor --parent 127.0.0.1:9491 --state journal.cbor
//! ```
//!
//! `--headless` ignores stdin and runs until killed (for scripts).
//!
//! Commands on stdin: `tap X Y`, `down X Y`, `move X Y`, `up X Y`,
//! `ptt down|up`, `status`, `texts`, `quit`.

use std::io::BufRead;
use std::net::SocketAddr;
use std::path::PathBuf;

use weftos_display_client::scene::Primitive;
use weftos_display_client::wire_leaf::{Parent, WireLeaf, WireLeafConfig};
use weftos_display_client::DisplaySize;

fn usage() -> ! {
    eprintln!("usage: leaf-emulator --seed FILE --cert FILE --parent (ADDR|discover:ADDR) [--state FILE] [--headless]");
    std::process::exit(2);
}

fn main() {
    let (mut seed, mut cert, mut parent, mut state) = (None, None, None, None);
    let mut headless = false;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        if a == "--headless" {
            headless = true;
            continue;
        }
        let v = it.next().unwrap_or_else(|| usage());
        match a.as_str() {
            "--seed" => seed = Some(PathBuf::from(v)),
            "--cert" => cert = Some(PathBuf::from(v)),
            "--parent" => parent = Some(v),
            "--state" => state = Some(PathBuf::from(v)),
            _ => usage(),
        }
    }
    let (Some(seed), Some(cert), Some(parent)) = (seed, cert, parent) else { usage() };
    let seed: [u8; 32] = std::fs::read(&seed)
        .ok()
        .and_then(|b| b.try_into().ok())
        .unwrap_or_else(|| fail("the seed file must be exactly 32 raw bytes"));
    let cert_cbor = std::fs::read(&cert).unwrap_or_else(|e| fail(&format!("cannot read {}: {e}", cert.display())));
    let parent = match parent.strip_prefix("discover:") {
        Some(a) => Parent::Discover(a.parse::<SocketAddr>().unwrap_or_else(|_| usage())),
        None => Parent::Direct(parent.parse::<SocketAddr>().unwrap_or_else(|_| usage())),
    };
    let leaf = WireLeaf::start(WireLeafConfig { seed, cert_cbor, parent, state_file: state, size: DisplaySize::PAD })
        .unwrap_or_else(|e| fail(&e));
    eprintln!("[leaf-emulator] leaf id {}", leaf.leaf_id());
    if headless {
        loop {
            std::thread::park();
        }
    }
    for line in std::io::stdin().lock().lines().map_while(Result::ok) {
        let w: Vec<&str> = line.split_whitespace().collect();
        let xy = || -> Option<(i32, i32)> { Some((w.get(1)?.parse().ok()?, w.get(2)?.parse().ok()?)) };
        match (w.first().copied(), xy()) {
            (Some("tap"), Some((x, y))) => leaf.tap(x, y),
            (Some("down"), Some((x, y))) => leaf.touch_down(x, y),
            (Some("move"), Some((x, y))) => leaf.touch_move(x, y),
            (Some("up"), Some((x, y))) => leaf.touch_up(x, y),
            (Some("ptt"), _) => leaf.ptt(w.get(1) == Some(&"down")),
            (Some("status"), _) => println!(
                "subscribed={} session={:?} nodes={} pending={}",
                leaf.is_subscribed(),
                leaf.session_subject(),
                leaf.sim().node_count(),
                leaf.pending_frames()
            ),
            (Some("texts"), _) => {
                for n in leaf.sim().snapshot().nodes {
                    if let Primitive::Text { content, .. } = n.primitive {
                        println!("{content}");
                    }
                }
            }
            (Some("quit"), _) => break,
            _ => eprintln!("unknown command: {line}"),
        }
    }
}

fn fail(msg: &str) -> ! {
    eprintln!("leaf-emulator: {msg}");
    std::process::exit(1);
}
