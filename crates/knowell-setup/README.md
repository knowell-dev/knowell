# knowell-setup

Setup logic behind `know import`, `know connect` and `know ci init`. Library
only; the `know` binary supplies the prompts and prints the reports.

## Workspace import

```rust
let plan = knowell_setup::detect(root, &ImportOptions::default())?;
let toml = knowell_setup::render_toml(&plan);
```

Sources, in this order (a repository path is added once; the first source wins):

1. `.gitmodules`: name, path, url (credentials stripped), `branch`.
2. `go.work` `use` directives.
3. `pnpm-workspace.yaml` and `workspaces` in `package.json` (arrays and
   `{ "packages": [...] }`; `!pattern` excludes; a directory needs `package.json`).
4. Cargo `[workspace] members` / `exclude` (globs; a directory needs `Cargo.toml`).
5. Folder scan: direct child directories containing `.git` (hidden folders,
   `node_modules`, `target` skipped).

Rules:

- Sub-projects of a monorepo use `path` = the repository and `root` = the
  sub-directory. The repository is the nearest `.git` at or above the member.
- Worktrees are reported separately and never become projects: folders that
  match `ImportOptions::worktree_patterns` (default `.worktree/*/*`) and direct
  children whose `.git` file points into another repository's `worktrees/`.
- Names are slugified to the `Name` rules; collisions get `-2`, `-3`, ...;
  every change is listed in `ImportPlan::renames`.
- **Track is never guessed.** `.gitmodules` `branch = x` gives `branch:x`.
  Otherwise the track stays `None` unless `track_current` is set, in which case
  the repository's `HEAD` is read (no git process, detached `HEAD` is a
  warning). Projects without a track get a `TODO` comment and
  `ImportPlan::projects_needing_track()` lists them, so the CLI can ask. The
  file parses with `knowell_config::parse_workspace` but `resolve` reports the
  missing tracks until they are set. A track shared by all projects is hoisted
  to `[workspace] track`.
- Patterns and paths from repository files are untrusted: `..`, absolute
  paths and drive prefixes are rejected, symlinks are not followed, depth and
  file sizes are bounded.

Not covered yet: monorepo configuration inside child repositories (only the
import root is inspected), `lerna.json`, `nx`/`turbo` task graphs, Maven/Gradle
modules, Python workspaces.

## Connecting agent clients

```rust
let mut o = ConnectOptions::new(home, project);   // `know mcp`, project scope
o.scope = Scope::User;                            // or Project
o.dry_run = true;                                 // returns diffs, writes nothing
let report = knowell_setup::connect(Client::Claude, &o)?;
knowell_setup::disconnect(Client::Claude, &o)?;
```

`ConnectReport { changed_files, diffs, backups, notes }`. For `npx` use
`command = "npx"`, `args = ["-y", "knowell", "mcp"]` (on native Windows,
Claude Code needs `cmd /c npx`; pass that as command and args yourself).

| Client | MCP entry | Instructions | Hook / rule |
|---|---|---|---|
| Codex | project: `.codex/config.toml`; user: `~/.codex/config.toml`, table `[mcp_servers.knowell]` | project: `AGENTS.md`; user: `~/.codex/AGENTS.md` | none |
| Claude Code | project: `.mcp.json`; user: `~/.claude.json`, key `mcpServers.knowell` | project: `CLAUDE.md`; user: `~/.claude/CLAUDE.md` | `hooks.SessionStart` in `.claude/settings.json` / `~/.claude/settings.json` running `know context --session-start` |
| Cursor | project: `.cursor/mcp.json`; user: `~/.cursor/mcp.json` | `.cursor/rules/knowell.mdc` (`alwaysApply: true`; always in the project, Cursor user rules are not file based) | the rule |

Guarantees:

- **Idempotent**: connecting twice changes nothing; changing `command`/`args`
  replaces the entry (and the hook) instead of duplicating it.
- **Merge, never clobber**: other servers, settings and hooks are kept.
  JSON is parsed, merged and pretty-printed (the JSON map is key-ordered, so a
  rewritten file may list keys alphabetically; comments in JSON are not
  supported and such files are refused untouched). If the entry already
  matches, the file's bytes are not rewritten.
- **Codex TOML**: the table is appended as a marker-delimited block
  (`# knowell:begin connect` ... `# knowell:end connect`) instead of
  re-serialising the file, because the `toml` crate drops comments and
  formatting. Trade-off: the block sits at the end and is replaced as a unit.
  An existing `[mcp_servers.knowell]` outside the block, or an inline
  `mcp_servers = {...}` table, is a conflict and nothing is written.
