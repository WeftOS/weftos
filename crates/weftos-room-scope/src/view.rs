//! Painting: the camera frame with its overlays (grid, markers, spots, links, person, CSI heat) and the
//! top-down room plan. All room geometry comes from the loaded run; image coordinates are source px.

use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, Shape, Stroke, pos2, vec2};

use crate::model::{self, Csi, IMG_H, IMG_W, Mat3, Meta, TrackFrame};

pub const CYAN: Color32 = Color32::from_rgb(120, 220, 255);
pub const YELLOW: Color32 = Color32::from_rgb(255, 212, 0);
pub const PINK: Color32 = Color32::from_rgb(255, 102, 204);
pub const PERSON: Color32 = Color32::from_rgb(0, 255, 140);
pub const FRONT: Color32 = Color32::from_rgb(255, 51, 85);
pub const BACK: Color32 = Color32::from_rgb(70, 110, 255);
pub const WARN: Color32 = Color32::from_rgb(255, 204, 102);

pub fn z_color(z: Option<f64>) -> Color32 {
    let [r, g, b] = model::z_rgb(z);
    Color32::from_rgb(r, g, b)
}

/// Camera-space projection for one run and homography.
pub struct Proj {
    pub h: Mat3,
    pub hi: Mat3,
    pub foot: [[f64; 2]; 4],
}

impl Proj {
    pub fn new(h: Mat3) -> Self {
        let hi = model::inv3(&h);
        let c = |x, y| {
            let (a, b, _) = model::apply(&h, x, y);
            [a, b]
        };
        Self {
            h,
            hi,
            foot: [c(0.0, 0.0), c(IMG_W, 0.0), c(IMG_W, IMG_H), c(0.0, IMG_H)],
        }
    }
    pub fn to_room(&self, px: [f64; 2]) -> [f64; 2] {
        let (x, y, _) = model::apply(&self.h, px[0], px[1]);
        [x, y]
    }
    /// Room point -> image px, None if behind the horizon.
    pub fn to_img(&self, p: [f64; 2]) -> Option<[f64; 2]> {
        let (x, y, w) = model::apply(&self.hi, p[0], p[1]);
        (w > 0.0).then_some([x, y])
    }
    /// Room segment clipped to the camera footprint, in image px.
    pub fn seg_img(&self, a: [f64; 2], b: [f64; 2]) -> Option<([f64; 2], [f64; 2])> {
        let (a, b) = model::clip_to_quad(a, b, &self.foot)?;
        Some((self.to_img(a)?, self.to_img(b)?))
    }
}

/// Image px -> screen inside `rect`.
pub struct Screen {
    pub rect: Rect,
}

impl Screen {
    pub fn p(&self, px: [f64; 2]) -> Pos2 {
        pos2(
            self.rect.min.x + (px[0] / IMG_W) as f32 * self.rect.width(),
            self.rect.min.y + (px[1] / IMG_H) as f32 * self.rect.height(),
        )
    }
    pub fn s(&self) -> f32 {
        self.rect.width() / IMG_W as f32
    }
}

fn seg(
    p: &egui::Painter,
    pr: &Proj,
    sc: &Screen,
    a: [f64; 2],
    b: [f64; 2],
    stroke: Stroke,
    off: f32,
) {
    if let Some((x, y)) = pr.seg_img(a, b) {
        let (p0, p1) = (sc.p(x), sc.p(y));
        let n = (p1 - p0).normalized().rot90() * off;
        p.line_segment([p0 + n, p1 + n], stroke);
    }
}

fn arrow(p: &egui::Painter, a: Pos2, b: Pos2, color: Color32, label: &str) {
    p.line_segment([a, b], Stroke::new(3.0, color));
    let d = (b - a).normalized() * 12.0;
    let n = d.rot90() * 0.45;
    p.add(Shape::convex_polygon(
        vec![b, b - d + n, b - d - n],
        color,
        Stroke::NONE,
    ));
    p.text(
        b + vec2(6.0, 4.0),
        Align2::LEFT_CENTER,
        label,
        FontId::proportional(13.0),
        color,
    );
}

