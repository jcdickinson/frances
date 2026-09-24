//! Build host instructions once per model context using the worker filesystem.
use crate::{HarnessDeps, HarnessFs};
use frances_worker_protocol::{FileSearchOptions, FileSearchPatterns, FileSearchQuery};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

pub async fn load<D: HarnessDeps>(deps: &D) -> String {
    let mut prompt = String::from(
        "You are Frances, a coding assistant. Use the tools to complete the user's request. Follow the workspace instructions.\n",
    );
    prompt.push_str(include_str!("tools/prompts/editing.md"));
    prompt.push_str(&format!(
        "\nWorking directory: {:?}\nEditable roots: {:?}\n",
        deps.current_cwd(),
        deps.editable_roots()
    ));
    let mut candidates = global_candidates();
    for root in deps.editable_roots() {
        candidates.extend(local_candidates(root));
    }
    let mut seen = HashSet::new();
    let mut contents = HashSet::new();
    for path in candidates {
        let canonical = match deps.fs().canonicalize(&path).await {
            Ok(path) => path,
            Err(error) => {
                tracing::debug!(%error, ?path, "instruction candidate unavailable");
                continue;
            }
        };
        if !seen.insert(canonical) {
            continue;
        }
        match deps.fs().read_to_string(&path).await {
            Ok(content) => {
                if contents.insert(content.clone()) {
                    prompt.push_str(&format!(
                        "\n# Instructions from {}\n{}\n",
                        path.display(),
                        content
                    ));
                }
            }
            Err(error) => tracing::warn!(%error, ?path, "read instructions failed"),
        }
    }
    let nested = discover_nested(deps).await;
    if !nested.is_empty() {
        prompt.push_str(&format!("\nRead applicable nested instructions before editing files in their directories:\n{}\n", nested.join("\n")));
    }
    prompt
}
fn dirs_home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
}
fn global_candidates() -> Vec<PathBuf> {
    let xdg = xdg::BaseDirectories::new();
    let home = dirs_home();
    let mut candidates = Vec::new();

    // 1. ~/.claude/CLAUDE.md (lowest)
    candidates.push(home.join(".claude").join("CLAUDE.md"));

    // 2. System XDG config dirs. The xdg crate's get_config_dirs()
    //    returns user home first, then system dirs. We split them and
    //    reverse system dirs so lowest-priority comes first.
    let all_config_dirs = xdg.get_config_dirs();
    let config_home = xdg.get_config_home();
    let system_dirs: Vec<PathBuf> = all_config_dirs
        .into_iter()
        .filter(|d| config_home.as_ref() != Some(d))
        .collect();
    for dir in system_dirs.into_iter().rev() {
        // Generic before frances-specific (less specific → more specific).
        candidates.push(dir.join("AGENTS.md"));
        candidates.push(dir.join("frances").join("AGENTS.md"));
    }

    // 3. XDG_CONFIG_HOME (user dir; outranks all system dirs).
    if let Some(ch) = config_home {
        candidates.push(ch.join("AGENTS.md"));
        candidates.push(ch.join("frances").join("AGENTS.md"));
    }

    // 4. $HOME/AGENTS.md (highest).
    candidates.push(home.join("AGENTS.md"));

    candidates
}

fn local_candidates(root: &Path) -> Vec<PathBuf> {
    vec![
        root.join("CLAUDE.md"),
        root.join("CLAUDE.local.md"),
        root.join("AGENTS.md"),
        root.join("AGENTS.local.md"),
        root.join(".agents").join("frances").join("AGENTS.md"),
        root.join(".agents").join("frances").join("AGENTS.local.md"),
    ]
}

async fn discover_nested<D: HarnessDeps>(deps: &D) -> Vec<String> {
    let roots = deps.editable_roots();
    if roots.is_empty() {
        return Vec::new();
    }

    let mut seen_canonical: HashSet<PathBuf> = HashSet::new();
    let mut results: Vec<String> = Vec::new();

    // Build the set of root-level canonical paths to exclude.
    let mut root_level: HashSet<PathBuf> = HashSet::new();
    for root in roots {
        for name in &[
            "AGENTS.md",
            "AGENTS.local.md",
            "CLAUDE.md",
            "CLAUDE.local.md",
        ] {
            let p = root.join(name);
            match deps.fs().canonicalize(&p).await {
                Ok(canonical) => {
                    root_level.insert(canonical);
                }
                Err(error) => {
                    tracing::debug!(?p, ?error, "root agent candidate not found");
                }
            }
        }
        let frances = root.join(".agents").join("frances");
        for name in &["AGENTS.md", "AGENTS.local.md"] {
            let p = frances.join(name);
            match deps.fs().canonicalize(&p).await {
                Ok(canonical) => {
                    root_level.insert(canonical);
                }
                Err(error) => {
                    tracing::debug!(?p, ?error, "root agent candidate not found");
                }
            }
        }
    }

    for root in roots {
        let patterns = FileSearchPatterns::new(vec!["**/AGENTS.md".to_owned()])
            .expect("one nested agent pattern");
        let found = match deps
            .fs()
            .find_or_grep(FileSearchOptions {
                cwd: None,
                root: Some(root.clone()),
                query: FileSearchQuery::Paths { patterns },
                exclude: Vec::new(),
                ignore: true,
                hidden: true,
                depth: None,
            })
            .await
        {
            Ok(found) => found,
            Err(error) => {
                tracing::warn!(?root, ?error, "nested agent discovery failed");
                continue;
            }
        };

        for entry in found.entries {
            let path = root.join(entry.file.path);
            let canonical = deps.fs().canonicalize(&path).await.unwrap_or_else(|error| {
                tracing::debug!(?path, ?error, "nested agent canonicalization failed");
                path.clone()
            });

            if root_level.contains(&canonical) {
                continue;
            }
            if seen_canonical.insert(canonical) {
                results.push(path.to_string_lossy().into_owned());
            }
        }
    }

    results
}
