---
name: urth-spatial-checks
description: Diagnose spatial data and results that are wrong without raising an error, for Urth ingest and any geospatial work in WeftOS. Use when layers don't line up, a region or capture lands in the wrong place, geometry looks shifted, mirrored or at (0,0), a spatial join returns too few or too many rows, areas or distances are off by orders of magnitude, a raster sits half a cell off its vectors, a land-cover raster shows classes that aren't in its legend, or a region's T_ecef_local transform is suspect. Looks before it reads - plot the data, read the axis ranges, zoom to feature scale - then runs frame, row-count, geometry-validity, magnitude and ground-truth checks. Also covers loud API-drift errors in geospatial libraries. Not for writing new ingest pipelines from scratch or non-spatial debugging.
---

# Urth spatial checks

For spatial answers that are **wrong but did not fail**: the join that kept 40 %
of its rows, the buffer measured in degrees, the DEM tile one cell east of the
OSM roads, the room capture that landed in the ocean because its site anchor was
stamped but never applied. A traceback says where code broke. Nothing here
broke; the answer is just false.

This skill directs attention and checks data. It does not write the pipeline.
Urth's frame and projection rules are in
[`references/urth-frames-and-rules.md`](references/urth-frames-and-rules.md);
loud library errors are in [`references/library-drift.md`](references/library-drift.md).

## 1. Look before you read

**A picture of the geometry is evidence. Everything the data says about itself
is a claim.**

A CRS string, a region's frame tag, a GeoTIFF header or a `T_ecef_local` on a
region record tells you what someone asserted. It does not tell you where the
coordinates are. They disagree often, and when they do the label is usually the
wrong one, because labels get attached after coordinates have been read,
swapped, truncated or transformed upstream.

How the label lies:

- **Assign instead of transform.** The frame label changes, the numbers don't
  (`set_crs` where `to_crs` was meant; a region tagged ENU whose points are still
  ECEF; a `T_ecef_local` recorded on a region but never applied to its leaves).
- **Default assumed.** A shapefile with no `.prj`, a GeoJSON with an old `crs`
  member, a capture with no anchor, read as WGS84 anyway.
- **Axis order.** An API returns `(lat, lon)`, it is loaded as `(x, y)`, and it is
  stamped EPSG:4326. Budapest is now in Somalia.
- **Stale sidecar.** The file was reprojected; its metadata wasn't.
- **Lost SRID.** Geometry rebuilt from WKT that dropped its reference system.

Pixels land where the coordinates put them. When the picture and the label
disagree, believe the picture. So the visual tier always runs first.

## 2. Tier 1: look (one minute)

Do these before opening the code that built the layer.

### 2a. All layers, one plot, raw coordinates

One figure, every layer, plain axes, no basemap. The question is only: **do
they land in the same window?**

| What you see | Usually means |
|---|---|
| One layer is a dot, the other a continent | degrees vs metres |
| Different hemispheres | sign flip or swapped axes |
| A layer sits at (0, 0) off West Africa | lost coordinates defaulted to zero |
| Right shapes, far apart | different frames, no transform, or assign-not-transform |
| Mirrored or rotated 90° | axis order swapped |
| A layer plots nothing | a filter or join already emptied it |
| ECEF numbers (~6.4e6 in magnitude) beside small local numbers | an ENU layer never placed with its `T_ecef_local` |

If they don't share a window, stop. Nothing downstream matters yet.

Don't let a web map hide it: an interactive viewer that reprojects everything to
Web Mercator, or axes that auto-fit to the union of both layers, can make a huge
offset look modest. Plot raw first.

### 2b. Read the axis numbers

1. **Which units?** ±180 / ±90 is degrees. Hundreds of thousands to millions is
   projected metres. Tens of millions is likely Web Mercator. Magnitudes near
   6.4 million on all three axes are ECEF metres. A few to a few thousand around
   zero is a local ENU frame.
2. **Plausible span for the LOD?** Check against the table in section 3, gate 4.
3. **x and y consistent?** Latitude can't exceed 90. A y over 90 with x inside
   ±180 is projected coordinates or a swap.
4. **Does a distance parameter fit these units?** A 500 buffer on a degree axis
   is 500 degrees.

**Hide axes last.** Final cartography drops the axes, and that removes the
cheapest check there is. Drop them in the export step only, never while
exploring.

### 2c. Zoom to feature scale

A 30 m offset across a city is about one pixel. Zoom until single buildings,
road centrelines or room walls are legible, and toggle layers on and off against
each other. Only visible at this scale:

- **Datum shift**: the same few-metre offset everywhere in one direction (e.g.
  WGS84 vs NAD83 or an ITRF epoch difference). Both layers "are WGS84".
- **Half-cell registration**: a raster shifted by exactly half a pixel (cell
  centre vs cell corner, `PixelIsPoint` vs `PixelIsArea`).
- **Resampling artefacts**: stair-stepped raster edges, or a categorical raster
  with in-between values along class borders.
- **Precision loss**: vertices snapped to a coarse grid by a low-precision
  round trip (float32 ECEF steps by half a metre at Earth radius).
