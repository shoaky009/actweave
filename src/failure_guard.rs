//! Task-local repeated failures; no interpretation of adapter state.
use adapter_api::{ActionOutcome, ActionReport, AdapterError, ToolCall};
use serde_json::Value;

#[derive(Default)]
pub(crate) struct FailureGuard {
    calls: Vec<(Value, u32)>,
    plans: Vec<(Value, u32)>,
}

fn record(entries: &mut Vec<(Value, u32)>, key: Value, failed: bool, limit: u32) -> bool {
    let index = entries.iter().position(|(existing, _)| *existing == key);
    if !failed {
        if let Some(index) = index {
            entries.swap_remove(index);
        }
        return false;
    }
    let count = if let Some(index) = index {
        entries[index].1 = entries[index].1.saturating_add(1);
        entries[index].1
    } else {
        entries.push((key, 1));
        1
    };
    count >= limit
}

impl FailureGuard {
    pub fn action(
        &mut self,
        call: &ToolCall,
        result: &Result<ActionReport, AdapterError>,
        limit: u32,
    ) -> Option<String> {
        if matches!(
            result,
            Err(AdapterError::Cancelled | AdapterError::TimedOut)
        ) {
            return None;
        }
        let failed = !matches!(result, Ok(report) if matches!(report.outcome,
            ActionOutcome::Completed { .. } | ActionOutcome::Continue { .. }));
        let key = serde_json::json!({"name": call.name, "arguments": call.arguments});
        record(&mut self.calls, key, failed, limit).then(|| {
            format!(
                "重复失败保护：Skill '{}' 的相同参数已失败 {limit} 次，停止任务",
                call.name
            )
        })
    }

    pub fn plan(&mut self, key: Value, rejected: bool, limit: u32) -> Option<String> {
        record(&mut self.plans, key, rejected, limit)
            .then(|| format!("重复失败保护：相同计划已被拒绝 {limit} 次，停止任务"))
    }
}
