//! Read-only discovery of user/project Claude skill and command files.
use anyhow::{ensure, Context, Result};
use serde::Serialize;
use serde_json::Value;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Serialize)]
pub struct SkillCommand {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
    pub content: String,
    pub user_invocable: bool,
    pub disable_model_invocation: bool,
    pub context: Option<String>,
    pub shell: Option<String>,
    pub agent: Option<String>,
}

impl SkillCommand {
    /// Model invocation uses the Skill tool, including fork/shell execution.
    /// user-invocable only controls manual slash invocation.
    pub fn model_invocable(&self) -> bool {
        !self.disable_model_invocation
    }
    pub fn render(&self, arguments: &str, session_id: &str) -> Result<String> {
        ensure!(
            self.user_invocable,
            "Skill /{} is not user-invocable",
            self.name
        );
        ensure!(
            self.context.as_deref() != Some("fork"),
            "Skill /{} requires host-managed fork isolation; invoke it through the CLI skill handler",
            self.name
        );
        let template = self.render_template(arguments, session_id)?;
        ensure!(
            !super::invocation::parse(&template)?.iter().any(|part| matches!(part, super::invocation::Segment::Shell(_))),
            "Skill /{} requires host-managed shell approval; invoke it through the CLI skill handler",
            self.name
        );
        Ok(template)
    }

    /// Pure substitution only. Hosts must handle fork isolation and explicitly
    /// authorize any parsed shell commands before sending this text to a model.
    pub fn render_template(&self, arguments: &str, session_id: &str) -> Result<String> {
        ensure!(
            self.user_invocable,
            "Skill /{} is not user-invocable",
            self.name
        );
        let mut text = expand_arguments(&self.content, arguments)?;
        text = text.replace("${CLAUDE_SESSION_ID}", session_id).replace(
            "${CLAUDE_SKILL_DIR}",
            &self
                .path
                .parent()
                .unwrap_or(Path::new("."))
                .to_string_lossy()
                .replace('\\', "/"),
        );
        if self.path.parent() == Some(Path::new("__embedded_cli_skills__")) {
            return Ok(format!(
                "Bundled workflow /{} (embedded instructions; no source file to read)\n\n{}",
                self.name, text
            ));
        }
        Ok(format!("Skill /{} from {}\nResolve referenced files relative to this skill directory: {}\n\n{}", self.name, self.path.display(), self.path.parent().unwrap_or(Path::new(".")).display(), text))
    }
}

pub fn discover(workspace: &Path) -> Result<BTreeMap<String, SkillCommand>> {
    let user = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".claude")));
    discover_from(user.as_deref(), workspace)
}

/// Explicit roots allow fixture tests without reading or running personal skills.
pub fn discover_from(
    user_config: Option<&Path>,
    workspace: &Path,
) -> Result<BTreeMap<String, SkillCommand>> {
    let mut commands = super::bundled::commands(workspace);
    if let Some(user) = user_config {
        scan_config(user, &mut commands)?;
    }
    scan_config(&workspace.join(".claude"), &mut commands)?;
    Ok(commands)
}

/// Discover from a materialized, validated plugin bundle. Namespace prevents
/// collisions with user/project commands while references keep their real path.
pub fn discover_plugin(
    root: &Path,
    namespace: &str,
    skill_paths: &[String],
    command_paths: &[String],
) -> Result<BTreeMap<String, SkillCommand>> {
    let mut commands = BTreeMap::new();
    for (paths, is_skill) in [(command_paths, false), (skill_paths, true)] {
        for relative in paths {
            let selected = root.join(relative);
            ensure!(
                selected.exists(),
                "Plugin feature path does not exist: {relative}"
            );
            for entry in walkdir::WalkDir::new(&selected)
                .follow_links(false)
                .max_depth(16)
            {
                let entry = entry?;
                if !entry.file_type().is_file() {
                    continue;
                }
                if (is_skill && entry.file_name() != "SKILL.md")
                    || (!is_skill
                        && entry
                            .path()
                            .extension()
                            .is_none_or(|extension| extension != "md"))
                {
                    continue;
                }
                let path = entry.path();
                let text = std::fs::read_to_string(path)?;
                let (metadata, content) = frontmatter(&text)?;
                if !boolean(&metadata, "enabled", true)? {
                    continue;
                }
                let name_path = if selected.is_file() {
                    selected
                        .file_stem()
                        .context("Plugin command filename missing")?
                        .to_string_lossy()
                        .into_owned()
                } else if is_skill {
                    let parent = path
                        .parent()
                        .context("Plugin skill directory missing")?
                        .strip_prefix(&selected)?;
                    if parent.as_os_str().is_empty() {
                        selected
                            .file_name()
                            .context("Plugin skill name missing")?
                            .to_string_lossy()
                            .into_owned()
                    } else {
                        parent.to_string_lossy().replace(['/', '\\'], ":")
                    }
                } else {
                    path.strip_prefix(&selected)?
                        .with_extension("")
                        .to_string_lossy()
                        .replace(['/', '\\'], ":")
                };
                let name = metadata
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or(&name_path);
                ensure!(
                    !name.is_empty()
                        && name
                            .chars()
                            .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | ':' | '.')),
                    "Invalid plugin skill name"
                );
                let name = format!("{namespace}:{name}");
                commands.insert(
                    name.clone(),
                    SkillCommand {
                        name,
                        description: metadata
                            .get("description")
                            .and_then(Value::as_str)
                            .unwrap_or("Run plugin skill instructions")
                            .split_whitespace()
                            .collect::<Vec<_>>()
                            .join(" "),
                        path: path.to_path_buf(),
                        content: content.replace(
                            "${CLAUDE_PLUGIN_ROOT}",
                            &root.to_string_lossy().replace('\\', "/"),
                        ),
                        user_invocable: boolean(&metadata, "user-invocable", true)?,
                        disable_model_invocation: boolean(
                            &metadata,
                            "disable-model-invocation",
                            false,
                        )?,
                        context: metadata
                            .get("context")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                        shell: metadata
                            .get("shell")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                        agent: metadata
                            .get("agent")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                    },
                );
            }
        }
    }
    Ok(commands)
}

