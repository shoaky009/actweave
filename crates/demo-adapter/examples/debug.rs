//! Independent developer host: cargo run -p demo-adapter --example debug
use adapter_runtime::Runtime;
use adapter_sdk::{ExecutionControl, Host, Registry, ToolCall};
use demo_adapter::{Scenario, register};
use serde_json::json;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut registry = Registry::default();
    register(&mut registry, Scenario::Normal)?;
    let host = Host::new(|message| println!("[INFO] {message}"));
    let mut runtime = Runtime::new(&registry, "demo", "完成三次试验", host)?;
    println!(
        "Skills: {:?}",
        runtime
            .skills()?
            .skills
            .iter()
            .map(|s| &s.name)
            .collect::<Vec<_>>()
    );
    let call = ToolCall {
        name: "perform_trial".into(),
        arguments: json!({}),
    };
    for _ in 0..3 {
        runtime.invoke(&call, &ExecutionControl::default()).await?;
    }
    println!("状态：{}", runtime.observe()?.facts);
    Ok(())
}
