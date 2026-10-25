# Library drift: loud geospatial API breaks

Everything in `SKILL.md` is about silent wrongness. This file is the opposite
class: an `AttributeError`, `ImportError` or compile error on the first call,
naming the missing symbol. It stops the program instead of corrupting the
answer, so tier 1 and tier 2 don't apply. The only question is which version is
installed and which version the code was written for.

```python
import importlib
for m in ("h3", "osmnx", "shapely", "rasterio", "geopandas", "pyproj"):
    try:
        print(m, importlib.import_module(m).__version__)
    except ImportError as e:
        print(m, "not importable:", e)
```

```bash
cargo tree -i geo; cargo tree -i proj   # Rust: which versions resolved
```

## h3 (v3 → v4)

v4 renamed the API to `<from>_to_<to>` without `h3_` prefixes:

| v3 | v4 |
|---|---|
| `geo_to_h3` | `latlng_to_cell` |
| `h3_to_geo` | `cell_to_latlng` |
| `h3_to_geo_boundary` | `cell_to_boundary` |
| `polyfill(geojson, res, geo_json_conformant=True)` | `polygon_to_cells(LatLngPoly(...), res)` |
| `k_ring` | `grid_disk` |
| `h3_to_parent` | `cell_to_parent` |
| `h3_get_resolution` | `get_resolution` |
| `h3_is_valid` | `is_valid_cell` |

Trap: `LatLngPoly` takes `(lat, lng)`; GeoJSON is `(lng, lat)`. Porting a
`geo_json_conformant=True` call without swapping pairs returns cells in the
wrong place, silently. Run tier 1 on the result.

## osmnx (1.x → 2.0)

`geometries_*` became `features_*` (`features_from_place`, `_point`, `_bbox`,
`_polygon`, `_address`). Internal modules (`ox.utils_*`, `ox._errors`) were
reorganised and are not API: import `Point` from `shapely.geometry`, and catch
documented exceptions rather than private classes.

## shapely (1.8 → 2.0)

`import shapely` does not bind `shapely.strtree`; use
`from shapely import STRtree` (2.0) or `from shapely.strtree import STRtree`
(both). More important: in 2.0 `STRtree.query()` returns **integer indices**,
where 1.8 returned geometries. Fixing only the import turns a loud error into a
silent one; index back into the input array.

## rasterio

`rasterio.plot`, `.mask`, `.features` and `.warp` are submodules that
`import rasterio` does not load; import them explicitly. There is no `Reproject`
class: it is `from rasterio.warp import reproject, calculate_default_transform,
Resampling`. Choosing the `Resampling` value is a silent-wrongness question; see
the rules reference, §3.

## geopandas

`ModuleNotFoundError: geopandas` is usually the wrong interpreter (a notebook
kernel or IDE pointing at a different environment), not a missing install. Check
`sys.executable` before reinstalling. A broken GDAL/GEOS/PROJ stack can surface
as an import error naming one of those. A mismatched PROJ data directory can
also give wrong transforms with no error, which belongs back in tier 1.

## Rust: geo and proj

- `geo` 0.29 introduced metric-space distance (`Euclidean`, `Haversine`,
  `Geodesic`, `Rhumb` with a common `Distance` trait) and deprecated the older
  per-metric traits such as `HaversineDistance` and `EuclideanDistance`.
  Deprecation warnings under `-D warnings` fail the build; port to the new form.
- `proj` binds the system `libproj` unless the `bundled_proj` feature is on, so
  the same code can build on one machine and fail to link on another, or link
  against a PROJ with different grid files. Pin the feature and record the PROJ
  version in ingest metadata.
