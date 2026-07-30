#!/usr/bin/env python3
"""Convert an Overpass API JSON response (ways with building=* tags, plus
their referenced nodes) into a GeoJSON FeatureCollection of Polygon
features for the Chicago block fixture.

Input:  overpass_raw.json  (Overpass [out:json] response containing
        `way` elements with tags and `nodes` refs, and `node` elements
        with lat/lon, as produced by `out body; >; out skel qt;`)
Output: chicago-block.geojson (GeoJSON FeatureCollection, sorted keys,
        trailing newline)

Only way-based building polygons are handled. If relation-based
multipolygon buildings appear, their outer ring(s) are assembled from
member ways and inner rings (holes) are counted but not emitted as
geometry (per task instructions: skip relations' inner holes but count
them).
"""
import json
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
FIXTURE_DIR = HERE.parent
RAW_PATH = FIXTURE_DIR / "raw" / "overpass_raw.json"
OUT_PATH = FIXTURE_DIR / "chicago-block.geojson"


def load_overpass(path):
    with open(path) as f:
        return json.load(f)


def build_node_index(elements):
    nodes = {}
    for el in elements:
        if el["type"] == "node":
            nodes[el["id"]] = (el["lon"], el["lat"])
    return nodes


def parse_height(tags):
    """Parse the 'height' tag (meters) if present and numeric-ish."""
    raw = tags.get("height")
    if raw is None:
        return None
    # OSM height values are usually plain meters, sometimes with a unit
    # suffix like "166 m" or "45'". Only accept plain/decimal meter values.
    cleaned = raw.strip()
    try:
        return float(cleaned)
    except ValueError:
        # strip trailing " m" if present
        if cleaned.endswith("m"):
            try:
                return float(cleaned[:-1].strip())
            except ValueError:
                return None
        return None


def parse_levels(tags):
    raw = tags.get("building:levels")
    if raw is None:
        return None
    try:
        return float(raw) if "." in raw else int(raw)
    except ValueError:
        return None


def ring_from_way(way, node_index):
    coords = []
    for ref in way["nodes"]:
        if ref not in node_index:
            raise ValueError(f"way {way['id']} references missing node {ref}")
        coords.append(list(node_index[ref]))
    return coords


def main():
    data = load_overpass(RAW_PATH)
    elements = data["elements"]
    node_index = build_node_index(elements)

    ways = [e for e in elements if e["type"] == "way"]
    relations = [e for e in elements if e["type"] == "relation"]

    features = []
    skipped_inner_holes = 0
    anomalies = []

    # Track way ids that are members of relations, so we know which
    # standalone building ways (if any) are actually part of a relation
    # and should not be double counted. In this dataset all buildings are
    # plain ways (verified: 0 relations returned), but the logic below
    # handles relations defensively in case future re-fetches include them.
    relation_member_way_ids = set()
    for rel in relations:
        tags = rel.get("tags", {})
        if "building" not in tags:
            continue
        outer_ways = []
        inner_count = 0
        for member in rel.get("members", []):
            if member["type"] != "way":
                continue
            relation_member_way_ids.add(member["ref"])
            if member.get("role") == "inner":
                inner_count += 1
            else:
                outer_ways.append(member["ref"])
        skipped_inner_holes += inner_count

        # Assemble outer ring(s) from member ways sharing endpoints.
        member_way_lookup = {w["id"]: w for w in ways if w["id"] in outer_ways}
        segments = []
        for wid in outer_ways:
            w = member_way_lookup.get(wid)
            if w is None:
                anomalies.append(
                    f"relation {rel['id']}: outer member way {wid} not found in fetched ways"
                )
                continue
            segments.append(ring_from_way(w, node_index))

        if not segments:
            anomalies.append(f"relation {rel['id']}: no outer ways resolved, skipping")
            continue

        # Naive stitch: assume a single outer way forms a closed ring, or
        # concatenate segments in order (sufficient for this fixture's
        # simple downtown block; flagged as anomaly if it doesn't close).
        ring = segments[0]
        for seg in segments[1:]:
            ring.extend(seg)

        if ring[0] != ring[-1]:
            anomalies.append(
                f"relation {rel['id']}: assembled outer ring not closed "
                f"(first={ring[0]}, last={ring[-1]})"
            )
            ring.append(ring[0])

        props = {
            "building": tags.get("building"),
            "name": tags.get("name"),
            "height": parse_height(tags),
            "levels": parse_levels(tags),
        }
        # Drop keys with None values except keep 'building' required.
        props = {k: v for k, v in props.items() if v is not None or k == "building"}

        features.append(
            {
                "type": "Feature",
                "properties": props,
                "geometry": {"type": "Polygon", "coordinates": [ring]},
                "id": f"relation/{rel['id']}",
            }
        )

    for way in ways:
        tags = way.get("tags", {})
        if "building" not in tags:
            continue
        if way["id"] in relation_member_way_ids:
            continue  # already emitted as part of a relation above

        nodes = way.get("nodes", [])
        if not nodes or nodes[0] != nodes[-1]:
            anomalies.append(
                f"way {way['id']} ({tags.get('name', 'unnamed')}): ring not closed "
                f"(first node={nodes[0] if nodes else None}, last={nodes[-1] if nodes else None})"
            )
            continue

        ring = ring_from_way(way, node_index)
        if ring[0] != ring[-1]:
            anomalies.append(f"way {way['id']}: coordinate ring not closed after lookup")
            continue

        props = {
            "building": tags.get("building"),
        }
        if tags.get("name"):
            props["name"] = tags["name"]
        height = parse_height(tags)
        if height is not None:
            props["height"] = height
        levels = parse_levels(tags)
        # Only include levels if height absent OR always include levels when present?
        # Task: "Where only levels exist, do NOT synthesize a height property
        # — leave levels for the compiler's fallback." This implies levels
        # should be kept whenever present (whether or not height is present),
        # so the compiler has the raw source data.
        if levels is not None:
            props["levels"] = levels

        features.append(
            {
                "type": "Feature",
                "properties": props,
                "geometry": {"type": "Polygon", "coordinates": [ring]},
                "id": f"way/{way['id']}",
            }
        )

    fc = {"type": "FeatureCollection", "features": features}

    OUT_PATH.parent.mkdir(parents=True, exist_ok=True)
    with open(OUT_PATH, "w") as f:
        json.dump(fc, f, sort_keys=True, indent=2)
        f.write("\n")

    print(f"Wrote {len(features)} features to {OUT_PATH}")
    print(f"Skipped inner holes (counted, not emitted): {skipped_inner_holes}")
    if anomalies:
        print("ANOMALIES:")
        for a in anomalies:
            print(" -", a)
    else:
        print("No anomalies.")


if __name__ == "__main__":
    main()
