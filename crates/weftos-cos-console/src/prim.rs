//! Node constructors in integer pixels. Everything the console draws
//! goes through these so the theme (1 px strokes, built-in mono
//! fonts, layers) is applied in one place.

use weftos_leaf_scene::{
    px, AnimatableProperty, CursorHint, EaseCurve, FontFace, HitShape, InputRegion, KerningHint,
    Layer, Node, Point, Primitive, PropertyValue, Rgba, SceneOp, Style, Transform,
};

use crate::theme::{nid, FontMetrics, STROKE_1PX};

fn stroke_style(color: Rgba) -> Style {
    Style {
        stroke: Some(color),
        stroke_width_q8: STROKE_1PX,
        ..Style::default()
    }
}

/// Single-line text at `(x, y)` (top-left of the first cell).
pub fn text(id: u32, font: &FontMetrics, x: i32, y: i32, s: impl Into<String>, color: Rgba) -> Node {
    Node {
        id: nid(id),
        layer: Layer::Text,
        transform: Transform::translate(px(x), px(y)),
        primitive: Primitive::Text {
            content: s.into(),
            face: FontFace::Builtin(font.face),
            size_q8: font.size_q8,
            weight: 400,
            kerning: KerningHint::Auto,
        },
        style: Style::filled(color),
        input: None,
    }
}

/// 1 px line from `(x1, y1)` to `(x2, y2)`.
pub fn line(id: u32, x1: i32, y1: i32, x2: i32, y2: i32, color: Rgba) -> Node {
    Node {
        id: nid(id),
        layer: Layer::Widget,
        transform: Transform::translate(px(x1), px(y1)),
        primitive: Primitive::Line {
            x2: px(x2 - x1),
            y2: px(y2 - y1),
            thickness_q8: STROKE_1PX,
        },
        style: stroke_style(color),
        input: None,
    }
}

fn circle(id: u32, cx: i32, cy: i32, r: i32, style: Style) -> Node {
    Node {
        id: nid(id),
        layer: Layer::Widget,
        transform: Transform::translate(px(cx), px(cy)),
        primitive: Primitive::Circle {
            radius_q16: (r.max(0) as u32) << 16,
        },
        style,
        input: None,
    }
}

/// Stroked circle (1 px ring).
pub fn circle_stroke(id: u32, cx: i32, cy: i32, r: i32, color: Rgba) -> Node {
    circle(id, cx, cy, r, stroke_style(color))
}

/// Filled disc.
pub fn circle_fill(id: u32, cx: i32, cy: i32, r: i32, color: Rgba) -> Node {
    circle(id, cx, cy, r, Style::filled(color))
}

/// Filled rectangle on `layer`.
pub fn rect_fill(id: u32, layer: Layer, x: i32, y: i32, w: i32, h: i32, color: Rgba) -> Node {
    Node {
        id: nid(id),
        layer,
        transform: Transform::translate(px(x), px(y)),
        primitive: Primitive::Rect {
            w: px(w),
            h: px(h),
            radius_q8: 0,
        },
        style: Style::filled(color),
        input: None,
    }
}

/// Stroked (1 px) rectangle on `layer`.
pub fn rect_stroke(id: u32, layer: Layer, x: i32, y: i32, w: i32, h: i32, color: Rgba) -> Node {
    Node {
        id: nid(id),
        layer,
        transform: Transform::translate(px(x), px(y)),
        primitive: Primitive::Rect {
            w: px(w),
            h: px(h),
            radius_q8: 0,
        },
        style: stroke_style(color),
        input: None,
    }
}

/// Make a rectangle node tappable over its whole extent.
pub fn tappable(mut node: Node) -> Node {
    if let Primitive::Rect { w, h, .. } = node.primitive {
        node.input = Some(InputRegion {
            shape: HitShape::Aabb { w, h },
            cursor_hint: CursorHint::Pointer,
            capture: true,
        });
    }
    node
}

/// Invisible touch target covering `(x, y, w, h)`; captures drags.
/// Drawn with no fill and no stroke, so the renderer emits nothing but
/// the hit-test still finds it.
pub fn hit_rect(id: u32, layer: Layer, x: i32, y: i32, w: i32, h: i32) -> Node {
    Node {
        id: nid(id),
        layer,
        transform: Transform::translate(px(x), px(y)),
        primitive: Primitive::Rect {
            w: px(w),
            h: px(h),
            radius_q8: 0,
        },
        style: Style::default(),
        input: Some(InputRegion {
            shape: HitShape::Aabb { w: px(w), h: px(h) },
            cursor_hint: CursorHint::Grab,
            capture: true,
        }),
    }
}

/// Polyline approximation of an elliptical arc, one node per segment,
/// ids `first_id..first_id + segs`. Angles in degrees, clockwise on
/// screen (y grows downward).
#[allow(clippy::too_many_arguments)]
pub fn arc(
    first_id: u32,
    cx: f32,
    cy: f32,
    rx: f32,
    ry: f32,
    a0_deg: f32,
    a1_deg: f32,
    segs: u32,
    color: Rgba,
) -> Vec<Node> {
    let segs = segs.max(1);
    let step = (a1_deg - a0_deg) / segs as f32;
    let pt = |i: u32| {
        let a = (a0_deg + step * i as f32).to_radians();
        (
            (cx + rx * a.cos()).round() as i32,
            (cy + ry * a.sin()).round() as i32,
        )
    };
    (0..segs)
        .map(|i| {
            let (x1, y1) = pt(i);
            let (x2, y2) = pt(i + 1);
            line(first_id + i, x1, y1, x2, y2, color)
        })
        .collect()
}

/// Set the drawing colour of any console node: stroke for lines and
/// rings, fill for text, discs and rectangles.
pub fn recolor(node: &mut Node, color: Rgba) {
    if node.style.stroke.is_some() {
        node.style.stroke = Some(color);
    }
    if node.style.fill.is_some() {
        node.style.fill = Some(color);
    }
}

/// Colour the node currently shows (stroke first, then fill).
pub fn color_of(node: &Node) -> Option<Rgba> {
    node.style.stroke.or(node.style.fill)
}

/// Mark a node hidden; it stays in the scene for a cheap reveal later.
pub fn hidden(mut node: Node) -> Node {
    node.style.visible = false;
    node
}

/// Stroke-colour tween over `duration_ms`.
pub fn tween_stroke(id: u32, from: Rgba, to: Rgba, duration_ms: u32) -> SceneOp {
    SceneOp::Tween {
        id: nid(id),
        property: AnimatableProperty::Stroke,
        from: PropertyValue::Color(from),
        to: PropertyValue::Color(to),
        duration_ms,
        start_at: None,
        curve: EaseCurve::EaseInOut,
    }
}

/// Position tween between integer-pixel points.
pub fn tween_pos(id: u32, from: (i32, i32), to: (i32, i32), duration_ms: u32) -> SceneOp {
    SceneOp::Tween {
        id: nid(id),
        property: AnimatableProperty::Position,
        from: PropertyValue::Position(Point::from_px(from.0, from.1)),
        to: PropertyValue::Position(Point::from_px(to.0, to.1)),
        duration_ms,
        start_at: None,
        curve: EaseCurve::Linear,
    }
}