fn label_bg(p: &egui::Painter, pos: Pos2, align: Align2, text: &str, color: Color32) {
    let g = p.layout_no_wrap(text.to_string(), FontId::proportional(12.0), color);
    let r = align.anchor_size(pos, g.size()).expand(2.0);
    p.rect_filled(r, 2.0, Color32::from_black_alpha(150));
    p.galley(r.min + vec2(2.0, 2.0), g, color);
}

/// A point if it is in frame, else an arrow at the frame edge pointing towards it.
fn mark_or_edge(
    p: &egui::Painter,
    pr: &Proj,
    sc: &Screen,
    pos: [f64; 2],
    label: &str,
    color: Color32,
) {
    let (x, y, w) = model::apply(&pr.hi, pos[0], pos[1]);
    if w > 0.0 && (0.0..=IMG_W).contains(&x) && (0.0..=IMG_H).contains(&y) {
        let q = sc.p([x, y]);
        p.circle_filled(q, 7.0, color);
        label_bg(p, q + vec2(9.0, -6.0), Align2::LEFT_BOTTOM, label, color);
        return;
    }
    let (mut dx, mut dy) = (x - IMG_W / 2.0, y - IMG_H / 2.0);
    if w <= 0.0 {
        dx = -dx;
        dy = -dy;
    }
    let t =
        ((IMG_W / 2.0 - 24.0) / dx.abs().max(1e-9)).min((IMG_H / 2.0 - 24.0) / dy.abs().max(1e-9));
    let e = sc.p([IMG_W / 2.0 + dx * t, IMG_H / 2.0 + dy * t]);
    let d = vec2(dx as f32, dy as f32).normalized() * 14.0;
    p.add(Shape::convex_polygon(
        vec![e + d, e - d + d.rot90() * 0.6, e - d - d.rot90() * 0.6],
        color,
        Stroke::NONE,
    ));
    let anchor = if dy > 0.0 {
        Align2::CENTER_BOTTOM
    } else {
        Align2::CENTER_TOP
    };
    let lp = e - d * 1.6;
    let lp = pos2(
        lp.x.clamp(sc.rect.left() + 80.0, sc.rect.right() - 80.0),
        lp.y,
    );
    label_bg(p, lp, anchor, &format!("{label} (off-frame)"), color);
}

pub struct Layers {
    pub cam: bool,
    pub grid: bool,
    pub site: bool,
    pub track: bool,
    pub csi: bool,
    pub heat: bool,
}

impl Default for Layers {
    fn default() -> Self {
        Self {
            cam: true,
            grid: true,
            site: true,
            track: true,
            csi: true,
            heat: false,
        }
    }
}

