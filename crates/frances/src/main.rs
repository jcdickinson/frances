mod app;
#[cfg(target_os = "linux")]
mod appimage;
mod install;

use std::path::PathBuf;
use std::process::{Command as ProcessCommand, Stdio};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use frances_session::workspace::Workspace;

#[derive(Debug, Parser)]
#[command(name = "frances", args_conflicts_with_subcommands = true)]
struct Cli {
    /// Directory or workspace file to open. Defaults to the current
    /// directory. Every launch starts a fresh session.
    path: Option<PathBuf>,

    /// Keep the desktop app attached to this process.
    #[arg(long, global = true)]
    foreground: bool,

    /// Print all built-in tool definitions and their strict mode flags as JSON.
    #[arg(long, conflicts_with_all = ["path", "foreground"])]
    export_tool_schemas: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Write a starter model/provider configuration.
    Install,
}

fn main() {
    if let Err(error) = real_main() {
        eprintln!("frances: {error:#}");
        std::process::exit(1);
    }
}

fn real_main() -> Result<()> {
    let cli = Cli::parse();

    if cli.export_tool_schemas {
        serde_json::to_writer_pretty(
            std::io::stdout().lock(),
            &frances_session::runtime::tool_schemas(),
        )?;
        return Ok(());
    }

    if let Some(Command::Install) = cli.command {
        return install::run();
    }

    // Canonicalize and validate before detaching so errors land on the
    // launching terminal and the child gets an unambiguous path.
    let path = cli.path.unwrap_or_else(|| PathBuf::from("."));
    let path = frances_core::env::invocation_dir().join(path);
    let workspace = Workspace::open(&path)?;

    if !cli.foreground {
        return launch_detached(&workspace);
    }

    app::run(workspace)
}

fn launch_detached(workspace: &Workspace) -> Result<()> {
    let current_executable = std::env::current_exe().context("resolve frances executable")?;
    #[cfg(target_os = "linux")]
    let executable = appimage::launcher_executable(&current_executable);
    #[cfg(not(target_os = "linux"))]
    let executable = current_executable;

    let mut command = ProcessCommand::new(executable);
    command.arg("--foreground");
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
    fn schema_export_is_exclusive() {
        assert!(
            Cli::try_parse_from(["frances", "--export-tool-schemas"])
                .unwrap()
                .export_tool_schemas
        );
        for args in [
            vec!["frances", "--export-tool-schemas", "some/dir"],
            vec!["frances", "--export-tool-schemas", "--foreground"],
            vec!["frances", "--export-tool-schemas", "install"],
        ] {
            assert!(Cli::try_parse_from(args).is_err());
        }
    }

    #[test]
    fn bare_launch_defaults_to_detached_cwd() {
        let cli = Cli::try_parse_from(["frances"]).unwrap();

        assert!(!cli.foreground);
        assert!(cli.path.is_none());
    }

    #[test]
    fn path_and_foreground_parse() {
        let cli = Cli::try_parse_from(["frances", "some/dir", "--foreground"]).unwrap();

        assert!(cli.foreground);
        assert_eq!(cli.path.unwrap(), std::path::Path::new("some/dir"));
    }
}
