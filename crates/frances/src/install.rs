//! `frances install` — write a starter config and drop the `main` workflow
//! into the user's config dir.
//!
//! The workflow script is always (re)installed; the questionnaire only runs
//! when `config.toml` doesn't already exist, so re-running `install` refreshes
//! `main.ts` without clobbering a config the user has since edited.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Stable id for the `main` workflow. It namespaces this workflow's rows in the
/// per-session DB. Frances has never shipped, so a fixed constant is fine.
const MAIN_WORKFLOW_ID: &str = "e3c5d9f6-141b-4cf8-b6ad-41e5a9cdee43";

/// The workflow script shipped with the binary, embedded at compile time.
const EMBEDDED_MAIN_TS: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../assets/workflows/main.ts"
));

/// Absolute path to the in-repo workflow script, baked in at compile time.
/// `--local` points the generated config at this instead of copying.
const LOCAL_MAIN_TS: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../assets/workflows/main.ts"
);

pub fn run(local: bool) -> Result<()> {
    let config_home = xdg::BaseDirectories::with_prefix("frances")
        .get_config_home()
        .context("could not determine the XDG config home (is HOME set?)")?;
    fs::create_dir_all(&config_home)
        .with_context(|| format!("create config dir {}", config_home.display()))?;

    let workflow_file = install_workflow(&config_home, local)?;

    let config_path = config_home.join("config.toml");
    if config_path.exists() {
        println!(
            "Config already exists at {} — leaving it untouched.",
            config_path.display()
        );
        return Ok(());
    }

    let config = prompt_config(&config_home, &workflow_file)?;
    fs::write(&config_path, config)
        .with_context(|| format!("write config to {}", config_path.display()))?;
    println!("Wrote config: {}", config_path.display());
    println!("\nDone. Run `frances` to open the desktop app.");
    Ok(())
}

/// Resolve the path the config's `[workflows.main]` should point at. With
/// `--local` that's the in-repo script (no copy); otherwise the embedded
/// script is copied into the config dir and that copy's path is returned.
fn install_workflow(config_home: &Path, local: bool) -> Result<PathBuf> {
    if local {
        let path = fs::canonicalize(LOCAL_MAIN_TS)
            .with_context(|| format!("locate in-repo workflow at {LOCAL_MAIN_TS}"))?;
        println!("Using in-repo workflow: {}", path.display());
        return Ok(path);
    }

    let dest = config_home.join("workflows/main/main.ts");
    let dir = dest.parent().expect("workflow dest always has a parent");
    fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    fs::write(&dest, EMBEDDED_MAIN_TS)
        .with_context(|| format!("write workflow to {}", dest.display()))?;
    println!("Wrote workflow: {}", dest.display());
    Ok(dest)
}

/// Ask the provider questions and render a starter `config.toml`. Values are
/// emitted as single-quoted TOML literals, so no escaping is needed.
fn prompt_config(config_home: &Path, workflow_file: &Path) -> Result<String> {
    let provider_id;
    let model_id;
    let provider_block;

    if prompt_yes_no("Use your Codex (ChatGPT) login?", true)? {
        provider_id = "codex".to_string();
        model_id = prompt_default("Model id", "gpt-5.5")?;
        provider_block = CODEX_PROVIDER_BLOCK.to_string();
    } else {
        provider_id = prompt_required("Provider id (e.g. deepseek)")?;
        let kind = prompt_required("Provider kind (e.g. deepseek, anthropic, openai-chat)")?;
        let base_url = prompt_required("Base URL")?;
        model_id = prompt_required("Model id")?;
        let token = prompt_required("API token")?;

        let token_path = config_home.join(format!("{provider_id}.txt"));
        fs::write(&token_path, format!("{token}\n"))
            .with_context(|| format!("write token to {}", token_path.display()))?;
        println!("Wrote token: {}", token_path.display());

        provider_block = format!(
            "[model_providers.{provider_id}]\n\
             kind = '{kind}'\n\
             base_url = '{base_url}'\n\
             auth = {{ file = '{}' }}\n",
            token_path.display()
        );
    }

    let effort_config = if provider_id == "codex" {
        "effort = 50\neffort_tiers = 'openai'\n"
    } else {
        ""
    };

    Ok(format!(
        "{provider_block}\n\
         [models.default]\n\
         model_provider = '{provider_id}'\n\
         id = '{model_id}'\n\
         {effort_config}\
         \n\
         default_workflow = 'main'\n\
         \n\
         [workflows.main]\n\
         id = '{MAIN_WORKFLOW_ID}'\n\
         file = '{}'\n",
        workflow_file.display()
    ))
}

const CODEX_PROVIDER_BLOCK: &str = "\
[model_providers.codex]
kind = 'openai-responses'
name = 'Codex'
base_url = 'https://chatgpt.com/backend-api/codex/'
auth = { codex = true }

[model_providers.codex.http_headers]
OpenAI-Beta = 'responses=experimental'
originator = 'codex_cli_rs'
";

fn prompt_line(question: &str) -> Result<String> {
    print!("{question} ");
    io::stdout().flush().ok();
    let mut line = String::new();
    io::stdin().read_line(&mut line).context("read stdin")?;
    Ok(line.trim().to_string())
}

fn prompt_required(label: &str) -> Result<String> {
    loop {
        let value = prompt_line(&format!("{label}:"))?;
        if !value.is_empty() {
            return Ok(value);
        }
        println!("  (required)");
    }
}

fn prompt_default(label: &str, default: &str) -> Result<String> {
    let value = prompt_line(&format!("{label} [{default}]:"))?;
    Ok(if value.is_empty() {
        default.to_string()
    } else {
        value
    })
}

fn prompt_yes_no(question: &str, default_yes: bool) -> Result<bool> {
    let hint = if default_yes { "[Y/n]" } else { "[y/N]" };
    loop {
        let value = prompt_line(&format!("{question} {hint}"))?;
        match value.to_ascii_lowercase().as_str() {
            "" => return Ok(default_yes),
            "y" | "yes" => return Ok(true),
            "n" | "no" => return Ok(false),
            _ => println!("  (please answer y or n)"),
        }
    }
}
