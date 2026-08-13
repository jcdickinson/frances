mod app;
mod install;

use std::path::PathBuf;
use std::process::{Command as ProcessCommand, Stdio};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use frances_session::workspace::Workspace;

#[derive(Debug, Parser)]
#[command(name = "frances")]
struct Cli {
    /// Directory or workspace file to open. Defaults to the current
    /// directory. Every launch starts a fresh session.
    path: Option<PathBuf>,

    /// Workflow to start the session with. Defaults to `default_workflow`.
    #[arg(long)]
    workflow: Option<String>,

    /// Keep the desktop app attached to this process.
    #[arg(long, global = true)]
    foreground: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Write a starter config and install the `main` workflow.
    Install {
        /// Point config at the in-repo workflow instead of copying it.
        #[arg(long)]
        local: bool,
    },
}

fn main() {
    if let Err(error) = real_main() {
        eprintln!("frances: {error:#}");
        std::process::exit(1);
    }
}

fn real_main() -> Result<()> {
    let cli = Cli::parse();

    if let Some(Command::Install { local }) = cli.command {
        return install::run(local);
    }

    // Canonicalize and validate before detaching so errors land on the
    // launching terminal and the child gets an unambiguous path.
    let path = cli.path.unwrap_or_else(|| PathBuf::from("."));
    let workspace = Workspace::open(&path)?;

    if !cli.foreground {
        return launch_detached(&workspace, cli.workflow.as_deref());
    }

    app::run(workspace, cli.workflow)
}

fn launch_detached(workspace: &Workspace, workflow: Option<&str>) -> Result<()> {
    let executable = std::env::current_exe().context("resolve frances executable")?;

    let mut command = ProcessCommand::new(executable);
    command.arg("--foreground");
    if let Some(workflow) = workflow {
        command.arg("--workflow").arg(workflow);
    }
    command
        .arg(workspace.source.identity_path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;

        // SAFETY: `setsid` takes no pointers and only changes process
        // metadata in the freshly-forked child before exec.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }

    command.spawn().context("launch frances desktop app")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::Cli;

    #[test]
    fn bare_launch_defaults_to_detached_cwd() {
        let cli = Cli::try_parse_from(["frances"]).unwrap();

        assert!(!cli.foreground);
        assert!(cli.path.is_none());
        assert!(cli.workflow.is_none());
    }

    #[test]
    fn path_and_workflow_parse() {
        let cli = Cli::try_parse_from([
            "frances",
            "some/dir",
            "--workflow",
            "review",
            "--foreground",
        ])
        .unwrap();

        assert!(cli.foreground);
        assert_eq!(cli.path.unwrap(), std::path::Path::new("some/dir"));
        assert_eq!(cli.workflow.as_deref(), Some("review"));
    }
}
