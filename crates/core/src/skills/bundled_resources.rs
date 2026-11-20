//! Installed built-in skill asset directories.
//!
//! Components that ship extra references or scripts (dataviz, skill-creator)
//! are installed under `<data>/skills/bundled/<name>/`. Invocation points at
//! that directory, and the Read tool serves registered asset names from it even
//! when it sits outside the workspace.
use anyhow::{ensure, Context, Result};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, LazyLock, Mutex, Weak},
};

#[path = "bundled_manifest.rs"]
mod manifest;
pub(crate) use manifest::FILES;

/// One skill directory prepared in this process. The engine and its active
/// children own the resources; the weak registry only exposes them to Read.
struct Bundle {
    name: String,
    directory: PathBuf,
}

static REGISTRY: LazyLock<Mutex<Vec<Weak<Bundle>>>> = LazyLock::new(|| Mutex::new(Vec::new()));

#[derive(Default)]
pub struct Resources {
    bundles: Mutex<HashMap<String, Arc<Bundle>>>,
}

pub fn supports(name: &str) -> bool {
    matches!(name, "dataviz" | "skill-creator")
}

/// Whether `relative` is one of the files installed for `name`.
fn registered(name: &str, relative: &str) -> bool {
    manifest::FILES
        .iter()
        .any(|(skill, path, _)| *skill == name && *path == relative)
}

impl Resources {
    /// Build the invocation prompt for an installed built-in skill. Instructions
    /// are read from disk, so a user-edited file is what the model receives.
    pub fn prepare(
        &self,
        name: &str,
        arguments: &str,
        workspace: &Path,
        directory: &Path,
    ) -> Result<String> {
        ensure!(supports(name), "Unknown bundled resource skill");
        let directory = directory.canonicalize().with_context(|| {
            format!(
                "Bundled skill directory unavailable: {}",
                directory.display()
            )
        })?;
        let instructions_path = directory.join("INSTRUCTIONS.md");
        let instructions = std::fs::read_to_string(&instructions_path)
            .with_context(|| format!("Reading {}", instructions_path.display()))?;
        let mut bundles = self
            .bundles
            .lock()
            .map_err(|_| anyhow::anyhow!("Skill resource cache unavailable"))?;
        let bundle = Arc::new(Bundle {
            name: name.to_owned(),
            directory: directory.clone(),
        });
        let mut registry = REGISTRY
            .lock()
            .map_err(|_| anyhow::anyhow!("Skill resource registry unavailable"))?;
        registry.retain(|entry| entry.strong_count() > 0);
        registry.push(Arc::downgrade(&bundle));
        drop(registry);
        bundles.insert(name.into(), bundle);
        drop(bundles);
        let provenance = if name == "skill-creator" {
            "Bundled skill-creator embedded in DeepSeek Code; no network or prior install is required. Adapted from OpenAI Codex's skill-creator under the Apache-2.0 license; see NOTICE.md and license.txt in the base directory."
        } else {
            "Bundled documentation snapshot: Claude Code 2.1.295."
        };
        Ok(format!(
        "Base directory for this skill: {}\n\n{} Resolve resource paths against the base directory. Use Read for referenced documents. Execute scripts only through normal Bash permissions; invocation itself executes no scripts. After restarting a worker, invoke this skill again to obtain its current resource directory.\n\n{instructions}\n\nCurrent workspace: {}\n\n## User Request\n\n{arguments}",
        directory.display(), provenance, workspace.display()
    ))
    }
}

