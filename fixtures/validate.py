#!/usr/bin/env python3
"""Dependency-free semantic checks for the controlled Phase A fixtures."""

import json
import hashlib
import math
import sys
from pathlib import Path


ROOT = Path(__file__).resolve().parent
SCHEMA_PATH = ROOT / "fixture.schema.json"
ASSETS_DIR = ROOT / "assets"
ASSET_SCHEMA_PATH = ASSETS_DIR / "asset.schema.json"
FIXTURES = (
    (ROOT / "s0-free-field" / "fixture.json", "s0-free-field-100m-approach", "S0"),
    (ROOT / "s3-corner" / "fixture.json", "s3-masonry-building-corner", "S3"),
    (ROOT / "s6a-four-sources" / "fixture.json", "s6a-four-sources-one-moving", "S6A"),
)
SCHEMA_ID_V1 = "https://fightbox.dev/schema/fixture-1.json"
SCHEMA_ID_S6A = "https://fightbox.dev/schema/fixture-s6a-1.json"
EPSILON = 1e-9
ANALYTIC_EPSILON = 1e-6
AZIMUTH_EPSILON_DEGREES = 1e-5
STEM_ORDER = ["direct", "path", "reflections"]
ASSET_VALIDATOR = ASSETS_DIR / "validate.py"
WARNINGS = []
S3_SOURCE = [-4, 6, 1.5]
S3_LISTENER = [6, -4, 1.5]
S3_PROBE_MINIMUM = [-8.75, -8.75, 0.5]
S3_PROBE_MAXIMUM = [8.25, 8.25, 2.5]
S3_PROBE_SPACING = 1.0
S3_VERTICES = [
    [0, 0, 0],
    [10, 0, 0],
    [10, 0, 6],
    [0, 0, 6],
    [0, 10, 0],
    [0, 10, 6],
    [-9, -9, 0],
    [9, -9, 0],
    [9, 9, 0],
    [-9, 9, 0],
]
S3_TRIANGLE_INDICES = [
    [0, 1, 2],
    [0, 2, 3],
    [2, 1, 0],
    [3, 2, 0],
    [0, 4, 5],
    [0, 5, 3],
    [5, 4, 0],
    [3, 5, 0],
    [6, 7, 8],
    [6, 8, 9],
]
S3_ANALYTIC_EDGE = [0, 0, 1.5]
S3_ANALYTIC_VECTOR = [-6, 4, 0]
S3_ANALYTIC_AZIMUTH = 303.690068


def reject_nonfinite(value):
    raise ValueError("non-finite JSON number: " + value)


def load_json(path):
    with path.open(encoding="utf-8") as handle:
        return json.load(handle, parse_constant=reject_nonfinite)


def finite_numbers(value, path, errors):
    if isinstance(value, bool):
        return
    if isinstance(value, (int, float)):
        if not math.isfinite(value):
            errors.append(f"{path}: non-finite number")
    elif isinstance(value, list):
        for index, item in enumerate(value):
            finite_numbers(item, f"{path}[{index}]", errors)
    elif isinstance(value, dict):
        for key, item in value.items():
            finite_numbers(item, f"{path}.{key}", errors)


def subtract(left, right):
    return [left[index] - right[index] for index in range(3)]


def cross(left, right):
    return [
        left[1] * right[2] - left[2] * right[1],
        left[2] * right[0] - left[0] * right[2],
        left[0] * right[1] - left[1] * right[0],
    ]


def dot(left, right):
    return sum(a * b for a, b in zip(left, right))


def norm(vector):
    return math.sqrt(dot(vector, vector))


def triangle_vertices(vertices, triangle):
    return [vertices[index] for index in triangle["indices"]]


def point_in_triangle_xy(point, triangle):
    a, b, c = ([vertex[0], vertex[1]] for vertex in triangle)
    denominator = ((b[1] - c[1]) * (a[0] - c[0]) +
                   (c[0] - b[0]) * (a[1] - c[1]))
    if abs(denominator) <= EPSILON:
        return False
    u = ((b[1] - c[1]) * (point[0] - c[0]) +
         (c[0] - b[0]) * (point[1] - c[1])) / denominator
    v = ((c[1] - a[1]) * (point[0] - c[0]) +
         (a[0] - c[0]) * (point[1] - c[1])) / denominator
    w = 1.0 - u - v
    return u >= -EPSILON and v >= -EPSILON and w >= -EPSILON


