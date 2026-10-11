# Field slate printed parts (ADR-119)

Parametric OpenSCAD sources; every dimension is a parameter at the top of each file.

| File | Part | Notes |
|---|---|---|
| `crowpanel-bezel.scad` (`part="bezel"`) | Screen bezel for the CrowPanel 7" | Glass pocket, gasket groove; from Elecrow's outline (181 x 104 board, holes Ø3.2 on 174 x 97) |
| `crowpanel-bezel.scad` (`part="clamp_frame"`) | Rear clamp frame with screw bosses | The panel screws to the bosses; set `boss_h` to the measured panel depth |
| `switch-plate.scad` | Switch plate | C&K toggle holes (6.35 mm bushing, keyed), DIP windows, cover hinge |

Set `lid_t` (the lid thickness at the cut-out) and `boss_h` from your measurements before printing.

Build STLs and previews (OpenSCAD's macOS cask fails Gatekeeper, so use the official Docker image):

```
docker run --rm -v "$PWD":/w -w /w openscad/openscad:latest sh -c '
  openscad -o out/crowpanel-bezel.stl -D part=\"bezel\" crowpanel-bezel.scad
  openscad -o out/crowpanel-clamp-frame.stl -D part=\"clamp_frame\" crowpanel-bezel.scad
  openscad -o out/switch-plate.stl switch-plate.scad
  xvfb-run -a openscad -o out/bezel.png --render --imgsize=1200,800 --autocenter --viewall -D part=\"bezel\" crowpanel-bezel.scad'
```

Blender can import the STLs for nicer renders or for fitting against Elecrow's STEP model
(`.planning/devices/crowpanel-display/.../3D_File_CrowPanel_7.0-ESP32_HMI-3.0.stp`, via a STEP importer).
