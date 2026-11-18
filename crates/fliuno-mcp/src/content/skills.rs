//! Validate remote skill metadata without loading any of its resource URIs.
use super::arguments::valid_uri;
use rmcp::model::{Skill, SkillResources, validate_skills};

fn within_budget(skill: &Skill) -> bool {
    valid_uri(&skill.uri)
        && match &skill.resources {
            SkillResources::Files(files) => {
                files.len() <= 256 && files.iter().all(|file| valid_uri(&file.uri))
            }
            SkillResources::Dynamic => true,
            _ => false,
        }
}

pub(super) fn valid_list(skills: &[Skill]) -> bool {
    skills.len() <= 256 && skills.iter().all(within_budget) && validate_skills(skills).is_ok()
}

pub(super) fn valid_get(skill: &Skill) -> bool {
    within_budget(skill) && skill.validate().is_ok()
}