fn scan_config(config: &Path, commands: &mut BTreeMap<String, SkillCommand>) -> Result<()> {
    // Skills win over legacy commands in the same scope.
    for (directory, skill_format) in [("commands", false), ("skills", true)] {
        let root = config.join(directory);
        if !root.exists() {
            continue;
        }
        let mut paths = Vec::new();
        for entry in walkdir::WalkDir::new(&root)
            .follow_links(false)
            .max_depth(16)
        {
            let entry = entry.with_context(|| format!("Scanning {}", root.display()))?;
            if !entry.file_type().is_file() {
                continue;
            }
            let filename = entry.file_name().to_string_lossy();
            if (skill_format && filename == "SKILL.md")
                || (!skill_format
                    && entry
                        .path()
                        .extension()
                        .is_some_and(|extension| extension == "md"))
            {
                paths.push(entry.into_path());
            }
        }
        paths.sort();
        for path in paths {
            ensure!(
                std::fs::metadata(&path)?.len() <= 1024 * 1024,
                "Skill file exceeds 1 MiB: {}",
                path.display()
            );
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("Reading UTF-8 skill {}", path.display()))?;
            let (metadata, content) =
                frontmatter(&text).with_context(|| format!("Parsing skill {}", path.display()))?;
            if boolean(&metadata, "enabled", true)? == false {
                continue;
            }
            let relative = path.strip_prefix(&root)?;
            let stem = if skill_format {
                relative
                    .parent()
                    .context("Skill must be in a named directory")?
                    .to_path_buf()
            } else {
                relative.with_extension("")
            };
            let mut parts = stem
                .iter()
                .map(|v| v.to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            // A plugin checkout inside a user skill directory has its own
            // namespace and source root; structural `skills/` is not a command
            // namespace. Ordinary nested skill collections retain their paths.
            if skill_format {
                if let Some(plugin_parts) = local_plugin_name(&root, &path)? {
                    parts = plugin_parts;
                }
            }
            ensure!(
                !parts.is_empty(),
                "Skill must be in a named directory: {}",
                path.display()
            );
            if let Some(name) = metadata.get("name").and_then(Value::as_str) {
                *parts.last_mut().unwrap() = name.into();
            }
            let name = parts.join(":");
            ensure!(
                name.chars()
                    .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | ':' | '.')),
                "Invalid skill command name: {name}"
            );
            let command = SkillCommand {
                name: name.clone(),
                description: metadata
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("Run local skill instructions")
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" "),
                path,
                content,
                user_invocable: boolean(
                    &metadata,
                    "user-invocable",
                    boolean(&metadata, "userInvocable", true)?,
                )?,
                disable_model_invocation: boolean(&metadata, "disable-model-invocation", false)?,
                context: metadata
                    .get("context")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                shell: metadata
                    .get("shell")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                agent: metadata
                    .get("agent")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            };
            commands.insert(name, command);
        }
    }
    Ok(())
}

