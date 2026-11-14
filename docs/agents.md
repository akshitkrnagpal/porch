# Porch for coding agents

Coding agents guess ports, test the wrong server and hardcode `localhost:3000`. Porch gives them names instead. There are two ways to connect an agent, and they work together:

- **Skills** teach the agent how to use the `porch` CLI: start apps in the background, read URLs from `porch list --json`, fix common errors. Any agent with a shell can use them.
- **The MCP server** (`porch mcp`) gives the agent tools to list routes, look up URLs and manage aliases without parsing CLI output.

With the `apps` profile (`porch mcp --tools core,apps`), agents start dev servers themselves through the `run` tool, which runs `porch run --detach` and returns the URL once the app answers, then read output with `logs` and end it with `stop`. Without it, start apps with `porch run` in a shell and use MCP to find them.

## Install the skills

The repo ships two skills:

| Skill | Use it for |
| --- | --- |
| [`porch`](../skills/porch/SKILL.md) | Running apps, finding URLs, monorepos, worktrees, HTTPS, troubleshooting |
| [`oauth`](../skills/oauth/SKILL.md) | Redirect URIs, provider rules, custom domains, auth library settings |

With the [skills CLI](https://skills.sh), which installs skills from a GitHub repo's `skills/` folder into Claude Code, Cursor, Codex and other agents:

```sh
npx skills add akshitkrnagpal/porch
```

Or copy them by hand. Each skill is one folder with a `SKILL.md`:

```sh
git clone --depth 1 https://github.com/akshitkrnagpal/porch /tmp/porch
cp -R /tmp/porch/skills/porch /tmp/porch/skills/oauth ~/.claude/skills/     # Claude Code, every project
cp -R /tmp/porch/skills/porch /tmp/porch/skills/oauth .claude/skills/       # Claude Code, this project only
```

For agents without skill support, paste the body of `skills/porch/SKILL.md` into the project's `AGENTS.md` or `CLAUDE.md`, or add a short pointer:

```md
## Dev servers
Run dev servers with Porch (`porch` or `porch run <cmd>`), in the background.
Find URLs with `porch list --json` or `porch get <name>`; never hardcode ports.
If a URL fails, run `porch doctor`.
```

## Connect the MCP server

`porch mcp` speaks MCP over stdio. Every client below runs the same command, `porch mcp`. If `porch` isn't on the PATH the client sees (GUI apps often have a shorter PATH than your shell), use the full path from `which porch`.

### Claude Code

```sh
claude mcp add porch -- porch mcp                         # this project
claude mcp add --scope user porch -- porch mcp            # every project
claude mcp add porch -- porch mcp --tools all             # include proxy control and diagnostics
```

Or commit `.mcp.json` at the project root so the whole team gets it:

```json
{
  "mcpServers": {
    "porch": { "command": "porch", "args": ["mcp"] }
  }
}
```

### Cursor

`.cursor/mcp.json` in the project, or `~/.cursor/mcp.json` for every project:

```json
{
  "mcpServers": {
    "porch": { "command": "porch", "args": ["mcp"] }
  }
}
```

### Codex

`~/.codex/config.toml`:

```toml
[mcp_servers.porch]
command = "porch"
args = ["mcp"]
```

Or `codex mcp add porch -- porch mcp`.

### VS Code (GitHub Copilot agent mode)

`.vscode/mcp.json`:

```json
{
  "servers": {
    "porch": { "type": "stdio", "command": "porch", "args": ["mcp"] }
  }
}
```

### Windsurf, Zed and other clients

Any client that launches stdio servers works. Use command `porch` and arguments `["mcp"]` in its MCP settings.

## Tool profiles

`porch mcp` starts with the `core` profile. Choose others with `--tools` (comma-separated) or as the positional argument (`porch mcp all`):

| Profile | Tools |
| --- | --- |
| `core` (also `default`) | `list`, `get`, `machines`, `logs`, `alias_add`, `alias_remove`, `tools_profiles` |
| `proxy` | `proxy_start`, `proxy_stop` |
| `diagnostics` | `doctor`, `prune`, `version`, `doctor_fix` |
| `apps` | `run`, `stop`, `logs`: start dev servers in the background and manage them |
| `readonly` | `list`, `get`, `machines`, `logs`, `tools_profiles`, `doctor`, `version`: nothing that changes routes, the proxy or processes |
| `all` (also `full`) | Everything above |

Example: `porch mcp --tools core,diagnostics`. Calling a tool outside the active profiles returns "Tool is disabled; restart porch mcp --tools all".

## Tools

Except `tools_profiles`, every tool accepts `cwd` (directory to run in) and `timeoutMs` (default 120000). Every option is a typed argument. Only `list` (`--all`, `--json`) and `get` (`--no-worktree`) also take `extraArgs`, limited to those flags, so an agent can't reach CLI options the schema doesn't show. `run` is marked destructive and open-world, because it runs a command you choose.

| Tool | Arguments | Runs | Notes |
| --- | --- | --- | --- |
| `list` | none | `porch list --json` | Routes with name, URL, target, pid, health, `private` and sharing URLs, plus proxy settings. `extraArgs: ["--all"]` includes paired machines. Read-only |
| `get` | `name` (required), `noWorktree` | `porch get NAME` | Pass `cwd` as the project directory so the worktree prefix matches. Read-only |
| `alias_add` | `name`, `port` (required), `force`, `tld`, `private`, `upstream` | `porch alias NAME PORT` | Routes a name to a local port; `upstream` takes `https`, `https://HOST` or `http://HOST` |
| `run` | `command` (array, required), `name`, `cwd` | `porch run --detach -- COMMAND` | Starts a dev server in the background and returns its URL once it answers |
| `stop` | `name` (required) | `porch stop NAME` | Stops an app and its process tree. Destructive |
| `logs` | `name` (required), `lines` | `porch logs NAME` | Output of a background app. Read-only |
| `machines` | none | `porch list --all --json` | This computer's and every paired machine's apps, with the local URL for each remote app. Read-only |
| `alias_remove` | `name` (required), `tld` | `porch remove NAME` | Removes an alias; app routes aren't touched |
| `tools_profiles` | none | — | Active and available profiles. Read-only |
| `proxy_start` | `port`, `noTls`, `lan`, `noLan`, `tld`, `wildcard`, `unprivileged` | `porch proxy start` | `--foreground` is refused over MCP |
| `proxy_stop` | `port` | `porch proxy stop` | Apps keep running |
| `doctor` | none | `porch doctor` | Report only; exits 1 when it finds a problem. Read-only |
| `doctor_fix` | `yes` | `porch doctor --fix` | Applies safe fixes; stops stopped or orphaned launches only with `yes`. Destructive |
| `prune` | `force` | `porch prune` | `force` also stops processes left behind |
| `version` | none | `porch --version` | Read-only |

Tools carry MCP annotations: `list`, `get`, `tools_profiles`, `doctor` and `version` are read-only; `alias_add`, `alias_remove`, `proxy_stop`, `prune` and `doctor_fix` are marked destructive, so clients that ask before destructive calls will ask.

The server runs commands non-interactively (`CI=1`, no colour), so nothing prompts. Stdout carries only protocol messages.

## A typical agent loop

1. Start the app in the background from the project directory: `porch` (or `porch run <cmd>`).
2. Poll `porch list --json` (or the `list` tool with `cwd`) until the route's `health` is `ok`.
3. Use the route's `url` for browser checks, `curl` or tests.
4. If it isn't `ok` after the app should be up, read the app's output, then run `porch doctor`.
5. Stop the app with Ctrl-C (or by ending the background process) when done; its route goes away with it.

Hand the URL to test runners through the environment instead of hardcoding it, for example `BASE_URL=$(porch get web) npx playwright test`.

## Why not just tell the agent the port?

Ports change between runs, collide across projects and differ between worktrees. A Porch name is the same every time for a given checkout, and `porch get` returns the right one for the directory the agent is in, including worktree prefixes, HTTPS and custom TLDs.
