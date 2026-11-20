//! The Bash and Monitor tool descriptions depend on the machine's default
//! shell: models otherwise mix shell syntaxes (for example PowerShell
//! arguments quoted bash-style).
//!
//! The description is part of the cached prompt prefix, so it is computed once
//! per process and rebuilt only after in-process shell settings change.

use super::background_shell;
use crate::services::shell_settings::{self, ResolvedShell, ShellSettings};
use std::sync::Mutex;

const GIT_BASH_NOTE: &str = "This tool runs Git Bash (POSIX sh), not cmd.exe or PowerShell. Use Unix shell syntax: /dev/null not NUL, forward slashes, $VAR not %VAR% or $env:VAR. Windows paths can be written as C:/Users/... or /c/Users/.... Arguments that look like POSIX paths are converted automatically when calling native .exe programs; when that breaks an argument (e.g. `cmd /c`, `adb push x /sdcard/...`), prefix the command with MSYS_NO_PATHCONV=1. When a native program needs a Windows path inside a variable, file or option value, convert it with `cygpath -w <path>` (or `cygpath -m` for C:/ form).";
const POWER_SHELL_NOTE: &str = "This tool runs Windows PowerShell 5.1, not bash or cmd. Use PowerShell syntax: $env:VAR, $null or Out-Null instead of /dev/null, ; between commands (&& and || are not supported), and the backtick as the escape character, not backslash.";
const PWSH_NOTE: &str = "This tool runs PowerShell 7, not bash or cmd. Use PowerShell syntax: $env:VAR, $null or Out-Null instead of /dev/null, ; between commands (&& and || work), and the backtick as the escape character, not backslash.";
const CMD_NOTE: &str = "This tool runs cmd.exe, not bash or PowerShell. Use %VAR%, NUL, backslashes in paths, && between commands, and double quotes; there is no $VAR, /dev/null or single-quote quoting.";
const FISH_NOTE: &str = "This tool runs fish, not bash: set variables with set -x VAR value; bash-only syntax may fail.";
const WSL_NOTE_PREFIX: &str = "This tool runs bash inside WSL";
const WSL_NOTE_BODY: &str = ", a Linux environment, not Windows. Use Linux paths and tools; Windows drives are under /mnt/c/...; Windows .exe programs run only through WSL interop.";
const OTHER_SHELL_HINT: &str = "Set shell only when you need a different shell; then use that shell's syntax (e.g. PowerShell quoting is not bash quoting).";

/// Shell-aware text for the Bash and Monitor tools.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct BashTool {
    description_suffix: String,
    shell_parameter: String,
}

impl BashTool {
    /// Today's base description plus the default-shell note and shell hint.
    pub(super) fn description_suffix(&self) -> &str {
        &self.description_suffix
    }

    pub(super) fn shell_parameter(&self) -> &str {
        &self.shell_parameter
    }
}

static CACHE: Mutex<Option<(u64, BashTool)>> = Mutex::new(None);

/// The shell-aware description for this machine, computed once per process and
/// shell-settings change. It must not vary between requests of a session.
pub(super) fn bash_tool() -> BashTool {
    let generation = shell_settings::generation();
    let mut cache = CACHE.lock().unwrap_or_else(|error| error.into_inner());
    if let Some((cached, tool)) = cache.as_ref() {
        if *cached == generation {
            return tool.clone();
        }
    }
    let settings = background_shell::shell_settings().unwrap_or_default();
    let resolved = settings.resolve(None, false).ok();
    let tool = build(&settings, resolved.as_ref(), cfg!(windows), &|name| {
        super::find_shell(name).is_some()
    });
    *cache = Some((generation, tool.clone()));
    tool
}