fn local_plugin_name(scan_root: &Path, skill_file: &Path) -> Result<Option<Vec<String>>> {
    for ancestor in skill_file.parent().into_iter().flat_map(Path::ancestors) {
        if ancestor == scan_root || !ancestor.starts_with(scan_root) {
            break;
        }
        let manifest_path = ancestor.join(".claude-plugin/plugin.json");
        if !manifest_path.is_file() {
            continue;
        }
        ensure!(
            std::fs::metadata(&manifest_path)?.len() <= 1024 * 1024,
            "Plugin manifest exceeds 1 MiB"
        );
        let manifest: Value = serde_json::from_str(&std::fs::read_to_string(&manifest_path)?)
            .with_context(|| format!("Parsing plugin manifest {}", manifest_path.display()))?;
        let namespace = manifest
            .get("name")
            .and_then(Value::as_str)
            .or_else(|| ancestor.file_name().and_then(|name| name.to_str()))
            .context("Plugin namespace missing")?;
        let mut sources = vec!["skills".to_owned()];
        if let Some(paths) = manifest.get("skills") {
            if let Some(path) = paths.as_str() {
                sources.push(path.into());
            } else if let Some(paths) = paths.as_array() {
                sources.extend(paths.iter().filter_map(Value::as_str).map(str::to_owned));
            }
        }
        for source in sources {
            let source = Path::new(&source);
            if source.is_absolute()
                || source.components().any(|part| {
                    matches!(
                        part,
                        std::path::Component::ParentDir | std::path::Component::Prefix(_)
                    )
                })
            {
                continue;
            }
            let source_root = ancestor.join(source);
            if let Ok(relative) = skill_file.strip_prefix(&source_root) {
                let parent = relative.parent().context("Plugin skill has no directory")?;
                let mut parts = vec![namespace.to_owned()];
                parts.extend(
                    parent
                        .iter()
                        .map(|part| part.to_string_lossy().into_owned()),
                );
                if parts.len() == 1 {
                    parts.push(
                        source_root
                            .file_name()
                            .and_then(|part| part.to_str())
                            .unwrap_or("skill")
                            .into(),
                    );
                }
                return Ok(Some(parts));
            }
        }
    }
    Ok(None)
}

fn boolean(metadata: &Value, key: &str, default: bool) -> Result<bool> {
    metadata
        .get(key)
        .map(|v| {
            v.as_bool()
                .with_context(|| format!("{key} must be boolean"))
        })
        .unwrap_or(Ok(default))
}

fn frontmatter(text: &str) -> Result<(Value, String)> {
    let text = text.trim_start_matches('\u{feff}').replace("\r\n", "\n");
    if !text.starts_with("---\n") {
        return Ok((serde_json::json!({}), text));
    }
    let end = text[4..]
        .find("\n---")
        .context("Unterminated YAML frontmatter")?
        + 4;
    let after = end + 4;
    ensure!(
        text[after..].starts_with('\n') || after == text.len(),
        "Invalid YAML frontmatter delimiter"
    );
    let yaml: serde_yaml::Value = serde_yaml::from_str(&text[4..end])?;
    let metadata = serde_json::to_value(yaml)?;
    ensure!(
        metadata.is_object() || metadata.is_null(),
        "Skill frontmatter must be a mapping"
    );
    Ok((metadata, text[after..].trim_start_matches('\n').to_owned()))
}

fn split_arguments(arguments: &str) -> Result<Vec<String>> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut active = false;
    for ch in arguments.chars() {
        if quote == Some(ch) {
            quote = None;
        } else if quote.is_none() && matches!(ch, '\'' | '"') {
            quote = Some(ch);
            active = true;
        } else if quote.is_none() && ch.is_whitespace() {
            if active {
                words.push(std::mem::take(&mut word));
                active = false;
            }
        } else {
            word.push(ch);
            active = true;
        }
    }
    ensure!(quote.is_none(), "Unterminated quote in skill arguments");
    if active {
        words.push(word);
    }
    Ok(words)
}

