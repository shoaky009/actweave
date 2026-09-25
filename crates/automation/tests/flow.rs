use automation::{Control, Error, action::*, flow::*, recognition::*};
use serde_json::{Value, json};
use std::collections::BTreeMap;

#[derive(Default)]
struct Mock {
    inputs: Vec<Input>,
    captures: usize,
    released: usize,
    fail_input: bool,
}
impl Backend for Mock {
    async fn capture(&mut self, _: &Control) -> Result<Frame, Error> {
        self.captures += 1;
        Frame::new(2, 1, vec![255, 0, 0, 0, 0, 0])
    }
    async fn input(&mut self, input: &Input, _: &Control) -> Result<(), Error> {
        if self.fail_input {
            self.fail_input = false;
            return Err(Error::Backend("lost window".into()));
        }
        self.inputs.push(input.clone());
        Ok(())
    }
    async fn release_all(&mut self) -> Result<(), Error> {
        self.released += 1;
        Ok(())
    }
}
fn node(actions: Vec<Action>, next: &[&str]) -> Node {
    Node {
        recognition: Recognition::DirectHit { roi: None },
        actions,
        next: next.iter().map(|s| s.to_string()).collect(),
        on_error: vec![],
        on_interrupted: vec![],
        timeout_ms: 100,
    }
}
fn flow(nodes: Vec<(&str, Node)>) -> Flow {
    Flow {
        entry: "entry".into(),
        nodes: nodes.into_iter().map(|(n, v)| (n.into(), v)).collect(),
        poll_interval_ms: 10,
        max_operations: 20,
    }
}
fn click() -> Action {
    Action::Click {
        target: Target::Match { index: 0 },
    }
}
async fn finish(
    runner: &mut Runner,
    mock: &mut Mock,
    rs: &mut Recognizers,
    actions: &mut Actions<Mock>,
    ctrl: &Control,
) {
    for _ in 0..30 {
        if runner.step(mock, rs, actions, ctrl).await.unwrap().status != Status::Running {
            return;
        }
    }
    panic!("unbounded runner");
}

#[tokio::test(start_paused = true)]
async fn chooses_first_matching_branch_and_clicks_recognition_center() {
    let mut miss = node(
        vec![Action::KeyDown {
            key: "wrong".into(),
        }],
        &[],
    );
    miss.recognition = Recognition::ColorMatch {
        roi: None,
        lower: [0, 255, 0],
        upper: [0, 255, 0],
        min_ratio: 1.0,
    };
    let mut hit = node(vec![click()], &[]);
    hit.recognition = Recognition::ColorMatch {
        roi: None,
        lower: [255, 0, 0],
        upper: [255, 0, 0],
        min_ratio: 0.5,
    };
    let pipeline = flow(vec![
        ("entry", node(vec![], &["miss", "hit", "later"])),
        ("miss", miss),
        ("hit", hit),
        ("later", node(vec![], &[])),
    ]);
    let mut rs = Recognizers::default();
    let mut actions = Actions::default();
    let mut mock = Mock::default();
    let mut runner = Runner::new(pipeline, &rs, &actions).unwrap();
    finish(
        &mut runner,
        &mut mock,
        &mut rs,
        &mut actions,
        &Control::default(),
    )
    .await;
    assert_eq!(runner.progress().status, Status::Completed);
    assert_eq!(runner.progress().node, "hit");
    assert!(matches!(
        mock.inputs.as_slice(),
        [Input::Click(Point { x: 1, y: 0 })]
    ));
    assert_eq!(mock.captures, 2);
    assert_eq!(mock.released, 1);
}

#[tokio::test(start_paused = true)]
async fn failed_action_enters_recovery_without_executing_tail() {
    let mut entry = node(vec![click(), Action::KeyDown { key: "tail".into() }], &[]);
    entry.on_error = vec!["recover".into()];
    let mut rs = Recognizers::default();
    let mut actions = Actions::default();
    let mut mock = Mock {
        fail_input: true,
        ..Default::default()
    };
    let mut runner = Runner::new(
        flow(vec![
            ("entry", entry),
            ("recover", node(vec![click()], &[])),
        ]),
        &rs,
        &actions,
    )
    .unwrap();
    finish(
        &mut runner,
        &mut mock,
        &mut rs,
        &mut actions,
        &Control::default(),
    )
    .await;
    assert_eq!(runner.progress().status, Status::Completed);
    assert_eq!(
        runner.progress().failure.as_ref().unwrap().action_index,
        Some(0)
    );
    assert_eq!(mock.inputs.len(), 1);
    assert_eq!(mock.released, 2);
}