fn build(
    settings: &ShellSettings,
    default: Option<&ResolvedShell>,
    is_windows: bool,
    found: &dyn Fn(&str) -> bool,
) -> BashTool {
    let mut shells = Vec::new();
    for name in [
        "bash",
        "zsh",
        "fish",
        "sh",
        "cmd",
        "powershell",
        "pwsh",
        "wsl",
    ] {
        let usable = match name {
            "bash" => true,
            "cmd" | "powershell" => is_windows,
            "wsl" => is_windows && settings.wsl.distribution.is_some(),
            _ => found(name),
        };
        if usable {
            shells.push(name);
        }
    }
    let available = default
        .map(|shell| shell.name.as_str())
        .filter(|name| shells.contains(name))
        .unwrap_or("bash");
    let is_git_bash_default = default
        .is_some_and(|shell| shell.name == "bash" && is_git_bash(&shell.program));

    let mut description_suffix = String::new();
    if let Some(note) = default.and_then(shell_note) {
        description_suffix.push_str("\n\n");
        description_suffix.push_str(&note);
    }
    let default_family = syntax_family(available, available, false);
    if shells
        .iter()
        .any(|name| syntax_family(name, available, is_git_bash_default) != default_family)
    {
        description_suffix.push_str("\n\n");
        description_suffix.push_str(OTHER_SHELL_HINT);
    }

    BashTool {
        description_suffix,
        shell_parameter: format!("Shell to use: {} (default: {available})", shells.join(", ")),
    }
}

/// Shells whose syntax differs when chosen explicitly. POSIX shells share one
/// family, but WSL counts separately from Git Bash and Windows shells because
/// its paths and available tools differ.
fn syntax_family(shell: &str, default: &str, is_git_bash_default: bool) -> &'static str {
    match shell {
        "bash" | "sh" | "zsh" => "posix",
        "wsl" => {
            if is_git_bash_default || matches!(default, "powershell" | "pwsh" | "cmd") {
                "wsl"
            } else {
                "posix"
            }
        }
        "powershell" | "pwsh" => "powershell",
        "cmd" => "cmd",
        "fish" => "fish",
        _ => "other",
    }
}

/// One note for the resolved default shell; `None` for ordinary POSIX shells.
fn shell_note(shell: &ResolvedShell) -> Option<String> {
    if let Some(wsl) = &shell.wsl {
        let distribution = wsl
            .distribution
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty());
        return Some(match distribution {
            Some(distribution) => {
                format!("{WSL_NOTE_PREFIX} ({distribution}){WSL_NOTE_BODY}")
            }
            None => format!("{WSL_NOTE_PREFIX}{WSL_NOTE_BODY}"),
        });
    }
    match shell.name.as_str() {
        "bash" if is_git_bash(&shell.program) => Some(GIT_BASH_NOTE.into()),
        "powershell" => Some(POWER_SHELL_NOTE.into()),
        "pwsh" => Some(PWSH_NOTE.into()),
        "cmd" => Some(CMD_NOTE.into()),
        "fish" => Some(FISH_NOTE.into()),
        _ => None,
    }
}