pub fn grid_and_markers(p: &egui::Painter, pr: &Proj, sc: &Screen, meta: &Meta) {
    for i in -2..=10 {
        let v = f64::from(i) * 0.5;
        let c = if i % 2 == 0 {
            CYAN.gamma_multiply(0.8)
        } else {
            CYAN.gamma_multiply(0.35)
        };
        seg(p, pr, sc, [v, -1.0], [v, 5.0], Stroke::new(1.0, c), 0.0);
        seg(p, pr, sc, [-1.0, v], [5.0, v], Stroke::new(1.0, c), 0.0);
    }
    for x in 0..=4 {
        for y in 0..=4 {
            if let Some(q) = pr.to_img([f64::from(x), f64::from(y)])
                && (0.0..IMG_W).contains(&q[0])
                && (0.0..IMG_H).contains(&q[1])
            {
                p.text(
                    sc.p(q) + vec2(3.0, -3.0),
                    Align2::LEFT_BOTTOM,
                    format!("{x},{y}"),
                    FontId::monospace(11.0),
                    CYAN,
                );
            }
        }
    }
    // axis arrows at the room-frame point nearest the image centre
    let c = pr.to_room([IMG_W / 2.0, IMG_H / 2.0]);
    let c = [(c[0] * 2.0).round() / 2.0, (c[1] * 2.0).round() / 2.0];
    if let (Some(o), Some(ex), Some(ny)) = (
        pr.to_img(c),
        pr.to_img([c[0] + 0.6, c[1]]),
        pr.to_img([c[0], c[1] + 0.6]),
    ) {
        arrow(
            p,
            sc.p(o),
            sc.p(ex),
            Color32::from_rgb(255, 128, 128),
            "x / E",
        );
        arrow(
            p,
            sc.p(o),
            sc.p(ny),
            Color32::from_rgb(128, 255, 128),
            "N (y)",
        );
    }
    for (id, xy) in &meta.markers {
        if let Some(q) = pr.to_img(*xy) {
            let q = sc.p(q);
            p.circle_stroke(q, 9.0, Stroke::new(2.0, YELLOW));
            p.text(
                q + vec2(11.0, 4.0),
                Align2::LEFT_CENTER,
                format!("M{id}"),
                FontId::proportional(14.0),
                YELLOW,
            );
        }
    }
}

pub fn site(
    p: &egui::Painter,
    pr: &Proj,
    sc: &Screen,
    meta: &Meta,
    csi: Option<&Csi>,
    k: Option<usize>,
    show_csi: bool,
) {
    let [rx, ry] = meta.room_max;
    let room = [[0.0, 0.0], [rx, 0.0], [rx, ry], [0.0, ry]];
    for i in 0..4 {
        seg(
            p,
            pr,
            sc,
            room[i],
            room[(i + 1) % 4],
            Stroke::new(2.0, Color32::from_white_alpha(200)),
            0.0,
        );
    }
    for (kind, mn, mx) in meta.rects("openings") {
        if kind == "door" && mn[1] <= 0.0 && mx[1] >= -0.2 {
            seg(
                p,
                pr,
                sc,
                [mn[0], 0.0],
                [mx[0], 0.0],
                Stroke::new(4.0, Color32::from_rgb(255, 160, 64)),
                0.0,
            );
        }
    }
    let Some(router) = meta.router() else { return };
    for (spot, pos, ids) in meta.spots() {
        let lanes: Vec<&String> = if show_csi && csi.is_some() {
            ids.iter().collect()
        } else {
            Vec::new()
        };
        if lanes.is_empty() {
            seg(p, pr, sc, router, pos, Stroke::new(2.0, PINK), 0.0);
        }
        for (li, nid) in lanes.iter().enumerate() {
            let off = (li as f32 - (lanes.len() as f32 - 1.0) / 2.0) * 5.0;
            let z = csi.zip(k).and_then(|(c, k)| c.live_z(nid, k));
            let stroke = Stroke::new(
                3.0,
                if z.is_some() {
                    z_color(z)
                } else {
                    Color32::from_gray(130)
                },
            );
            if z.is_some() {
                seg(p, pr, sc, router, pos, stroke, off);
            } else if let Some((a, b)) = pr.seg_img(router, pos) {
                // gated: dashed grey
                let (a, b) = (sc.p(a), sc.p(b));
                let n = (b - a).normalized().rot90() * off;
                p.extend(Shape::dashed_line(&[a + n, b + n], stroke, 6.0, 5.0));
            }
        }
        let names: Vec<String> = ids.iter().map(|i| format!("n{i}")).collect();
        mark_or_edge(
            p,
            pr,
            sc,
            pos,
            &format!("spot {spot} ({})", names.join(",")),
            PINK,
        );
    }
    mark_or_edge(p, pr, sc, router, "router", Color32::WHITE);
}

