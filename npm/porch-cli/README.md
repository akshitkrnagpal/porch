# porch-cli

[Porch](https://github.com/akshitkrnagpal/porch) gives your dev servers stable, named URLs such as `http://web.localhost`, on this machine and on every machine you pair with.

```sh
npm install --save-dev porch-cli     # or: pnpm add -D porch-cli, yarn add -D porch-cli, bun add -d porch-cli
```

Then use it from a package script:

```json
{ "scripts": { "dev": "porch run next dev" } }
```

Or once, without installing: `npx porch-cli run pnpm dev`.

This package holds a tiny launcher. npm installs the prebuilt binary for your platform from one of `porch-cli-darwin-arm64`, `porch-cli-darwin-x64`, `porch-cli-linux-x64`, `porch-cli-linux-arm64` or `porch-cli-win32-x64`. Nothing is downloaded by an install script, and every package is published from the Porch release workflow with npm provenance.

See the [README](https://github.com/akshitkrnagpal/porch#readme) and [reference](https://github.com/akshitkrnagpal/porch/blob/main/docs/reference.md).
