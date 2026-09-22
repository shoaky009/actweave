//! Compile-time plugin boundary. No Core dependency and no dynamic-library ABI.
pub use adapter_api::*;
pub use automation;
use std::{collections::BTreeMap, future::Future, pin::Pin, sync::Arc};

/// Host-owned diagnostics supplied when creating a plugin instance.
#[derive(Clone)]
pub struct Host {
    log: Arc<dyn Fn(&str) + Send + Sync>,
}
impl Default for Host {
    fn default() -> Self {
        Self::new(|_| {})
    }
}
impl Host {
    pub fn new(log: impl Fn(&str) + Send + Sync + 'static) -> Self {
        Self { log: Arc::new(log) }
    }
    pub fn info(&self, message: &str) {
        (self.log)(message);
    }
}

type ExecuteFuture<'a> = Pin<Box<dyn Future<Output = Result<ActionReport, AdapterError>> + 'a>>;
trait Instance {
    fn observe(&self) -> Result<AppState, AdapterError>;
    fn context(&self, context: &SkillContext<'_>) -> Result<DecisionContext, AdapterError>;
    fn execute<'a>(
        &'a mut self,
        call: &'a ToolCall,
        control: &'a ExecutionControl,
    ) -> ExecuteFuture<'a>;
}
impl<T: Adapter> Instance for T {
    fn observe(&self) -> Result<AppState, AdapterError> {
        Adapter::observe(self)
    }
    fn context(&self, context: &SkillContext<'_>) -> Result<DecisionContext, AdapterError> {
        self.decision_context(context)
    }
    fn execute<'a>(
        &'a mut self,
        call: &'a ToolCall,
        control: &'a ExecutionControl,
    ) -> ExecuteFuture<'a> {
        Box::pin(Adapter::execute(self, call, control))
    }
}
/// Type-erased instance still implements the shared adapter contract.
pub struct RegisteredAdapter(Box<dyn Instance>);
impl Adapter for RegisteredAdapter {
    fn observe(&self) -> Result<AppState, AdapterError> {
        self.0.observe()
    }
    fn decision_context(
        &self,
        context: &SkillContext<'_>,
    ) -> Result<DecisionContext, AdapterError> {
        self.0.context(context)
    }
    async fn execute(
        &mut self,
        call: &ToolCall,
        control: &ExecutionControl,
    ) -> Result<ActionReport, AdapterError> {
        self.0.execute(call, control).await
    }
}
type Factory = Box<dyn Fn(Host) -> Result<RegisteredAdapter, AdapterError>>;
#[derive(Default)]
pub struct Registry {
    factories: BTreeMap<String, Factory>,
}
impl Registry {
    pub fn register<G: Adapter + 'static>(
        &mut self,
        name: &str,
        factory: impl Fn(Host) -> Result<G, AdapterError> + 'static,
    ) -> Result<(), AdapterError> {
        if name.trim().is_empty() || self.factories.contains_key(name) {
            return Err(AdapterError::Invalid(format!(
                "duplicate or empty adapter: {name}"
            )));
        }
        self.factories.insert(
            name.into(),
            Box::new(move |host| factory(host).map(|g| RegisteredAdapter(Box::new(g)))),
        );
        Ok(())
    }
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.factories.keys().map(String::as_str)
    }
    pub fn create(&self, name: &str, host: Host) -> Result<RegisteredAdapter, AdapterError> {
        self.factories
            .get(name)
            .ok_or_else(|| AdapterError::Invalid(format!("unknown adapter: {name}")))?(host)
    }
}
