# Urth frames, projections, resampling and join rules

> **Draft for owner review, 2026-10-25.** These are Urth's own rules, built on
> ADR-079 §5 (ECEF/WGS84 identity, ENU local frames, `T_ecef_local` per region).
> Numbers marked *policy* are choices, not facts, and are the owner's to change.
> Everything else is geodesy and can be checked.

## 1. Frames

### 1.1 Which frame for what

| Frame | EPSG | Use in Urth | Never use for |
|---|---|---|---|
| ECEF (WGS 84 geocentric) | 4978 | region identity, the parent of every `T_ecef_local`, storage of anchors | anything rendered or measured on a map; it has no "up" |
| WGS 84 geographic 3D | 4979 | exchange with the outside world, anchors as lat/lon/ellipsoidal height | distances, buffers, areas (units are degrees) |
| WGS 84 geographic 2D | 4326 | ingest of 2D vector feeds (OSM, admin boundaries) | same as above |
| ENU local tangent | (per region, from its anchor) | captures, quilts, indoor BVH, all L3–L5 metric work | regions wider than §1.3 allows |
| UTM zone of the region centroid | 326xx / 327xx | L2 city and L3 site metric work on 2D vector data | regions crossing a zone edge by more than §1.3 allows |
| Local equal-area (LAEA centred on the region) | custom | area and density statistics for L1–L3 | shape, direction, display |
| Global equal-area (Equal Earth 8857, or EASE-Grid 2.0 6933) | 8857 / 6933 | planet-wide area statistics at L0 | local measurement |
| Web Mercator | 3857 | display tiles only | any measurement |

### 1.2 Units

Degrees are not distances. One degree of latitude is about 111.1 km
everywhere; one degree of longitude is about 111.3 km × cos(latitude) (about
78.8 km at 45°, 55.8 km at 60°). Any distance, buffer, clustering radius or
snapping tolerance is applied in a metric frame (ENU, UTM or a local projection),
never in 4326/4979.

### 1.3 How far a local frame stretches

ENU is a rigid rotation and translation of ECEF, so 3D distances in ENU are
exact at any size. What degrades with distance from the origin is the meaning
of "up": the ENU plane leaves the ellipsoid by about d²/2R.

| Distance from ENU origin | Plane above ellipsoid |
|---|---|
| 100 m | 0.8 mm |
| 1 km | 7.8 cm |
| 2 km | 31 cm |
| 5 km | 2.0 m |
| 10 km | 7.8 m |

*Policy:* one ENU frame per region up to 2 km radius (L3 and below). Beyond that,
split the region or use UTM for 2D work and convert heights through geodetic
coordinates.

UTM scale error is 0.9996 at the central meridian and about 1.0010 at a zone
edge near the equator, so under about 0.1 % within a zone. *Policy:* a region
may use its centroid's UTM zone if it extends no more than 1° past the zone edge;
otherwise use a local LAEA or transverse Mercator centred on the region.

Antimeridian: geographic bounds that wrap ±180° must be split or handled in
ECEF; a naive bounding box of a region crossing it spans the whole planet.

### 1.4 Axis order and null island

- EPSG:4326 and 4979 are formally latitude-first. GeoJSON, OGC:CRS84, PostGIS,
  GDAL's traditional order and most web APIs are longitude-first. Every ingest
  states which it reads, and gate 1 checks the result.
- (0, 0) in geographic coordinates is open ocean in the Gulf of Guinea. Any
  feature there is lost coordinates until proven otherwise. In ECEF the
  equivalent is a point near the origin (|p| far below 6.36e6 m).

### 1.5 Datums and epochs

- WGS 84 realisations are aligned to ITRF at the centimetre level. NAD83(2011)
  differs from WGS 84 by about 1–2 m across the continental US; ETRS89 drifts from
  WGS 84 by about 2.5 cm a year as Europe moves.
- *Policy:* anchors record the datum and, for anything better than 1 m accuracy,
  the epoch. A layer whose datum is unknown is labelled so and is not used for
  anchoring.
- A constant offset of a metre or two in one direction across a whole region is
  a datum mismatch until proven otherwise.

### 1.6 Precision

float32 steps by about half a metre at Earth radius. ECEF and geographic
coordinates are stored as float64. Local ENU coordinates for L4–L5 may be
float32 (millimetre steps out to about 8 km).

## 2. Equal-area, equidistant, conformal