#[tokio::test(start_paused = true)]
async fn recognition_timeout_routes_to_recovery_and_cycles_remain_bounded() {
    let mut entry = node(vec![], &[]);
    entry.recognition = Recognition::ColorMatch {
        roi: None,
        lower: [0, 255, 0],
        upper: [0, 255, 0],
        min_ratio: 1.0,
    };
    entry.on_error = vec!["entry".into()];
    let mut pipeline = flow(vec![("entry", entry)]);
    // Even timeout before a capture consumes the overall budget.
    pipeline.poll_interval_ms = 200;
    pipeline.max_operations = 5;
    let mut rs = Recognizers::default();
    let mut actions = Actions::default();
    let mut mock = Mock::default();
    let mut runner = Runner::new(pipeline, &rs, &actions).unwrap();
    finish(
        &mut runner,
        &mut mock,
        &mut rs,
        &mut actions,
        &Control::default(),
    )
    .await;
    assert_eq!(runner.progress().status, Status::Failed);
    assert_eq!(runner.progress().operations, 5);
    assert!(runner.progress().message.contains("budget"));
}

#[tokio::test(start_paused = true)]
async fn cancellation_interrupts_wait_and_releases_held_keys() {
    let mut rs = Recognizers::default();
    let mut actions = Actions::default();
    let mut mock = Mock::default();
    let ctrl = Control::default();
    let mut runner = Runner::new(
        flow(vec![(
            "entry",
            node(
                vec![
                    Action::KeyDown { key: "F".into() },
                    Action::Wait { duration_ms: 90 },
                    Action::KeyUp { key: "F".into() },
                ],
                &[],
            ),
        )]),
        &rs,
        &actions,
    )
    .unwrap();
    runner
        .step(&mut mock, &mut rs, &mut actions, &ctrl)
        .await
        .unwrap();
    runner
        .step(&mut mock, &mut rs, &mut actions, &ctrl)
        .await
        .unwrap();
    let cancel = async {
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        ctrl.cancel();
    };
    let (result, ()) = tokio::join!(runner.step(&mut mock, &mut rs, &mut actions, &ctrl), cancel);
    assert_eq!(result.unwrap().status, Status::Cancelled);
    assert_eq!(mock.inputs.len(), 1);
    assert_eq!(mock.released, 1);
}

struct Text;
impl Recognizer for Text {
    fn recognize<'a>(
        &'a mut self,
        _: &'a Frame,
        roi: Rect,
        parameters: &'a Value,
        _: &'a Control,
    ) -> RecognitionFuture<'a> {
        Box::pin(async move {
            Ok(RecognitionResult {
                matches: vec![Detection {
                    bounds: Some(roi),
                    score: Some(0.9),
                    text: Some(parameters["text"].as_str().unwrap().into()),
                    label: None,
                    detail: json!({"source":"mock"}),
                }],
                children: vec![],
            })
        })
    }
}
struct Inspect;
impl CustomAction<Mock> for Inspect {
    fn execute<'a>(
        &'a mut self,
        _: &'a mut Mock,
        parameters: &'a Value,
        recognition: &'a RecognitionResult,
        _: &'a Control,
    ) -> ActionFuture<'a> {
        Box::pin(async move {
            Ok(ActionResult::Continue(
                json!({"text":recognition.matches[0].text,"parameters":parameters}),
            ))
        })
    }
}

struct Jump;
impl CustomAction<Mock> for Jump {
    fn execute<'a>(
        &'a mut self,
        _: &'a mut Mock,
        _: &'a Value,
        _: &'a RecognitionResult,
        _: &'a Control,
    ) -> ActionFuture<'a> {
        Box::pin(async {
            Ok(ActionResult::Route {
                node: "target".into(),
                output: Value::Null,
            })
        })
    }
}

