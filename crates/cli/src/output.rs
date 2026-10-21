//! Machine-readable failures before the turn event stream has started.
use crate::args::{Cli, Commands, OutputFormat, SkillsCommands};
use std::io::{self, IsTerminal, Write};

/// Marks a failure whose terminal result has already been written to stdout.
#[derive(Debug)]
pub struct ReportedError(pub String);

impl std::fmt::Display for ReportedError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ReportedError {}

pub fn report_startup_error(cli: &Cli, error: &anyhow::Error) -> io::Result<()> {
    if error.downcast_ref::<ReportedError>().is_some()
        || cli.output_format == OutputFormat::Text
        || cli.info
    {
        return Ok(());
    }
    let conversation = matches!(
        &cli.command,
        None | Some(Commands::Query { .. })
            | Some(Commands::Repl { .. })
            | Some(Commands::Agent { .. })
            | Some(Commands::Skills {
                action: SkillsCommands::Execute { .. }
            })
    );
    let single = cli.print
        || cli.print_short
        || cli.no_interactive
        || cli.no_interactive_short
        || !io::stdin().is_terminal()
        || matches!(
            &cli.command,
            Some(Commands::Query { .. })
                | Some(Commands::Agent { .. })
                | Some(Commands::Skills {
                    action: SkillsCommands::Execute { .. }
                })
        );
    if conversation && single {
        let result = serde_json::json!({
            "type": "result", "session_id": null, "result": "",
            "is_error": true, "error": format!("{error:#}")
        });
        let mut stdout = io::stdout().lock();
        serde_json::to_writer(&mut stdout, &result)?;
        writeln!(stdout)?;
        stdout.flush()?;
    }
    Ok(())
}
