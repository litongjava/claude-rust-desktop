//! Embedded workflow discovery; resource bundles are materialized on invocation.
use super::{discovery::SkillCommand, Skill, SkillFile, SkillSource};
use std::{collections::BTreeMap, path::Path};

pub(super) const TASK_TEMPLATE_NAME: &str = "task-template";
pub(super) const TASK_TEMPLATE_ID: &str = "embedded:task-template";
pub(super) const TASK_TEMPLATE_DOCUMENT: &str = include_str!("bundled/task-template.md");
const TASK_TEMPLATE_DESCRIPTION: &str = "Prepare a brief after deciding to delegate substantial work through Agent, to a teammate, or to a background agent. Skip ordinary background shell commands.";

pub(super) fn task_template_skill() -> Skill {
    Skill {
        id: TASK_TEMPLATE_ID.into(),
        name: TASK_TEMPLATE_NAME.into(),
        description: TASK_TEMPLATE_DESCRIPTION.into(),
        content: Some(TASK_TEMPLATE_DOCUMENT.into()),
        when_to_use: None,
        enabled: true,
        source: SkillSource::Bundled,
        loaded_from: "bundled".into(),
        source_dir: None,
        is_example: false,
        files: vec![SkillFile {
            name: "SKILL.md".into(),
            file_type: "file".into(),
            children: None,
        }],
        created_at: None,
        od_metadata: None,
    }
}