def segment_intersects_triangle(origin, destination, triangle):
    """Möller-Trumbore segment test; edge hits count as occlusion."""
    a, b, c = triangle
    direction = subtract(destination, origin)
    edge_one = subtract(b, a)
    edge_two = subtract(c, a)
    pvec = cross(direction, edge_two)
    determinant = dot(edge_one, pvec)
    if abs(determinant) <= EPSILON:
        return False
    inverse = 1.0 / determinant
    tvec = subtract(origin, a)
    u = dot(tvec, pvec) * inverse
    if u < -EPSILON or u > 1.0 + EPSILON:
        return False
    qvec = cross(tvec, edge_one)
    v = dot(direction, qvec) * inverse
    if v < -EPSILON or u + v > 1.0 + EPSILON:
        return False
    distance = dot(edge_two, qvec) * inverse
    return -EPSILON <= distance <= 1.0 + EPSILON


def segment_plane_intersection(origin, destination, axis, plane_coordinate):
    """Return an interior segment/axis-plane intersection, or ``None``."""
    delta = destination[axis] - origin[axis]
    if abs(delta) <= EPSILON:
        return None
    parameter = (plane_coordinate - origin[axis]) / delta
    if parameter <= EPSILON or parameter >= 1.0 - EPSILON:
        return None
    return [
        origin[index] + parameter * (destination[index] - origin[index])
        for index in range(3)
    ]


def uniform_probe_axis(minimum, maximum, spacing):
    """Mirror Steam Audio 4.8.1's centered UNIFORMFLOOR axis generation."""
    span = maximum - minimum
    count = math.floor(span / spacing) + 1
    residual = (span - (count - 1) * spacing) / 2.0
    return [minimum + residual + index * spacing for index in range(count)]


def is_exact_order(value, expected):
    return isinstance(value, list) and value == expected and len(value) == len(set(value))


def check_mesh(fixture, label, errors):
    geometry = fixture.get("geometry", {})
    vertices = geometry.get("vertices_m")
    triangles = geometry.get("triangles")
    materials = geometry.get("materials")
    if not isinstance(vertices, list) or not isinstance(triangles, list) or not isinstance(materials, dict):
        errors.append(f"{label}: geometry must contain vertices_m, triangles, and materials")
        return None
    valid_triangles = []
    for triangle_index, triangle in enumerate(triangles):
        prefix = f"{label}: geometry.triangles[{triangle_index}]"
        if not isinstance(triangle, dict):
            errors.append(f"{prefix}: must be an object")
            continue
        indices = triangle.get("indices")
        material = triangle.get("material")
        if not isinstance(indices, list) or len(indices) != 3 or any(
            isinstance(index, bool) or not isinstance(index, int) or index < 0 or index >= len(vertices)
            for index in indices
        ):
            errors.append(f"{prefix}: invalid vertex indices")
            continue
        if material not in materials:
            errors.append(f"{prefix}: unknown material {material!r}")
            continue
        try:
            points = triangle_vertices(vertices, triangle)
            if norm(cross(subtract(points[1], points[0]), subtract(points[2], points[0]))) <= EPSILON:
                errors.append(f"{prefix}: degenerate triangle")
                continue
        except (IndexError, TypeError):
            errors.append(f"{prefix}: invalid vertex vector")
            continue
        valid_triangles.append(points)
    return valid_triangles


def resolve_asset_descriptor(asset_id, label, errors):
    """Resolve and structurally validate the asset descriptor bound by asset_id.

    The fixture no longer duplicates an inline ``asset_calibration``. Instead it
    binds a source to the descriptor under ``fixtures/assets`` via ``asset_id``.
    This resolves that descriptor, validates it through the asset validator, and
    returns the parsed record (or ``None`` on failure). The descriptor supplies
    the decoded program RMS that the scene-owned source drive (ADR 0002) scales.
    """
    descriptor_path = ASSETS_DIR / f"{asset_id}.json"
    if not descriptor_path.is_file():
        errors.append(f"{label}: asset_id {asset_id!r} has no descriptor at {descriptor_path}")
        return None
    try:
        descriptor = load_json(descriptor_path)
    except (OSError, ValueError, json.JSONDecodeError) as error:
        errors.append(f"{label}: asset descriptor {asset_id!r}: {error}")
        return None
    finite_numbers(descriptor, f"{asset_id}.json", errors)
    if not isinstance(descriptor, dict) or descriptor.get("asset_id") != asset_id:
        errors.append(
            f"{label}: asset descriptor {asset_id!r} must carry matching asset_id"
        )
        return None
    if descriptor.get("schema_version") != "fightbox.asset-descriptor.v1":
        errors.append(
            f"{label}: asset descriptor {asset_id!r} schema_version must be "
            "fightbox.asset-descriptor.v1"
        )
    check_wav_asset_descriptor(descriptor, descriptor_path, errors)
    return descriptor