#[tokio::test]
async fn custom_step_routes_only_to_a_declared_next_node() {
    let mut recognizers = Recognizers::default();
    let mut actions = Actions::default();
    actions.register("jump".into(), Jump).unwrap();
    let entry = node(
        vec![Action::Custom {
            name: "jump".into(),
            parameters: Value::Null,
        }],
        &["target"],
    );
    let mut runner = Runner::new(
        flow(vec![("entry", entry), ("target", node(vec![click()], &[]))]),
        &recognizers,
        &actions,
    )
    .unwrap();
    let mut backend = Mock::default();
    finish(
        &mut runner,
        &mut backend,
        &mut recognizers,
        &mut actions,
        &Control::default(),
    )
    .await;
    assert_eq!(runner.progress().status, Status::Completed);
    assert_eq!(runner.progress().node, "target");
    assert_eq!(backend.inputs.len(), 1);

    let entry = node(
        vec![Action::Custom {
            name: "jump".into(),
            parameters: Value::Null,
        }],
        &[],
    );
    let mut runner = Runner::new(
        flow(vec![("entry", entry), ("target", node(vec![click()], &[]))]),
        &recognizers,
        &actions,
    )
    .unwrap();
    let mut backend = Mock::default();
    finish(
        &mut runner,
        &mut backend,
        &mut recognizers,
        &mut actions,
        &Control::default(),
    )
    .await;
    assert_eq!(runner.progress().status, Status::Failed);
    assert!(backend.inputs.is_empty());
}
#[tokio::test]
async fn custom_recognition_and_action_share_structured_results() {
    let mut rs = Recognizers::default();
    rs.register_custom("text".into(), Text).unwrap();
    let mut actions = Actions::default();
    actions.register("inspect".into(), Inspect).unwrap();
    let mut entry = node(
        vec![Action::Custom {
            name: "inspect".into(),
            parameters: json!({"choice":1}),
        }],
        &[],
    );
    entry.recognition = Recognition::Custom {
        name: "text".into(),
        roi: None,
        parameters: json!({"text":"继续"}),
    };
    let mut runner = Runner::new(flow(vec![("entry", entry)]), &rs, &actions).unwrap();
    let mut mock = Mock::default();
    finish(
        &mut runner,
        &mut mock,
        &mut rs,
        &mut actions,
        &Control::default(),
    )
    .await;
    assert_eq!(runner.progress().action_result["text"], "继续");
    assert_eq!(runner.progress().action_result["parameters"]["choice"], 1);
}

#[tokio::test]
async fn unsupported_recognizer_is_rejected_before_any_effect() {
    let mut entry = node(vec![click()], &[]);
    entry.recognition = Recognition::Algorithm {
        algorithm: Algorithm::Ocr,
        roi: None,
        parameters: json!({}),
    };
    assert!(matches!(
        Runner::new(
            flow(vec![("entry", entry)]),
            &Recognizers::default(),
            &Actions::<Mock>::default()
        ),
        Err(Error::Unsupported(_))
    ));
}

#[tokio::test]
async fn composites_retain_evidence_and_short_circuit() {
    let mut rs = Recognizers::default();
    let ctrl = Control::default();
    let frame = Frame::new(1, 1, vec![255, 0, 0]).unwrap();
    let yes = Recognition::DirectHit { roi: None };
    let no = Recognition::ColorMatch {
        roi: None,
        lower: [0, 0, 0],
        upper: [0, 0, 0],
        min_ratio: 1.0,
    };
    let result = rs
        .recognize(
            &Recognition::And {
                conditions: vec![yes.clone(), no.clone()],
            },
            &frame,
            &ctrl,
        )
        .await
        .unwrap();
    assert!(!result.matched());
    assert_eq!(result.children.len(), 2);
    let result = rs
        .recognize(
            &Recognition::Or {
                conditions: vec![
                    no,
                    yes,
                    Recognition::Custom {
                        name: "not registered".into(),
                        roi: None,
                        parameters: Value::Null,
                    },
                ],
            },
            &frame,
            &ctrl,
        )
        .await
        .unwrap();
    assert!(result.matched());
    assert_eq!(result.children.len(), 2);
}

#[test]
fn configuration_roundtrips_and_invalid_edges_are_rejected() {
    let pipeline = flow(vec![("entry", node(vec![click()], &[]))]);
    let parsed: Flow = serde_json::from_value(serde_json::to_value(pipeline).unwrap()).unwrap();
    parsed.validate().unwrap();
    assert!(
        flow(vec![("entry", node(vec![], &["missing"]))])
            .validate()
            .is_err()
    );
    assert!(Frame::new(2, 2, vec![0; 3]).is_err());
    assert!(
        Flow {
            nodes: BTreeMap::new(),
            entry: "missing".into(),
            poll_interval_ms: 0,
            max_operations: 0
        }
        .validate()
        .is_err()
    );
}

