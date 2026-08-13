mod app;
mod install;
mod tty;

use std::ffi::OsString;
use std::process::{Command as ProcessCommand, Stdio};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use frances_session::tty::TtyKey;

#[derive(Debug, Parser)]
#[command(name = "frances")]
struct Cli {
    /// Keep the desktop app attached to this process.
    #[arg(long, global = true)]
    foreground: bool,

    /// TTY identity captured by the detached launcher.
    #[arg(long, global = true, hide = true)]
    tty_key: Option<String>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Unlink the current terminal's session so this launch starts fresh.
    New {
        /// Workflow to start in the new session. Defaults to `default_workflow`.
        workflow: Option<String>,
    },
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

    let tty_key = match cli.tty_key {
        Some(key) => TtyKey(key),
        None => tty::controlling_tty_key()?,
    };

    if !cli.foreground {
        return launch_detached(&tty_key);
    }

    app::run(tty_key, cli.command)
}

fn launch_detached(tty_key: &TtyKey) -> Result<()> {
    let executable = std::env::current_exe().context("resolve frances executable")?;
    let mut args: Vec<OsString> = std::env::args_os().skip(1).collect();
    args.push("--foreground".into());
    args.push("--tty-key".into());
    args.push(tty_key.0.clone().into());

    let mut command = ProcessCommand::new(executable);
    command
        .args(args)
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

    use super::{Cli, Command};

    #[test]
    fn launches_detached_by_default() {
        let cli = Cli::try_parse_from(["frances"]).unwrap();

        assert!(!cli.foreground);
        assert!(cli.command.is_none());
    }

    #[test]
    fn foreground_is_global() {
        let cli = Cli::try_parse_from(["frances", "new", "review", "--foreground"]).unwrap();

        assert!(cli.foreground);
        assert!(matches!(
            cli.command,
            Some(Command::New {
                workflow: Some(ref workflow)
            }) if workflow == "review"
        ));
    }
}
