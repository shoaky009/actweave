//! Task-local skill exposure. Selection comes from the agent; availability comes from adapters.
use crate::core::{Availability, Error, Skill};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum SkillMode {
    #[default]
    All,
    OnDemand,
}

/// Exact names and tags are combined with OR. Each successful request replaces the selection.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoadSkills {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub names: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}
#[derive(Debug, Clone, Serialize)]
pub struct SkillSummary {
    pub name: String,
    pub description: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    pub availability: Availability,
}
#[derive(Debug, Clone, Serialize)]
pub struct LoadResult {
    pub request: LoadSkills,
    pub success: bool,
    pub selected: Vec<String>,
    pub message: String,
}
/// Directory contains only skills whose full definition is not currently injected.
#[derive(Debug, Serialize)]
pub struct SkillView {
    #[serde(skip_serializing_if = "String::is_empty")]
    pub guidance: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<crate::core::ExecutionFailure>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution: Option<crate::execution::ExecutionProgress>,
    pub max_actions: usize,
    /// Definitions selected for planning, including currently unavailable skills.
    #[serde(skip)]
    pub catalog: Vec<Skill>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub batch: Option<crate::batch::BatchProgress>,
    pub max_repeat: u32,
    pub discovery_enabled: bool,
    pub directory: Vec<SkillSummary>,
    pub loaded: Vec<Skill>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_load: Option<LoadResult>,
}

pub(crate) fn view(
    skills: &[Skill],
    mode: SkillMode,
    selected: &BTreeSet<String>,
    last_load: Option<LoadResult>,
) -> Result<SkillView, Error> {
    let mut names = BTreeSet::new();
    let mut directory = vec![];
    let mut loaded = vec![];
    for skill in skills {
        if skill.name.trim().is_empty() || !names.insert(&skill.name) {
            return Err(Error::Invalid(
                "skill names must be nonempty and unique".into(),
            ));
        }
        if skill.availability.is_available()
            && (mode == SkillMode::All || selected.contains(&skill.name))
        {
            loaded.push(skill.clone());
        } else {
            directory.push(SkillSummary {
                name: skill.name.clone(),
                description: skill.description.clone(),
                tags: skill.tags.clone(),
                availability: skill.availability.clone(),
            });
        }
    }
    Ok(SkillView {
        guidance: String::new(),
        failure: None,
        execution: None,
        max_actions: 4,
        catalog: skills
            .iter()
            .filter(|s| mode == SkillMode::All || selected.contains(&s.name))
            .cloned()
            .collect(),
        batch: None,
        max_repeat: crate::batch::MAX_REPEAT,
        discovery_enabled: mode == SkillMode::OnDemand,
        directory,
        loaded,
        last_load,
    })
}

pub(crate) fn select(request: &LoadSkills, skills: &[Skill]) -> Result<BTreeSet<String>, String> {
    if request.names.is_empty() && request.tags.is_empty() {
        return Err("specify at least one skill name or tag".into());
    }
    for name in &request.names {
        if !skills.iter().any(|s| &s.name == name) {
            return Err(format!("unknown skill name: {name}"));
        }
    }
    for tag in &request.tags {
        if !skills.iter().any(|s| s.tags.contains(tag)) {
            return Err(format!("unknown skill tag: {tag}"));
        }
    }
    Ok(skills
        .iter()
        .filter(|s| {
            request.names.contains(&s.name) || request.tags.iter().any(|tag| s.tags.contains(tag))
        })
        .map(|s| s.name.clone())
        .collect())
}
