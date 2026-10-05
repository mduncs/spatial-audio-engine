//! Checked-in determinism expectations for representative city path bakes.
//!
//! `assert_in_linked_test` distinguishes reproducible fixtures that CI can bake
//! from documentary entries whose source artifact deliberately lives outside
//! the repository. When an intentional bake-format or algorithm change moves a
//! reproducible value, update this table in the same change.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BakeExpectation {
    pub name: &'static str,
    pub input: &'static str,
    pub configuration: &'static str,
    pub artifact_sha256: &'static str,
    pub probe_count: u32,
    pub assert_in_linked_test: bool,
}

pub(crate) const EXPECTED_BAKES: &[BakeExpectation] = &[
    BakeExpectation {
        name: "synthetic-block-default",
        input: "fixtures/city/synthetic/block.geojson",
        configuration: "city bake defaults: path_range_m=100, visibility_range_m=6, \
            visibility_samples=1, visibility_threshold=0.5, probe_spacing_m=4, \
            probe_height_above_floor_m=1.5, probe_ceiling_m=3, elevated_layers=[], \
            bake_threads=1",
        artifact_sha256: "661295456292dd82f18c3c68eb4b0f7823cd37cfef0c7a06e684751b5405091d",
        probe_count: 135,
        assert_in_linked_test: true,
    },
    BakeExpectation {
        name: "megablock-floor-default-no-flags",
        input: "external megablock floor-bake artifact (not checked in)",
        configuration: "city bake defaults with no elevated-probe flags",
        artifact_sha256: "d41a2e976b2f6b010ee81e6d6119785f780c5417e1da3c869db400dc9634aeaa",
        probe_count: 19_881,
        assert_in_linked_test: false,
    },
];

pub(crate) fn expectation(name: &str) -> &'static BakeExpectation {
    EXPECTED_BAKES
        .iter()
        .find(|entry| entry.name == name)
        .unwrap_or_else(|| panic!("unknown checked-in bake expectation {name:?}"))
}

#[test]
fn expectation_table_is_named_well_formed_and_explicitly_gated() {
    let synthetic = expectation("synthetic-block-default");
    assert!(synthetic.assert_in_linked_test);
    assert!(
        !expectation("megablock-floor-default-no-flags").assert_in_linked_test,
        "external megablock evidence must not become a default test dependency"
    );
    for (index, entry) in EXPECTED_BAKES.iter().enumerate() {
        assert!(!entry.name.is_empty());
        assert!(!entry.input.is_empty());
        assert!(!entry.configuration.is_empty());
        assert!(entry.probe_count > 0);
        assert_eq!(entry.artifact_sha256.len(), 64);
        assert!(
            entry
                .artifact_sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        );
        assert!(
            EXPECTED_BAKES[..index]
                .iter()
                .all(|prior| prior.name != entry.name),
            "duplicate bake expectation name {:?}",
            entry.name
        );
    }
}