def check_wav_asset_descriptor(descriptor, descriptor_path, errors):
    """Validate the additive file-backed WAV descriptor contract.

    Large recordings may intentionally live outside git. A missing referenced
    file is therefore a warning, while a present file must match its mandatory
    SHA-256 provenance hash. Audio format validation remains the CLI loader's
    job so this dependency-free fixture check does not grow a second decoder.
    """
    if not isinstance(descriptor, dict) or descriptor.get("kind") != "wav":
        return
    label = descriptor_path.relative_to(ROOT).as_posix()
    generator = descriptor.get("generator")
    if not isinstance(generator, dict):
        errors.append(f"{label}: generator must be an object")
        return
    if set(generator) != {"wav"}:
        errors.append(f"{label}: wav kind requires exactly generator.wav and no module")
        return
    block = generator.get("wav")
    if not isinstance(block, dict):
        errors.append(f"{label}: generator.wav must be an object")
        return
    required = {"path", "sha256", "start_frame", "loop"}
    missing = sorted(required - set(block))
    unknown = sorted(set(block) - required)
    if missing:
        errors.append(f"{label}: generator.wav missing fields: {', '.join(missing)}")
    if unknown:
        errors.append(f"{label}: generator.wav unknown fields: {', '.join(unknown)}")
    if missing or unknown:
        return
    path_value = block["path"]
    expected_hash = block["sha256"]
    start_frame = block["start_frame"]
    loop = block["loop"]
    if not isinstance(path_value, str) or not path_value:
        errors.append(f"{label}: generator.wav.path must be a non-empty string")
        return
    if (
        not isinstance(expected_hash, str)
        or len(expected_hash) != 64
        or any(character not in "0123456789abcdef" for character in expected_hash)
    ):
        errors.append(f"{label}: generator.wav.sha256 must be 64 lowercase hex characters")
    if isinstance(start_frame, bool) or not isinstance(start_frame, int) or start_frame < 0:
        errors.append(f"{label}: generator.wav.start_frame must be a non-negative integer")
    if not isinstance(loop, bool):
        errors.append(f"{label}: generator.wav.loop must be boolean")
    if descriptor.get("channels") != 1:
        errors.append(f"{label}: wav assets must declare channels=1")
    if descriptor.get("sample_rate_hz") != 48000:
        errors.append(f"{label}: wav assets must declare sample_rate_hz=48000")

    asset_path = Path(path_value)
    if not asset_path.is_absolute():
        asset_path = ROOT.parent / asset_path
    if not asset_path.is_file():
        warning = f"{label}: WAV file is not present: {asset_path}"
        if warning not in WARNINGS:
            WARNINGS.append(warning)
        return
    try:
        actual_hash = hashlib.sha256(asset_path.read_bytes()).hexdigest()
    except OSError as error:
        errors.append(f"{label}: cannot hash WAV file {asset_path}: {error}")
        return
    if isinstance(expected_hash, str) and actual_hash != expected_hash:
        errors.append(
            f"{label}: WAV sha256 mismatch for {asset_path}: "
            f"descriptor {expected_hash}, file {actual_hash}"
        )


def check_asset_coherence(fixture, descriptor, label, errors):
    """Ensure the bound asset's delivered target program RMS is coherent with
    the fixture/source contract.

    Coherence here is narrow and honest: the descriptor must declare a finite
    ``target_rms_dbfs`` strictly below 0 dBFS, because that is the decoded
    program RMS the scene-owned source drive operates on. The fixture's source
    declares a scene level (``SplAtOneMeter``); under ADR 0002 the drive maps
    that declared SPL to PCM through one gain chain, so a ``SplAtOneMeter``
    source must be bound to an asset with a real, finite deliverable program
    level — a null or non-finite value would make the one gain chain undefined.
    This check deliberately does not compute the SPL→PCM drive itself; that is
    the runtime/evidence concern, not the fixture contract.
    """
    if descriptor is None:
        return
    target = descriptor.get("target_rms_dbfs")
    if isinstance(target, bool) or not isinstance(target, (int, float)):
        errors.append(
            f"{label}: bound asset {descriptor.get('asset_id')!r} target_rms_dbfs "
            "must be a finite JSON number"
        )
        return
    if not math.isfinite(target) or target >= 0.0:
        errors.append(
            f"{label}: bound asset {descriptor.get('asset_id')!r} target_rms_dbfs "
            f"{target} must be finite and strictly below 0 dBFS"
        )
        return
    # The fixture source declares a scene level (currently SplAtOneMeter). Under
    # ADR 0002 the one gain chain needs the bound asset's deliverable program
    # RMS, which the finite sub-0 check above just confirmed is present. The
    # fixture contract records the declared SPL and the asset binding; it does
    # not compute the SPL→PCM drive, which is the runtime/evidence concern.
    if isinstance(fixture, dict):
        source = fixture.get("source", {})
        mode = source.get("reference_level", {}).get("mode")
        if mode == "SplAtOneMeter":
            db_spl = source.get("reference_level", {}).get("db_spl")
            if isinstance(db_spl, bool) or not isinstance(db_spl, (int, float)) or not math.isfinite(db_spl):
                errors.append(
                    f"{label}: SplAtOneMeter source db_spl must be finite to pair "
                    "with the bound program RMS"
                )


