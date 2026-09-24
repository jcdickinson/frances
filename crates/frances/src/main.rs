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
    #[arg(long, conflicts_with_all = ["path", "foreground", "presets", "mcp_servers", "mcp"])]
    export_tool_schemas: bool,

    /// Compose MCP presets for this session (repeat, or separate with +).
    #[arg(long = "preset", value_delimiter = '+')]
    presets: Vec<String>,

    /// Enable an additional configured MCP server.
    #[arg(long = "mcp-server")]
    mcp_servers: Vec<String>,

    /// Load MCP server definitions from an explicit mcp.json file.
    #[arg(long, value_name = "PATH")]
    mcp: Option<PathBuf>,

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

    let selection = frances_mcp::Selection {
        presets: cli.presets,
        servers: cli.mcp_servers,
    };
    let mcp_path = cli
        .mcp
        .map(|path| frances_core::env::invocation_dir().join(path));
    if !cli.foreground {
        return launch_detached(&workspace, &selection, mcp_path.as_deref());
    }

    let mut overrides = frances_session::runtime::StartOverrides {
        mcp_selection: selection,
        ..Default::default()
    };
    if let Some(path) = mcp_path {
        overrides
            .extra_config_providers
            .push(std::sync::Arc::new(frances_mcp::McpJsonProvider::new(path)));
    }
    app::run(workspace, overrides)
}

fn launch_detached(
    workspace: &Workspace,
    selection: &frances_mcp::Selection,
    mcp_path: Option<&std::path::Path>,
) -> Result<()> {
    let current_executable = std::env::current_exe().context("resolve frances executable")?;
    #[cfg(target_os = "linux")]
    let executable = appimage::launcher_executable(&current_executable);
    #[cfg(not(target_os = "linux"))]
    let executable = current_executable;

    let mut command = ProcessCommand::new(executable);
    command.arg("--foreground");
    if let Some(path) = mcp_path {
        command.arg("--mcp").arg(path);
    }
    for preset in &selection.presets {
        command.arg("--preset").arg(preset);
    }
    for server in &selection.servers {
        command.arg("--mcp-server").arg(server);
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
        assert!(cli.presets.is_empty());
        assert!(cli.mcp_servers.is_empty());
        assert!(cli.mcp.is_none());
    }

    #[test]
    fn mcp_presets_compose_at_launch() {
        let cli = Cli::try_parse_from([
            "frances",
            "--preset",
            "frances+rust",
            "--preset",
            "project",
            "--mcp-server",
            "docs",
        ])
        .unwrap();
        assert_eq!(cli.presets, ["frances", "rust", "project"]);
        assert_eq!(cli.mcp_servers, ["docs"]);
    }

    #[test]
    fn explicit_mcp_source_does_not_select_servers() {
        let cli = Cli::try_parse_from(["frances", "--mcp", "config/mcp.jsonc"]).unwrap();
        assert_eq!(cli.mcp.unwrap(), std::path::Path::new("config/mcp.jsonc"));
        assert!(cli.mcp_servers.is_empty());
        assert!(
            Cli::try_parse_from(["frances", "--export-tool-schemas", "--mcp", "mcp.json"]).is_err()
        );
        assert!(Cli::try_parse_from(["frances", "--mcp"]).is_err());
    }

    #[test]
    fn path_and_foreground_parse() {
        let cli = Cli::try_parse_from(["frances", "some/dir", "--foreground"]).unwrap();

        assert!(cli.foreground);
        assert_eq!(cli.path.unwrap(), std::path::Path::new("some/dir"));
    }
}
