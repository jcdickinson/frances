// `frances:v1/context-sections` — environment and cwd prompt sections.
//
// Two stable section objects for `session.promptSections`:
//
//   - `envBlock` (immutable, front of prompt): OS, shell, platform, repo root,
//     date, and the quasi-persistent shell guidance rule. Always emits a string.
//   - `cwdBlock` (mutable, late in prompt): the live working directory.
//     Returns null when cwd is unavailable.

/**
 * Immutable environment block — cache-stable across turns.
 * Contains OS, shell, platform, repo root, date, and shell behavior rules.
 */
export const envBlock = {
  name: "env",

  prompt(ctx) {
    const lines = [];
    lines.push("Environment:");
    lines.push(`- OS: ${ctx.os}`);
    lines.push(`- Shell: ${ctx.shell}`);
    lines.push(`- Platform: ${ctx.platform}`);
    if (ctx.repoRoot != null) {
      lines.push(`- Repo root: ${ctx.repoRoot}`);
    }
    lines.push(`- Date: ${ctx.date}`);
    lines.push("");
    lines.push("Shell behavior:");
    lines.push(
      "- Shell tools use quasi-persistent shell state: the working directory always persists across completed shell_run calls.",
    );
    lines.push(
      "- Exported environment variables persist only when a shell_run call includes them in `persist`; `persist` applies to that one run and is not a durable watch list.",
    );
    lines.push(
      "- `FRANCES_ROOT` is reserved and Frances-managed. Persisted environment cannot override it.",
    );
    lines.push(
      '- You are already in the working directory shown below. Do not prefix commands with `cd` to an absolute path.',
    );
    lines.push(
      '- To change directory for subsequent commands, run `cd <dir>` as its own command; it persists.',
    );
    lines.push(
      "- Use paths relative to the working directory, or absolute paths.",
    );
    lines.push(
      "- Prefer the dedicated tools over shell equivalents: `file_read` instead of `cat`/`head`/`tail`, `file_find_or_grep` instead of shell `grep`/`find`. Use `shell_run` for actually running programs.",
    );

    return lines.join("\n");
  },
};

/**
 * Live working directory — mutable, re-rendered each turn.
 * Placed late in the prompt to keep it out of the cache prefix.
 * Returns null when cwd is unavailable.
 */
export const cwdBlock = {
  name: "cwd",

  prompt(ctx) {
    if (ctx.cwd == null) return null;
    return `Current working directory: ${ctx.cwd}`;
  },
};

