#!/usr/bin/env python3
"""Validate a workbench fixture with no third-party Python dependencies.

The schema intentionally describes only keys consumed by fightbox-workbench.
Unknown keys are therefore useful migration evidence: they are printed as
warnings rather than making legacy city fixtures unloadable.
"""

from __future__ import annotations

import argparse
import json
import math
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Any


@dataclass(frozen=True, order=True)
class Issue:
    path: str
    message: str


class SchemaValidator:
    """Small JSON Schema 2020-12 subset used by workbench.schema.json."""

    def __init__(self, root_schema: dict[str, Any]) -> None:
        self.root_schema = root_schema

    def validate(
        self, instance: Any, schema: dict[str, Any] | bool, path: str = "$"
    ) -> tuple[list[Issue], list[Issue]]:
        if schema is True:
            return [], []
        if schema is False:
            return [Issue(path, "value is forbidden by the schema")], []

        if "$ref" in schema:
            return self.validate(instance, self._resolve_ref(schema["$ref"]), path)

        errors: list[Issue] = []
        warnings: list[Issue] = []

        expected_type = schema.get("type")
        if expected_type is not None and not self._matches_type(instance, expected_type):
            return [Issue(path, f"expected {self._type_label(expected_type)}, got {self._json_type(instance)}")], []

        if "const" in schema and not self._json_equal(instance, schema["const"]):
            errors.append(Issue(path, f"expected constant {schema['const']!r}"))
        if "enum" in schema and not any(
            self._json_equal(instance, choice) for choice in schema["enum"]
        ):
            errors.append(Issue(path, f"expected one of {schema['enum']!r}"))

        if "oneOf" in schema:
            branch_results = [
                self.validate(instance, branch, path) for branch in schema["oneOf"]
            ]
            matching = [result for result in branch_results if not result[0]]
            if len(matching) == 1:
                warnings.extend(matching[0][1])
            elif len(matching) > 1:
                errors.append(Issue(path, "matched more than one oneOf branch"))
            else:
                best_errors, best_warnings = min(
                    branch_results, key=lambda result: (len(result[0]), len(result[1]))
                )
                errors.append(Issue(path, "did not match any oneOf branch"))
                errors.extend(best_errors)
                warnings.extend(best_warnings)

        if "anyOf" in schema:
            branch_results = [
                self.validate(instance, branch, path) for branch in schema["anyOf"]
            ]
            matching = [result for result in branch_results if not result[0]]
            if matching:
                warnings.extend(min(matching, key=lambda result: len(result[1]))[1])
            else:
                best_errors, best_warnings = min(
                    branch_results, key=lambda result: (len(result[0]), len(result[1]))
                )
                errors.append(Issue(path, "did not match any anyOf branch"))
                errors.extend(best_errors)
                warnings.extend(best_warnings)

        if isinstance(instance, dict):
            properties = schema.get("properties", {})
            for required in schema.get("required", []):
                if required not in instance:
                    errors.append(Issue(self._property_path(path, required), "required key is missing"))
            for key, value in instance.items():
                child_path = self._property_path(path, key)
                if key in properties:
                    child_errors, child_warnings = self.validate(
                        value, properties[key], child_path
                    )
                    errors.extend(child_errors)
                    warnings.extend(child_warnings)
                elif schema.get("additionalProperties") is False:
                    warnings.append(
                        Issue(child_path, "ignored key: fightbox-workbench does not consume it")
                    )
                elif isinstance(schema.get("additionalProperties"), (dict, bool)):
                    child_errors, child_warnings = self.validate(
                        value, schema["additionalProperties"], child_path
                    )
                    errors.extend(child_errors)
                    warnings.extend(child_warnings)

        if isinstance(instance, list):
            if "minItems" in schema and len(instance) < schema["minItems"]:
                errors.append(Issue(path, f"requires at least {schema['minItems']} items"))
            if "maxItems" in schema and len(instance) > schema["maxItems"]:
                errors.append(Issue(path, f"allows at most {schema['maxItems']} items"))
            if schema.get("uniqueItems"):
                canonical = [
                    json.dumps(item, sort_keys=True, separators=(",", ":"))
                    for item in instance
                ]
                if len(canonical) != len(set(canonical)):
                    errors.append(Issue(path, "items must be unique"))
            item_schema = schema.get("items")
            if item_schema is not None:
                for index, value in enumerate(instance):
                    child_errors, child_warnings = self.validate(
                        value, item_schema, f"{path}[{index}]"
                    )
                    errors.extend(child_errors)
                    warnings.extend(child_warnings)

        if isinstance(instance, str):
            if "minLength" in schema and len(instance) < schema["minLength"]:
                errors.append(Issue(path, f"requires at least {schema['minLength']} characters"))
            if "maxLength" in schema and len(instance) > schema["maxLength"]:
                errors.append(Issue(path, f"allows at most {schema['maxLength']} characters"))

        if self._is_number(instance):
            number = instance
            if isinstance(number, float) and not math.isfinite(number):
                errors.append(Issue(path, "number must be finite"))
            else:
                if "minimum" in schema and number < schema["minimum"]:
                    errors.append(Issue(path, f"must be >= {schema['minimum']}"))
                if "maximum" in schema and number > schema["maximum"]:
                    errors.append(Issue(path, f"must be <= {schema['maximum']}"))
                if "exclusiveMinimum" in schema and number <= schema["exclusiveMinimum"]:
                    errors.append(Issue(path, f"must be > {schema['exclusiveMinimum']}"))
                if "exclusiveMaximum" in schema and number >= schema["exclusiveMaximum"]:
                    errors.append(Issue(path, f"must be < {schema['exclusiveMaximum']}"))

        return self._deduplicate(errors), self._deduplicate(warnings)

    def _resolve_ref(self, reference: str) -> dict[str, Any] | bool:
        if not reference.startswith("#/"):
            raise ValueError(f"only local JSON pointers are supported, got {reference!r}")
        node: Any = self.root_schema
        for encoded in reference[2:].split("/"):
            key = encoded.replace("~1", "/").replace("~0", "~")
            node = node[key]
        if not isinstance(node, (dict, bool)):
            raise ValueError(f"schema reference {reference!r} does not name a schema")
        return node

    @staticmethod
    def _matches_type(instance: Any, expected: str | list[str]) -> bool:
        choices = [expected] if isinstance(expected, str) else expected
        return any(SchemaValidator._matches_single_type(instance, choice) for choice in choices)

    @staticmethod
    def _matches_single_type(instance: Any, expected: str) -> bool:
        return {
            "null": instance is None,
            "boolean": isinstance(instance, bool),
            "object": isinstance(instance, dict),
            "array": isinstance(instance, list),
            "number": SchemaValidator._is_number(instance),
            "integer": isinstance(instance, int) and not isinstance(instance, bool),
            "string": isinstance(instance, str),
        }.get(expected, False)

    @staticmethod
    def _is_number(instance: Any) -> bool:
        return isinstance(instance, (int, float)) and not isinstance(instance, bool)

    @staticmethod
    def _json_type(instance: Any) -> str:
        if instance is None:
            return "null"
        if isinstance(instance, bool):
            return "boolean"
        if isinstance(instance, dict):
            return "object"
        if isinstance(instance, list):
            return "array"
        if isinstance(instance, str):
            return "string"
        if isinstance(instance, int):
            return "integer"
        if isinstance(instance, float):
            return "number"
        return type(instance).__name__

    @staticmethod
    def _type_label(expected: str | list[str]) -> str:
        return expected if isinstance(expected, str) else " or ".join(expected)

    @staticmethod
    def _json_equal(left: Any, right: Any) -> bool:
        if isinstance(left, bool) or isinstance(right, bool):
            return type(left) is type(right) and left == right
        return left == right

    @staticmethod
    def _property_path(parent: str, key: str) -> str:
        if key.isidentifier():
            return f"{parent}.{key}"
        return f"{parent}[{key!r}]"

    @staticmethod
    def _deduplicate(issues: list[Issue]) -> list[Issue]:
        return sorted(set(issues))


