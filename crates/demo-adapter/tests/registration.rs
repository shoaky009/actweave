use adapter_runtime::Runtime;
use adapter_sdk::{AdapterError, ExecutionControl, Host, Registry, ToolCall};
use demo_adapter::{Scenario, register};
use serde_json::json;
use std::sync::{Arc, Mutex};

#[tokio::test]
async fn registered_instances_are_isolated_and_debug_host_logs_calls() {
    let mut registry = Registry::default();
    register(&mut registry, Scenario::Normal).unwrap();
    assert!(register(&mut registry, Scenario::Normal).is_err());
    assert!(registry.create("missing", Host::default()).is_err());
    let messages = Arc::new(Mutex::new(Vec::new()));
    let logs = messages.clone();
    let host = Host::new(move |m| logs.lock().unwrap().push(m.to_owned()));
    let mut first = Runtime::new(&registry, "demo", "trial", host).unwrap();
    let second = Runtime::new(&registry, "demo", "trial", Host::default()).unwrap();
    let call = ToolCall {
        name: "perform_trial".into(),
        arguments: json!({}),
    };
    first
        .invoke(&call, &ExecutionControl::default())
        .await
        .unwrap();
    assert_eq!(first.observe().unwrap().facts["trial"]["completed"], 1);
    assert_eq!(second.observe().unwrap().facts["trial"]["completed"], 0);
    assert!(
        messages
            .lock()
            .unwrap()
            .iter()
            .any(|m| m.contains("perform_trial"))
    );
    let control = ExecutionControl::default();
    control.cancellation.cancel();
    assert!(matches!(
        first.invoke(&call, &control).await,
        Err(AdapterError::Cancelled)
    ));
    assert_eq!(first.observe().unwrap().facts["trial"]["completed"], 1);
}

#[tokio::test]
async fn debug_host_rejects_unavailable_skills_before_effects() {
    let mut registry = Registry::default();
    register(&mut registry, Scenario::Normal).unwrap();
    let mut runtime = Runtime::new(&registry, "demo", "train", Host::default()).unwrap();
    let call = ToolCall {
        name: "start_training".into(),
        arguments: json!({}),
    };
    assert!(
        runtime
            .invoke(&call, &ExecutionControl::default())
            .await
            .is_err()
    );
    assert_eq!(
        runtime.observe().unwrap().facts["training"]["status"],
        "not_started"
    );
}
