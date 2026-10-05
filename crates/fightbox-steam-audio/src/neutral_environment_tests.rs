use super::EnuVector3;
use super::decode_path_direction_enu;
use super::neutral_environment::{
    MAX_NEUTRAL_ENVIRONMENT_CHANNELS, MAX_NEUTRAL_ENVIRONMENT_ORDER,
    NEUTRAL_ENVIRONMENT_CHANNEL_COUNTS, NEUTRAL_ENVIRONMENT_LAYOUT, NeutralEnvironmentChannelOrder,
    NeutralEnvironmentError, NeutralEnvironmentNormalization, NeutralEnvironmentWorldBasis,
    STEAM_TO_NEUTRAL_ACN_INDICES, STEAM_TO_NEUTRAL_N3D_GAINS, active_channel_count,
    steam_environment_frame_to_neutral,
};

const Y_0_0: f32 = 0.282_095;
const Y_1_CARDINAL: f32 = 0.488_603;
const Y_2_HORIZONTAL_ZONAL: f32 = 0.315_392;
const Y_2_HORIZONTAL_SECTORAL: f32 = 0.546_274;
const Y_2_UP_ZONAL: f32 = 0.630_784;

#[test]
fn frozen_layout_is_steam_right_handed_acn_n3d() {
    assert_eq!(
        NEUTRAL_ENVIRONMENT_LAYOUT.world_basis,
        NeutralEnvironmentWorldBasis::SteamRightHandedXRightYUpZBack
    );
    assert_eq!(
        NEUTRAL_ENVIRONMENT_LAYOUT.channel_order,
        NeutralEnvironmentChannelOrder::Acn
    );
    assert_eq!(
        NEUTRAL_ENVIRONMENT_LAYOUT.normalization,
        NeutralEnvironmentNormalization::N3d
    );
    assert_eq!(
        NEUTRAL_ENVIRONMENT_LAYOUT.max_order,
        MAX_NEUTRAL_ENVIRONMENT_ORDER
    );
    assert_eq!(
        NEUTRAL_ENVIRONMENT_LAYOUT.max_channels,
        MAX_NEUTRAL_ENVIRONMENT_CHANNELS
    );
}

#[test]
fn order_zero_one_two_are_exact_acn_prefixes() {
    assert_eq!(NEUTRAL_ENVIRONMENT_CHANNEL_COUNTS, [1, 4, 9]);
    for (order, expected) in [(0, 1), (1, 4), (2, 9)] {
        assert_eq!(active_channel_count(order), Ok(expected));
    }
    assert_eq!(
        active_channel_count(-1),
        Err(NeutralEnvironmentError::UnsupportedOrder { order: -1 })
    );
    assert_eq!(
        active_channel_count(3),
        Err(NeutralEnvironmentError::UnsupportedOrder { order: 3 })
    );
}

#[test]
fn audited_steam_to_neutral_matrix_is_identity_i9() {
    assert_eq!(STEAM_TO_NEUTRAL_ACN_INDICES, [0, 1, 2, 3, 4, 5, 6, 7, 8]);
    assert_eq!(STEAM_TO_NEUTRAL_N3D_GAINS, [1.0; 9]);

    let raw = [
        Y_0_0,
        -0.0,
        f32::MIN_POSITIVE,
        -1.25,
        2.5,
        -3.75,
        4.0,
        -5.5,
        f32::MAX,
    ];
    let mut neutral = [f32::NAN; 9];
    steam_environment_frame_to_neutral(2, &raw, &mut neutral).unwrap();

    for (actual, expected) in neutral.into_iter().zip(raw) {
        assert_eq!(actual.to_bits(), expected.to_bits());
    }
}

#[test]
fn lower_orders_zero_every_inactive_plane() {
    let raw = [
        Y_0_0,
        -Y_1_CARDINAL,
        Y_1_CARDINAL,
        -Y_1_CARDINAL,
        f32::NAN,
        f32::INFINITY,
        f32::NEG_INFINITY,
        81.0,
        -92.0,
    ];

    for order in [0, 1] {
        let active = active_channel_count(order).unwrap();
        let mut neutral = [7.0; 9];
        steam_environment_frame_to_neutral(order, &raw, &mut neutral).unwrap();
        assert_eq!(&neutral[..active], &raw[..active]);
        assert!(neutral[active..].iter().all(|sample| sample.to_bits() == 0));
    }
}

#[test]
fn unsupported_order_and_nonfinite_active_planes_fail_closed() {
    let mut neutral = [7.0; 9];
    let finite = [1.0; 9];
    assert_eq!(
        steam_environment_frame_to_neutral(3, &finite, &mut neutral),
        Err(NeutralEnvironmentError::UnsupportedOrder { order: 3 })
    );
    assert!(neutral.iter().all(|sample| sample.to_bits() == 0));

    for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        let mut raw = [1.0; 9];
        raw[3] = bad;
        neutral.fill(7.0);
        assert_eq!(
            steam_environment_frame_to_neutral(1, &raw, &mut neutral),
            Err(NeutralEnvironmentError::NonFiniteActivePlane { channel: 3 })
        );
        assert!(neutral.iter().all(|sample| sample.to_bits() == 0));
    }
}