| Property | Keeps | Distorts | Urth uses it for |
|---|---|---|---|
| Equal-area (LAEA, Albers, Equal Earth) | areas and densities | shape and angles away from the centre | area and density statistics |
| Equidistant (azimuthal equidistant) | distance from one point | everything else | range rings from a sensor |
| Conformal (UTM, transverse Mercator, Web Mercator) | local angles and shape | area (Mercator: badly) | 2D metric work at L2–L3 (UTM); display (3857) |
| ENU (not a map projection) | exact 3D geometry | the meaning of up, far from origin | everything L3–L5 |

Web Mercator scales length by sec(latitude) and area by sec²(latitude): areas
are about 2× too large at 45°, 4× at 60°, 33× at 80°. It is undefined beyond
about ±85.05°. In Urth it is a tile format, never an analysis frame.

When the choice changes the conclusion, not just the number:

- ranking regions by area or density across latitudes in Web Mercator ranks the
  northern ones too high;
- a coverage percentage ("how much of the country is captured") computed in a
  conformal projection is biased toward high latitudes;
- buffers in degrees grow east-west narrower toward the poles, so "within 1 km"
  means different things in different places.

*Policy tolerances:* area error ≤ 1 %, distance error ≤ 0.1 %, before the frame
must change.

## 3. Resampling

### 3.1 By what the pixels mean

| Raster kind | Upsample | Downsample | Never |
|---|---|---|---|
| Categorical (land cover, class ids, masks) | nearest | mode | bilinear, cubic, average (they invent classes) |
| Continuous (elevation, reflectance, temperature) | bilinear (default), cubic for smooth display | average | nearest when downsampling (aliasing) |
| Embeddings (e.g. OlmoEarth per-pixel vectors) | nearest | nearest, or recompute at the target scale | interpolating vectors, unless the model documents that blends are meaningful |
| Counts (population, detections) | redistribute by area | sum | average (loses totals) |

No-data is masked before resampling. A continuous resample across a no-data
edge pulls fill values into real cells.

### 3.2 Registration

- GeoTIFF `PixelIsArea` puts a pixel's value over its whole cell;
  `PixelIsPoint` puts it at the cell centre. Reading one as the other shifts the
  raster by half a cell.
- SRTM `.hgt` tiles are point-registered: samples sit on whole-degree grid lines,
  and adjacent tiles share their edge row and column.
- *Policy:* every raster ingest records its registration and the target grid's
  origin and cell size. Residual offset after alignment ≤ 0.1 cell.

### 3.3 Mixed resolutions

*Policy:* statistics run on the coarsest input's grid (finer data is aggregated
down; coarse data is never interpolated up to look detailed). Display may use the
finest.

## 4. When to refuse a join

### 4.1 Hard refusals

| Condition | Why |
|---|---|
| The two sides are in different frames | the join runs and returns wrong rows |
| Their bounds don't overlap at all | zero rows is guaranteed, and it looks like a data result |
| Either side has null or invalid geometry not yet handled | nulls vanish, invalid polygons match wrongly |
| The geometry types can't satisfy the predicate (points `contains` polygons) | the predicate is always false |
| The time windows don't overlap where time matters (a 2026 capture against 2015 footprints for change detection) | the relationship is meaningless |

### 4.2 Match-rate thresholds (*policy*)

| Match rate (left rows matched) | Verdict |
|---|---|
| ≥ 98 % | proceed; report the unmatched count |
| 90–98 % | proceed only after the unmatched rows are listed and explained |
| < 90 % | refuse; report and investigate |
| > 100 % (one-to-many) | refuse to aggregate until a rule is chosen: keep the largest overlap, split by area, or keep all and label |

### 4.3 Predicates

| Predicate | Use when | Common misuse |
|---|---|---|
| `intersects` | any contact counts | counting a point on a shared border twice |
| `within` / `contains` | strict membership | points exactly on a boundary drop out |
| `nearest` (with a max distance) | snapping features to a network | no max distance, in degrees |

### 4.4 What a refusal reports

The two layers, their frames and bounds, row counts in and out, the failed
condition, three example rows that failed, and the next check to run.

## 5. Ground-truth anchors

*Policy:* every Urth region at L3 and below carries at least one hand-checkable
anchor:

- L3 site: one surveyed or imagery-measured point (lat, lon, ellipsoidal height,
  datum, accuracy).
- L4 room: one measured dimension (wall length, door width) with its tolerance.
- L5 object: one measured size.

Gate 5 checks the region against its anchor after every structural update.
