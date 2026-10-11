// Switch plate for the field slate (ADR-119 §1, §3): C&K miniature toggles and DIP switches
// under a hinged cover. Measure your switches; C&K miniature toggles (7000 series) commonly use
// a 1/4-40 (6.35 mm) bushing, and DIP windows depend on the position count.

/* [Plate] */
plate_w     = 150;
plate_h     = 40;
plate_t     = 2.5;
corner_r    = 3;

/* [Toggles] */
toggle_count   = 4;
toggle_bushing = 6.6;    // hole for a 6.35 mm bushing plus clearance
toggle_pitch   = 18;
toggle_key     = true;   // anti-rotation key slot
toggle_y       = 10;

/* [DIP switches] */
dip_positions  = [8, 4];  // banks, left to right
dip_pitch      = 2.54;    // pitch per position
dip_win_h      = 7.5;     // window height over the actuators
dip_gap        = 10;
dip_y          = -10;

/* [Cover hinge] */
hinge_d        = 4;
hinge_len      = plate_w - 20;

$fn = 40;

module plate() {
    difference() {
        linear_extrude(plate_t) offset(corner_r) offset(-corner_r)
            square([plate_w, plate_h], center = true);
        // toggles
        start = -(toggle_count - 1) * toggle_pitch / 2;
        for (i = [0 : toggle_count - 1]) {
            translate([start + i*toggle_pitch, toggle_y, -1]) cylinder(d = toggle_bushing, h = plate_t + 2);
            if (toggle_key) translate([start + i*toggle_pitch - 0.6, toggle_y + toggle_bushing/2, -1]) cube([1.2, 1.6, plate_t + 2]);
        }
        // DIP windows
        widths = [for (n = dip_positions) n * dip_pitch + 1.5];
        total  = sum(widths) + dip_gap * (len(widths) - 1);
        for (b = [0 : len(widths) - 1]) {
            x0 = -total/2 + sum_to(widths, b) + b*dip_gap;
            translate([x0, dip_y - dip_win_h/2, -1]) cube([widths[b], dip_win_h, plate_t + 2]);
        }
    }
    // hinge knuckle along the top edge for the cover
    translate([-hinge_len/2, plate_h/2 - hinge_d/2, plate_t]) rotate([0, 90, 0])
        difference() { cylinder(d = hinge_d, h = hinge_len); translate([0, 0, -1]) cylinder(d = 1.8, h = hinge_len + 2); }
}

function sum(v, i = 0) = i >= len(v) ? 0 : v[i] + sum(v, i + 1);
function sum_to(v, n, i = 0) = i >= n ? 0 : v[i] + sum_to(v, n, i + 1);

plate();