/// Exact registered filenames only; no directory, glob, alias or traversal
/// grant. Returns the installed file's current bytes.
pub(crate) fn read(path: &Path) -> Option<Vec<u8>> {
    if !path.is_absolute()
        || path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return None;
    }
    let canonical = path.canonicalize().ok()?;
    let registry = REGISTRY.lock().ok()?;
    for bundle in registry.iter().filter_map(Weak::upgrade) {
        let Ok(relative) = canonical.strip_prefix(&bundle.directory) else {
            continue;
        };
        let relative = relative.to_str()?.replace('\\', "/");
        if registered(&bundle.name, &relative) {
            return std::fs::read(&canonical).ok();
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Install the embedded tree the way the SkillsManager does.
    fn installed() -> (TempDir, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let bundled = temp.path().join("skills/bundled");
        super::super::bundled_install::install(&bundled).unwrap();
        (temp, bundled)
    }

    fn embedded(skill: &str, relative: &str) -> &'static [u8] {
        FILES
            .iter()
            .find(|(name, path, _)| *name == skill && *path == relative)
            .map(|(_, _, bytes)| *bytes)
            .expect("missing embedded fixture")
    }

    #[test]
    fn packaged_resources_match_acquired_sha256_manifest() {
        let provenance: serde_json::Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/skills/bundled/resources/manifest.json"
        )))
        .unwrap();
        let files = provenance["files"].as_array().unwrap();
        for entry in files {
            // The installed skill-creator SKILL.md adds the installer's
            // frontmatter flags to the upstream body.
            if entry["skill"] == "skill-creator" && entry["path"] == "SKILL.md" {
                continue;
            }
            let (_, _, bytes) = manifest::FILES
                .iter()
                .find(|(name, path, _)| entry["skill"] == *name && entry["path"] == *path)
                .expect("Missing upstream asset");
            let hash = ring::digest::digest(&ring::digest::SHA256, bytes);
            let hash: String = hash
                .as_ref()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            assert_eq!(hash, entry["sha256"].as_str().unwrap());
            assert_eq!(bytes.len() as u64, entry["bytes"].as_u64().unwrap());
        }
    }

    #[test]
    fn installed_resources_are_served_from_disk_and_edits_are_honored() {
        let (_temp, bundled) = installed();
        let directory = bundled.join("dataviz");
        let resources = Resources::default();
        let workspace = tempfile::tempdir().unwrap();
        let prompt = resources
            .prepare("dataviz", "", workspace.path(), &directory)
            .unwrap();
        assert!(prompt.contains(&directory.display().to_string()));
        let path = directory.join("references/palette.md");
        let original = read(&path).unwrap();
        assert_eq!(
            original.as_slice(),
            embedded("dataviz", "references/palette.md")
        );
        std::fs::write(&path, "user edit").unwrap();
        assert_eq!(read(&path).unwrap(), b"user edit");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "user edit",
            "Read never rewrites an installed resource"
        );
        drop(resources);
        assert!(read(&path).is_none());
        assert!(
            path.exists(),
            "the installed file survives Process teardown"
        );
    }

    #[test]
    fn skill_creator_bundle_is_installed_and_served() {
        let (_temp, bundled) = installed();
        let directory = bundled.join("skill-creator");
        let workspace = tempfile::tempdir().unwrap();
        let resources = Resources::default();
        let prompt = resources
            .prepare(
                "skill-creator",
                "build a PDF skill",
                workspace.path(),
                &directory,
            )
            .unwrap();
        assert!(prompt.contains("build a PDF skill"));
        assert!(prompt.contains("NOTICE.md"));
        assert!(prompt.contains("Apache-2.0"));
        assert!(prompt.contains(&directory.display().to_string()));
        for relative in [
            "SKILL.md",
            "INSTRUCTIONS.md",
            "NOTICE.md",
            "license.txt",
            "references/frontmatter.md",
            "scripts/init_skill.py",
            "scripts/quick_validate.py",
        ] {
            let installed_file = std::fs::read(directory.join(relative)).unwrap();
            assert_eq!(
                installed_file.as_slice(),
                embedded("skill-creator", relative),
                "{relative}"
            );
        }
        let skill_md = std::fs::read_to_string(directory.join("SKILL.md")).unwrap();
        assert!(skill_md.contains("name: skill-creator"));
        let digest = ring::digest::digest(
            &ring::digest::SHA256,
            &std::fs::read(directory.join("license.txt")).unwrap(),
        );
        let digest: String = digest
            .as_ref()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        assert_eq!(
            digest,
            "cfc7749b96f63bd31c3c42b5c471bf756814053e847c10f3eb003417bc523d30"
        );
        assert!(read(&directory.join("scripts/init_skill.py")).is_some());
        assert!(read(&directory.join("agents/openai.yaml")).is_none());
        assert!(std::fs::read_dir(workspace.path())
            .unwrap()
            .next()
            .is_none());
    }

    #[test]
    fn python_scripts_create_and_validate_skills_without_site_packages() {
        let (_temp, bundled) = installed();
        let directory = bundled.join("skill-creator");
        let workspace = tempfile::tempdir().unwrap();
        let resources = Resources::default();
        resources
            .prepare("skill-creator", "", workspace.path(), &directory)
            .unwrap();
        let python = if cfg!(windows) { "python" } else { "python3" };
        if std::process::Command::new(python)
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("skipping: {python} is not available");
            return;
        }
        let output_dir = tempfile::tempdir().unwrap();
        let skill_dir = output_dir.path().join("test-skill");
        // -S disables site-packages, proving the scripts use only the standard library.
        let init = std::process::Command::new(python)
            .arg("-S")
            .arg(directory.join("scripts/init_skill.py"))
            .arg("test-skill")
            .arg("--path")
            .arg(output_dir.path())
            .output()
            .unwrap();
        assert!(
            init.status.success(),
            "init failed: {}",
            String::from_utf8_lossy(&init.stderr)
        );
        for (relative, needle) in [
            ("SKILL.md", "name: test-skill"),
            ("SKILL.md", "description: \"Explain"),
        ] {
            assert!(std::fs::read_to_string(skill_dir.join(relative))
                .unwrap()
                .contains(needle));
        }
        let validate = std::process::Command::new(python)
            .arg("-S")
            .arg(directory.join("scripts/quick_validate.py"))
            .arg(&skill_dir)
            .output()
            .unwrap();
        assert!(
            validate.status.success(),
            "{}",
            String::from_utf8_lossy(&validate.stdout)
        );
        assert!(String::from_utf8_lossy(&validate.stdout).contains("Skill is valid!"));

        for (label, content, needle) in [
            (
                "missing name",
                "---\ndescription: test\n---\nbody\n",
                "Missing 'name'",
            ),
            (
                "missing description",
                "---\nname: test\n---\nbody\n",
                "Missing 'description'",
            ),
            (
                "bad name",
                "---\nname: \"Bad Name\"\ndescription: test\n---\nbody\n",
                "may contain only",
            ),
            (
                "unterminated frontmatter",
                "---\nname: test\ndescription: test\nbody\n",
                "Unterminated",
            ),
            (
                "non-boolean enabled",
                "---\nname: test\ndescription: test\nenabled: yes\n---\nbody\n",
                "must be a YAML boolean",
            ),
            (
                "missing space after key colon",
                "---\nname:test\ndescription: test\n---\nbody\n",
                "Invalid frontmatter line",
            ),
        ] {
            std::fs::write(skill_dir.join("SKILL.md"), content).unwrap();
            let output = std::process::Command::new(python)
                .arg("-S")
                .arg(directory.join("scripts/quick_validate.py"))
                .arg(&skill_dir)
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(!output.status.success(), "{label} was accepted");
            assert!(stdout.contains(needle), "{label}: {stdout}");
        }

        // Quoted keys and block scalars are valid YAML that DeepSeek Code reads.
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\n\"name\": \"quoted:name\"\ndescription: |\n  A block description\n  on two lines.\nwhenToUse: test\n---\nbody\n",
        )
        .unwrap();
        let output = std::process::Command::new(python)
            .arg("-S")
            .arg(directory.join("scripts/quick_validate.py"))
            .arg(&skill_dir)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "block scalar/quoted key must be accepted: {}",
            String::from_utf8_lossy(&output.stdout)
        );

        // DeepSeek Code ignores unknown keys, so the validator must not reject them.
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: test\ndescription: test\nmetadata:\n  short-description: fixture\ncustom: true\n---\nbody\n",
        )
        .unwrap();
        let output = std::process::Command::new(python)
            .arg("-S")
            .arg(directory.join("scripts/quick_validate.py"))
            .arg(&skill_dir)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "unknown keys must be tolerated: {}",
            String::from_utf8_lossy(&output.stdout)
        );
    }

    #[test]
    fn served_resources_stay_inside_the_installed_directory_and_keep_arguments_literal() {
        let (_temp, bundled) = installed();
        let directory = bundled.join("dataviz");
        let workspace = tempfile::tempdir().unwrap();
        let resources = Resources::default();
        let prompt = resources
            .prepare(
                "dataviz",
                "literal !`echo should-not-run` $ARGUMENTS 中文😀",
                workspace.path(),
                &directory,
            )
            .unwrap();
        assert!(prompt.contains("literal !`echo should-not-run` $ARGUMENTS 中文😀"));
        assert!(!prompt.contains("official-skills-mq4AvY"));
        assert!(!prompt.contains("Describe the available local instructions only."));
        let example = directory.join("scripts/validate_palette.js");
        assert_eq!(
            std::fs::read(&example).unwrap().as_slice(),
            embedded("dataviz", "scripts/validate_palette.js")
        );
        assert!(read(&example).is_some());
        let response = crate::tools::execute_tool(
            "Read",
            serde_json::json!({"file_path":example,"limit":4}),
            workspace.path().to_str().unwrap(),
        )
        .unwrap();
        assert!(response["content"].as_str().unwrap().contains("1\t"));
        assert!(response["lines"].as_u64().unwrap() > 4);
        assert!(read(&directory.join("../unregistered.txt")).is_none());
        assert!(read(&directory.join("not-in-manifest.md")).is_none());
        assert!(crate::tools::resolve_path(
            example.to_str().unwrap(),
            workspace.path().to_str().unwrap()
        )
        .is_err());
        assert!(std::fs::read_dir(workspace.path())
            .unwrap()
            .next()
            .is_none());
        drop(resources);
        assert!(read(&example).is_none());
        assert!(example.exists());
    }
}