def check_s6a(fixture, label, errors):
    sources = fixture.get("sources")
    simulation = fixture.get("simulation")
    probe_volume = simulation.get("probe_volume") if isinstance(simulation, dict) else None
    if not isinstance(sources, list) or len(sources) != 4:
        errors.append(f"{label}: S6A requires exactly 4 sources")
        return

    # Check for exactly one trajectory source
    trajectory_count = 0
    for source_index, source in enumerate(sources):
        if not isinstance(source, dict):
            continue
        has_position = "position_m" in source
        has_trajectory = "trajectory" in source
        if has_trajectory:
            trajectory_count += 1

    if trajectory_count != 1:
        errors.append(f"{label}: S6A must have exactly one trajectory source, found {trajectory_count}")

    # Extract probe bounds for waypoint checks
    probe_min = probe_volume.get("min_m") if isinstance(probe_volume, dict) else None
    probe_max = probe_volume.get("max_m") if isinstance(probe_volume, dict) else None

    source_ids = set()
    asset_ids = set()

    for source_index, source in enumerate(sources):
        if not isinstance(source, dict):
            errors.append(f"{label}: sources[{source_index}] must be an object")
            continue

        prefix = f"{label}: sources[{source_index}]"
        source_id = source.get("id")
        asset_id = source.get("asset_id")
        reference_level = source.get("reference_level")
        position_m = source.get("position_m")
        trajectory = source.get("trajectory")

        # Check source_id uniqueness
        if isinstance(source_id, str):
            if source_id in source_ids:
                errors.append(f"{prefix}: source id {source_id!r} is not unique")
            source_ids.add(source_id)
        else:
            errors.append(f"{prefix}: id must be a non-empty string")

        # Check asset_id
        if isinstance(asset_id, str) and asset_id:
            asset_ids.add(asset_id)
            descriptor = resolve_asset_descriptor(asset_id, prefix, errors)
            check_asset_coherence(source, descriptor, prefix, errors)
        else:
            errors.append(f"{prefix}: asset_id is required and must be a non-empty string")

        # Check reference_level mode and db_spl
        if isinstance(reference_level, dict):
            if reference_level.get("mode") != "SplAtOneMeter":
                errors.append(f"{prefix}: reference_level.mode must be SplAtOneMeter")
            db_spl = reference_level.get("db_spl")
            if isinstance(db_spl, bool) or not isinstance(db_spl, (int, float)):
                errors.append(f"{prefix}: reference_level.db_spl must be a finite number")
        else:
            errors.append(f"{prefix}: reference_level is required")

        # Check position_m or trajectory
        if isinstance(position_m, list) and len(position_m) == 3:
            # Static source: verify position is inside probe bounds
            if all(isinstance(x, (int, float)) and not isinstance(x, bool) for x in position_m):
                if not all(math.isfinite(x) for x in position_m):
                    errors.append(f"{prefix}: position_m contains non-finite coordinates")
                elif probe_min and probe_max:
                    for axis in range(3):
                        if position_m[axis] < probe_min[axis] - EPSILON or position_m[axis] > probe_max[axis] + EPSILON:
                            errors.append(f"{prefix}: position_m[{axis}] is outside probe bounds")
            else:
                errors.append(f"{prefix}: position_m must be three finite numbers")
        elif isinstance(trajectory, dict):
            # Trajectory source: verify waypoints and speeds
            waypoints_m = trajectory.get("waypoints_m")
            speed_mps = trajectory.get("speed_mps")
            max_speed_mps = trajectory.get("max_speed_mps")

            if not isinstance(waypoints_m, list) or len(waypoints_m) < 2:
                errors.append(f"{prefix}: trajectory.waypoints_m must be an array with at least 2 vec3 items")
            else:
                for wp_index, waypoint in enumerate(waypoints_m):
                    wp_prefix = f"{prefix}.trajectory.waypoints_m[{wp_index}]"
                    if not isinstance(waypoint, list) or len(waypoint) != 3:
                        errors.append(f"{wp_prefix}: must be a vec3")
                        continue
                    if not all(isinstance(x, (int, float)) and not isinstance(x, bool) for x in waypoint):
                        errors.append(f"{wp_prefix}: must contain three numbers")
                        continue
                    if not all(math.isfinite(x) for x in waypoint):
                        errors.append(f"{wp_prefix}: contains non-finite coordinates")
                    elif probe_min and probe_max:
                        for axis in range(3):
                            if waypoint[axis] < probe_min[axis] - EPSILON or waypoint[axis] > probe_max[axis] + EPSILON:
                                errors.append(f"{wp_prefix}[{axis}] is outside probe bounds")

            if isinstance(speed_mps, bool) or not isinstance(speed_mps, (int, float)):
                errors.append(f"{prefix}: trajectory.speed_mps must be a positive number")
            elif not math.isfinite(speed_mps) or speed_mps <= 0:
                errors.append(f"{prefix}: trajectory.speed_mps must be positive and finite")

            if isinstance(max_speed_mps, bool) or not isinstance(max_speed_mps, (int, float)):
                errors.append(f"{prefix}: trajectory.max_speed_mps must be a number")
            elif not math.isfinite(max_speed_mps):
                errors.append(f"{prefix}: trajectory.max_speed_mps must be finite")
            elif max_speed_mps > 20:
                errors.append(f"{prefix}: trajectory.max_speed_mps {max_speed_mps} exceeds Brown Line authority speed cap of 20 m/s")

            # Check speed_mps <= max_speed_mps
            if (isinstance(speed_mps, (int, float)) and not isinstance(speed_mps, bool) and
                isinstance(max_speed_mps, (int, float)) and not isinstance(max_speed_mps, bool) and
                math.isfinite(speed_mps) and math.isfinite(max_speed_mps)):
                if speed_mps > max_speed_mps + EPSILON:
                    errors.append(f"{prefix}: trajectory.speed_mps {speed_mps} exceeds max_speed_mps {max_speed_mps}")
        else:
            errors.append(f"{prefix}: must have either position_m (static) or trajectory (moving)")

    # Check that exactly four distinct asset_ids exist
    if len(asset_ids) != 4:
        errors.append(f"{label}: expected 4 distinct asset_ids, found {len(asset_ids)}")


