//! The owner commands the workspace popups route to (doc 15 §3 "Actions are
//! proposals"). A popup never writes itself: it builds the argument list of an
//! existing owner command, shows it as a proposal, and only after the
//! operator confirms runs that command's own entry point, parsed by that
//! command's own `clap` definition. The command keeps every check it has on
//! the CLI: `quality groups create|select` and every `replay` command refuse a
//! worker execution context before any write, and the store refuses worker,
//! attempt and import principals. Only the commands below are routable.
use anyhow::{Result, bail};
use clap::Parser;
use std::path::Path;

#[derive(Parser)]
#[command(no_binary_name = true)]
struct Quality { #[command(subcommand)] command: super::super::quality::Command }

#[cfg(target_os = "linux")]
#[derive(Parser)]
#[command(no_binary_name = true)]
struct Replay { #[command(subcommand)] command: crate::replay::Command }

/// An owner command a popup may run, as its CLI arguments after `telemetry <slug>` (quality) or `replay <slug>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Owner {
    /// `telemetry <slug> quality <args>`: `groups create|select|show`.
    Quality(Vec<String>),
    /// `replay <slug> <args>`: `run|subset|report|show`.
    Replay(Vec<String>),
}

impl Owner {
    /// The command line the operator would type for the same effect.
    pub fn command_line(&self, slug: &str) -> String {
        let quote = |a: &String| if !a.is_empty() && a.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.:/#=,".contains(&b)) { a.clone() } else { format!("'{}'", a.replace('\'', "'\\''")) };
        match self {
            Owner::Quality(args) => format!("herdr-farm telemetry {slug} quality {}", args.iter().map(quote).collect::<Vec<_>>().join(" ")),
            Owner::Replay(args) => format!("herdr-farm replay {slug} {}", args.iter().map(quote).collect::<Vec<_>>().join(" ")),
        }
    }

    fn check(&self) -> Result<()> {
        let allowed = match self {
            Owner::Quality(args) => args.first().map(String::as_str) == Some("groups") && matches!(args.get(1).map(String::as_str), Some("create" | "select" | "show")),
            Owner::Replay(args) => matches!(args.first().map(String::as_str), Some("run" | "subset" | "report" | "show")),
        };
        if !allowed { bail!("the workspace routes only `quality groups create|select|show` and `replay run|subset|report|show`"); }
        Ok(())
    }
}

/// Run `owner` on `project` through the command's own parser and entry point; returns its stdout.
pub fn run(project: &Path, owner: &Owner) -> Result<String> {
    owner.check()?;
    match owner {
        Owner::Quality(args) => {
            let parsed = Quality::try_parse_from(args).map_err(|e| anyhow::anyhow!("{}", e.to_string().trim()))?;
            super::super::quality::run(project, parsed.command)
        }
        #[cfg(target_os = "linux")]
        Owner::Replay(args) => {
            let parsed = Replay::try_parse_from(args).map_err(|e| anyhow::anyhow!("{}", e.to_string().trim()))?;
            Ok(serde_json::to_string_pretty(&crate::replay::run(project, parsed.command)?)? + "\n")
        }
        #[cfg(not(target_os = "linux"))]
        Owner::Replay(_) => bail!("the replay suite runs on Linux only"),
    }
}