#[tokio::test(start_paused = true)]
async fn pause_preserves_position_and_excludes_paused_time() {
    let mut rs = Recognizers::default();
    let mut actions = Actions::default();
    let mut mock = Mock::default();
    let ctrl = Control::default();
    let mut entry = node(vec![click(), click()], &[]);
    entry.on_interrupted = vec!["remaining".into()];
    let mut runner = Runner::new(
        flow(vec![
            ("entry", entry),
            ("remaining", node(vec![click()], &[])),
        ]),
        &rs,
        &actions,
    )
    .unwrap();
    runner
        .step(&mut mock, &mut rs, &mut actions, &ctrl)
        .await
        .unwrap();
    runner
        .step(&mut mock, &mut rs, &mut actions, &ctrl)
        .await
        .unwrap();
    ctrl.pause();
    let wait = async {
        ctrl.paused().await;
        tokio::time::advance(std::time::Duration::from_secs(10)).await;
        ctrl.resume();
    };
    let (result, ()) = tokio::join!(runner.step(&mut mock, &mut rs, &mut actions, &ctrl), wait);
    result.unwrap();
    assert_eq!(mock.inputs.len(), 1);
    finish(&mut runner, &mut mock, &mut rs, &mut actions, &ctrl).await;
    assert_eq!(runner.progress().status, Status::Completed);
    assert_eq!(mock.inputs.len(), 2);
}

#[tokio::test]
async fn relative_roi_tracks_frame_size_and_action_target() {
    let mut recognizers = Recognizers::default();
    let control = Control::default();
    let recognition = Recognition::ColorMatch {
        roi: Some(Roi::Relative {
            x: 0.5,
            y: 0.5,
            width: 0.5,
            height: 0.5,
        }),
        lower: [255, 0, 0],
        upper: [255, 0, 0],
        min_ratio: 1.0,
    };
    for (width, height) in [(8, 4), (16, 8)] {
        let mut pixels = vec![0; (width * height * 3) as usize];
        for y in height / 2..height {
            for x in width / 2..width {
                pixels[((y * width + x) * 3) as usize] = 255;
            }
        }
        let frame = Frame::new(width, height, pixels).unwrap();
        let result = recognizers
            .recognize(&recognition, &frame, &control)
            .await
            .unwrap();
        assert!(result.matched());
        let point = Target::Match { index: 0 }.resolve(&result).unwrap();
        assert_eq!(
            (point.x, point.y),
            ((width * 3 / 4) as i32, (height * 3 / 4) as i32)
        );
    }
}

#[tokio::test]
async fn ocr_extension_receives_resolved_roi_without_needing_real_ocr_backend() {
    let mut recognizers = Recognizers::default();
    recognizers
        .register_algorithm(Algorithm::Ocr, Text)
        .unwrap();
    let recognition = Recognition::Algorithm {
        algorithm: Algorithm::Ocr,
        roi: Some(Roi::Relative {
            x: 0.0,
            y: 0.5,
            width: 1.0,
            height: 0.5,
        }),
        parameters: json!({"text":"继续"}),
    };
    let frame = Frame::new(10, 8, vec![0; 240]).unwrap();
    let result = recognizers
        .recognize(&recognition, &frame, &Control::default())
        .await
        .unwrap();
    let detection = &result.matches[0];
    let bounds = detection.bounds.unwrap();
    assert_eq!(
        (bounds.x, bounds.y, bounds.width, bounds.height),
        (0, 4, 10, 4)
    );
    assert_eq!(detection.text.as_deref(), Some("继续"));
    assert_eq!(detection.score, Some(0.9));
}

#[test]
fn roi_rejects_invalid_geometry_and_rounds_outward() {
    let frame = Frame::new(3, 3, vec![0; 27]).unwrap();
    let region = Roi::Relative {
        x: 0.5,
        y: 0.5,
        width: 0.5,
        height: 0.5,
    }
    .resolve(&frame)
    .unwrap();
    assert_eq!(
        (region.x, region.y, region.width, region.height),
        (1, 1, 2, 2)
    );
    for roi in [
        Roi::Relative {
            x: f64::NAN,
            y: 0.0,
            width: 1.0,
            height: 1.0,
        },
        Roi::Relative {
            x: 0.0,
            y: 0.0,
            width: 0.0,
            height: 1.0,
        },
        Roi::Relative {
            x: 0.5,
            y: 0.0,
            width: 0.6,
            height: 1.0,
        },
        Roi::Fixed {
            rect: Rect {
                x: 2,
                y: 0,
                width: 2,
                height: 1,
            },
        },
    ] {
        assert!(roi.resolve(&frame).is_err());
    }
}
