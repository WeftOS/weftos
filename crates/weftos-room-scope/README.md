# weft-room-scope

An overhead-camera room viewer (native egui). It plays pre-extracted camera frames and paints over them:

1. **Camera and room frame.** A 0.5 m grid through the inverse floor homography, axis arrows, the floor markers, the room outline and door, and sensor spots and the router as edge arrows when they are off-frame.
2. **Tracked person.** Floor point, silhouette or shoulder ellipse with its ends, keypoints and head/shoulder points when the backend gives them, a 3 s trail, and a "front?" arrow labelled with its source (`motion` or `keypoints`). The backend that produced the track is shown, and the top bar switches between backends.
3. **CSI deviation.** Router→spot links with one lane per node, coloured by robust z (dashed grey when gated), per-node z bars with a gated indicator, and an optional "naive link-weighted heat, not a localisation" layer.

The side panel also carries a top-down room plan, per-marker tracker accuracy for every backend, and the CSI agreement and hold-out false-alarm tables. Below the camera, two plots show per-node z and the tracked x/y over time, with cue markers and a playhead. Click or drag either plot to scrub.

```bash
scripts/build.sh scope room
target/release/weft-room-scope --data <dir> [--run <name>] [--t <video s>] [--backend <id>] [--heat] [--play]
# or ROOM_SCOPE_DATA=<dir>; COMPANION_SCREENSHOT=<file.bmp> [COMPANION_SCREENSHOT_AFTER=<s>] for a self-screenshot
```

## Data directory

The data directory is private and is never part of this crate. It is produced by the preprocessing scripts that live next to the data:

| Path | Content |
|---|---|
| `data/<run>_meta.json` | cues, `video_origin_epoch`, `video_info.duration`, `homographies` (px → room m, 3×3), floor markers, site layout (`room`, `walls`, `openings`, `objects`, `router`, `spots`, `nodes`) |
| `data/<run>_track.<backend>.json` | `room-scope.track.v1`: per frame `t`, `present`, `conf`, `xy` (smoothed m), `raw_xy`, `floor_px`, `ellipse_px` / `ellipse_m` `{cx, cy, a, b, ang}`, `heading_rad` + `heading_source`, optional `keypoints_px` (COCO-17) and `points3_px` |
| `data/<run>_track_eval.<backend>.json` | optional per-marker error during still phases |
| `data/<run>_csi.json`, `data/csi_summary.json` | optional per-node z / fps / gate at a fixed `dt`, plus baseline, false-alarm and agreement summaries |
| `cache/frames/<run>/index.json` + JPEGs | frames keyed by video time (`fps`, `count`, `pattern`) |

The sync offset per run is saved to `cache/room-scope.json` in the same directory. Image coordinates are source pixels of a 1280×720 frame.

All positions are ESTIMATED. The mapping is a planar homography, the lens is not undistorted, and the heat layer is not a localisation. The tests use only the synthetic fixture in `tests/fixture`.
