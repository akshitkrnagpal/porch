# Getting started

Porch runs your dev command, gives it a free port, and puts it behind a local proxy at a name you choose:

```console
$ porch run pnpm dev
http://web.localhost -> 4480
```

Your app gets the port as `PORT`, and `web.localhost` keeps pointing at it however many times you restart. Pair two machines and each can open the other's apps the same way, at `http://storybook.mac-mini.localhost`.

## 1. Install

```sh
curl -fsSL https://raw.githubusercontent.com/akshitkrnagpal/porch/main/scripts/install.sh | sh
```

The script needs no sudo and no Python. It checks the download against the release's checksum file and, when the GitHub CLI (2.49 or newer) is installed and logged in, the signed build attestation that shows Porch's release workflow built it; otherwise it says that check was skipped. Other options: `brew install akshitkrnagpal/tap/porch`, the Windows script (`irm https://raw.githubusercontent.com/akshitkrnagpal/porch/main/scripts/install.ps1 | iex`), or a [release archive](https://github.com/akshitkrnagpal/porch/releases/latest).

## 2. Run an app

In a project folder:

```sh
porch                      # runs package.json's "dev" script; at a workspace root, every package
porch run pnpm dev         # any command; the name comes from package.json, Git or the folder
porch api node server.js   # pick the name yourself: http://api.localhost
porch run -d pnpm dev      # in the background; then `porch logs NAME` and `porch stop NAME`
```

Porch passes the port as `PORT` and adds the right flags for Vite, Astro, Angular, React Router, Expo, Wrangler, Django, Flask, FastAPI, uvicorn, Hugo, Jekyll and more. If an app ignores `PORT`, Porch says which port it found and how to route to it.

## 3. See what's running

```sh
porch list                 # every app, its URL and whether it answers
```

Or open `http://localhost` in a browser for a live dashboard.

## 4. Optional extras

- **HTTPS:** `porch setup` turns on HTTPS with a local CA trusted for your user only (no sudo).
- **Other machines:** `porch pair` on one machine and `porch pair CODE` on the other. See [Apps across machines](discovery.md).
- **Phones on your network:** `porch run --lan pnpm dev` serves `web.local`.
- **Coding agents:** `claude mcp add porch -- porch mcp`. See [Coding agents and MCP](agents.md).
- **Something wrong?** `porch doctor` checks everything and `porch doctor --fix` repairs what it safely can. See [Troubleshooting](troubleshooting.md).

URLs carry no port where your OS lets Porch serve port 80 without sudo (macOS, Windows, and Linux with a lowered `net.ipv4.ip_unprivileged_port_start`). Elsewhere they look like `http://web.localhost:7462`. `porch get NAME` always prints the right one.

## How names and HTTPS reach each client

| Client | How `web.localhost` resolves | How the HTTPS certificate is trusted |
| --- | --- | --- |
| Chrome, Edge, Firefox, Arc, Brave | The browser itself sends every `*.localhost` name to loopback | macOS keychain or Windows user store; on Linux, the NSS databases `porch trust` writes |
| Safari | The system resolver: recent macOS versions answer every `*.localhost` name with 127.0.0.1 and ::1; on older ones run `sudo porch hosts sync` | macOS login keychain |
| `curl`, Python, Go, Java | The system resolver: works on recent macOS and on Linux with systemd (`nss-myhostname`); elsewhere use `curl --resolve` or `sudo porch hosts sync` | Pass `--cacert ~/.porch/ca.pem`, or run `porch trust --system` |
| Node apps Porch starts | Same as above | Automatic: Porch sets `NODE_EXTRA_CA_CERTS` |
| Phones on your network (`--lan`) | mDNS `NAME.local` | Install the CA on the device, or use plain HTTP |

`porch doctor` checks both for each route: whether its name resolves on this computer, and whether the CA is actually in the trust store. [Troubleshooting](troubleshooting.md) has a fix for each case.

Next: the [command and configuration reference](reference.md), or [recipes](recipes.md) for your framework.
