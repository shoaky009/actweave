//! In-memory training program. Owns domain rules and elapsed time, never task decisions.
use std::time::{Duration, Instant};

/// Select a repeatable demo environment, not an execution plan.
#[derive(Debug, Default, Clone, Copy)]
pub enum Scenario {
    #[default]
    Normal,
    TransientFailure,
    Blocked,
    BatchInterrupted,
    BatchBlocked,
    Exploration,
    ExplorationInterrupted,
}

#[derive(Default)]
pub struct DemoRuntime {
    mode: Mode,
    scenario: Scenario,
    transient_consumed: bool,
    started_at: Option<Instant>,
    trial_completed: u32,
    trial_successful: u32,
    trial_needs_reset: bool,
    trial_interruption_used: bool,
    exploration: ExplorationSnapshot,
    exploration_interruption_used: bool,
}

/// Only observed progress is exposed, not the number of undiscovered demo items.
#[derive(Debug, Default, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct ExplorationSnapshot {
    pub collected: u32,
    pub finished: bool,
    pub needs_reset: bool,
}

#[derive(Debug, Default, Clone, Copy, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    Idle,
    Training,
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TrainingStatus {
    NotStarted,
    Running,
    Completed,
}

/// Concrete program errors; the adapter translates them into tool feedback.
#[derive(Debug, PartialEq, Eq)]
pub enum RuntimeError {
    WrongMode,
    Busy,
    Unavailable,
    AlreadyStarted,
    TrainingInProgress,
    TrialNeedsReset,
    TrialUnavailable,
    ExplorationNeedsReset,
}

impl DemoRuntime {
    pub fn exploration_snapshot(&self) -> Option<ExplorationSnapshot> {
        matches!(
            self.scenario,
            Scenario::Exploration | Scenario::ExplorationInterrupted
        )
        .then_some(self.exploration)
    }
    /// The runtime owns discovery and the termination condition, not the host.
    pub fn explore_step(&mut self) -> Result<bool, RuntimeError> {
        if self.exploration_snapshot().is_none() {
            return Err(RuntimeError::Unavailable);
        }
        if self.exploration.needs_reset {
            return Err(RuntimeError::ExplorationNeedsReset);
        }
        if matches!(self.scenario, Scenario::ExplorationInterrupted)
            && self.exploration.collected == 1
            && !self.exploration_interruption_used
        {
            self.exploration_interruption_used = true;
            self.exploration.needs_reset = true;
            return Err(RuntimeError::ExplorationNeedsReset);
        }
        if self.exploration.collected == 3 {
            self.exploration.finished = true;
            return Ok(true);
        }
        self.exploration.collected += 1;
        Ok(false)
    }
    pub fn reset_exploration(&mut self) -> Result<(), RuntimeError> {
        if !self.exploration.needs_reset {
            return Err(RuntimeError::Unavailable);
        }
        self.exploration.needs_reset = false;
        Ok(())
    }
    pub fn new(scenario: Scenario) -> Self {
        Self {
            scenario,
            ..Self::default()
        }
    }
    pub fn mode(&self) -> Mode {
        self.mode
    }
    pub fn available(&self) -> bool {
        !matches!(self.scenario, Scenario::Blocked)
    }
    pub fn set_mode(&mut self, mode: Mode) -> Result<(), RuntimeError> {
        if mode != self.mode && self.training_status() == TrainingStatus::Running {
            return Err(RuntimeError::TrainingInProgress);
        }
        self.mode = mode;
        Ok(())
    }
    /// Training completes after elapsed time; observing it does not advance or mutate it.
    pub fn training_status(&self) -> TrainingStatus {
        match self.started_at {
            None => TrainingStatus::NotStarted,
            Some(start) if start.elapsed() < Duration::from_millis(500) => TrainingStatus::Running,
            Some(_) => TrainingStatus::Completed,
        }
    }
    pub fn trial_snapshot(&self) -> (u32, u32, bool, bool) {
        (
            self.trial_completed,
            self.trial_successful,
            !self.trial_needs_reset && !matches!(self.scenario, Scenario::BatchBlocked),
            self.trial_needs_reset,
        )
    }
    pub fn perform_trial(&mut self) -> Result<bool, RuntimeError> {
        if matches!(self.scenario, Scenario::BatchBlocked) {
            return Err(RuntimeError::TrialUnavailable);
        }
        if self.trial_needs_reset {
            return Err(RuntimeError::TrialNeedsReset);
        }
        if matches!(self.scenario, Scenario::BatchInterrupted)
            && self.trial_completed == 3
            && !self.trial_interruption_used
        {
            self.trial_interruption_used = true;
            self.trial_needs_reset = true;
            return Err(RuntimeError::TrialNeedsReset);
        }
        self.trial_completed += 1;
        let successful = !self.trial_completed.is_multiple_of(3);
        self.trial_successful += u32::from(successful);
        Ok(successful)
    }
    pub fn reset_trial(&mut self) {
        self.trial_needs_reset = false;
    }

    pub fn start_training(&mut self) -> Result<(), RuntimeError> {
        if !self.available() {
            return Err(RuntimeError::Unavailable);
        }
        if self.mode != Mode::Training {
            return Err(RuntimeError::WrongMode);
        }
        if self.started_at.is_some() {
            return Err(RuntimeError::AlreadyStarted);
        }
        if matches!(self.scenario, Scenario::TransientFailure) && !self.transient_consumed {
            self.transient_consumed = true;
            return Err(RuntimeError::Busy);
        }
        self.started_at = Some(Instant::now());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preconditions_and_single_transient_failure_preserve_training_state() {
        let mut runtime = DemoRuntime::new(Scenario::TransientFailure);
        assert_eq!(runtime.start_training(), Err(RuntimeError::WrongMode));
        runtime.set_mode(Mode::Training).unwrap();
        assert_eq!(runtime.start_training(), Err(RuntimeError::Busy));
        assert_eq!(runtime.training_status(), TrainingStatus::NotStarted);
        runtime.start_training().unwrap();
        assert_eq!(runtime.training_status(), TrainingStatus::Running);
        assert_eq!(
            runtime.set_mode(Mode::Idle),
            Err(RuntimeError::TrainingInProgress)
        );
        assert_eq!(runtime.start_training(), Err(RuntimeError::AlreadyStarted));
        runtime.started_at = Some(Instant::now() - Duration::from_secs(1));
        assert_eq!(runtime.training_status(), TrainingStatus::Completed);
        assert_eq!(runtime.training_status(), TrainingStatus::Completed);
    }
    #[test]
    fn blocked_training_never_starts() {
        let mut runtime = DemoRuntime::new(Scenario::Blocked);
        runtime.set_mode(Mode::Training).unwrap();
        for _ in 0..3 {
            assert_eq!(runtime.start_training(), Err(RuntimeError::Unavailable));
        }
        assert_eq!(runtime.training_status(), TrainingStatus::NotStarted);
    }
}
