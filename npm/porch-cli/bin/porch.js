#!/usr/bin/env node
// Runs the prebuilt Porch binary for this platform. npm installs exactly one
// porch-cli-<os>-<cpu> package (the others don't match its os/cpu fields), so
// there is no install script and no download at install time.
"use strict";
const { spawnSync } = require("node:child_process");
const path = require("node:path");

const platforms = {
  "darwin arm64": "porch-cli-darwin-arm64",
  "darwin x64": "porch-cli-darwin-x64",
  "linux x64": "porch-cli-linux-x64",
  "linux arm64": "porch-cli-linux-arm64",
  "win32 x64": "porch-cli-win32-x64",
};
const pkg = platforms[`${process.platform} ${process.arch}`];
if (!pkg) {
  console.error(`porch: no prebuilt binary for ${process.platform} ${process.arch}. Build it with: cargo install --git https://github.com/akshitkrnagpal/porch porch`);
  process.exit(1);
}
let binary;
try {
  const dir = path.dirname(require.resolve(`${pkg}/package.json`));
  binary = path.join(dir, process.platform === "win32" ? "porch.exe" : "porch");
} catch {
  console.error(`porch: the ${pkg} package is missing. Reinstall without --no-optional / --omit=optional.`);
  process.exit(1);
}
const result = spawnSync(binary, process.argv.slice(2), { stdio: "inherit" });
if (result.error) {
  console.error(`porch: could not run ${binary}: ${result.error.message}`);
  process.exit(1);
}
if (result.signal) {
  process.kill(process.pid, result.signal);
}
process.exit(result.status ?? 1);