- **Partial alignment**: one part fits, another doesn't. A warp or mosaic seam,
  not a global transform error. At Urth L4 a seam between quilt patches is
  allowed; a seam inside one patch is not.

## 3. Tier 2: checks (only after tier 1 looks right)

These print and assert. They don't transform. If tier 1 failed, they will pass
cleanly on ruined data.

### Gate 1: frame and bounds around every transform

Print the frame label **and the bounds** before and after every read, transform,
reprojection and geometry rebuild, so the step that changed things is
identifiable.

- Label changed, bounds didn't: assign where transform was meant.
- Bounds changed, label didn't: transformed without relabelling.
- For a region: transform one known leaf point ECEF→ENU→ECEF and check it
  returns within a millimetre. Anything else means the stored transform and the
  leaves disagree.

### Gate 2: row accounting on every join and filter

Record left in, right in, out.

- **out = 0**: no spatial relationship. Almost always a frame mismatch, which
  tier 1 should have caught. If tier 1 was clean, check the predicate and the
  geometry types.
- **out < left**: unmatched rows dropped. Decide deliberately; use a left join if
  they must stay.
- **out > left**: one-to-many. Every later sum is inflated. This is the
  plausible total that is 1.4× too big.

Rejection thresholds are in the rules reference, section 4.

### Gate 3: null and invalid geometry, before anything consumes it

Count nulls and invalid geometries (self-intersections, unclosed rings, empty
geometries) at read time and after every operation that builds geometry. Nulls
pass silently through joins and dissolves; invalid polygons give wrong areas,
not missing ones.

### Gate 4: magnitude, stated before you look

Write the expected order of magnitude down first, then compare.

| Measure | Expect |
|---|---|
| L0 region span | thousands of km |
| L1 country / state | hundreds to a few thousand km |
| L2 city | 10–50 km across |
| L3 site / campus | 100 m – 2 km |
| L4 room / zone | 2–50 m |
| L5 object | 1 cm – 10 m |
| Urban parcel area | 10²–10⁴ m² |
| Building height | 3–500 m |
| Adjacent-feature distance | metres, never degrees |

Signatures:

- Off by 10³–10⁵ in one dimension (about 10¹⁰ in area): degrees used as metres.
  One degree of latitude is about 111 km.
- Off by a factor that grows with latitude: area computed in Web Mercator, which
  is conformal, not equal-area (about 2× at 45°, 4× at 60°).
- Off by exactly 1000: a millimetre/metre or metre/kilometre slip, common in
  capture and DEM metadata.

### Gate 5: one feature checked by hand

Take one building, one road segment, one room corner. Verify it end to end
against an outside reference: measure it on imagery, look up its surveyed
position, or tape-measure the real room. A pipeline can produce a believable
mean, median and spread while every value is wrong by the same factor; one
hand-checked feature catches that in a minute. Every Urth region should carry
at least one such anchor (see the rules reference, section 5).

## 4. How to open a diagnosis

The first reply is not code. It is tier 1, handed back as two or three things to
look at, and a request to say what they see. Code written before anyone has
looked at the geometry encodes whatever is already wrong with it.

When they report back, go straight to tier 2 and the work. Don't ask for a
second round of looking unless their answer showed a tier 1 failure.

Openings that work:

- **"Cluster these points."** Plot them alone, axes on, and tell me the x and y
  ranges and whether they land where you expect. The clustering distance is in
  the geometry's units: in degrees, `eps=500` is everything in one cluster, and
  `eps=0.005` is about 500 m at the equator but about 300 m at 55° N.
- **"Join these to the admin boundaries / sites."** Plot both on one map, points
  over polygons, axes on, and give me each layer's row count. A frame mismatch
  returns zero rows or a few accidental matches and looks like a data problem.
- **"Buffer and count."** Axis range and the real-world buffer distance. A 500 m
  buffer on degree coordinates covers most of a hemisphere and returns "all of
  them", which reads as an answer.
- **"These areas look wrong."** Axis range, the frame the areas were computed in,
  and one polygon whose real area you know.
- **"This capture / room is in the wrong place."** The site anchor's lat/lon/h,
  the region's `T_ecef_local`, and one point from the capture in both frames. If
  the capture plots at the ECEF origin or the anchor's antipode, the transform
  was recorded but not applied, or applied inverted.

## 5. Routing the explanation

End a diagnosis with what went wrong, what to change, and where the concept is
written down. Point to the reference rather than repeating it:

| Diagnosis | Read |
|---|---|
| Frame mismatch, assign-vs-transform, ENU/ECEF placement | rules reference §1, ADR-079 §5 |
| Axis order, null island | rules reference §1.4 |
| Distances or buffers in degrees | rules reference §1.2 |
| Area error growing with latitude | rules reference §2 |
| Constant small offset at building scale | rules reference §1.5 (datums) |
| Raster half a cell off, invented classes | rules reference §3 |
| Join returning zero or inflated rows | rules reference §4 |
| Aggregates plausible, individual values wrong | gate 5; rules reference §5 |
| `AttributeError` / `ImportError` naming a geo library | [`references/library-drift.md`](references/library-drift.md), not this method |