def check_contract(fixture, label, expected_id, expected_gate, errors):
    if expected_gate == "S6A":
        required = {"schema_version", "fixture_id", "gate", "coordinate_frame", "kernel", "sources", "listener", "geometry", "simulation", "expected"}
        expected_schema_version = "fightbox.fixture.s6a.v1"
    else:
        required = {"schema_version", "fixture_id", "gate", "coordinate_frame", "kernel", "source", "listener", "geometry", "simulation", "expected"}
        expected_schema_version = "fightbox.fixture.v1"

    missing = sorted(required - fixture.keys()) if isinstance(fixture, dict) else sorted(required)
    if missing:
        errors.append(f"{label}: missing required root keys {','.join(missing)}")
        return False
    if fixture["schema_version"] != expected_schema_version:
        errors.append(f"{label}: schema_version must be {expected_schema_version}")
    if fixture["fixture_id"] != expected_id:
        errors.append(f"{label}: fixture_id must be {expected_id}")
    if fixture["gate"] != expected_gate:
        errors.append(f"{label}: gate must be {expected_gate}")
    return True


def check_s0(fixture, label, errors):
    source = fixture["source"].get("position_m")
    trajectory = fixture["listener"].get("trajectory_m")
    if not isinstance(source, list) or not isinstance(trajectory, list) or len(trajectory) < 2:
        errors.append(f"{label}: S0 requires a source and at least two trajectory points")
        return
    try:
        distances = [norm(subtract(point, source)) for point in trajectory]
    except (IndexError, TypeError):
        errors.append(f"{label}: invalid S0 trajectory vectors")
        return
    if any(next_distance >= distance - EPSILON for distance, next_distance in zip(distances, distances[1:])):
        errors.append(f"{label}: S0 approach distances must be strictly decreasing")


