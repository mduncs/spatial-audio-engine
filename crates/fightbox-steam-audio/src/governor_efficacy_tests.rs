#[test]
fn ineffective_render_demotion_rolls_back_and_holds_until_pressure_worsens() {
    let mut governor = governor(1);
    timing_window(&mut governor, 1_900_000);
    assert_eq!(
        governor.telemetry().reflections.level,
        ReflectionQualityLevel::Reduced
    );

    timing_window(&mut governor, 1_900_000);
    assert_eq!(
        governor.telemetry().reflections.level,
        ReflectionQualityLevel::Full
    );
    for duration_ns in [1_900_000, 2_000_000] {
        for _ in 0..4 {
            timing_window(&mut governor, duration_ns);
            assert_eq!(
                governor.telemetry().reflections.level,
                ReflectionQualityLevel::Full
            );
            assert_eq!(governor.telemetry().ladder_position, 0);
        }
    }
    assert_eq!(governor.telemetry().callback_deadline_misses, 0);
}

#[test]
fn useful_render_demotion_sticks_without_recovery_headroom() {
    let mut governor = governor(1);
    timing_window(&mut governor, 1_900_000);
    assert_eq!(
        governor.telemetry().reflections.level,
        ReflectionQualityLevel::Reduced
    );

    for _ in 0..5 {
        timing_window(&mut governor, 1_500_000);
        assert_eq!(
            governor.telemetry().reflections.level,
            ReflectionQualityLevel::Reduced
        );
        assert_eq!(governor.telemetry().ladder_position, 1);
    }
}

#[test]
fn percentile_recovery_margin_tolerates_a_single_safe_spike() {
    let mut governor = governor(1);
    timing_window(&mut governor, 1_900_000);
    timing_window(&mut governor, 500_000);
    assert_eq!(
        governor.telemetry().reflections.level,
        ReflectionQualityLevel::Reduced
    );
    assert_eq!(
        governor.telemetry().reason,
        GovernorTransitionReason::RenderP99OverBudget
    );

    for block in 0..3_000 {
        governor.observe_block_timing(if block % 128 == 0 { 1_200_000 } else { 500_000 });
    }
    let telemetry = governor.telemetry();
    assert_eq!(telemetry.reflections.level, ReflectionQualityLevel::Full);
    assert_eq!(telemetry.ladder_position, 0);
    assert_eq!(telemetry.reason, GovernorTransitionReason::AtFullQuality);
    assert_eq!(telemetry.callback_deadline_misses, 0);
}

#[test]
fn first_deadline_miss_demotes_immediately_and_keeps_the_cooldown() {
    let mut governor = governor(1);
    governor.observe_block_timing(3_000_000);
    let missed = governor.telemetry();
    assert_eq!(missed.reflections.level, ReflectionQualityLevel::Reduced);
    assert_eq!(missed.ladder_position, 1);
    assert_eq!(missed.reason, GovernorTransitionReason::RenderDeadlineMiss);
    assert_eq!(missed.callback_deadline_misses, 1);

    timing_window(&mut governor, 750_000);
    assert_eq!(
        governor.telemetry().reflections.level,
        ReflectionQualityLevel::Reduced
    );
    assert_eq!(governor.telemetry().ladder_position, 1);
    assert_eq!(governor.telemetry().callback_deadline_misses, 1);
}
