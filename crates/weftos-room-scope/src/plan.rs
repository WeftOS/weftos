//! Top-down room plan: walls/openings/objects, camera footprint, router->spot links coloured per node,
//! naive link-weighted heat, markers and the tracked person's footprint ellipse with front/back.

use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, Shape, Stroke, pos2, vec2};

use crate::model::{self, Csi, Meta, TrackFrame};
use crate::view::{BACK, FRONT, PERSON, PINK, Proj, YELLOW, z_color};

pub fn show(
    ui: &mut egui::Ui,
    meta: &Meta,
    pr: &Proj,
    csi: Option<&Csi>,
    k: Option<usize>,
    trail: &[[f64; 2]],
    cur: Option<(&TrackFrame, [f64; 2])>,
) {
    let router = meta.router().unwrap_or([0.0, 0.0]);
    // bounds: room, router and spots, with a margin
    let mut lo = [-0.5f64, -0.5];
    let mut hi = [meta.room_max[0] + 0.5, meta.room_max[1] + 0.5];
    for p in std::iter::once(router).chain(meta.spots().iter().map(|s| s.1)) {
        lo = [lo[0].min(p[0] - 0.4), lo[1].min(p[1] - 0.4)];
        hi = [hi[0].max(p[0] + 0.4), hi[1].max(p[1] + 0.4)];
    }
    let w = ui.available_width();
    let sc = (w as f64 / (hi[0] - lo[0])).min(320.0 / (hi[1] - lo[1]));
    let size = vec2(((hi[0] - lo[0]) * sc) as f32, ((hi[1] - lo[1]) * sc) as f32);
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    let p = ui.painter_at(rect);
    p.rect_filled(rect, 2.0, Color32::from_gray(16));
    let to = |x: f64, y: f64| -> Pos2 {
        pos2(
            rect.min.x + ((x - lo[0]) * sc) as f32,
            rect.min.y + ((hi[1] - y) * sc) as f32,
        )
    };
    let r = |mn: [f64; 2], mx: [f64; 2]| Rect::from_two_pos(to(mn[0], mn[1]), to(mx[0], mx[1]));
    for (_, mn, mx) in meta.rects("objects") {
        p.rect_stroke(
            r(mn, mx),
            0.0,
            Stroke::new(1.0, Color32::from_white_alpha(45)),
            egui::StrokeKind::Inside,
        );
    }
    for (kind, mn, mx) in meta.rects("walls") {
        let c = match kind.as_str() {
            "exterior" => Color32::from_gray(85),
            "block" => Color32::from_rgb(122, 106, 90),
            _ => Color32::from_gray(136),
        };
        p.rect_filled(r(mn, mx), 0.0, c);
    }
    for (kind, mn, mx) in meta.rects("openings") {
        let c = if kind == "window" {
            Color32::from_rgba_unmultiplied(120, 180, 255, 150)
        } else {
            Color32::from_rgba_unmultiplied(255, 160, 64, 200)
        };
        p.rect_filled(r(mn, mx), 0.0, c);
    }
    // heat inside the room (0.1 m cells)
    if let (Some(csi), Some(k)) = (csi, k) {
        let spots = meta.spots();
        let wts: Vec<f64> = spots
            .iter()
            .map(|(_, _, ids)| {
                ids.iter()
                    .filter_map(|n| csi.live_z(n, k))
                    .map(|z| z.max(0.0))
                    .sum()
            })
            .collect();
        let step = 0.1;
        let (nx, ny) = (
            (meta.room_max[0] / step) as usize,
            (meta.room_max[1] / step) as usize,
        );
        for j in 0..ny {
            for i in 0..nx {
                let (x, y) = ((i as f64 + 0.5) * step, (j as f64 + 0.5) * step);
                let h: f64 = spots
                    .iter()
                    .zip(&wts)
                    .map(|((_, pos, _), w)| {
                        let d = model::seg_dist([x, y], router, *pos);
                        w * (-d * d / (2.0 * 0.4 * 0.4)).exp()
                    })
                    .sum();
                let u = (h / 8.0).min(1.0);
                if u > 0.02 {
                    p.rect_filled(
                        r(
                            [i as f64 * step, j as f64 * step],
                            [(i + 1) as f64 * step, (j + 1) as f64 * step],
                        ),
                        0.0,
                        Color32::from_rgba_unmultiplied(
                            255,
                            (200.0 * (1.0 - u)) as u8,
                            40,
                            (165.0 * u) as u8,
                        ),
                    );
                }
            }
        }
    }
    p.rect_stroke(
        r([0.0, 0.0], meta.room_max),
        0.0,
        Stroke::new(1.5, Color32::from_gray(220)),
        egui::StrokeKind::Middle,
    );
    let fp: Vec<Pos2> = pr.foot.iter().map(|q| to(q[0], q[1])).collect();
    let mut closed = fp.clone();
    closed.push(fp[0]);
    p.extend(Shape::dashed_line(
        &closed,
        Stroke::new(1.0, crate::view::CYAN),
        4.0,
        4.0,
    ));
    for (spot, pos, ids) in meta.spots() {
        let (a, b) = (to(router[0], router[1]), to(pos[0], pos[1]));
        let n = (b - a).normalized().rot90();
        for (li, nid) in ids.iter().enumerate() {
            let off = (li as f32 - (ids.len() as f32 - 1.0) / 2.0) * 4.0;
            let z = csi.zip(k).and_then(|(c, k)| c.live_z(nid, k));
            let st = Stroke::new(
                2.5,
                if z.is_some() {
                    z_color(z)
                } else {
                    Color32::from_gray(130)
                },
            );
            if z.is_some() {
                p.line_segment([a + n * off, b + n * off], st);
            } else {
                p.extend(Shape::dashed_line(
                    &[a + n * off, b + n * off],
                    st,
                    5.0,
                    4.0,
                ));
            }
        }
        p.circle_filled(b, 5.0, PINK);
        let names: Vec<String> = ids.iter().map(|i| format!("n{i}")).collect();
        p.text(
            b + vec2(6.0, -6.0),
            Align2::LEFT_BOTTOM,
            format!("{spot}: {}", names.join(" ")),
            FontId::proportional(11.0),
            PINK,
        );
    }
    p.circle_filled(to(router[0], router[1]), 5.0, Color32::WHITE);
    p.text(
        to(router[0], router[1]) + vec2(-6.0, 0.0),
        Align2::RIGHT_CENTER,
        "router",
        FontId::proportional(11.0),
        Color32::WHITE,
    );
    for (id, xy) in &meta.markers {
        p.circle_stroke(to(xy[0], xy[1]), 4.0, Stroke::new(1.5, YELLOW));
        p.text(
            to(xy[0], xy[1]) + vec2(5.0, 0.0),
            Align2::LEFT_CENTER,
            format!("M{id}"),
            FontId::proportional(10.0),
            YELLOW,
        );
    }
    let tp: Vec<Pos2> = trail.iter().map(|q| to(q[0], q[1])).collect();
    if tp.len() > 1 {
        p.add(Shape::line(
            tp,
            Stroke::new(1.5, PERSON.gamma_multiply(0.5)),
        ));
    }
    if let Some((f, xy)) = cur {
        let c = to(xy[0], xy[1]);
        match f.heading_rad {
            Some(h) => {
                // fixed 0.5 x 0.3 m body footprint, shoulders across the heading; front/back dots
                let (fx, fy) = (h.cos(), h.sin());
                let pts: Vec<Pos2> = (0..40)
                    .map(|i| {
                        let a = f64::from(i) / 40.0 * std::f64::consts::TAU;
                        let (u, v) = (0.15 * a.cos(), 0.25 * a.sin());
                        to(xy[0] + u * fx - v * fy, xy[1] + u * fy + v * fx)
                    })
                    .collect();
                p.add(Shape::closed_line(pts, Stroke::new(2.0, PERSON)));
                p.circle_filled(to(xy[0] + 0.15 * fx, xy[1] + 0.15 * fy), 3.5, FRONT);
                p.circle_filled(to(xy[0] - 0.15 * fx, xy[1] - 0.15 * fy), 3.0, BACK);
            }
            None => {
                p.circle_stroke(c, (0.2 * sc) as f32, Stroke::new(2.0, PERSON));
            }
        }
        p.circle_filled(c, 2.5, PERSON);
    }
    p.text(
        rect.left_bottom() + vec2(4.0, -4.0),
        Align2::LEFT_BOTTOM,
        "naive link-weighted heat, not a localisation",
        FontId::proportional(11.0),
        crate::view::WARN,
    );
    p.text(
        rect.right_top() + vec2(-6.0, 4.0),
        Align2::RIGHT_TOP,
        "N up, x = E",
        FontId::proportional(11.0),
        Color32::LIGHT_GRAY,
    );
}
