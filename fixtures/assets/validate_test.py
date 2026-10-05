#!/usr/bin/env python3
"""Negative-regression tests for the source-asset cross-field validator."""

from __future__ import annotations

import copy
import json
import unittest
from pathlib import Path

import validate


HERE = Path(__file__).resolve().parent
STEREO = json.loads(
    (HERE / "source-contract-authored-stereo.json").read_text(encoding="utf-8")
)


def mono_expanded_descriptor() -> dict:
    descriptor = copy.deepcopy(STEREO)
    descriptor["layout"] = "mono"
    descriptor["presentation_provenance"] = "mono_expanded"
    descriptor["compatible_geometries"] = ["stereo_image"]
    descriptor["original"]["format"]["layout"] = "mono"
    descriptor["canonical"]["format"]["layout"] = "mono"
    descriptor["measurements"]["per_channel"] = [
        {
            "channel": "mono",
            "levels": descriptor["measurements"]["aggregate"],
        }
    ]
    del descriptor["measurements"]["stereo"]
    descriptor["derivation"] = {
        "source_artifact_id": (
            validate.CANONICAL_ARTIFACT_PREFIX + "5" * 64
        ),
        "recipe_sha256": "6" * 64,
    }
    return descriptor


class SourceAssetValidatorRegressionTests(unittest.TestCase):
    def test_sign_flipped_pca_basis_is_rejected(self) -> None:
        descriptor = copy.deepcopy(STEREO)
        descriptor["measurements"]["stereo"]["pca"]["center_weights_lr"] = [
            -0.7071067811865476,
            -0.7071067811865476,
        ]
        descriptor["measurements"]["stereo"]["pca"]["width_weights_lr"] = [
            0.7071067811865476,
            -0.7071067811865476,
        ]

        with self.assertRaisesRegex(validate.Invalid, "first-nonzero-positive"):
            validate.validate_descriptor(descriptor)

    def test_mono_expanded_artifact_id_with_extra_colon_is_rejected(self) -> None:
        descriptor = mono_expanded_descriptor()
        descriptor["derivation"]["source_artifact_id"] = (
            validate.CANONICAL_ARTIFACT_PREFIX + "extra:" + "5" * 64
        )

        with self.assertRaisesRegex(validate.Invalid, "64 lowercase hex"):
            validate.validate_descriptor(descriptor)

    def test_valid_mono_expanded_artifact_id_remains_accepted(self) -> None:
        validate.validate_descriptor(mono_expanded_descriptor())


if __name__ == "__main__":
    unittest.main()
