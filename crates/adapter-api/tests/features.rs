use adapter_api::*;
use serde_json::json;
fn feature() -> Feature {
    Feature {
        id: "test".into(),
        name: "Test".into(),
        description: String::new(),
        parameters: vec![
            Parameter {
                id: "times".into(),
                name: "次数".into(),
                kind: ParameterKind::Integer { min: 1, max: 100 },
                default: Some(json!(10)),
            },
            Parameter {
                id: "text".into(),
                name: "文字".into(),
                kind: ParameterKind::String,
                default: Some(json!("")),
            },
            Parameter {
                id: "enabled".into(),
                name: "启用".into(),
                kind: ParameterKind::Boolean,
                default: Some(json!(true)),
            },
            Parameter {
                id: "mode".into(),
                name: "模式".into(),
                kind: ParameterKind::Enum {
                    choices: vec![Choice {
                        value: "safe".into(),
                        label: "安全".into(),
                    }],
                },
                default: Some(json!("safe")),
            },
        ],
    }
}
#[test]
fn validates_types_defaults_and_cli_values() {
    let f = feature();
    assert_eq!(f.resolve(&json!({})).unwrap()["times"], 10);
    let input = f
        .parse_arguments(&["text=123".into(), "enabled=false".into(), "times=2".into()])
        .unwrap();
    assert_eq!(input["text"], "123");
    assert_eq!(input["enabled"], false);
    for bad in [
        json!({"times":0}),
        json!({"times":1.5}),
        json!({"times":"2"}),
        json!({"mode":"bad"}),
        json!({"extra":1}),
    ] {
        assert!(f.resolve(&bad).is_err());
    }
    assert!(
        f.parse_arguments(&["times=1".into(), "times=2".into()])
            .is_err()
    );
}