fn expand_arguments(content: &str, arguments: &str) -> Result<String> {
    let words = split_arguments(arguments)?;
    let pattern = regex::Regex::new(r"\$ARGUMENTS\[(\d+)\]|\$(\d+)|\$ARGUMENTS\b")?;
    let had_placeholder = pattern.is_match(content);
    let mut rendered = pattern
        .replace_all(content, |captures: &regex::Captures<'_>| {
            if let Some(index) = captures.get(1).or_else(|| captures.get(2)) {
                return index
                    .as_str()
                    .parse::<usize>()
                    .ok()
                    .and_then(|i| words.get(i))
                    .cloned()
                    .unwrap_or_default();
            }
            arguments.to_owned()
        })
        .into_owned();
    if !had_placeholder && !arguments.trim().is_empty() {
        rendered.push_str(&format!("\n\nUser arguments: {arguments}"));
    }
    Ok(rendered)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fork_metadata_does_not_bypass_user_invocation_restrictions() {
        let temp = tempfile::tempdir().unwrap();
        let skill = temp.path().join(".claude/skills/private");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(skill.join("SKILL.md"), "---\ncontext: fork\nagent: Explore\nshell: powershell\nuser-invocable: false\n---\n!`Write-Output hidden`").unwrap();
        let commands = discover_from(None, temp.path()).unwrap();
        let skill = &commands["private"];
        assert_eq!(skill.context.as_deref(), Some("fork"));
        assert_eq!(skill.agent.as_deref(), Some("Explore"));
        assert_eq!(skill.shell.as_deref(), Some("powershell"));
        assert!(skill.model_invocable());
        let mut dynamic = skill.clone();
        dynamic.context = None;
        assert!(dynamic.model_invocable());
        dynamic.content = "Read and explain this project".into();
        assert!(dynamic.model_invocable());
        assert!(skill
            .render_template("", "session")
            .unwrap_err()
            .to_string()
            .contains("not user-invocable"));
    }

    #[test]
    fn project_overrides_user_and_nested_names_are_preserved() {
        let root = tempfile::tempdir().unwrap();
        let user = root.path().join("user");
        let workspace = root.path().join("project");
        for config in [&user, &workspace.join(".claude")] {
            std::fs::create_dir_all(config.join("skills/team/review")).unwrap();
            std::fs::write(config.join("skills/team/review/SKILL.md"), format!("---\ndescription: Review files\ndisable-model-invocation: true\n---\n{} $ARGUMENTS", config.display())).unwrap();
        }
        std::fs::create_dir_all(workspace.join(".claude/commands")).unwrap();
        std::fs::write(
            workspace.join(".claude/commands/hidden.md"),
            "---\nuser-invocable: false\n---\nHidden",
        )
        .unwrap();
        let found = discover_from(Some(&user), &workspace).unwrap();
        assert!(found["team:review"].path.starts_with(&workspace));
        assert!(found["team:review"].disable_model_invocation);
        assert!(found["team:review"]
            .render("src/main.rs", "id")
            .unwrap()
            .contains("src/main.rs"));
        assert!(!found["hidden"].user_invocable);
        assert!(found["hidden"].render("", "id").is_err());
    }

    #[test]
    fn argument_expansion_preserves_quotes_and_does_not_recursively_expand_user_input() {
        assert_eq!(
            expand_arguments("$0|$1|$ARGUMENTS[2]", "'two words' second third").unwrap(),
            "two words|second|third"
        );
        assert_eq!(expand_arguments("$ARGUMENTS", "$0").unwrap(), "$0");
        assert_eq!(
            expand_arguments("Instructions", "foo").unwrap(),
            "Instructions\n\nUser arguments: foo"
        );
        assert!(expand_arguments("$0", "'unclosed").is_err());
    }

    #[test]
    fn utf8_bom_crlf_and_invalid_frontmatter_are_handled() {
        assert_eq!(
            frontmatter("\u{feff}---\r\ndescription: test\r\n---\r\nbody")
                .unwrap()
                .1,
            "body"
        );
        assert!(frontmatter("---\nname: test\nbody").is_err());
        assert!(boolean(&serde_json::json!({"enabled":"false"}), "enabled", true).is_err());
    }
    #[test]
    fn plugin_checkouts_use_manifest_namespace_and_skip_structural_skills_directory() {
        let temp = tempfile::tempdir().unwrap();
        let user = temp.path().join("user");
        let plugin = user.join("skills/checkout-name");
        std::fs::create_dir_all(plugin.join(".claude-plugin")).unwrap();
        std::fs::write(
            plugin.join(".claude-plugin/plugin.json"),
            r#"{"name":"vendor-tools","skills":["./extra"]}"#,
        )
        .unwrap();
        for (directory, name) in [
            ("skills/source-name", "deploy"),
            ("extra/local-name", "inspect"),
        ] {
            std::fs::create_dir_all(plugin.join(directory)).unwrap();
            std::fs::write(
                plugin.join(directory).join("SKILL.md"),
                format!("---\nname: {name}\n---\nRun $ARGUMENTS"),
            )
            .unwrap();
        }
        let ordinary = user.join("skills/collection/skills/nested");
        std::fs::create_dir_all(&ordinary).unwrap();
        std::fs::write(ordinary.join("SKILL.md"), "Ordinary nested skill").unwrap();
        let found = discover_from(Some(&user), temp.path()).unwrap();
        assert!(found.contains_key("vendor-tools:deploy"));
        assert!(found.contains_key("vendor-tools:inspect"));
        assert!(found.contains_key("collection:skills:nested"));
        assert!(!found.keys().any(|name| name.starts_with("checkout-name:")));
        assert!(found["vendor-tools:deploy"]
            .render("production", "id")
            .unwrap()
            .contains("Run production"));
    }
}
