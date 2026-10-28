# Hi-Link — Shenzhen Hi-Link Electronic

Shenzhen Hi-Link Electronic Co., Ltd. sells power modules, radios, and sensors
under the Hi-Link / Hilink name. This note is the 2026-10-05 pull of the public
product index at <https://www.hlktech.net/index.php?id=product> (pageid 1–61)
plus the category pages linked from that index. The English mirror
<https://www.hlktech.com/en/Product/> was not needed; every .net page requested
returned HTTP 200.

Live Sensor Explorer D1 was not updated. Nothing was written with wrangler, and
the catalog was not reseeded.

## What was fetched

| Fetch | Pages | Failed |
|---|---|---|
| Product index `pageid=1..61` | 61 | 0 |
| Category listings (66 categories, including their own pagination) | 146 | 0 |
| Product pages opened for parameter tables (radar, gyroscope, fingerprint, photoelectric) | 77 | 0 |

543 unique product-page ids. One page is one catalog module. A family listing
such as VRB4805/12/24 stays one row; the other tokens are `spec.variants`.
532 rows were appended to `crates/cog-market/catalog/catalog.json` in [WeftOS/weftos-cogs](https://github.com/WeftOS/weftos-cogs/blob/v0.1.4/crates/cog-market/catalog/catalog.json).
11 pages were skipped because the model is already in the catalog (below).

New rows by assigned category. A page that sits in a parent and a leaf is
counted once, under the leaf. `Products` means the page was only on the
unfiltered index (the brick-size nav links for those six have no `cate=`).

| Category | New rows |
|---|---|
| Radar Module | 60 |
| DC DC 1W | 34 |
| WiFi Router Module | 29 |
| DC DC 6W | 24 |
| DC DC 10W | 22 |
| 300W brick power module | 19 |
| DC DC 2W | 16 |
| AC DC 10W | 15 |
| DC DC 3W | 15 |
| AC DC 3W | 14 |
| AC DC 5W | 12 |
| DC DC 20W | 12 |
| DC DC 30W | 11 |
| AC DC 15W | 11 |
| AC DC 30W | 10 |
| DC DC 5W | 10 |
| Fingerprint recognition module | 10 |
| AC DC 20W | 9 |
| Bluetooth Module | 9 |
| half-brick power module | 9 |
| AC DC 1000W | 9 |
| AC DC 1200W | 9 |
| AC DC 1500W | 9 |
| AC DC 2000W | 9 |
| AC DC 3000W | 9 |
| AC DC 4000W | 9 |
| AC DC 5000W | 9 |
| Antenna | 9 |
| Switching Power Supply | 9 |
| 150W brick power module | 8 |
| AC DC 40W | 6 |
| IOT WiFi Module | 6 |
| DC DC 12W | 6 |
| Products | 6 |
| DC DC 15W | 5 |
| Lora Module | 4 |
| DC DC K78XX Series | 4 |
| AC DC 2W | 4 |
| DC DC Power Module | 4 |
| 400W brick power module | 4 |
| 350W brick power module | 4 |
| 100W brick power module | 4 |
| DC DC 40W | 3 |
| DC DC 25W | 3 |
| Sensor Module | 3 |
| Smart Device and Accessories | 3 |
| Gyroscope Module | 3 |
| AC DC 200W | 3 |
| 200W brick power module | 3 |
| electric motor control module | 2 |
| AC DC 22W | 2 |
| Power Fiter Module | 2 |
| Full brick power module | 2 |
| Interface power module | 1 |
| DC DC 50W | 1 |
| AC DC 50W | 1 |
| AC DC 60W | 1 |
| RS485 Series | 1 |
| 75W brick power module | 1 |

Kind is `board` for 455 rows (power, wifi, bluetooth, LoRa, antenna, router,
motor-control, heatsink) and `sensor` for 77 (radar, gyro, fingerprint,
photoelectric, and the SW01S presence switch). No display, tool, or actuator
rows. These six category pages returned no products: AC DC 100W, 120W, 150W,
220W, 250W, 300W.

## Id rule

Lowercase `[a-z0-9-]+`. The id is `hlk-` plus the first model token on the
page (`HLK-LD1040C` → `hlk-ld1040c`, `LD2417` → `hlk-ld2417`,
`VRB4805YMD-20WR3` → `hlk-vrb4805ymd-20wr3`). A test-kit / testboard /
development-kit page gets `-test-kit`. A title with no model token, or a
second page whose model token was already used, is `hlk-p` plus the numeric
page id (`hlk-p1603` is the 4G antenna with no model token).

`hash` is `wh_` plus the first 16 hex digits of SHA-256 of `module:<id>`.
`hlk-ld2450` is still `wh_a2305f6c35cedba3`.

## Already present

These catalog ids were not edited. Pages that are clearly the same model were
skipped, including test-kit pages of that model.

| Catalog id | Skipped hlktech.net pages |
|---|---|
| `hlk-ld2410c` | 1095 module, 1184 test kit |
| `hlk-ld2450` | 1157 module, 1182 test kit |
| `hlk-ld6002` | 1180 module, 1179 test kit |
| `hlk-ld6004` | 1391 module, 1392 test kit |
| `hlk-as201-module` | 1383 and 1396 modules, 1401 test board |

Also left as they were, and not listed as their own product page on this
index: `hlk-ld1115h`, `hlk-ld1125h-24g`, `hlk-ld6002b`, chip `as201-imu`,
chip `hi-link-hlk-ld101v`.

AS201-6 and AS201-9 are different tokens from AS201, so they were added
(`hlk-as201-6-test-kit`, `hlk-as201-9`, `hlk-as201-9-test-kit`). LD6002C and
LD6002H were added; they are not LD6002. HLK-LD1040 and HLK-LD1040C were
added (`hlk-ld1040`, `hlk-ld1040c`, plus `-test-kit` pages). Both product
pages print the same 10.525 GHz parameter table; that is the vendor HTML,
not a conclusion that the modules are identical.

No new id collided with an existing module or chip id. Twenty-three later
pages reused a model token already taken by an earlier new row and were
stored as `hlk-p1147`, `hlk-p1174`, `hlk-p1287`, `hlk-p1288`, `hlk-p1316`,
`hlk-p1317`, `hlk-p1334`, `hlk-p1420`, `hlk-p1522`, `hlk-p1558`, `hlk-p1567`,
`hlk-p1568`, `hlk-p1570`, `hlk-p1571`, `hlk-p1572`, `hlk-p1573`, `hlk-p1574`,
`hlk-p1575`, `hlk-p1576`, `hlk-p1577`, `hlk-p1578`, `hlk-p1579`, `hlk-p1580`.

## What was not guessed

Power, wifi, bluetooth, LoRa, antenna, and other long-tail rows have the
listing title, price, category, and URL only. No electrical ratings were
invented for them.

Twenty-two sensor pages had no parameter table in the HTML (the numbers are
in pictures, or the page is only a title). No size, range, or voltage was
read off an image. Those ids: `hlk-ld012-5g`, `hlk-ld2410`, `hlk-ld2410b`,
`hlk-ld2410b-test-kit`, `hlk-ld2411`, `hlk-ld2411s`, `hlk-zw111`,
`hlk-zw111-test-kit`, `hlk-zw101`, `hlk-zw101-test-kit`, `hlk-ld2401`,
`hlk-sx670`, `hlk-sx671`, `hlk-sx672`, `hlk-ld021`, `hlk-sw01s`,
`hlk-ld2417`, `hlk-ld2417-test-kit`, `hlk-ld6003b`, `hlk-ld6003b-test-kit`,
`hlk-ld6003d`, `hlk-ld6003d-test-kit`.

On the HLK-LD8001H page the beam-width max cells are the words "bpm" and
"Min". Those were not copied as limits. A stray min cell of "1" on the
vertical-direction row is still in `spec` because that digit is what the
table prints.

Chip names printed next to a module (IPQ5018, MT7981B, and the rest) stayed
in the title. No chip rows were added. `cogs` is empty. `photo` is empty.
Datasheet URLs are the 51 `drive.google.com` folders linked from product
pages; the folders and any PDFs were not downloaded. The site-wide download
center was not treated as a datasheet. Minimum order on every card is 1
piece. Heatsink pages are `kind: board` because the catalog has no accessory
kind. PCB silk 90-00074 was not added.