#[test]
fn analytic_enu_cardinals_have_the_pinned_steam_acn_signs() {
    let cases = [
        (
            "+east",
            EnuVector3::new(1.0, 0.0, 0.0),
            [
                Y_0_0,
                -Y_1_CARDINAL,
                0.0,
                0.0,
                0.0,
                0.0,
                -Y_2_HORIZONTAL_ZONAL,
                0.0,
                -Y_2_HORIZONTAL_SECTORAL,
            ],
        ),
        (
            "-east",
            EnuVector3::new(-1.0, 0.0, 0.0),
            [
                Y_0_0,
                Y_1_CARDINAL,
                0.0,
                0.0,
                0.0,
                0.0,
                -Y_2_HORIZONTAL_ZONAL,
                0.0,
                -Y_2_HORIZONTAL_SECTORAL,
            ],
        ),
        (
            "+north",
            EnuVector3::new(0.0, 1.0, 0.0),
            [
                Y_0_0,
                0.0,
                0.0,
                Y_1_CARDINAL,
                0.0,
                0.0,
                -Y_2_HORIZONTAL_ZONAL,
                0.0,
                Y_2_HORIZONTAL_SECTORAL,
            ],
        ),
        (
            "-north",
            EnuVector3::new(0.0, -1.0, 0.0),
            [
                Y_0_0,
                0.0,
                0.0,
                -Y_1_CARDINAL,
                0.0,
                0.0,
                -Y_2_HORIZONTAL_ZONAL,
                0.0,
                Y_2_HORIZONTAL_SECTORAL,
            ],
        ),
        (
            "+up",
            EnuVector3::new(0.0, 0.0, 1.0),
            [
                Y_0_0,
                0.0,
                Y_1_CARDINAL,
                0.0,
                0.0,
                0.0,
                Y_2_UP_ZONAL,
                0.0,
                0.0,
            ],
        ),
        (
            "-up",
            EnuVector3::new(0.0, 0.0, -1.0),
            [
                Y_0_0,
                0.0,
                -Y_1_CARDINAL,
                0.0,
                0.0,
                0.0,
                Y_2_UP_ZONAL,
                0.0,
                0.0,
            ],
        ),
    ];

    for (name, expected_enu, raw) in cases {
        for order in 0..=2 {
            let active = active_channel_count(order).unwrap();
            let mut neutral = [99.0; 9];
            steam_environment_frame_to_neutral(order, &raw, &mut neutral).unwrap();
            assert_eq!(&neutral[..active], &raw[..active], "{name}, order {order}");
            assert!(
                neutral[active..].iter().all(|sample| sample.to_bits() == 0),
                "{name}, order {order}"
            );
        }

        let decoded = decode_path_direction_enu(1, &raw[..4])
            .unwrap()
            .expect("cardinal first-order field has a direction");
        assert_vector_close(decoded.mean_arrival_direction_enu, expected_enu, name);
    }
}

#[test]
fn cardinal_energy_matches_n3d_addition_theorem_at_each_order() {
    let cardinals = [
        [
            Y_0_0,
            -Y_1_CARDINAL,
            0.0,
            0.0,
            0.0,
            0.0,
            -Y_2_HORIZONTAL_ZONAL,
            0.0,
            -Y_2_HORIZONTAL_SECTORAL,
        ],
        [
            Y_0_0,
            0.0,
            0.0,
            Y_1_CARDINAL,
            0.0,
            0.0,
            -Y_2_HORIZONTAL_ZONAL,
            0.0,
            Y_2_HORIZONTAL_SECTORAL,
        ],
        [
            Y_0_0,
            0.0,
            Y_1_CARDINAL,
            0.0,
            0.0,
            0.0,
            Y_2_UP_ZONAL,
            0.0,
            0.0,
        ],
    ];

    for coefficients in cardinals {
        for order in 0..=2 {
            let active = active_channel_count(order).unwrap();
            let actual = coefficients[..active]
                .iter()
                .map(|coefficient| coefficient * coefficient)
                .sum::<f32>();
            let expected = ((order + 1) * (order + 1)) as f32 / (4.0 * core::f32::consts::PI);
            assert!(
                (actual - expected).abs() <= 2.0e-6,
                "order {order}: {actual} != {expected}"
            );
        }
    }
}

#[test]
fn lower_order_prefix_and_world_space_field_do_not_depend_on_listener_rotation() {
    let raw = [Y_0_0, -0.2, 0.3, 0.4, -0.5, 0.6, -0.7, 0.8, -0.9];
    let listener_rotations = [
        ([1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, -1.0]),
        ([0.0, 0.0, -1.0], [0.0, 1.0, 0.0], [-1.0, 0.0, 0.0]),
        ([-1.0, 0.0, 0.0], [0.0, 0.0, 1.0], [0.0, 1.0, 0.0]),
    ];

    let mut reference = [0.0; 9];
    steam_environment_frame_to_neutral(2, &raw, &mut reference).unwrap();
    for _listener_rotation in listener_rotations {
        // Listener orientation is deliberately absent from the conversion
        // seam. Steam owns an unrotated world-space field here.
        let mut actual = [0.0; 9];
        steam_environment_frame_to_neutral(2, &raw, &mut actual).unwrap();
        assert_eq!(actual, reference);
    }

    for order in 0..=2 {
        let active = active_channel_count(order).unwrap();
        let mut actual = [0.0; 9];
        steam_environment_frame_to_neutral(order, &raw, &mut actual).unwrap();
        assert_eq!(&actual[..active], &reference[..active]);
    }
}

fn assert_vector_close(actual: EnuVector3, expected: EnuVector3, name: &str) {
    for (axis, actual, expected) in [
        ("east", actual.x, expected.x),
        ("north", actual.y, expected.y),
        ("up", actual.z, expected.z),
    ] {
        assert!(
            (actual - expected).abs() <= 1.0e-6,
            "{name} {axis}: {actual} != {expected}"
        );
    }
}
