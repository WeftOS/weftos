// CrowPanel ESP32 7" (DIS08070H) bezel for the field slate (ADR-119 §7).
// Mounts in a cut-out in the aluminium clipboard lid; the panel screws in from behind on
// its 4 x Ø3.2 holes. Dimensions from Elecrow's 7.0inch_HMI_Outline_Dimensions.pdf.
// All values are parameters: measure your lid and panel and adjust.

/* [Panel (from the Elecrow outline)] */
panel_w        = 181;     // board outline width, mm
panel_h        = 104;     // board outline height
hole_dx        = 174;     // mounting hole spacing, x
hole_dy        = 97;      // mounting hole spacing, y
hole_d         = 3.2;     // mounting hole diameter
glass_w        = 164.85;  // cover glass width
glass_h        = 99.95;   // cover glass height
view_w         = 156;     // visible window (active area 154.2 x 85.9 plus margin)
view_h         = 88;

/* [Lid and fit] */
lid_t          = 1.6;     // aluminium lid thickness at the cut-out (measure)
clearance      = 0.4;     // fit clearance around glass and board
lip_w          = 6;       // bezel lip overlapping the lid, each side
face_t         = 2.0;     // bezel face thickness above the lid
gasket_w       = 2.0;     // gasket groove width (cord or foam strip)
gasket_d       = 1.2;     // gasket groove depth
boss_h         = 6;       // standoff height behind the lid (panel stack depth, measure)
boss_d         = 7;
screw_d        = 2.6;     // M3 self-tap pilot into the boss

$fn = 48;

cut_w = glass_w + 2*clearance;   // lid cut-out the bezel neck passes through
cut_h = glass_h + 2*clearance;

module rounded_rect(w, h, r, t) {
    linear_extrude(t) offset(r) offset(-r) square([w, h], center = true);
}

module bezel() {
    difference() {
        union() {
            // face: sits on the outside of the lid
            translate([0, 0, lid_t]) rounded_rect(cut_w + 2*lip_w, cut_h + 2*lip_w, 3, face_t);
            // neck: passes through the lid cut-out
            rounded_rect(cut_w, cut_h, 1.5, lid_t);
        }
        // viewing window
        translate([0, 0, -1]) rounded_rect(view_w, view_h, 1.5, lid_t + face_t + 2);
        // pocket for the cover glass, so the glass sits flush under the face
        translate([0, 0, -0.01]) rounded_rect(glass_w + 2*clearance, glass_h + 2*clearance, 1, lid_t + face_t - 1.0);
        // gasket groove on the underside of the face, around the neck
        translate([0, 0, lid_t - 0.01])
            difference() {
                rounded_rect(cut_w + 2*gasket_w + 1, cut_h + 2*gasket_w + 1, 2, gasket_d);
                translate([0, 0, -0.1]) rounded_rect(cut_w + 1, cut_h + 1, 2, gasket_d + 0.2);
            }
    }
}

// Clamp frame behind the lid: the panel screws to its bosses, and the frame clamps the
// bezel and gasket to the lid with four screws through the lid (or bonded).
module clamp_frame() {
    difference() {
        union() {
            rounded_rect(panel_w + 8, panel_h + 8, 3, 2);
            for (sx = [-1, 1], sy = [-1, 1])
                translate([sx*hole_dx/2, sy*hole_dy/2, 0]) cylinder(d = boss_d, h = boss_h);
        }
        translate([0, 0, -1]) rounded_rect(cut_w, cut_h, 1.5, 4);
        for (sx = [-1, 1], sy = [-1, 1])
            translate([sx*hole_dx/2, sy*hole_dy/2, -1]) cylinder(d = screw_d, h = boss_h + 2);
    }
}

part = "bezel"; // [bezel, clamp_frame, both]
if (part == "bezel" || part == "both") bezel();
if (part == "clamp_frame") clamp_frame();
if (part == "both") translate([0, panel_h + 20, 0]) clamp_frame();
