# WeftOS design tokens — machine-readable reference

Mirrors `docs/DESIGN.md` §2. Audit scripts read this file; the egui
runtime at `crates/clawft-gui-egui/src/theming/` must agree.

## Palette

| token | role | sRGB | alpha |
|---|---|---|---|
| `bg_app` | surface 0 (app root) | `#08080A` | 255 |
| `bg_panel` | surface 1 (panels, dock, tray) | `#0E0E12` | 255 |
| `bg_sidebar` | sidebar (warm-lifted charcoal) | `#2A2A30` | 255 |
| `bg_surface` | surface 2 (windows, cards) | `#16161C` | 255 |
| `bg_hover` | surface 3 (hover) | `#202028` | 255 |
| `bg_active` | surface 4 (active/selected) | `#2C2C36` | 255 |
| `stroke_hair` | 1px separators | `#FFFFFF` | 14 |
| `stroke_soft` | 1px panel/window edges | `#FFFFFF` | 24 |
| `text_primary` | body, headings | `#E0DEE8` | 255 |
| `text_secondary` | labels, monospace meta | `#AAA8B4` | 255 |
| `text_dim` | captions, hints | `#706E7A` | 255 |
| `accent` | focus / active *only* | `#C4A25C` | 255 |
| `accent_dim` | selection background | `#C4A25C` | 80 |
| `ok` | healthy, success, online | `#6EC896` | 255 |
| `warn` | degraded, attention | `#DCAF55` | 255 |
| `crit` | failed, offline, security | `#DC5F5F` | 255 |

## Type scale

| style | family | size | use |
|---|---|---|---|
| Heading | Proportional | 18 px | window titles, app headers (1/window) |
| Body | Proportional | 13 px | default |
| Small | Proportional | 11 px | captions, hints, dim labels |
| Mono | Monospace | 12.5 px | paths, IDs, code, terminals |
| Button | Proportional | 13 px | same as body |

## Spacing & radius

| token | value | use |
|---|---|---|
| `item_spacing` | `6 × 6` px | between items |
| `button_padding` | `10 × 5` px | inside buttons |
| `panel_inner_margin` | `8` px | inside panels |
| `window_inner_margin` | `8` px | inside windows |
| `indent` | `14` px | tree/list indent |
| `rounding` | `4` px | panels, cards, buttons |
| `rounding_tight` | `3` px | tight elements |
| `rounding_window` | `8` px | window corners (= rounding × 2) |
| `stroke` | `1.0` px | every stroke |

## Motion

| element | duration | easing |
|---|---|---|
| wallpaper grid | ~60 fps ambient | linear, no reaction |
| hover | instant | none |
| selection / focus | instant | none |
| toast (e.g. Copy Path) | 1500 ms | linear fade |
| stream / plot | data-driven | none decorative |

## Banned

- pure white (`#FFFFFF`) or pure black (`#000000`) foregrounds
- gradients on backgrounds (chrome)
- saturated brand colors outside `accent`
- spinners in calm chrome (loading state uses italic dim text)
- stroke widths other than 1.0
