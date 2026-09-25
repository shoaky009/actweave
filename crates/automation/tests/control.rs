use automation::control::*;

fn config() -> ApproachConfig {
    ApproachConfig {
        turn_pixels_per_offset: 100.0,
        center_tolerance: 0.1,
        max_turn_pixels: 30,
        move_ms: 100,
        max_missing_observations: 1,
        min_range_progress: 0.05,
        max_stalled_moves: 2,
    }
}

fn controller(config: ApproachConfig) -> ApproachController {
    ApproachController::new(
        ApproachGoal {
            target: TargetRef(7),
        },
        config,
    )
    .unwrap()
}

fn target(id: u64, offset: f32, range: Option<f32>) -> ObservedTarget {
    ObservedTarget {
        id: TargetRef(id),
        horizontal_offset: offset,
        range,
    }
}

#[test]
fn policy_steers_toward_the_selected_target() {
    let mut controller = controller(config());
    let observation = Observation {
        targets: vec![target(7, 0.8, Some(4.0)), target(8, 0.0, Some(0.0))],
    };
    assert_eq!(
        controller.step(&observation).unwrap(),
        Decision::Turn { dx: 30 }
    );
}

#[test]
fn aligned_target_gets_one_short_move_then_requires_new_observation() {
    let mut controller = controller(config());
    let observation = Observation {
        targets: vec![target(7, 0.02, Some(4.0))],
    };
    assert_eq!(
        controller.step(&observation).unwrap(),
        Decision::Move { duration_ms: 100 }
    );
}

#[test]
fn missing_target_ends_after_the_configured_retry_count() {
    let mut controller = controller(config());
    assert_eq!(
        controller.step(&Observation::default()).unwrap(),
        Decision::ObserveAgain
    );
    assert_eq!(
        controller.step(&Observation::default()).unwrap(),
        Decision::Blocked(Failure::TargetLost)
    );
}

#[test]
fn unchanged_range_after_two_moves_reports_no_progress() {
    let mut controller = controller(config());
    let observation = Observation {
        targets: vec![target(7, 0.0, Some(4.0))],
    };
    assert_eq!(
        controller.step(&observation).unwrap(),
        Decision::Move { duration_ms: 100 }
    );
    controller.move_completed(Some(4.0));
    assert_eq!(
        controller.step(&observation).unwrap(),
        Decision::Move { duration_ms: 100 }
    );
    controller.move_completed(Some(4.0));
    assert_eq!(
        controller.step(&observation).unwrap(),
        Decision::Blocked(Failure::NoProgress)
    );
}

#[test]
fn interrupted_input_does_not_create_false_progress_evidence() {
    let mut controller = controller(config());
    let observation = Observation {
        targets: vec![target(7, 0.0, Some(4.0))],
    };
    assert_eq!(
        controller.step(&observation).unwrap(),
        Decision::Move { duration_ms: 100 }
    );
    controller.input_interrupted();
    assert_eq!(
        controller.step(&observation).unwrap(),
        Decision::Move { duration_ms: 100 }
    );
}

#[test]
fn signed_camera_calibration_sets_turn_direction() {
    let mut config = config();
    config.turn_pixels_per_offset = -100.0;
    let mut controller = controller(config);
    let observation = Observation {
        targets: vec![target(7, 0.2, None)],
    };
    assert_eq!(
        controller.step(&observation).unwrap(),
        Decision::Turn { dx: -20 }
    );
}

#[test]
fn duplicate_goal_detection_is_rejected() {
    let mut controller = controller(config());
    let observation = Observation {
        targets: vec![target(7, 0.1, None), target(7, 0.2, None)],
    };
    assert!(controller.step(&observation).is_err());
}