fn ellipse_pts(sc: &Screen, e: &model::Ellipse) -> Vec<Pos2> {
    (0..48)
        .map(|i| {
            let a = f64::from(i) / 48.0 * std::f64::consts::TAU;
            let (ca, sa) = (e.ang.cos(), e.ang.sin());
            let (x, y) = (e.a * a.cos(), e.b * a.sin());
            sc.p([e.cx + x * ca - y * sa, e.cy + x * sa + y * ca])
        })
        .collect()
}

/// Person on the camera: silhouette ellipse with its ends, floor point, far point, keypoints, heading, trail.
pub fn person(
    p: &egui::Painter,
    pr: &Proj,
    sc: &Screen,
    trail: &[TrackFrame],
    f: Option<&TrackFrame>,
    xy: Option<[f64; 2]>,
) {
    let pts: Vec<Pos2> = trail
        .iter()
        .filter_map(|t| t.floor_px)
        .map(|q| sc.p(q))
        .collect();
    if pts.len() > 1 {
        p.add(Shape::line(
            pts,
            Stroke::new(2.0, PERSON.gamma_multiply(0.6)),
        ));
    }
    let Some(f) = f.filter(|f| f.present) else {
        label_bg(
            p,
            sc.rect.min + vec2(8.0, 8.0),
            Align2::LEFT_TOP,
            "nobody visible (tracker)",
            Color32::LIGHT_GRAY,
        );
        return;
    };
    if let Some(e) = &f.ellipse_px {
        p.add(Shape::closed_line(
            ellipse_pts(sc, e),
            Stroke::new(2.0, PERSON),
        ));
        for s in [-1.0, 1.0] {
            p.circle_filled(
                sc.p([e.cx + s * e.a * e.ang.cos(), e.cy + s * e.a * e.ang.sin()]),
                4.0,
                PERSON,
            );
        }
    }
    if let Some(kp) = &f.keypoints_px {
        // COCO skeleton subset: shoulders, hips, arms
        const BONES: [(usize, usize); 10] = [
            (5, 6),
            (5, 7),
            (7, 9),
            (6, 8),
            (8, 10),
            (5, 11),
            (6, 12),
            (11, 12),
            (0, 5),
            (0, 6),
        ];
        for (a, b) in BONES {
            if let (Some(u), Some(v)) = (kp.get(a), kp.get(b))
                && u[2] > 0.3
                && v[2] > 0.3
            {
                p.line_segment(
                    [sc.p([u[0], u[1]]), sc.p([v[0], v[1]])],
                    Stroke::new(2.0, Color32::from_rgb(255, 170, 0)),
                );
            }
        }
    }
    if let Some(p3) = &f.points3_px {
        for (q, c) in [
            (p3.head, Color32::from_rgb(0, 208, 255)),
            (p3.l_shoulder, YELLOW),
            (p3.r_shoulder, Color32::from_rgb(255, 120, 0)),
        ] {
            if let Some(q) = q {
                p.circle_filled(sc.p(q), 5.0, c);
            }
        }
    } else if let Some(q) = f.far_px {
        p.circle_filled(sc.p(q), 5.0, Color32::from_rgb(0, 208, 255));
    }
    if let Some(q) = f.centroid_px {
        p.circle_filled(sc.p(q), 4.0, Color32::WHITE);
    }
    if let Some(q) = f.floor_px {
        let q = sc.p(q);
        p.circle_filled(q, 7.0, FRONT);
        if let Some(xy) = xy {
            label_bg(
                p,
                q + vec2(10.0, 12.0),
                Align2::LEFT_TOP,
                &format!("({:.2}, {:.2}) m", xy[0], xy[1]),
                Color32::WHITE,
            );
        }
    }
    if let (Some(xy), Some(h)) = (xy, f.heading_rad)
        && let (Some(a), Some(b)) = (
            pr.to_img(xy),
            pr.to_img([xy[0] + 0.45 * h.cos(), xy[1] + 0.45 * h.sin()]),
        )
    {
        let src = f.heading_source.as_deref().unwrap_or("?");
        arrow(p, sc.p(a), sc.p(b), FRONT, &format!("front? ({src})"));
    }
}