def check_s3(fixture, label, valid_triangles, errors):
    simulation = fixture["simulation"]
    probe_volume = simulation.get("probe_volume")
    probe_generation = simulation.get("probe_generation")
    source = fixture["source"].get("position_m")
    listener = fixture["listener"].get("position_m")
    trajectory = fixture["listener"].get("trajectory_m")
    if not isinstance(probe_volume, dict) or not isinstance(probe_generation, dict):
        errors.append(f"{label}: S3 requires probe_volume and probe_generation")
        return
    height = probe_generation.get("height_m")
    if probe_generation.get("type") != "uniform_floor" or isinstance(height, bool) or not isinstance(height, (int, float)):
        errors.append(f"{label}: S3 probe generation must be uniform_floor with height_m")
    minimum = probe_volume.get("min_m")
    maximum = probe_volume.get("max_m")
    if not all(isinstance(vector, list) and len(vector) == 3 for vector in (minimum, maximum, source, listener)):
        errors.append(f"{label}: invalid S3 probe/source/listener vector")
        return
    geometry = fixture["geometry"]
    triangles = geometry.get("triangles", [])
    triangle_indices = [
        triangle.get("indices") if isinstance(triangle, dict) else None
        for triangle in triangles
    ]
    triangle_materials = [
        triangle.get("material") if isinstance(triangle, dict) else None
        for triangle in triangles
    ]
    if geometry.get("vertices_m") != S3_VERTICES:
        errors.append(
            f"{label}: vertices must exactly encode the ADR 0003 convex exterior corner"
        )
    if triangle_indices != S3_TRIANGLE_INDICES:
        errors.append(
            f"{label}: triangles must exactly encode two double-sided façades "
            "and two upward floor triangles"
        )
    if triangle_materials != ["masonry"] * 10:
        errors.append(f"{label}: the exact ten S3 triangles must all use masonry")
    if geometry.get("materials") != {
        "masonry": {
            "absorption": [0.03, 0.05, 0.07],
            "scattering": 0.1,
            "transmission": [0.0, 0.0, 0.0],
        }
    }:
        errors.append(f"{label}: S3 masonry acoustic properties must remain fixed")
    if source != S3_SOURCE or listener != S3_LISTENER:
        errors.append(
            f"{label}: source/listener must be exactly {S3_SOURCE} and {S3_LISTENER}"
        )
    if fixture["listener"].get("forward_enu") != [0, 1, 0]:
        errors.append(f"{label}: listener forward_enu must remain [0,1,0]")
    if fixture["listener"].get("up_enu") != [0, 0, 1]:
        errors.append(f"{label}: listener up_enu must remain [0,0,1]")
    if minimum != S3_PROBE_MINIMUM or maximum != S3_PROBE_MAXIMUM:
        errors.append(
            f"{label}: probe bounds must be exactly "
            f"{S3_PROBE_MINIMUM} to {S3_PROBE_MAXIMUM}"
        )
    if probe_volume.get("spacing_m") != S3_PROBE_SPACING:
        errors.append(f"{label}: S3 probe spacing must be exactly 1 m")
    if height != 1.5:
        errors.append(f"{label}: S3 uniform-floor probe height must be exactly 1.5 m")
    if any(minimum[index] >= maximum[index] for index in range(3)):
        errors.append(f"{label}: probe volume min_m must be smaller than max_m")
    if isinstance(height, (int, float)) and not isinstance(height, bool) and not (minimum[2] - EPSILON <= height <= maximum[2] + EPSILON):
        errors.append(f"{label}: probe generation height_m is outside the probe volume")
    for name, point in (("source", source), ("listener", listener)):
        if any(point[index] < minimum[index] - EPSILON or point[index] > maximum[index] + EPSILON for index in range(3)):
            errors.append(f"{label}: {name} is outside the probe volume")
    valid_trajectory = (
        isinstance(trajectory, list)
        and len(trajectory) >= 2
        and all(isinstance(point, list) and len(point) == 3 for point in trajectory)
    )
    if not valid_trajectory:
        errors.append(f"{label}: S3 listener trajectory must contain at least two vec3 points")
    else:
        if trajectory[0] != listener:
            errors.append(f"{label}: S3 trajectory must start at listener.position_m")
        for index, point in enumerate(trajectory):
            if any(
                point[axis] < minimum[axis] - EPSILON
                or point[axis] > maximum[axis] + EPSILON
                for axis in range(3)
            ):
                errors.append(
                    f"{label}: listener trajectory point {index} is outside the probe volume"
                )

    if not (
        source[0] < 0.0
        and 0.0 < source[1] < 10.0
        and listener[1] < 0.0
        and 0.0 < listener[0] < 10.0
    ):
        errors.append(
            f"{label}: source and listener must be outside adjacent convex façades"
        )

    floor_triangles = []
    for triangle in valid_triangles or []:
        if max(vertex[2] for vertex in triangle) - min(vertex[2] for vertex in triangle) <= EPSILON:
            normal = cross(subtract(triangle[1], triangle[0]), subtract(triangle[2], triangle[0]))
            if normal[2] > EPSILON:
                floor_triangles.append(triangle)
    if not floor_triangles:
        errors.append(f"{label}: no upward CCW horizontal floor triangles")
    else:
        floor_height = floor_triangles[0][0][2]
        if floor_height > minimum[2] + EPSILON:
            errors.append(f"{label}: horizontal floor is above the probe volume")
        coverage_points = [source[:2], listener[:2]] + (
            [point[:2] for point in trajectory] if valid_trajectory else []
        ) + [
            [x, y] for x in (minimum[0], maximum[0]) for y in (minimum[1], maximum[1])
        ]
        for point in coverage_points:
            if not any(point_in_triangle_xy(point, triangle) for triangle in floor_triangles):
                errors.append(f"{label}: horizontal floor does not cover {point}")

    spacing = probe_volume.get("spacing_m")
    if (
        isinstance(spacing, (int, float))
        and not isinstance(spacing, bool)
        and spacing > 0.0
    ):
        x_probes = uniform_probe_axis(minimum[0], maximum[0], spacing)
        y_probes = uniform_probe_axis(minimum[1], maximum[1], spacing)
        if len(x_probes) != 18 or len(y_probes) != 18:
            errors.append(f"{label}: offset lattice must generate 18 probes per floor axis")
        if any(abs(coordinate) <= EPSILON for coordinate in x_probes):
            errors.append(f"{label}: offset lattice places probes on the x=0 façade plane")
        if any(abs(coordinate) <= EPSILON for coordinate in y_probes):
            errors.append(f"{label}: offset lattice places probes on the y=0 façade plane")
        if not (
            min(x_probes) < 0.0 < max(x_probes)
            and min(y_probes) < 0.0 < max(y_probes)
        ):
            errors.append(f"{label}: offset lattice must cover both sides of both façades")

    corner_triangles = [
        triangle for triangle in valid_triangles or []
        if max(vertex[2] for vertex in triangle) - min(vertex[2] for vertex in triangle) > EPSILON
    ]
    if valid_triangles is not None and not any(segment_intersects_triangle(source, listener, triangle) for triangle in corner_triangles):
        errors.append(f"{label}: initial source/listener line of sight misses corner geometry")
    if valid_trajectory and any(
        segment_intersects_triangle(source, trajectory[-1], triangle)
        for triangle in corner_triangles
    ):
        errors.append(
            f"{label}: final listener trajectory point must return to direct line of sight"
        )
    north_facade_hit = segment_plane_intersection(source, listener, 0, 0.0)
    east_facade_hit = segment_plane_intersection(source, listener, 1, 0.0)
    if north_facade_hit is None or any(
        abs(actual - expected) > ANALYTIC_EPSILON
        for actual, expected in zip(north_facade_hit, [0, 2, 1.5])
    ):
        errors.append(
            f"{label}: direct line of sight must cross the north-running façade "
            "at [0,2,1.5]"
        )
    if east_facade_hit is None or any(
        abs(actual - expected) > ANALYTIC_EPSILON
        for actual, expected in zip(east_facade_hit, [2, 0, 1.5])
    ):
        errors.append(
            f"{label}: direct line of sight must cross the east-running façade "
            "at [2,0,1.5]"
        )

    analytic = fixture["expected"].get("analytic")
    if not isinstance(analytic, dict):
        errors.append(f"{label}: S3 requires expected.analytic")
    else:
        edge = analytic.get("edge_m")
        declared = analytic.get("listener_to_edge_enu")
        if not isinstance(edge, list) or not isinstance(declared, list) or len(edge) != 3 or len(declared) != 3:
            errors.append(f"{label}: invalid analytic edge vectors")
        else:
            computed = subtract(edge, listener)
            if edge != S3_ANALYTIC_EDGE:
                errors.append(
                    f"{label}: analytic edge must be exactly {S3_ANALYTIC_EDGE}"
                )
            if declared != S3_ANALYTIC_VECTOR:
                errors.append(
                    f"{label}: analytic listener-to-edge vector must be exactly "
                    f"{S3_ANALYTIC_VECTOR}"
                )
            if any(abs(a - b) > ANALYTIC_EPSILON for a, b in zip(computed, declared)):
                errors.append(f"{label}: analytic listener_to_edge_enu does not equal edge_m - listener.position_m")
            azimuth = math.degrees(math.atan2(computed[0], computed[1])) % 360.0
            if abs(azimuth - analytic.get("arrival_azimuth_degrees_clockwise_from_north", math.inf)) > AZIMUTH_EPSILON_DEGREES:
                errors.append(f"{label}: analytic arrival azimuth is incorrect")
            if analytic.get("arrival_azimuth_degrees_clockwise_from_north") != S3_ANALYTIC_AZIMUTH:
                errors.append(
                    f"{label}: declared analytic azimuth must be exactly "
                    f"{S3_ANALYTIC_AZIMUTH} degrees"
                )
            if analytic.get("tolerance_degrees") != 15:
                errors.append(f"{label}: analytic azimuth tolerance must remain 15 degrees")

    pathing = simulation.get("pathing", {})
    direct = simulation.get("direct", {})
    reflections = simulation.get("reflections", {})
    path_bake = simulation.get("path_bake", {})
    stems = fixture["expected"].get("stems", {})
    if direct != {
        "distance_attenuation": True,
        "occlusion": True,
        "occlusion_samples": 64,
    }:
        errors.append(f"{label}: direct simulation contract must remain enabled at 64 samples")
    if reflections != {
        "enabled": True,
        "rays": 4096,
        "bounces": 2,
        "duration_s": 1.0,
    }:
        errors.append(f"{label}: reflection simulation contract has changed")
    if (
        pathing.get("enabled") is not True
        or pathing.get("order") != 2
        or pathing.get("validation") is not True
        or pathing.get("alternate_paths") is not True
    ):
        errors.append(
            f"{label}: pathing must remain order 2 with validation and alternates enabled"
        )
    if path_bake != {
        "identifier": "s3-masonry-corner-path-bake-v1",
        "required_call": "iplPathBakerBake",
        "probe_batch_serialization": "required",
        "fresh_process_reload": True,
        "bake_order": 2,
    }:
        errors.append(f"{label}: path-bake serialization/reload contract has changed")
    if not is_exact_order(pathing.get("runtime_order"), STEM_ORDER):
        errors.append(f"{label}: runtime_order must uniquely be direct,path,reflections")
    if not is_exact_order(stems.get("required"), STEM_ORDER):
        errors.append(f"{label}: required stems must uniquely be direct,path,reflections")
    if not is_exact_order(stems.get("pathing_toggle_captures"), ["on", "off"]):
        errors.append(f"{label}: pathing toggles must uniquely be on,off")


