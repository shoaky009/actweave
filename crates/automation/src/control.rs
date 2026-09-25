//! Bounded, one-step approach decisions over observations supplied by an adapter.
use crate::Error;

/// Stable identity assigned by the adapter for one target within an approach attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TargetRef(pub u64);

#[derive(Debug, Clone)]
pub struct ApproachGoal {
    pub target: TargetRef,
}

/// Values are measured and normalized by the adapter, not inferred by the controller.
#[derive(Debug, Clone, Copy)]
pub struct ObservedTarget {
    pub id: TargetRef,
    /// -1 is the left edge, 0 is the desired center, 1 is the right edge.
    pub horizontal_offset: f32,
    /// Optional adapter-defined distance estimate; lower means closer.
    pub range: Option<f32>,
}

#[derive(Debug, Clone, Default)]
pub struct Observation {
    pub targets: Vec<ObservedTarget>,
}

#[derive(Debug, Clone, Copy)]
pub struct ApproachConfig {
    /// Signed calibration: relative mouse pixels for a full-screen horizontal offset.
    pub turn_pixels_per_offset: f32,
    pub center_tolerance: f32,
    pub max_turn_pixels: i32,
    pub move_ms: u64,
    pub max_missing_observations: u32,
    /// Applied only when the adapter supplies comparable range estimates.
    pub min_range_progress: f32,
    pub max_stalled_moves: u32,
}

impl ApproachConfig {
    fn validate(self) -> Result<(), Error> {
        if !self.turn_pixels_per_offset.is_finite()
            || self.turn_pixels_per_offset == 0.0
            || !self.center_tolerance.is_finite()
            || !(0.0..1.0).contains(&self.center_tolerance)
            || self.max_turn_pixels <= 0
            || self.move_ms == 0
            || !self.min_range_progress.is_finite()
            || self.min_range_progress < 0.0
            || self.max_stalled_moves == 0
        {
            return Err(Error::Invalid("invalid approach configuration".into()));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    TargetLost,
    NoProgress,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Turn { dx: i32 },
    Move { duration_ms: u64 },
    ObserveAgain,
    Blocked(Failure),
}

pub struct ApproachController {
    goal: ApproachGoal,
    config: ApproachConfig,
    missing: u32,
    stalled: u32,
    range_before_move: Option<f32>,
    terminal: Option<Decision>,
}

impl ApproachController {
    pub fn new(goal: ApproachGoal, config: ApproachConfig) -> Result<Self, Error> {
        config.validate()?;
        Ok(Self {
            goal,
            config,
            missing: 0,
            stalled: 0,
            range_before_move: None,
            terminal: None,
        })
    }

    /// Call once per fresh observation. The caller executes a returned command,
    /// then captures and recognizes again before calling `step`.
    pub fn step(&mut self, observation: &Observation) -> Result<Decision, Error> {
        if let Some(terminal) = self.terminal {
            return Ok(terminal);
        }
        let mut target = None;
        for candidate in &observation.targets {
            if candidate.id != self.goal.target {
                continue;
            }
            if target.replace(candidate).is_some() {
                return Err(Error::Invalid("duplicate target observation".into()));
            }
        }
        let Some(target) = target else {
            self.range_before_move = None;
            self.stalled = 0;
            if self.missing >= self.config.max_missing_observations {
                return Ok(self.finish(Decision::Blocked(Failure::TargetLost)));
            }
            self.missing += 1;
            return Ok(Decision::ObserveAgain);
        };
        if !target.horizontal_offset.is_finite()
            || !(-1.0..=1.0).contains(&target.horizontal_offset)
            || target
                .range
                .is_some_and(|range| !range.is_finite() || range < 0.0)
        {
            return Err(Error::Invalid("invalid target observation".into()));
        }
        self.missing = 0;

        match (self.range_before_move.take(), target.range) {
            (Some(before), Some(after)) => {
                if before - after < self.config.min_range_progress {
                    self.stalled += 1;
                    if self.stalled >= self.config.max_stalled_moves {
                        return Ok(self.finish(Decision::Blocked(Failure::NoProgress)));
                    }
                } else {
                    self.stalled = 0;
                }
            }
            _ => self.stalled = 0,
        }
        if target.horizontal_offset.abs() > self.config.center_tolerance {
            let raw = (target.horizontal_offset * self.config.turn_pixels_per_offset).round();
            let dx = (raw as i32).clamp(-self.config.max_turn_pixels, self.config.max_turn_pixels);
            Ok(Decision::Turn {
                dx: if dx == 0 {
                    (target.horizontal_offset * self.config.turn_pixels_per_offset).signum() as i32
                } else {
                    dx
                },
            })
        } else {
            Ok(Decision::Move {
                duration_ms: self.config.move_ms,
            })
        }
    }

    /// Record evidence for progress only after the platform confirms the input finished.
    pub fn move_completed(&mut self, observed_range: Option<f32>) {
        self.range_before_move = observed_range;
    }

    /// An interrupted input may have taken effect; discard comparisons with old frames.
    pub fn input_interrupted(&mut self) {
        self.range_before_move = None;
        self.stalled = 0;
    }

    fn finish(&mut self, decision: Decision) -> Decision {
        self.terminal = Some(decision);
        decision
    }
}