def load_json(path: Path, label: str) -> Any:
    try:
        with path.open("r", encoding="utf-8") as handle:
            return json.load(handle)
    except OSError as error:
        raise ValueError(f"cannot read {label} {path}: {error}") from error
    except json.JSONDecodeError as error:
        raise ValueError(
            f"invalid JSON in {label} {path}:{error.lineno}:{error.colno}: {error.msg}"
        ) from error


def validate_cross_fields(fixture: Any) -> list[Issue]:
    """Rules that relate ordinary sources and triggerable events."""
    if not isinstance(fixture, dict):
        return []
    sources = fixture.get("sources", [])
    events = fixture.get("events", [])
    if not isinstance(sources, list) or not isinstance(events, list):
        return []
    errors: list[Issue] = []
    seen: dict[str, str] = {}
    for index, source in enumerate(sources):
        if isinstance(source, dict) and isinstance(source.get("id"), str):
            seen[source["id"]] = f"$.sources[{index}].id"
    for event_index, event in enumerate(events):
        if not isinstance(event, dict):
            continue
        direction = event.get("direction_enu")
        if (
            isinstance(direction, list)
            and len(direction) == 3
            and all(SchemaValidator._is_number(component) for component in direction)
            and all(math.isfinite(component) for component in direction)
            and sum(component * component for component in direction) <= 1.0e-12
        ):
            errors.append(
                Issue(
                    f"$.events[{event_index}].direction_enu",
                    "ballistic direction must be non-zero",
                )
            )
        event_sources = event.get("event_sources")
        if not isinstance(event_sources, dict):
            continue
        for role in ("crack", "blast"):
            source = event_sources.get(role)
            if not isinstance(source, dict) or not isinstance(source.get("id"), str):
                continue
            path = f"$.events[{event_index}].event_sources.{role}.id"
            source_id = source["id"]
            if source_id in seen:
                errors.append(Issue(path, f"duplicates source id declared at {seen[source_id]}"))
            else:
                seen[source_id] = path
    return sorted(set(errors))


