//! Discover only enabled, unambiguous skills that the shared engine can execute.
use claude_core::{
    runtime::RuntimePaths,
    skills::{Skill, SkillsManager},
};
use std::collections::{BTreeMap, HashMap, HashSet};

pub async fn load(paths: &RuntimePaths) -> anyhow::Result<BTreeMap<String, String>> {
    let skills = SkillsManager::with_path(paths.data_dir.join("skills"))
        .list_enabled_skills()
        .await?;
    Ok(from_skills(&skills))
}

fn from_skills(skills: &[Skill]) -> BTreeMap<String, String> {
    let aliases = |skill: &Skill| {
        let mut names = HashSet::new();
        names.insert(skill.id.clone());
        names.insert(
            skill
                .name
                .to_lowercase()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join("-"),
        );
        names
    };
    let mut counts = HashMap::<String, usize>::new();
    for skill in skills.iter().filter(|s| s.enabled) {
        for name in aliases(skill) {
            *counts.entry(name).or_default() += 1;
        }
    }
    let mut commands = BTreeMap::new();
    for skill in skills.iter().filter(|s| s.enabled && s.content.is_some()) {
        let slug = skill
            .name
            .to_lowercase()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join("-");
        for name in [slug, skill.id.clone()] {
            let command = format!("/{name}");
            if !name.is_empty()
                && !name
                    .chars()
                    .any(|c| c.is_whitespace() || c.is_control() || c == '/')
                && counts.get(&name) == Some(&1)
                && !crate::command_catalog::COMMANDS
                    .iter()
                    .any(|(builtin, _)| *builtin == command)
            {
                commands.insert(command, format!("[skill] {}", skill.description));
                break;
            }
        }
    }
    commands
}

pub fn catalog(skills: &BTreeMap<String, String>) -> Vec<(String, String)> {
    crate::command_catalog::COMMANDS
        .iter()
        .map(|(name, description)| (name.to_string(), description.to_string()))
        .chain(
            skills
                .iter()
                .map(|(name, description)| (name.clone(), description.clone())),
        )
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use claude_core::skills::SkillSource;

    fn skill(id: &str, name: &str, enabled: bool) -> Skill {
        Skill {
            id: id.into(),
            name: name.into(),
            description: "Check the project".into(),
            content: Some("Instructions".into()),
            when_to_use: None,
            enabled,
            source: SkillSource::User,
            loaded_from: String::new(),
            source_dir: None,
            is_example: false,
            files: vec![],
            created_at: None,
            od_metadata: None,
        }
    }

    #[test]
    fn discovery_avoids_disabled_ambiguous_and_builtin_commands() {
        let skills = vec![
            skill("one", "Review Code", true),
            skill("two", "Review Code", true),
            skill("three", "help", true),
            skill("four", "Hidden", false),
            skill("five", "Verify", true),
        ];
        let commands = from_skills(&skills);
        assert!(commands.contains_key("/one") && commands.contains_key("/two"));
        assert!(!commands.contains_key("/review-code"));
        assert!(commands.contains_key("/three") && !commands.contains_key("/help"));
        assert!(commands.contains_key("/verify") && !commands.contains_key("/hidden"));
    }
}