pub fn commands() -> BTreeMap<String, SkillCommand> {
    let definitions = [
        (
            "code-review",
            "Review a diff for actionable correctness and security issues",
            include_str!("bundled/code-review.md"),
        ),
        (
            "security-review",
            "Review pending changes for exploitable security vulnerabilities",
            include_str!("bundled/security-review.md"),
        ),
        (
            "dataviz",
            "Create charts and data visualizations, choose chart forms, and validate palettes, contrast and interaction using runnable reference assets.",
            include_str!("bundled/dataviz.md"),
        ),
        (
            "deep-research",
            "Research a question using cited, verified sources",
            include_str!("bundled/deep-research.md"),
        ),
        (
            "doctor",
            "Inspect local CLI configuration and diagnose failures; /doctor prompt-audit audits the instruction files loaded into sessions; /doctor skills and /doctor errors answer without the model",
            include_str!("bundled/doctor.md"),
        ),
        (
            TASK_TEMPLATE_NAME,
            TASK_TEMPLATE_DESCRIPTION,
            TASK_TEMPLATE_DOCUMENT,
        ),
        // The reference CLI's bundled workflow commands.
        (
            "simplify",
            "Clean up the changed code without changing behavior",
            include_str!("bundled/simplify.md"),
        ),
        (
            "fewer-permission-prompts",
            "Pre-approve safe read-only commands for this project",
            include_str!("bundled/fewer-permission-prompts.md"),
        ),
        (
            "update-config",
            "Change settings: hooks and permission rules",
            include_str!("bundled/update-config.md"),
        ),
        (
            "insights",
            "Generate a report analyzing your DeepSeek Code sessions",
            include_str!("bundled/insights.md"),
        ),
        (
            "team-onboarding",
            "Help teammates ramp up on DeepSeek Code with a guide for this project",
            include_str!("bundled/team-onboarding.md"),
        ),
        (
            "run-skill-generator",
            "Create a skill that knows how to run this project's app",
            include_str!("bundled/run-skill-generator.md"),
        ),
        (
            super::skill_creator::NAME,
            super::skill_creator::DESCRIPTION,
            include_str!("bundled/skill-creator.md"),
        ),
        (
            "auto-mode-setup",
            "Prepare auto mode for your environment, plus permission rules",
            include_str!("bundled/auto-mode-setup.md"),
        ),
        // User-only by default (see `states::default_state`): hidden from the
        // model listing until the user changes their state to `user`.
        (
            "translate-zh-en",
            "Translate Chinese text into English",
            include_str!("bundled/translate-zh-en.md"),
        ),
        (
            "translate-en-zh",
            "Translate English text into Chinese",
            include_str!("bundled/translate-en-zh.md"),
        ),
    ];
    definitions
        .into_iter()
        .map(|(name, description, body)| {
            let content = if body.lines().next() == Some("---") {
                super::discovery::frontmatter(body)
                    .expect("Embedded skill frontmatter must be valid")
                    .1
            } else {
                body.to_owned()
            };
            // Discovery runs during SDK turns while the input reader owns stdin.
            // Keep diagnostics lazy: terminal probing here can block on that lock
            // until the SDK client closes its stream. /doctor runs the checks.
            (
                name.into(),
                SkillCommand {
                    name: name.into(),
                    description: description.into(),
                    path: Path::new("__embedded_cli_skills__").join(name),
                    content,
                    user_invocable: true,
                    // The inline task template is model-invocable by default.
                    // Translation skills opt in through their configurable state.
                    disable_model_invocation: !super::bundled_resources::supports(name)
                        && !matches!(
                            name,
                            TASK_TEMPLATE_NAME | "translate-zh-en" | "translate-en-zh"
                        ),
                    listing_name_only: false,
                    context: None,
                    shell: None,
                    agent: None,
                },
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_template_frontmatter_matches_the_embedded_command() {
        let (metadata, body) =
            super::super::discovery::frontmatter(TASK_TEMPLATE_DOCUMENT).unwrap();
        let commands = super::commands();
        let command = &commands["task-template"];
        assert_eq!(metadata["name"].as_str(), Some(command.name.as_str()));
        assert_eq!(
            metadata["description"].as_str(),
            Some(command.description.as_str())
        );
        assert_eq!(command.content, body);
    }

    #[test]
    fn task_template_is_discovered_and_loads_the_delegation_brief() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        let (commands, invalid) =
            super::super::discovery::discover_runtime_from(&data, temp.path());
        assert!(invalid.is_empty(), "{invalid:?}");
        let command = &commands["task-template"];
        assert!(command.model_invocable());
        assert!(command.user_invocable);
        assert_eq!(
            command.path,
            std::path::Path::new("__embedded_cli_skills__").join("task-template")
        );
        let rendered = command.render("inspect the fixture", "session").unwrap();
        assert!(rendered.contains("inspect the fixture"));
        for text in [&command.description, &rendered] {
            assert!(text
                .to_ascii_lowercase()
                .contains("after deciding to delegate substantial work"));
            for trigger in [
                "Agent",
                "teammate",
                "background agent",
                "Skip ordinary background shell commands.",
            ] {
                assert!(text.contains(trigger), "{text}");
            }
            assert!(!text.contains("Agent/Task"), "{text}");
            assert!(!text.contains("background job"), "{text}");
        }
        for section in [
            "Problem",
            "Exact change",
            "How to verify",
            "Report to write",
        ] {
            assert!(rendered.contains(&format!("**{section}**")), "{rendered}");
        }
        assert!(rendered.contains("requirement → evidence → unverified"));
        assert!(!data.exists());
    }

    #[tokio::test]
    async fn task_template_inventory_serves_a_read_only_document_and_honors_user_override() {
        let temp = tempfile::tempdir().unwrap();
        let skills_dir = temp.path().join("skills");
        let manager = super::super::SkillsManager::with_path(skills_dir.clone());
        let skills = manager.load_skills().await.unwrap();
        let matching: Vec<_> = skills
            .iter()
            .filter(|skill| skill.name == TASK_TEMPLATE_NAME)
            .collect();
        assert_eq!(matching.len(), 1);
        let skill = matching[0];
        assert_eq!(skill.id, TASK_TEMPLATE_ID);
        assert_eq!(skill.source, SkillSource::Bundled);
        assert!(skill.enabled && skill.source_dir.is_none());
        assert_eq!(skill.files.len(), 1);
        assert_eq!(skill.files[0].name, "SKILL.md");
        assert_eq!(skill.content.as_deref(), Some(TASK_TEMPLATE_DOCUMENT));
        assert_eq!(
            manager.get_skill_content(&skill.id, "SKILL.md").unwrap(),
            TASK_TEMPLATE_DOCUMENT
        );
        assert!(manager.get_skill_content(&skill.id, "../SKILL.md").is_err());
        assert!(manager.get_skill_content(&skill.id, "missing.md").is_err());
        assert!(manager.update_skill(&skill.id, Default::default()).is_err());
        assert!(manager.delete_skill(&skill.id).is_err());
        assert!(manager.toggle_skill(&skill.id).await.is_err());
        assert!(!skills_dir.join("bundled").join(TASK_TEMPLATE_NAME).exists());

        let custom = manager
            .create_skill(TASK_TEMPLATE_NAME, "Custom template", "Custom brief")
            .unwrap();
        let skills = manager.load_skills().await.unwrap();
        let matching: Vec<_> = skills
            .iter()
            .filter(|skill| skill.name == TASK_TEMPLATE_NAME)
            .collect();
        assert_eq!(matching.len(), 1);
        assert_eq!(matching[0].id, custom.id);
        assert_eq!(matching[0].source, SkillSource::User);
    }

    #[test]
    fn embedded_workflows_render_arguments_without_nonexistent_file_references() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("Cargo.toml"), "[package]").unwrap();
        let commands = super::super::discovery::discover_from(temp.path()).unwrap();
        for name in [
            "code-review",
            "dataviz",
            "deep-research",
            "doctor",
            "security-review",
            "skill-creator",
        ] {
            let command = &commands[name];
            let rendered = command
                .render("inspect my project", "session-test")
                .unwrap();
            assert!(rendered.contains("inspect my project"));
            assert!(!rendered.contains("__embedded_cli_skills__"));
            assert_eq!(command.disable_model_invocation, !super::super::bundled_resources::supports(name));
        }
        assert!(!commands.contains_key("claude-api"));
        assert!(!commands["doctor"].content.contains("Read-only local diagnostic snapshot:"));
    }
    #[test]
    fn translation_skills_discover_as_user_only_until_set_to_user() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let data = temp.path().join("data");

        let (commands, invalid) =
            super::super::discovery::discover_runtime_from(&data, &workspace);
        assert!(invalid.is_empty(), "{invalid:?}");
        for name in ["translate-zh-en", "translate-en-zh"] {
            let command = &commands[name];
            assert!(command.disable_model_invocation, "{name}");
            assert!(command.user_invocable, "{name}");
        }

        crate::skills::states::set(
            &data,
            "translate-zh-en",
            crate::skills::states::SkillState::User,
        )
        .unwrap();
        let (commands, invalid) =
            super::super::discovery::discover_runtime_from(&data, &workspace);
        assert!(invalid.is_empty(), "{invalid:?}");
        assert!(!commands["translate-zh-en"].disable_model_invocation);
        assert!(commands["translate-zh-en"].user_invocable);
        assert!(commands["translate-en-zh"].disable_model_invocation);
    }

    #[test]
    fn project_workflow_overrides_embedded_default() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join(".claude/commands");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("code-review.md"), "Custom review $ARGUMENTS").unwrap();
        let commands = super::super::discovery::discover_from(temp.path()).unwrap();
        assert!(commands["code-review"]
            .render("HEAD", "id")
            .unwrap()
            .contains("Custom review HEAD"));
    }
}
