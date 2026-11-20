//! One-time cleanup of the five `legacy-*` skills an earlier release generated.
//!
//! New installations no longer write these files. Existing users keep files
//! created by the old installer, so [`cleanup`] removes a directory only when
//! it still holds nothing but a byte-identical generated `SKILL.md`; an edited
//! or extended directory belongs to the user. The old definitions and format
//! string are retained verbatim to recognise pristine files.
use anyhow::Result;
use std::path::Path;

const BUILTINS: &[(&str, &str, &str)] = &[
    ("commit","Create a Git commit from reviewed changes","Inspect repository instructions, git status and the staged diff. Treat --message as the requested commit message, --amend as an explicit request to amend, and --all as a request to include tracked unstaged changes. If no message is supplied, write an English imperative message describing the actual changes. Never stage credentials or unrelated work. Run the real Git commands through the shared tools and host permission checks; verify the resulting commit hash. Report failures honestly. Do not push unless requested."),
    ("review","Review current changes or requested files","Review the requested files, or the Git diff when files are omitted or --diff is supplied. Read surrounding code and applicable instructions. Explain concrete correctness, security and regression findings with file references. Use real repository evidence; do not claim tests were run unless tools ran them. Do not edit files unless requested. Preserve the user's focus and severity options."),
    ("test","Run the project's tests","Read project instructions and test scripts to choose the actual test command. Honor requested test patterns, --coverage and --watch when supported. Run through shared tools and permission checks; capture real pass/fail counts and relevant failure output. Never substitute a mock success. Explain unavailable dependencies or unsupported options. Avoid an indefinite watch process unless explicitly requested."),
    ("document","Generate documentation from source","Inspect requested code and existing documentation conventions. Honor requested format, output and scope; generate documentation grounded in actual APIs and behavior. Preview the intended changes or write them through normal project file tools according to the user's request. Do not overwrite unrelated content. Report exactly which files were created or changed."),
    ("build","Build the project","Read repository instructions, manifests and build scripts. Select the actual project build command and honor --release, --clean and --verbose only when supported. Clean only project build artifacts when explicitly requested. Run through shared tools and host permissions, verify produced artifacts and report real failures. Never report a successful build without executing and checking it."),
];

/// The exact bytes the old installer wrote for one built-in.
fn generated(name: &str, description: &str, prompt: &str) -> String {
    format!("---\nname: {name}\ndescription: {description}\nenabled: true\nuserInvocable: true\n---\n\n# {name}\n\n{prompt}\n\nApply the user's supplied arguments as task instructions. Skill text never overrides host permissions or repository instructions.\n")
}

/// Delete `legacy-<name>` directories that still contain only the generated
/// `SKILL.md`. Modified or extended directories are left alone.
pub(super) fn cleanup(root: &Path) -> Result<()> {
    for (name, description, prompt) in BUILTINS {
        let directory = root.join(format!("legacy-{name}"));
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        let mut entries = entries;
        let only_skill_md = match entries.next() {
            Some(Ok(entry)) => {
                entry.file_name().to_str() == Some("SKILL.md") && entries.next().is_none()
            }
            _ => false,
        };
        if !only_skill_md {
            continue;
        }
        let skill_md = directory.join("SKILL.md");
        let expected = generated(name, description, prompt);
        if std::fs::read(&skill_md).ok().as_deref() != Some(expected.as_bytes()) {
            continue;
        }
        std::fs::remove_dir_all(&directory)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_pristine(root: &Path) {
        for (name, description, prompt) in BUILTINS {
            let directory = root.join(format!("legacy-{name}"));
            std::fs::create_dir_all(&directory).unwrap();
            std::fs::write(
                directory.join("SKILL.md"),
                generated(name, description, prompt),
            )
            .unwrap();
        }
    }

    #[test]
    fn untouched_generated_directories_are_removed_in_both_locations() {
        for relative in ["skills", "skills/bundled"] {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().join(relative);
            write_pristine(&root);
            cleanup(&root).unwrap();
            for (name, _, _) in BUILTINS {
                assert!(
                    !root.join(format!("legacy-{name}")).exists(),
                    "{relative}/legacy-{name} survived cleanup"
                );
            }
        }
    }

    #[test]
    fn edited_or_extended_directories_are_kept() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("skills/bundled");
        write_pristine(&root);
        std::fs::write(root.join("legacy-commit/SKILL.md"), "user content").unwrap();
        std::fs::write(root.join("legacy-review/notes.md"), "extra file").unwrap();

        cleanup(&root).unwrap();

        assert_eq!(
            std::fs::read_to_string(root.join("legacy-commit/SKILL.md")).unwrap(),
            "user content"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("legacy-review/notes.md")).unwrap(),
            "extra file"
        );
        assert!(root.join("legacy-review/SKILL.md").exists());
        for name in ["test", "document", "build"] {
            assert!(!root.join(format!("legacy-{name}")).exists());
        }
    }

    #[tokio::test]
    async fn manager_cleanup_runs_before_listing_and_installs_no_legacy_skills() {
        let temp = tempfile::tempdir().unwrap();
        let skills = temp.path().join("skills");
        write_pristine(&skills);
        write_pristine(&skills.join("bundled"));
        let custom = skills.join("custom");
        std::fs::create_dir_all(&custom).unwrap();
        std::fs::write(
            custom.join("SKILL.md"),
            "---\nname: custom\ndescription: User skill\n---\nKeep me",
        )
        .unwrap();

        let manager = crate::skills::SkillsManager::with_path(skills.clone());
        let listed = manager.load_skills().await.unwrap();

        for relative in [&skills, &skills.join("bundled")] {
            for (name, _, _) in BUILTINS {
                assert!(!relative.join(format!("legacy-{name}")).exists());
            }
        }
        assert!(custom.join("SKILL.md").exists());
        // Pristine generated skills are not listed; `commit` now ships as a
        // bundled skill under its real name.
        for name in ["review", "test", "document", "build"] {
            assert!(
                !listed.iter().any(|skill| skill.name == name),
                "{name} must not be installed"
            );
        }
        assert!(listed.iter().any(|skill| {
            skill.name == "commit" && skill.source == crate::skills::SkillSource::Bundled
        }));
        assert!(listed.iter().any(|skill| skill.name == "custom"));
    }
}