/// Git for Windows keeps `bash.exe` under a `Git` install directory.
fn is_git_bash(program: &str) -> bool {
    let normalized = program.replace('\\', "/").to_ascii_lowercase();
    let file = normalized.rsplit('/').next().unwrap_or_default();
    matches!(file, "bash.exe" | "git-bash.exe")
        && normalized.split('/').any(|component| component == "git")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::shell_settings::WslSettings;

    fn resolved(name: &str, program: &str) -> ResolvedShell {
        ResolvedShell {
            name: name.into(),
            program: program.into(),
            command_flag: "-c",
            source: "detected",
            wsl: None,
        }
    }

    fn wsl(distribution: Option<&str>) -> ResolvedShell {
        ResolvedShell {
            name: "wsl".into(),
            program: "wsl.exe".into(),
            command_flag: "-c",
            source: "settings",
            wsl: Some(WslSettings {
                distribution: distribution.map(str::to_owned),
                bash_path: "/bin/bash".into(),
            }),
        }
    }

    fn settings(default: &str, distribution: Option<&str>) -> ShellSettings {
        ShellSettings {
            tool_default: default.into(),
            wsl: WslSettings {
                distribution: distribution.map(str::to_owned),
                bash_path: "/bin/bash".into(),
            },
            ..Default::default()
        }
    }

    #[test]
    fn note_for_each_default_shell() {
        assert_eq!(
            shell_note(&resolved("bash", r"C:\Program Files\Git\bin\bash.exe")),
            Some(GIT_BASH_NOTE.to_string())
        );
        assert_eq!(
            shell_note(&resolved("bash", r"D:\tools\Git\usr\bin\bash.exe")),
            Some(GIT_BASH_NOTE.to_string())
        );
        assert_eq!(shell_note(&resolved("bash", "/bin/bash")), None);
        assert_eq!(shell_note(&resolved("bash", "/usr/bin/bash")), None);
        assert_eq!(shell_note(&resolved("zsh", "/bin/zsh")), None);
        assert_eq!(shell_note(&resolved("sh", "/bin/sh")), None);
        assert_eq!(
            shell_note(&wsl(Some("Ubuntu"))),
            Some("This tool runs bash inside WSL (Ubuntu), a Linux environment, not Windows. Use Linux paths and tools; Windows drives are under /mnt/c/...; Windows .exe programs run only through WSL interop.".to_string())
        );
        assert_eq!(
            shell_note(&wsl(None)),
            Some("This tool runs bash inside WSL, a Linux environment, not Windows. Use Linux paths and tools; Windows drives are under /mnt/c/...; Windows .exe programs run only through WSL interop.".to_string())
        );
        assert_eq!(
            shell_note(&resolved("powershell", "powershell.exe")),
            Some(POWER_SHELL_NOTE.to_string())
        );
        assert_eq!(
            shell_note(&resolved("pwsh", "pwsh.exe")),
            Some(PWSH_NOTE.to_string())
        );
        assert_eq!(
            shell_note(&resolved("cmd", "cmd.exe")),
            Some(CMD_NOTE.to_string())
        );
        assert_eq!(
            shell_note(&resolved("fish", "fish")),
            Some(FISH_NOTE.to_string())
        );
    }

    #[test]
    fn git_bash_detection_needs_a_git_install_and_exe() {
        assert!(is_git_bash(r"C:\Program Files\Git\bin\bash.exe"));
        assert!(is_git_bash("C:/Program Files/Git/usr/bin/bash.exe"));
        assert!(is_git_bash(r"C:\Program Files\Git\git-bash.exe"));
        assert!(!is_git_bash("/bin/bash"));
        assert!(!is_git_bash("/home/git/bin/bash"));
        assert!(!is_git_bash(r"C:\msys64\usr\bin\bash.exe"));
    }

    #[test]
    fn linux_lists_only_detected_shells_and_keeps_bash() {
        let tool = build(
            &settings("bash", None),
            Some(&resolved("bash", "/bin/bash")),
            false,
            &|name| name == "bash",
        );
        assert_eq!(tool.shell_parameter(), "Shell to use: bash (default: bash)");
        assert_eq!(tool.description_suffix(), "");

        let tool = build(
            &settings("bash", None),
            Some(&resolved("bash", "/bin/bash")),
            false,
            &|name| matches!(name, "bash" | "zsh" | "fish" | "sh" | "pwsh"),
        );
        assert_eq!(
            tool.shell_parameter(),
            "Shell to use: bash, zsh, fish, sh, pwsh (default: bash)"
        );
        assert_eq!(tool.description_suffix(), format!("\n\n{OTHER_SHELL_HINT}"));
    }

    #[test]
    fn hint_only_appears_for_another_syntax_family() {
        let posix_only = build(
            &settings("bash", None),
            Some(&resolved("bash", "/bin/bash")),
            false,
            &|name| matches!(name, "bash" | "sh" | "zsh"),
        );
        assert_eq!(
            posix_only.shell_parameter(),
            "Shell to use: bash, zsh, sh (default: bash)"
        );
        assert_eq!(posix_only.description_suffix(), "");

        let with_pwsh = build(
            &settings("bash", None),
            Some(&resolved("bash", "/bin/bash")),
            false,
            &|name| matches!(name, "bash" | "sh" | "pwsh"),
        );
        assert_eq!(
            with_pwsh.shell_parameter(),
            "Shell to use: bash, sh, pwsh (default: bash)"
        );
        assert_eq!(
            with_pwsh.description_suffix(),
            format!("\n\n{OTHER_SHELL_HINT}")
        );

        let git_bash = build(
            &settings("bash", None),
            Some(&resolved("bash", r"C:\Program Files\Git\bin\bash.exe")),
            true,
            &|name| matches!(name, "bash" | "sh" | "zsh"),
        );
        assert_eq!(
            git_bash.shell_parameter(),
            "Shell to use: bash, zsh, sh, cmd, powershell (default: bash)"
        );
        assert_eq!(
            git_bash.description_suffix(),
            format!("\n\n{GIT_BASH_NOTE}\n\n{OTHER_SHELL_HINT}")
        );

        let powershell_default = build(
            &settings("powershell", None),
            Some(&resolved("powershell", "powershell.exe")),
            true,
            &|name| name == "bash",
        );
        assert_eq!(
            powershell_default.shell_parameter(),
            "Shell to use: bash, cmd, powershell (default: powershell)"
        );
        assert_eq!(
            powershell_default.description_suffix(),
            format!("\n\n{POWER_SHELL_NOTE}\n\n{OTHER_SHELL_HINT}")
        );
    }

    #[test]
    fn wsl_counts_as_a_separate_family_from_git_bash_and_windows_shells() {
        let git_bash_with_wsl = build(
            &settings("bash", Some("Ubuntu")),
            Some(&resolved("bash", r"C:\Program Files\Git\bin\bash.exe")),
            true,
            &|_| false,
        );
        assert_eq!(
            git_bash_with_wsl.shell_parameter(),
            "Shell to use: bash, cmd, powershell, wsl (default: bash)"
        );
        assert!(git_bash_with_wsl
            .description_suffix()
            .ends_with(OTHER_SHELL_HINT));

        assert_eq!(syntax_family("wsl", "bash", true), "wsl");
        assert_eq!(syntax_family("wsl", "bash", false), "posix");
        assert_eq!(syntax_family("wsl", "wsl", false), "posix");
        assert_eq!(syntax_family("wsl", "powershell", false), "wsl");
    }

    #[test]
    fn non_windows_never_offers_cmd_powershell_or_wsl() {
        let tool = build(&settings("cmd", Some("Ubuntu")), None, false, &|name| {
            matches!(
                name,
                "bash" | "zsh" | "fish" | "sh" | "cmd" | "powershell" | "pwsh" | "wsl"
            )
        });
        assert_eq!(
            tool.shell_parameter(),
            "Shell to use: bash, zsh, fish, sh, pwsh (default: bash)"
        );
        assert!(!tool.shell_parameter().contains("cmd"));
        assert!(!tool.shell_parameter().contains("powershell"));
        assert!(!tool.shell_parameter().contains("wsl"));
    }

    #[test]
    fn windows_lists_builtin_shells_and_configured_wsl() {
        let tool = build(
            &settings("powershell", Some("Ubuntu")),
            Some(&resolved(
                "powershell",
                r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe",
            )),
            true,
            &|name| name == "pwsh",
        );
        assert_eq!(
            tool.shell_parameter(),
            "Shell to use: bash, cmd, powershell, pwsh, wsl (default: powershell)"
        );
        assert!(tool
            .description_suffix()
            .starts_with(&format!("\n\n{POWER_SHELL_NOTE}")));
        assert!(tool.description_suffix().ends_with(OTHER_SHELL_HINT));

        let tool = build(
            &settings("bash", None),
            Some(&resolved("bash", r"C:\Program Files\Git\bin\bash.exe")),
            true,
            &|name| matches!(name, "bash" | "zsh" | "fish" | "sh"),
        );
        assert_eq!(
            tool.shell_parameter(),
            "Shell to use: bash, zsh, fish, sh, cmd, powershell (default: bash)"
        );
        assert_eq!(
            tool.description_suffix(),
            format!("\n\n{GIT_BASH_NOTE}\n\n{OTHER_SHELL_HINT}")
        );
    }

    #[test]
    fn default_other_than_bash_states_the_actual_default() {
        let tool = build(
            &settings("zsh", None),
            Some(&resolved("zsh", "/bin/zsh")),
            false,
            &|name| matches!(name, "bash" | "zsh"),
        );
        assert_eq!(
            tool.shell_parameter(),
            "Shell to use: bash, zsh (default: zsh)"
        );
        assert_eq!(tool.description_suffix(), "");
    }

    #[test]
    fn cached_description_is_stable_for_the_process() {
        assert_eq!(bash_tool(), bash_tool());
        assert!(!bash_tool().shell_parameter().is_empty());
    }
}
