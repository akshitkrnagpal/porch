# Monorepo example

A pnpm workspace with two apps. One `porch` at the root starts both, each on its own port and URL:

```console
$ porch
http://shop.localhost:7462 -> 4747
http://api.shop.localhost:7462 -> 4009
```

No dependencies to install: both apps are plain Node `http` servers.

## Layout

```
porch.json             names for each package
pnpm-workspace.yaml    apps/*
apps/web/              @shop/web, served at shop.localhost
apps/api/              @shop/api, served at api.shop.localhost
```

## How the names are chosen

Without `porch.json`, Porch names workspace packages `<package>.<project>`. Both packages share the `@shop` scope, so they would be `web.shop.localhost` and `api.shop.localhost`.

`porch.json` overrides that per package, keyed by path relative to the root:

```json
{
  "apps": {
    "apps/web": { "name": "shop" },
    "apps/api": { "name": "api.shop" }
  }
}
```

An `apps` entry can also set `script`, `appPort`, `proxy` and `worktree`. A package's own `porch.json` or `porch` key in its `package.json` takes precedence over its entry here, and CLI options take precedence over both. The `$schema` line gives editors completion and validation from [`schema/porch.schema.json`](../../schema/porch.schema.json).

## Run it

```sh
cd examples/monorepo
porch                 # or: pnpm dev
```

Open http://shop.localhost:7462. The page fetches http://api.shop.localhost:7462 and shows the response.

Other ways to run it:

```sh
cd apps/api && porch      # just the API, still at api.shop.localhost
porch list                # both routes and whether they answer
porch list --json         # the same, for scripts and agents
```

## Cross-app URLs

`apps/web/server.js` finds the API with `porch get api.shop`. `porch get` prints a URL whether or not the app is running, so start order doesn't matter, and it applies the same worktree prefix the API would get. In a `package.json` script you can do the same in one line:

```json
{ "scripts": { "dev": "API_URL=$(porch get api.shop) node server.js" } }
```

## In a Git worktree

In a linked worktree on branch `feat/cart`, both names get the branch's last segment as a prefix: `cart.shop.localhost` and `cart.api.shop.localhost`. `porch get` inside that worktree returns the prefixed URL, so the web app still finds its own API rather than the one in your main checkout. Add a `worktree` block to `porch.json` to change the layout:

```json
{ "worktree": { "prefix": "branch", "hostnameTemplate": "{worktree}-{name}" } }
```

## With Turborepo

Add a `turbo.json` with a `dev` task and run `porch` at the root as before. Porch reserves a route for each package, runs `turbo run dev` once, and gives each package its own `PORT` and `PORCH_URL`, so Turbo keeps its task graph and TUI. Pass Turbo options after `--`, for example `porch -- --filter=@shop/api`. Set `"turbo": false` in the root `porch.json` to start packages directly instead.
