fn reflection_work(governor: &mut QualityGovernor, elapsed_ns: u64, interval_ns: u64) {
    for _ in 0..SIMULATION_WORK_WINDOW {
        governor.observe_simulation_work(
            GovernorSimulationPass::Reflections,
            elapsed_ns,
            interval_ns,
            governor.render.reflections,
        );
    }
}

#[test]
fn simulation_failure_cannot_recover_on_render_headroom_alone() {
    let mut governor = reference_governor();
    governor.observe_simulation_pass_overrun(GovernorSimulationPass::Reflections, 1);
    evaluation(&mut governor, 100_000);
    assert_eq!(
        governor.render.reflections.level,
        ReflectionQualityLevel::Reduced
    );
    governor.observed_blocks = governor.simulation_recovery_memory[0].retry_after_block;
    for _ in 0..4_000 {
        governor.observe_block_timing(100_000);
    }
    assert_eq!(
        governor.render.reflections.level,
        ReflectionQualityLevel::Reduced
    );
    assert!(governor.recovery_probation.is_none());
}

#[test]
fn repeated_simulation_failures_extend_the_rungs_backoff() {
    let mut governor = reference_governor();
    governor.observe_simulation_pass_overrun(GovernorSimulationPass::Reflections, 1);
    evaluation(&mut governor, 100_000);
    let first = governor.simulation_recovery_memory[0];
    governor.observed_blocks = first.retry_after_block;
    reflection_work(&mut governor, 1_000_000, 80_000_000);
    assert!(governor.simulation_recovery_allows(RecoveryRung::ReflectionFull));
    governor.apply_recovery(RecoveryRung::ReflectionFull);
    governor.observe_simulation_pass_overrun(GovernorSimulationPass::Reflections, 1);
    evaluation(&mut governor, 100_000);
    let second = governor.simulation_recovery_memory[0];
    assert_eq!(second.failures, 2);
    assert!(
        second.retry_after_block - governor.observed_blocks
            >= 2 * SIMULATION_RECOVERY_BACKOFF_NS / governor.block_period_ns
    );
    reflection_work(&mut governor, 1_000_000, 80_000_000);
    assert!(!governor.simulation_recovery_allows(RecoveryRung::ReflectionFull));
    governor.observed_blocks = second.retry_after_block;
    assert!(governor.simulation_recovery_allows(RecoveryRung::ReflectionFull));
}

#[test]
fn simulation_pressure_keeps_a_three_bounce_floor_until_render_emergency() {
    let mut governor = reference_governor();
    governor.requested.reflection_bounces = 8;
    governor.render.reflections = delivered_reflections(
        governor.requested,
        QualityTier::Desktop,
        ReflectionQualityLevel::Full,
        false,
    );
    for _ in 0..5 {
        governor.observe_simulation_pass_overrun(GovernorSimulationPass::Reflections, 1);
        evaluation(&mut governor, 100_000);
    }
    let reflections = governor.render.reflections;
    assert_eq!(reflections.level, ReflectionQualityLevel::Intermediate);
    assert_eq!(
        (
            reflections.rays,
            reflections.bounces,
            reflections.cadence_divisor
        ),
        (1_024, 3, 4)
    );
    assert_eq!(reflections.ir_duration_s, 0.75);
    assert!(governor.render.validate_paths && governor.render.find_alternate_paths);
    governor.observe_block_timing(3_000_000);
    assert_eq!(
        governor.render.reflections.level,
        ReflectionQualityLevel::Minimum
    );
    assert_eq!(governor.render.reflections.bounces, 0);
    assert_eq!(
        governor.reason,
        GovernorTransitionReason::RenderDeadlineMiss
    );
}

#[test]
fn reflection_recovery_scales_measured_work_to_the_target_cadence() {
    let mut governor = reference_governor();
    governor.requested.reflection_bounces = 8;
    governor.render.reflections = delivered_reflections(
        governor.requested,
        QualityTier::Desktop,
        ReflectionQualityLevel::Full,
        false,
    );
    governor.observe_simulation_pass_overrun(GovernorSimulationPass::Reflections, 1);
    evaluation(&mut governor, 100_000);
    governor.observed_blocks = governor.simulation_recovery_memory[0].retry_after_block;
    reflection_work(&mut governor, 25_000_000, 80_000_000);
    assert!(!governor.simulation_recovery_allows(RecoveryRung::ReflectionFull));
    reflection_work(&mut governor, 10_000_000, 80_000_000);
    assert!(governor.simulation_recovery_allows(RecoveryRung::ReflectionFull));
}
