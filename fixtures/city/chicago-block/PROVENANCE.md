# Chicago Block Fixture — Provenance

## Chosen block

**Printers Row, Chicago Loop** — bounded by S Federal St (west), S Dearborn
St (east), W Polk St (south), W Harrison St (north).

This is a *substitution* for the block originally proposed in the task
(S Dearborn / W Jackson / S Federal / W Van Buren, containing the Monadnock
Building). That block was queried first and found to contain only a single
building footprint (the Monadnock Building itself) once buildings belonging
to neighboring blocks were excluded — see "Why the Monadnock block was
rejected" below. The task explicitly permitted falling back to "any single
dense Loop block with 4+ buildings of varied heights," so this Printers Row
block was used instead. It is a real, single, street-bounded Chicago block
containing four separate building footprints with real height/level data.

### Why the Monadnock block was rejected

Overpass was first queried for `building=*` ways/relations in a bounding box
around S Dearborn / W Jackson / S Federal / W Van Buren
(`41.87680,-87.63000,41.87830,-87.62900`, chosen generously around the
block). That query returned 17 building features, but inspecting each
feature's node coordinates against the actual street centerlines (also
queried from Overpass, see below) showed that only the Monadnock Building's
footprint (way `73671128`) lies fully inside the block; every other returned
feature — Kluczynski Federal Building, Old Colony Building, Fisher Building,
The Standard Club, Union League Club, 318 South Federal, 400 Dearborn, and
several transit-shelter/roof micro-features — belongs to one of the four
adjacent blocks and was only pulled in because a corner or edge node fell
inside the query bbox. Chicago's S Federal St and S Dearborn St run only
~36–40 m apart at this point in the Loop, so the Federal/Jackson/Dearborn/
Van Buren block is a narrow single-building parcel (the Monadnock Building
spans its entire Federal St frontage from Jackson to Van Buren), not a block
with multiple separate buildings. Street centerlines were confirmed with a
separate Overpass query for `highway` ways named "South Federal Street",
"South Dearborn Street", "West Jackson Boulevard", and "West Van Buren
Street" in the same area; that query's raw responses are not retained (only
used to disambiguate block boundaries), but the derived centerline
longitudes/latitudes are reproducible by rerunning the same query. A second
candidate (the LaSalle–Federal block containing the Chicago Board of Trade)
and a third (the Clark–Dearborn/Adams–Jackson "Federal Plaza" block
containing the Kluczynski Federal Building) were also checked and rejected
for the same reason: each block in this part of the Loop is dominated by one
or two large single-parcel buildings, not four-plus separate structures.

## Endpoint and query

- **Endpoint:** `https://overpass-api.de/api/interpreter` (primary; no
  fallback mirror was needed — the query succeeded on retry after one
  transient 504 on an earlier, unrelated exploratory query against the same
  endpoint).
- **Retrieval timestamp (UTC):** `2026-07-29T20:58:36Z` (Overpass server's
  `osm3s.timestamp_osm_base`, i.e. the OSM database snapshot time used to
  answer the query; wall-clock fetch time was `2026-07-29T21:03:20Z`).
- **Bounding box used:** `41.87229,-87.62963,41.87453,-87.62919`
  (south lat, west lon, north lat, east lon). This is ~249 m north–south by
  ~36 m east–west. The east–west dimension is well under the "100–200 m on a
  side" guidance because S Federal St and S Dearborn St are unusually close
  together in this part of the Loop (see rejection note above); the
  north–south dimension (Polk to Harrison) is a full, real Chicago block
  length and modestly exceeds 200 m. Both deviations reflect the real street
  grid, not an error in bbox construction.

Exact query text (also saved as `raw/query.overpassql`):

```
[out:json][timeout:90];
(
  way["building"](41.87229,-87.62963,41.87453,-87.62919);
  relation["building"](41.87229,-87.62963,41.87453,-87.62919);
);
out body;
>;
out skel qt;
```

The raw Overpass JSON response is saved at `raw/overpass_raw.json`.

## Feature summary

4 building features, all OSM ways (no relations/multipolygons returned, so
no inner-ring holes were skipped: 0 counted).

| OSM way ID | Name | building tag | height (m) | levels |
|---|---|---|---|---|
| 148517232 | Transportation Building | apartments | — | 22 |
| 148261311 | New Franklin Building | apartments | — | 13 |
| 148261329 | Rowe Building | apartments | — | 9 |
| 843220032 | RESERV | retail | — | — |

None of the four buildings carry an OSM `height` tag in this data; three
carry `building:levels`. Per task instructions, no height was synthesized
from levels — `levels` is left in the properties for the city compiler's own
fallback logic. All addresses are on S Dearborn St (720–736, 714, 600, and
744 respectively); RESERV and Transportation Building's street-address
housenumbers (744 and 600) are not literally within the 700-Dearborn
address range implied by the block, which is a known real-world OSM
addressing quirk for large/older Chicago buildings (their footprints,
independently confirmed by node coordinates, are unambiguously inside the
chosen bbox).

## Sanity checks performed

- Every polygon ring closed (first coordinate equals last): **pass**, all 4
  features.
- Feature count ≥ 4: **pass** (4).
- At least one feature with height or levels ≥ 10: **pass** (Transportation
  Building, 22 levels; New Franklin Building, 13 levels).
- All coordinates inside the query bounding box
  `41.87229,-87.62963,41.87453,-87.62919`: **pass**, verified
  programmatically against every ring vertex of every feature.

## Data quality caveats

- Only 4 buildings met the strict "fully inside this single block" filter —
  right at the task's stated minimum. Two nearby, taller buildings (Donohue
  Building and Printers Square Condominium) were investigated and confirmed
  to sit just across Dearborn St and Federal St respectively (outside this
  block) rather than being edge-clipping artifacts.
- No building in this block carries a meters `height` tag; all height
  information is `building:levels` only, except RESERV which has neither.
  The compiler consuming this fixture must exercise its levels-based
  fallback path for every feature here (and its no-data path for RESERV) —
  this fixture does not exercise the `height`-tag-present code path at all.
- The block's real-world footprint (~36 m × ~249 m) is a narrow rectangle,
  not a compact square, because of Chicago's tight Federal/Dearborn street
  spacing in this part of the Loop. If the compiler or its tests assume
  roughly square/compact city blocks, this fixture will not represent that
  assumption well.
- `RESERV` is tagged `building=retail` with `amenity=events_venue`; it has
  no height or levels data at all, so it contributes only footprint/name/
  type information to the fixture.

## License

Data © OpenStreetMap contributors, available under the Open Database
License (ODbL) 1.0. See https://www.openstreetmap.org/copyright.

## File checksum

```
sha256(chicago-block.geojson) = 363583a00d22307fe6146726ba57b324e2000c6ab07e508abb1744c8cfe29200
```