def main():
    errors = []
    try:
        schema = load_json(SCHEMA_PATH)
    except (OSError, ValueError, json.JSONDecodeError) as error:
        print(f"fixture-validation: failed\n- schema: {error}", file=sys.stderr)
        return 1
    if schema.get("$id") != SCHEMA_ID_V1:
        errors.append("schema: unexpected $id")
    if schema.get("properties", {}).get("schema_version", {}).get("const") != "fightbox.fixture.v1":
        errors.append("schema: schema_version contract mismatch")
    if schema.get("properties", {}).get("gate", {}).get("enum") != ["S0", "S3"]:
        errors.append("schema: gate contract mismatch")

    # Load S6a schema
    s6a_schema_path = ROOT / "s6a.schema.json"
    try:
        s6a_schema = load_json(s6a_schema_path)
    except (OSError, ValueError, json.JSONDecodeError) as error:
        errors.append(f"s6a.schema.json: {error}")
        s6a_schema = None
    if s6a_schema and s6a_schema.get("$id") != SCHEMA_ID_S6A:
        errors.append("s6a.schema.json: unexpected $id")

    # Validate additive file-backed records even when no checked-in fixture
    # currently binds them. Generated descriptor semantics remain owned by the
    # existing asset validator.
    for descriptor_path in sorted(ASSETS_DIR.glob("*.json")):
        if descriptor_path == ASSET_SCHEMA_PATH:
            continue
        try:
            descriptor = load_json(descriptor_path)
        except (OSError, ValueError, json.JSONDecodeError) as error:
            errors.append(f"{descriptor_path.relative_to(ROOT)}: {error}")
            continue
        check_wav_asset_descriptor(descriptor, descriptor_path, errors)

    fixture_count = 0
    triangle_count = 0
    for path, fixture_id, gate in FIXTURES:
        label = path.relative_to(ROOT).as_posix()
        try:
            fixture = load_json(path)
        except (OSError, ValueError, json.JSONDecodeError) as error:
            errors.append(f"{label}: {error}")
            continue
        fixture_count += 1
        finite_numbers(fixture, label, errors)
        if not check_contract(fixture, label, fixture_id, gate, errors):
            continue

        if gate == "S6A":
            # S6A has its own multi-source asset checking in check_s6a
            check_s6a(fixture, label, errors)
        else:
            # S0 and S3 have a single source
            source = fixture.get("source", {})
            asset_id = source.get("asset_id")
            descriptor = None
            if isinstance(asset_id, str) and asset_id:
                descriptor = resolve_asset_descriptor(asset_id, label, errors)
                check_asset_coherence(fixture, descriptor, label, errors)
            else:
                errors.append(f"{label}: source.asset_id is required and must be non-empty")
            valid_triangles = check_mesh(fixture, label, errors)
            triangle_count += len(fixture["geometry"].get("triangles", []))
            if gate == "S0":
                check_s0(fixture, label, errors)
            else:
                check_s3(fixture, label, valid_triangles, errors)

    if errors:
        print("fixture-validation: failed", file=sys.stderr)
        for error in errors:
            print(f"- {error}", file=sys.stderr)
        return 1
    for warning in WARNINGS:
        print(f"WARN    {warning}", file=sys.stderr)
    print(json.dumps({"fixture_validation": "ok", "fixtures": fixture_count, "triangles": triangle_count}, separators=(",", ":")))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