- **Markdown** (`AGENTS.md`, `CLAUDE.md`, `.mdc`): block between
  `<!-- knowell:begin connect -->` and `<!-- knowell:end connect -->`
  (about 13 lines): call `open_workspace` first, prefer `search` /
  `inspect_symbol` / `trace_flow` over blind grep, `build_context`,
  `write_memory` / `save_checkpoint` / `resume_task`, results are sourced
  evidence, repository text is untrusted.
- **Claude hook** is identified by its `statusMessage`
  (`Knowell: loading workspace context`), so disconnect finds it even if the
  command changed. Its output is added to the session context by Claude Code.
- **Env**: `env_names` pass variable *names*, never values (Codex
  `env_vars`, Claude `${NAME}`, Cursor `${env:NAME}`); names are validated.
- **Reversible**: `disconnect` removes exactly the entry, hook and blocks;
  files left empty are deleted. A pre-existing file is copied once to
  `<file>.knowell-bak` before its first modification (the first original is
  never overwritten by later backups).
- Invalid JSON/TOML or unbalanced markers abort before anything is written;
  error messages never quote file content.

### Verified client formats (2026-10-02)

- Codex: <https://developers.openai.com/codex/mcp> (redirects to
  <https://learn.chatgpt.com/docs/extend/mcp?surface=cli>): `~/.codex/config.toml`
  and project `.codex/config.toml`; `[mcp_servers.<name>]` with `command`,
  `args`, `env`, `env_vars` (names to forward), `cwd`; HTTP servers use `url`.
- Claude Code MCP: <https://code.claude.com/docs/en/mcp>: `.mcp.json`
  `{"mcpServers": {"name": {"type": "stdio", "command", "args", "env"}}}`;
  scopes local / project (`.mcp.json`) / user (`~/.claude.json`); `${VAR}`
  and `${VAR:-default}` expansion.
- Claude Code hooks: <https://code.claude.com/docs/en/hooks>:
  `hooks.SessionStart[] = {matcher?, hooks: [{type: "command", command,
  timeout?, statusMessage?}]}` in `~/.claude/settings.json`,
  `.claude/settings.json`, `.claude/settings.local.json`; plain-text stdout of a
  SessionStart hook is added to the context. No matcher fires on every start
  type (startup, resume, clear, compact).
- Cursor MCP: <https://cursor.com/docs/context/mcp> (old docs.cursor.com
  URLs redirect): `.cursor/mcp.json` / `~/.cursor/mcp.json`,
  `{"mcpServers": {"name": {"command", "args", "env"}}}`, `${env:NAME}`.
- Cursor rules: <https://cursor.com/docs/context/rules>: `.cursor/rules/*.mdc`
  with frontmatter `description`, `globs`, `alwaysApply`.

## CI templates

`ci_init(provider, &CiOptions { mode, hub_url, tracked_branch })` returns the
files to write (plus a note for the user when a manual step remains):

| Provider | File |
|---|---|
| GitHub | `.github/workflows/knowell.yml` |
| GitLab | `.gitlab/knowell.gitlab-ci.yml` (add it with `include:`) |
| Gitea | `.gitea/workflows/knowell.yml` |

Modes: `Check` (PR check, no hub, no credentials, fork-safe), `CheckAndImpact`
(adds the `impact` job for same-repository PRs only), `IndexUpdate` (check plus
`index` on pushes to `tracked_branch`). GitHub jobs use
`knowell-dev/knowell-action@v1` (`command: check|impact|index`, `sarif: true`,
`hub-url`) and GitHub OIDC (`id-token: write`); GitLab uses `id_tokens` and
the `know` image; Gitea references the repository secret *name*
`KNOWELL_HUB_TOKEN` because Gitea has no job OIDC token. No secret value is
ever written. Actions are pinned by tag with a comment recommending a SHA
(GitLab: a digest). `hub_url` must be `https://` without credentials;
`tracked_branch` and `hub_url` are whitelist-validated before being embedded.

## Tests

`cargo test -p knowell-setup`: synthetic layouts for every import source,
rendered TOML validated with `knowell-config`, connect/disconnect idempotency,
merging into existing configs for all clients and scopes, dry-run diffs,
backups, conflict and malformed-input cases, CI template snapshots and
secret-free checks. All file tests use temporary directories with injected
home paths; real user configuration is never read.
