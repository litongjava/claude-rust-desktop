//! Terminal commands migrated from the former CLI, now backed by claude-core.
use clap::{Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    name = "claude-code",
    version,
    args_override_self = true,
    about = "Claude Rust — standalone CLI using the Desktop engine"
)]
pub struct Cli {
    /// Initial prompt; piped stdin is appended in non-interactive mode.
    pub prompt: Option<String>,
    #[command(subcommand)]
    pub command: Option<Commands>,
    /// Project directory (defaults to the current directory).
    #[arg(short = 'C', long, global = true)]
    pub path: Option<PathBuf>,
    /// Shared application data directory; defaults to Desktop's data directory.
    #[arg(long, env = "CLAUDE_RUST_DATA_DIR", global = true)]
    pub data_dir: Option<PathBuf>,
    /// Exact model ID, legacy alias, or provider::model.
    #[arg(short, long, env = "CLAUDE_MODEL", global = true)]
    pub model: Option<String>,
    #[arg(long, global = true)]
    pub provider: Option<String>,
    /// API endpoint for a temporary provider; never saved automatically.
    #[arg(long, env = "API_BASE_URL", global = true)]
    pub base_url: Option<String>,
    /// Environment variable containing the API key (avoid secrets in arguments).
    #[arg(long, global = true)]
    pub api_key_env: Option<String>,
    #[arg(long, value_enum, global = true)]
    pub api_format: Option<ApiFormatArg>,
    /// Run once, stream the answer to stdout, then exit.
    #[arg(long, global = true)]
    pub print: bool,
    #[arg(short = 'n', long, global = true)]
    pub no_interactive: bool,
    #[arg(long, value_enum, default_value = "text", global = true)]
    pub output_format: OutputFormat,
    #[arg(long, value_enum, default_value = "ask", global = true)]
    pub permission_mode: PermissionModeArg,
    #[arg(long, global = true)]
    pub resume: Option<String>,
    #[arg(long, value_parser = ["off", "low", "high", "max"], global = true)]
    pub effort: Option<String>,
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..), global = true)]
    pub max_tokens: Option<u32>,
    #[arg(long, global = true)]
    pub system_prompt: Option<String>,
    #[arg(short, long, global = true)]
    pub verbose: bool,
    #[arg(long)]
    pub info: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    Text,
    Json,
    StreamJson,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ApiFormatArg {
    Anthropic,
    Openai,
}
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum PermissionModeArg {
    Ask,
    AcceptEdits,
    Plan,
    Bypass,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Start an interactive session; `rust` remains a legacy alias.
    #[command(alias = "rust")]
    Repl {
        #[arg(short, long)]
        prompt: Option<String>,
    },
    /// Execute one coding task with the shared tool loop.
    Query {
        #[arg(short, long)]
        prompt: Option<String>,
    },
    Config {
        #[command(subcommand)]
        action: ConfigCommands,
    },
    Mcp {
        #[command(subcommand)]
        action: McpCommands,
    },
    Skills {
        #[command(subcommand)]
        action: SkillsCommands,
    },
    /// List or inspect persistent sessions shared with Desktop.
    Sessions {
        #[command(subcommand)]
        action: SessionCommands,
    },
    /// Create project instruction directories without overwriting files.
    Init {
        #[arg(short, long)]
        name: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommands {
    /// Show effective saved configuration with credentials redacted.
    Show,
    /// Set model, base_url, api_format, api_key, or max_tokens for the CLI provider.
    Set { key: String, value: String },
    /// Remove only the CLI-managed provider and CLI preferences.
    Reset,
    /// Import an old .claude-code/settings.json; the source is read-only.
    ImportLegacy { source: PathBuf },
}

#[derive(Debug, Subcommand)]
pub enum McpCommands {
    List,
    Add {
        name: String,
        command: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    Remove {
        name: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum SkillsCommands {
    List,
    Search {
        query: String,
    },
    Help {
        skill: String,
    },
    Execute {
        skill: String,
        #[arg(trailing_var_arg = true)]
        args: Vec<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum SessionCommands {
    List,
    Show { id: String },
}
