# Recipes

Short setups for common frameworks and workflows. Examples show plain HTTP on port 7462 and the `.localhost` TLD. If your proxy uses another port, HTTPS or another TLD, the URLs change to match; `porch get NAME` always prints the real one.

Two rules cover most apps:

1. Porch sets `PORT`, `HOST=127.0.0.1` and `PORCH_URL` for the app. If the app reads `PORT`, nothing else is needed.
2. If it doesn't, pass the port on the command line with `$PORT`, or let Porch add flags for the frameworks it knows (Vite, VitePress, VitePlus, Astro, Angular, React Router, Rsbuild, Expo, React Native, Wrangler).

Contents: [Next.js](#nextjs) · [Vite](#vite) · [Astro](#astro) · [Nuxt](#nuxt) · [SvelteKit](#sveltekit) · [Express and Node](#express-and-node) · [Rails](#rails) · [Django](#django) · [Flask and FastAPI](#flask-and-fastapi) · [Go and other languages](#go-and-other-languages) · [Docker Compose](#docker-compose) · [Monorepos](#monorepos-pnpm-npm-yarn-bun) · [Turborepo](#turborepo) · [Git worktrees](#git-worktrees) · [Cross-app URLs](#cross-app-urls) · [OAuth callbacks](#oauth-callbacks) · [HTTPS](#https) · [Phones and tablets](#phones-and-tablets) · [Sharing outside your machine](#sharing-outside-your-machine) · [CI and scripts](#ci-and-scripts)

## Next.js

```sh
porch                    # runs "dev" (next dev); Next reads PORT
porch run next dev       # same, without a package script
```

Optionally pin the name in `package.json`: `"porch": { "name": "web" }`. If Next 15+ logs "Blocked cross-origin request", add `allowedDevOrigins: ["*.localhost"]` to `next.config.js`.

## Vite

```sh
porch                    # "dev": "vite" gets --port and --host added
```

Porch adds `--port <PORT> --strictPort --host 127.0.0.1` (keeping any port or host flag you already set) and allows the `.localhost` hosts through `__VITE_ADDITIONAL_SERVER_ALLOWED_HOSTS`. Works the same for `vite preview`, VitePress and `vp`. Scripts with `&&`, `|`, a trailing comment or their own `--` are left alone; set `--port $PORT` yourself there.

To call another local app through Vite's dev proxy, set `changeOrigin` so the request doesn't loop back to the same app:

```js
server: { proxy: { "/api": { target: "http://api.localhost:7462", changeOrigin: true } } }
```

## Astro

```sh
porch                    # "dev": "astro dev" gets --port and --host
```

## Nuxt

Nuxt reads `PORT` and `HOST`:

```sh
porch                    # "dev": "nuxt dev"
```

If Vite inside Nuxt blocks the host, add `vite: { server: { allowedHosts: [".localhost"] } }` to `nuxt.config.ts`.

## SvelteKit

SvelteKit's dev server is Vite, so `"dev": "vite dev"` gets flags automatically:

```sh
porch
```

## Express and Node

```js
const port = Number(process.env.PORT ?? 3000);
app.listen(port, process.env.HOST ?? "127.0.0.1");
```

```sh
porch api node server.js        # http://api.localhost:7462
porch api node --watch server.js
```

If the app hard-codes a port you can't change, route to it instead: `porch api --app-port 3000 -- node server.js`.

## Rails

`rails server` reads `PORT`:

```sh
porch app bin/rails server      # http://app.localhost:7462
```

Rails allows `.localhost` hosts in development. With a custom TLD, add it: `config.hosts << ".local.example.com"`.

## Django

`runserver` doesn't read `PORT`, so pass it through a shell:

```sh
porch app sh -c 'python manage.py runserver 127.0.0.1:$PORT'
```

In development settings: `ALLOWED_HOSTS = [".localhost"]`.

## Flask and FastAPI

```sh
porch api sh -c 'flask --app app run --port $PORT'
porch api sh -c 'uvicorn main:app --reload --port $PORT'
```

## Go and other languages

Any server that reads `PORT` works directly:

```go
http.ListenAndServe("127.0.0.1:"+os.Getenv("PORT"), nil)
```

```sh
porch api go run .
```

For anything else, use the `sh -c '... $PORT'` pattern above, or `--app-port N` if the port is fixed.

## Docker Compose

```sh
porch compose up            # also: porch compose -f compose.dev.yml -p shop up -d
porch compose down
```

Porch reads the Compose model, publishes each HTTP service's port on a stable loopback port, and registers `SERVICE.PROJECT`, for example `http://web.shop.localhost:7462`. A service with several HTTP ports also gets `PORT.SERVICE.PROJECT` names. Ports for databases and other non-HTTP services are left alone. Running it again reuses the same ports, and `down` removes only that project's routes.

Control it per service with labels:

```yaml
services:
  web:
    labels:
      portless.name: shop        # name instead of the service name
      portless.ports: "3000"     # which container ports to route
  worker:
    labels:
      portless.enable: "false"   # no route
```

To name a container you started some other way, alias its published port: `porch alias grafana 3001`.

## Monorepos (pnpm, npm, Yarn, Bun)

```sh
porch                       # at the workspace root: every package with a "dev" script
cd apps/web && porch        # one package
porch workspace             # list the packages Porch found
```

Packages are named `<package>.<project>`: with `@shop/web` and `@shop/api` you get `web.shop.localhost` and `api.shop.localhost`. Set names in the root `porch.json`:

```json
{
  "$schema": "https://raw.githubusercontent.com/akshitkrnagpal/porch/main/schema/porch.schema.json",
  "apps": {
    "apps/web": { "name": "shop" },
    "apps/api": { "name": "api.shop", "appPort": 4500 },
    "packages/ui": { "proxy": false }
  }
}
```

Build-only scripts (`tsc`, `tsup`) run without a route. If two packages would get the same name, Porch stops before starting anything. [examples/monorepo](../examples/monorepo) is a runnable two-package workspace.

## Turborepo

With `turbo.json` at the root, `porch` runs the Turbo task once, and each package still gets its own `PORT`, URL and framework flags. Turbo keeps its task graph and TUI.

```sh
porch                         # turbo run dev, with a route per package
porch -- --filter=web         # arguments after -- go to Turbo
```

Don't wrap Turbo yourself at a multi-package root (`porch run turbo dev`, or a root `"dev": "turbo dev"` run through `porch run pnpm dev`): Porch rejects it, because every package would share one port. If a package's own `dev` script must be `porch`, put the real command in another script and point Porch at it:

```json
{ "scripts": { "dev": "porch", "dev:app": "next dev" }, "porch": { "script": "dev:app" } }
```

Set `"turbo": false` in the root `porch.json` to start packages directly instead.

## Git worktrees

Nothing to set up. In a linked worktree on branch `fix/login`, names get a prefix:

```
main checkout        http://web.localhost:7462
worktree fix/login   http://login.web.localhost:7462
```

So two checkouts of the same app run side by side. Change the layout in `porch.json`:

```json
{ "worktree": { "prefix": "branch", "hostnameTemplate": "{worktree}-{name}" } }
```

That gives `fix-login-web.localhost`. `--prefix ''` (or `PORCH_PREFIX=`) turns the prefix off for one run; `--prefix demo` sets it yourself.

## Cross-app URLs

Don't hardcode one app's port in another. `porch get` prints a URL whether or not the app is running yet, with the same worktree prefix the target will get:

```json
{ "scripts": { "dev": "NEXT_PUBLIC_API_URL=$(porch get api) next dev" } }
```

The app's own URL is always in `PORCH_URL`. For CORS, allow that origin rather than `localhost:3000`.

## OAuth callbacks

Register `$(porch get web)/api/auth/callback/github` once; it stays valid across restarts. Have your auth library build callbacks from `PORCH_URL` (for NextAuth, `NEXTAUTH_URL`). Google, Apple and Facebook need a real domain and HTTPS:

```sh
porch proxy start --https --tld local.example.com
```

See [skills/oauth](../skills/oauth/SKILL.md) for each provider and [examples/oauth-nextauth](../examples/oauth-nextauth) for a working app.

## HTTPS

```sh
porch proxy start --https       # https://web.localhost:7462, HTTP/2, no sudo
porch trust                     # once; trusts the CA for your user
porch proxy start --https --port 443   # https://web.localhost, needs sudo to bind 443
```

`porch setup` asks about HTTPS and saves the answer. Apps that Porch launches trust the CA through `NODE_EXTRA_CA_CERTS`, so server-side `fetch` to another HTTPS Porch URL works.

## Phones and tablets

```sh
porch web --lan pnpm dev        # also publishes web.local over mDNS
porch lan                       # LAN mode status and addresses
```

Open `http://web.local:7462` on the phone. The proxy then accepts connections from your network for every app, so allow TCP 7462 through the firewall. LAN mode lasts until the proxy restarts (`porch proxy stop`); `porch proxy start --lan` keeps it on across restarts, and `porch proxy start --no-lan` turns it off. Pin the address with `--ip 192.168.1.42` if the machine has several. Expo in LAN mode keeps its own host handling.

## Sharing outside your machine

| Flag | Gives you | Needs |
| --- | --- | --- |
| `--tailscale` | An HTTPS URL on your tailnet (Tailscale Serve) | Tailscale with HTTPS enabled |
| `--funnel` | A public HTTPS URL (Tailscale Funnel) | Funnel enabled for your tailnet |
| `--ngrok` | A public ngrok URL | `ngrok` installed and signed in |
| `--cloudflare` | A public Cloudflare quick tunnel | `cloudflared` installed |

```sh
porch web --ngrok pnpm dev
```

Porch waits for the public URL before starting the app, passes it as `PORCH_NGROK_URL` (or `PORCH_TAILSCALE_URL`, `PORCH_CLOUDFLARE_URL`), shows it in `porch list`, and removes the share when the app exits. `porch share ngrok 4480` shares a port you already have until you press Ctrl-C.

To open apps on your own other machines without anything public, [pair them](discovery.md) instead: `porch pair`.

## CI and scripts

- `porch list --json` and `porch get NAME` are stable outputs to script against.
- `PORCH=0 pnpm dev` runs the command with Porch switched off: no port assignment, no route.
- Unattended runs never prompt. An HTTPS start without a terminal skips the trust prompt and prints a `porch trust` hint.
- Set `PORCH_STATE_DIR` to a temporary directory to isolate a run from your own routes.
