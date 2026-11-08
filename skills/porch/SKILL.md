---
name: porch
description: Run dev servers behind stable named URLs with Porch (http://web.localhost:7462 instead of localhost:3000), and find the URL of any running app. Use when starting or restarting a dev server, when you need an app's URL to test or open it, when wiring one local app to another (API_URL, CORS, OAuth callbacks), in monorepos, Turborepo or git worktrees, or when a local URL shows a 502, "Waiting for the app to start", 404, 508 or certificate error. Triggers include "start the dev server", "what URL is the app on", "porch", "*.localhost", "port already in use", "EADDRINUSE", "app not reachable locally", "porch list", "porch doctor".
---

# Porch

Porch runs a dev command on a free port and serves it at a stable name such as `http://NAME.localhost:7462`. The proxy port depends on the user's setup (7462 by default; some setups serve on 80 or 443 with no port in the URL), so never guess or hardcode a URL or port. Ask Porch.

## Find URLs (do this first)

```sh
porch list --json        # every route: name, url, target, pid, health
porch get api            # URL for "api", even if it isn't running yet
```

`porch list --json` returns `{"running", "proxy", "proxy_version", "routes": [...]}`. Each route has `name`, `url`, `target`, `pid`, `health` and `urls` (sharing URLs, if any). Health is one of:

| `health` | Meaning | What to do |
| --- | --- | --- |
| `ok` | Target accepts connections | Use `url` |
| `not listening` | Nothing on the target port | App is starting, crashed, or ignores `PORT`. Check its output |
| `stopped` | Suspended by job control | `porch doctor --fix` |
| `dead` | Launcher gone, processes left | `porch prune --force` |

Use the `url` field verbatim. It already includes the scheme, port, TLD and any worktree prefix.

## Start apps

```sh
porch                         # run package.json "dev" (every package at a workspace root)
porch run pnpm dev            # any command; name from package.json, Git root or folder
porch api node server.js      # explicit name -> http://api.localhost:7462
porch --name list node x.js   # when the name clashes with a subcommand
porch --script start          # a different package script
porch api --app-port 4500 -- node server.js   # fixed app port
```

Porch sets `PORT`, `HOST=127.0.0.1` and `PORCH_URL` for the app. Most servers read `PORT`. Vite, VitePress, Astro, Angular, React Router, Rsbuild, Expo, React Native and Wrangler get `--port`/`--host` flags added automatically. For anything else, pass the port yourself: `porch api sh -c 'python manage.py runserver 127.0.0.1:$PORT'`.

Dev servers are long-running. Start them in the background (or a separate terminal), then poll `porch list --json` until the route's `health` is `ok`. Porch waits up to 15 s for the port to open and prints a warning if it doesn't; it never kills the app for being slow.

Ctrl-C stops the app and removes its route. `PORCH=0 pnpm dev` runs the command with no Porch at all.

## Names that already exist

- `route NAME already exists; use --force`: another app has the name. Check `porch list` first. `--force` stops that app and takes over; only do this if the other app is yours to stop.
- `porch alias db 5432` routes a name to something Porch didn't start (a port or an `http(s)://` URL). Remove it with `porch alias --remove db`.

## Cross-app references

Don't hardcode ports between apps. Resolve them by name:

```json
{ "scripts": { "dev": "API_URL=$(porch get api) next dev" } }
```

`porch get` applies the current checkout's worktree prefix, so each worktree talks to its own API. `--no-worktree` gives the unprefixed URL.

## Monorepos and Turborepo

Run bare `porch` at a pnpm, npm, Yarn or Bun workspace root. Each package with the script gets its own route, named `<package>.<project>` (for `@shop/web` and `@shop/api`: `web.shop`, `api.shop`). Build-only scripts (`tsc`, `tsup`) run without a route. `porch workspace` lists the packages it found.

Override names in the root `porch.json`, keyed by path:

```json
{ "apps": { "apps/web": { "name": "shop" }, "apps/api": { "name": "api.shop" } } }
```

With `turbo.json`, `porch` runs the Turbo task once and still gives each package its own port and URL. Filter with `porch -- --filter=web`. Don't run `porch run turbo dev` at a multi-package root: Porch rejects it because every package would share one port.

## Git worktrees

In a linked worktree on branch `fix/login`, names get a prefix: `login.web.localhost`. Main checkouts and `main`/`master` get none. Always read the URL from `porch get` or `porch list` rather than assuming the unprefixed name. `--prefix ''` turns the prefix off for one run.

## HTTPS, other TLDs, LAN

```sh
porch proxy start --https              # https://web.localhost:7462, HTTP/2, per-user CA, no sudo
porch trust                            # trust the CA (browser warnings otherwise)
porch proxy start --tld test           # web.test instead of web.localhost (needs hosts entries)
porch api --lan pnpm dev               # also publish api.local over mDNS for phones
```

Proxy settings are saved. If a proxy with different settings is running, Porch says `the running proxy uses different settings (...); run porch proxy stop and retry`. Stopping the proxy doesn't stop apps.

Apps Porch launches trust its CA through `NODE_EXTRA_CA_CERTS`. For other tools: `curl --cacert ~/.porch/ca.pem` or `porch trust --system` (sudo).

## Coding-agent integration (MCP)

If the `porch` MCP server is connected, prefer its tools: `list`, `get`, `machines`, `logs`, `alias_add`, `alias_remove`, `tools_profiles` (default `core` profile); `run`, `stop`, `logs` (`--tools apps`: start a dev server in the background and get its URL); `proxy_start`, `proxy_stop` (`--tools proxy`); `doctor`, `doctor_fix`, `prune`, `version` (`--tools diagnostics`). Pass `cwd` to `get` and `run` so names match the project. Without MCP, `porch run -d COMMAND` does the same as the `run` tool from a shell. Setup: `docs/agents.md` in the Porch repo.

## When something is wrong

Run `porch doctor` first. It checks the proxy, routes, DNS for `.localhost`, the CA and external tools, and prints each problem with the fix. `porch doctor --fix` applies safe fixes (moves off a busy port, prunes dead routes, restarts an outdated proxy); add `--yes` to also stop stuck processes.

| You see | Cause | Fix |
| --- | --- | --- |
| "Waiting for the app to start" page, or JSON `upstream_unavailable` | Route exists, nothing on its port | Wait; if it persists the app crashed or ignores `PORT`. Porch's warning names the port it found: rerun with `--app-port N` |
| 404 "No app is registered for this hostname" | Wrong name or app not running | `porch list`; check the worktree prefix |
| 508 `loop_detected` | A dev-server proxy forwards to a Porch URL without changing Host | Set `changeOrigin: true` in that proxy config |
| Vite "Blocked request. This host is not allowed" | Host not in Vite's allow list | Porch sets `__VITE_ADDITIONAL_SERVER_ALLOWED_HOSTS` for configured TLDs; add others to `server.allowedHosts` |
| `curl: Could not resolve host` | Tool doesn't resolve `*.localhost` | `curl --resolve web.localhost:7462:127.0.0.1 ...` or `sudo porch hosts sync` |
| `proxy port 7462 unavailable: it is used by ...` | Port taken | `porch doctor --fix`, or `porch proxy start --port N` |

Porch's own error responses are JSON when the client doesn't ask for HTML: `{"error": "not_found", "upstream": "web.localhost:7462", "message": "..."}`. Parse that instead of scraping pages. A response without that `error`/`upstream` shape came from the app itself. More: `docs/troubleshooting.md` in the Porch repo.

## Don'ts

- Don't hardcode `localhost:3000`-style ports in code, tests or env files. Use `PORCH_URL` or `porch get`.
- Don't kill processes by port. Use Ctrl-C on the launcher, `porch prune --force` or `porch doctor --fix`.
- Don't use `--force` on a name you didn't start without asking.
- Don't run `porch clean` unless asked: it removes trust, services, hosts entries and all state.