def parse_args() -> argparse.Namespace:
    repository = Path(__file__).resolve().parent.parent
    parser = argparse.ArgumentParser(
        description="Validate the JSON fields consumed by fightbox-workbench"
    )
    parser.add_argument("fixture", type=Path, help="fixture JSON to validate")
    parser.add_argument(
        "--schema",
        type=Path,
        default=repository / "fixtures" / "workbench.schema.json",
        help="schema path (default: fixtures/workbench.schema.json)",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    try:
        schema = load_json(args.schema, "schema")
        fixture = load_json(args.fixture, "fixture")
        if not isinstance(schema, dict):
            raise ValueError(f"schema {args.schema} must contain a JSON object")
        errors, warnings = SchemaValidator(schema).validate(fixture, schema)
        errors = sorted(set(errors + validate_cross_fields(fixture)))
    except (KeyError, TypeError, ValueError) as error:
        print(f"FAIL {error}", file=sys.stderr)
        return 2

    for warning in warnings:
        print(f"WARNING {warning.path}: {warning.message}")
    for error in errors:
        print(f"ERROR {error.path}: {error.message}", file=sys.stderr)

    if errors:
        print(
            f"FAIL {args.fixture}: {len(errors)} error(s), {len(warnings)} ignored-key warning(s)",
            file=sys.stderr,
        )
        return 1

    print(
        f"PASS {args.fixture}: workbench-consumed fields are valid; "
        f"{len(warnings)} ignored-key warning(s)"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
