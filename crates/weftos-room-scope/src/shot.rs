//! Self-screenshot for headless visual checks (native only), copied from the
//! Cogs companion shell: with `COMPANION_SCREENSHOT=<path.bmp>` the app
//! captures its own window after `COMPANION_SCREENSHOT_AFTER` seconds
//! (default 6), writes a 24-bit BMP and exits.

use eframe::egui;
use std::sync::OnceLock;
use std::time::Instant;

struct Plan {
    path: String,
    after_s: f64,
    start: Instant,
}

static PLAN: OnceLock<Option<Plan>> = OnceLock::new();
static REQUESTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn plan() -> &'static Option<Plan> {
    PLAN.get_or_init(|| {
        let path = std::env::var("COMPANION_SCREENSHOT")
            .ok()
            .filter(|p| !p.is_empty())?;
        let after_s = std::env::var("COMPANION_SCREENSHOT_AFTER")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(6.0);
        Some(Plan {
            path,
            after_s,
            start: Instant::now(),
        })
    })
}

pub fn poll(ctx: &egui::Context) {
    let Some(p) = plan() else { return };
    use std::sync::atomic::Ordering;
    if !REQUESTED.load(Ordering::Relaxed) && p.start.elapsed().as_secs_f64() >= p.after_s {
        REQUESTED.store(true, Ordering::Relaxed);
        ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(Default::default()));
    }
    let shot = ctx.input(|i| {
        i.raw.events.iter().find_map(|e| match e {
            egui::Event::Screenshot { image, .. } => Some(image.clone()),
            _ => None,
        })
    });
    if let Some(img) = shot {
        let ok = std::fs::write(&p.path, bmp(&img)).is_ok();
        eprintln!(
            "screenshot {} -> {}",
            if ok { "written" } else { "FAILED" },
            p.path
        );
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }
}

fn bmp(img: &egui::ColorImage) -> Vec<u8> {
    let (w, h) = (img.size[0] as u32, img.size[1] as u32);
    let row = (w * 3).div_ceil(4) * 4;
    let size = 54 + row * h;
    let mut out = Vec::with_capacity(size as usize);
    out.extend_from_slice(b"BM");
    for v in [size, 0, 54, 40] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out.extend_from_slice(&(w as i32).to_le_bytes());
    out.extend_from_slice(&(h as i32).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&24u16.to_le_bytes());
    for v in [0u32, row * h, 2835, 2835, 0, 0] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    for y in (0..h).rev() {
        let start = out.len();
        for x in 0..w {
            let c = img.pixels[(y * w + x) as usize];
            out.extend_from_slice(&[c.b(), c.g(), c.r()]);
        }
        out.resize(start + row as usize, 0);
    }
    out
}
