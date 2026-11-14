# Troubleshooting

Start with:

```sh
porch doctor          # checks the proxy, routes, .localhost DNS, the CA and external tools
porch doctor --fix    # applies the safe fixes; add --yes to also stop stuck processes
porch list            # every route and whether it answers
```

`porch doctor` prints each problem as `problem → fix applied` or `problem → command to run`. The entries below are grouped by what you see. Quoted messages are Porch's own wording, so you can search this page for them.

- [Ports](#ports)
- [Pages that don't load](#pages-that-dont-load)
- [Name resolution](#name-resolution)
- [HTTPS and certificates](#https-and-certificates)
- [Frameworks](#frameworks)
- [Other machines and the tray](#other-machines-and-the-tray)
- [Platforms](#platforms)

## Ports

### "proxy port 7462 unavailable: it is used by …"

**Cause.** Another process holds the proxy port, and you chose that port explicitly (`--port` or `PORCH_PORT`). A default or saved port moves to the next free one on its own; an explicit one never does.

**Fix.** Stop the named process, or pick another port: `porch proxy start --port 7470`. `porch doctor --fix` moves a stopped proxy off a busy port and saves the new one. If the owner shows as unknown, it may belong to another user; `sudo lsof -iTCP:7462 -sTCP:LISTEN` shows it.

### "Waiting for port 7462 to be released by recently closed connections..."

**Cause.** The proxy just stopped and the OS is holding the port for closed connections (usually about 30 seconds).

**Fix.** Nothing. Porch waits up to 60 seconds instead of moving to another port.

### "app port 4500 is already reserved by another route"

**Cause.** You asked for a fixed port (`--app-port`, `appPort` or `PORCH_APP_PORT`) that another route uses.

**Fix.** `porch list` shows which route. Pick another port or drop the fixed port and let Porch choose. Ports picked by Porch come from 4000–4999; set `PORCH_APP_PORTS=5000-5999` to use another range.

### "route web already exists; use --force"

**Cause.** Another app or alias has the name.

**Fix.** Check `porch list`. If the old app is yours and you want to replace it, add `--force`: Porch stops that app and takes the name. Otherwise choose another name.

### `EADDRINUSE` from the app itself

**Cause.** The app ignores `PORT` and binds a hard-coded port that something else holds.

**Fix.** Make the app read `PORT` (see [the next entry](#porch-warning-nothing-is-listening-on-port-4480-after-15s)). Porch never needs two apps on the same port.

### "porch: warning: nothing is listening on port 4480 after 15s"

**Cause.** The app is still starting, crashed, or listens somewhere other than the `PORT` Porch gave it. If Porch finds the app listening elsewhere, the warning says so:

```
  The app is listening on 3000 instead (...). Make it use the PORT env var (Porch set PORT=4480),
  or pin Porch to that port: `porch run --app-port 3000 pnpm dev`
```

If it says "The app listens on [::1]:4480 only", the app bound IPv6 loopback only.

**Fix.** Make the app read `PORT` (most frameworks do; see [recipes](recipes.md) for ones that need a flag), or run with `--app-port` as suggested. For `[::1]`-only apps, bind `127.0.0.1` or read `HOST`, which Porch sets to `127.0.0.1`. A slow first compile isn't a problem: Porch leaves the app running and the route starts working once it listens. Set `PORCH_LISTEN_TIMEOUT=60` to wait longer before warning, or `0` to turn the check off.

## Pages that don't load

### "Waiting for the app to start" page (502, `upstream_unavailable`)

**Cause.** The route points at this computer, but nothing accepts connections on its port. The page reloads every two seconds, so if the app is compiling it will appear on its own.

**Fix.** If it never loads, check the app's output in the terminal that ran `porch`. `porch list` shows the route as `not listening`. The usual causes are a crash, or the app ignoring `PORT` (see above).

### 502 or 504 with `upstream_error`, `upstream_timeout` or `upstream_certificate`

**Cause.** The backend was reached, or should have been, but the request failed for another reason, and the page says which: the app closed the connection before answering (often a crash mid-request), a remote alias's host couldn't be resolved or refused the connection, the backend didn't accept the connection in time (504, `upstream_timeout`), or its TLS certificate isn't trusted (`upstream_certificate`). These pages don't reload by themselves.

**Fix.** For a local app, check its output. For a remote alias, check its URL with `porch list` and that the site is reachable from this computer. For a certificate error, trust the CA that issued it, or for a `--upstream` alias re-register it with `--insecure`.

### 404 "No app is registered for this hostname"

**Cause.** No route matches the name. Common reasons: the app isn't running; you're in a linked Git worktree, so the name has a branch prefix (`login.web.localhost`); or a typo.

**Fix.** `porch list` shows the exact URLs. `porch get web` prints the URL this checkout uses. The 404 page lists running apps. To route subdomains like `tenant1.web.localhost` to `web`, start the proxy with `--wildcard`.

### 421 "DNS sent example.com to this computer. This is not the real site."

**Cause.** A name outside Porch's TLDs resolved to 127.0.0.1, often from a hosts-file entry or a DNS blocklist, and reached the proxy.

**Fix.** That isn't a Porch app. Remove the stray hosts entry or blocklist rule, or add the domain with `--tld` if you meant to serve it.

### 508 "Request passed through Porch too many times. Add changeOrigin: true to your dev server proxy config."

**Cause.** A request looped through the proxy five times. Usually a dev-server proxy (Vite `server.proxy`, `http-proxy-middleware`, webpack-dev-server) forwards to another Porch URL but keeps the original `Host` header, so Porch routes it back to the same app.

**Fix.** Set `changeOrigin: true` on that proxy entry, so the `Host` header matches the target:

```js
// vite.config.js
server: { proxy: { "/api": { target: "http://api.localhost:7462", changeOrigin: true } } }
```

### "the running proxy uses different settings (`http://<name>.localhost:7462`); run `porch proxy stop` and retry"

**Cause.** You asked for HTTPS, a TLD or a port that differs from the running proxy's.

**Fix.** `porch proxy stop`, then start it with the new options. Running apps keep their routes, and the new settings are saved for next time.

### Apps stop working after an upgrade

**Cause.** An older proxy is still running.

**Fix.** Launching an app or running `porch proxy start` restarts an outdated proxy with the same settings, and `porch doctor --fix` does it on request. Routes are kept.

## Name resolution

### `curl: (6) Could not resolve host: web.localhost`, or a tool can't find `*.localhost`

**Cause.** Browsers resolve every `*.localhost` name to loopback themselves. Other tools ask the system resolver. Recent macOS versions answer `*.localhost` too (both 127.0.0.1 and ::1), older ones don't, and so do Linux systems with systemd's `nss-myhostname`, but some minimal Linux images, containers and older Windows setups don't.

**Fix.** Any of:

```sh
curl --resolve web.localhost:7462:127.0.0.1 http://web.localhost:7462
curl -H 'Host: web.localhost:7462' http://127.0.0.1:7462
sudo porch hosts sync      # writes Porch's block in the hosts file for current routes
```

`porch hosts sync` covers the routes that exist when you run it; rerun it after adding apps. `porch hosts clean` removes the block. `porch doctor` marks the route with ✗ and "does not resolve on this computer; run `sudo porch hosts sync`" when the system resolver can't find its name.

### Safari can't open `web.localhost`

**Cause.** Safari uses the system resolver. Recent macOS versions answer `*.localhost` themselves; older ones, or a VPN or DNS profile, may not.

**Fix.** `porch doctor` shows whether the name resolves. If it doesn't, check the VPN or DNS profile, or run `sudo porch hosts sync`.

### Custom TLD (`--tld test`, `--tld local.example.com`) doesn't resolve

**Cause.** Only `.localhost` resolves to loopback automatically.

**Fix.** For a domain you own, add a wildcard DNS record (`*.local.example.com A 127.0.0.1`). Otherwise run `sudo porch hosts sync` after apps start. If a public record exists but lookups fail, your router or resolver may block public names that point at 127.0.0.1 (DNS rebinding protection).

### A Docker container can't reach `web.localhost`

**Cause.** Inside a container, `localhost` and `*.localhost` mean the container itself, not your machine. The Porch proxy also only accepts connections from loopback unless LAN mode is on.

**Fix.** Porch doesn't route traffic from containers into host apps. Give the container a URL it can reach (for example through Docker's `host.docker.internal` with the app listening on an address the container can reach), or run the dependency in Compose too. To give Compose services names the other way round, use `porch compose up` ([recipe](recipes.md#docker-compose)).

## HTTPS and certificates

### Browser warns "Your connection is not private" / `NET::ERR_CERT_AUTHORITY_INVALID`

**Cause.** Porch's local CA isn't trusted for your user. An unattended start (no terminal, a service, an agent) skips the trust prompt and prints `Browsers will warn about https://<app>.localhost:7462 until you run porch trust once in a terminal (no sudo).`

**Fix.** `porch trust` in a terminal. macOS asks for your password or Touch ID once. On Linux it installs into `~/.pki/nssdb` (Chrome, Chromium) and every Firefox profile; restart the browser afterwards. `porch doctor` checks the trust store itself (not just Porch's record) and shows "not trusted yet; run `porch trust`" until it's done, or "its trust was removed from the trust store" if something removed it later.

### "per-user trust needs NSS certutil: install libnss3-tools (Debian/Ubuntu) or nss-tools (Fedora/Arch) …"

**Cause.** Linux per-user trust uses NSS `certutil`, which isn't installed.

**Fix.** Install the package the message names, then `porch trust`. Or trust machine-wide with `porch trust --system` (sudo).

### Node, curl or Python reject the certificate

**Cause.** Apps Porch launches trust the CA through `NODE_EXTRA_CA_CERTS`, but other processes don't, and many command-line tools ignore per-user trust stores.

**Fix.** Point the tool at the CA, or trust it machine-wide:

```sh
curl --cacert ~/.porch/ca.pem https://web.localhost:7462
NODE_EXTRA_CA_CERTS=~/.porch/ca.pem node script.js
SSL_CERT_FILE=~/.porch/ca.pem python script.py   # replaces the default bundle for that run
porch trust --system                              # macOS System keychain or the distro CA store; sudo
```

### "local CA is not trusted; run `porch trust` interactively once, or use --skip-trust / PORCH_SKIP_TRUST=1 …"

**Cause.** An unattended HTTPS start on port 443 can't show a trust prompt.

**Fix.** Run `porch trust` once in a terminal, or pass `--skip-trust` if your clients load the CA themselves.

### HTTPS on port 443 fails without a terminal

**Cause.** On this OS, ports below 1024 are reserved for root (Linux by default; macOS and Windows let Porch use 443 without sudo). An unattended start uses cached sudo credentials or fails.

**Fix.** Let Porch pick the port: `porch proxy start --https` uses 443 where that needs no sudo and 7462 otherwise (`https://web.localhost:7462`). On Linux, allow low ports once with `echo 'net.ipv4.ip_unprivileged_port_start=80' | sudo tee /etc/sysctl.d/50-porch.conf && sudo sysctl --system`, or run `porch proxy start --https --port 443` in a terminal.

## Frameworks

### Vite: "Blocked request. This host ("web.localhost") is not allowed."

**Cause.** Vite only answers hosts on its allow list.

**Fix.** Porch sets `__VITE_ADDITIONAL_SERVER_ALLOWED_HOSTS` to the proxy's TLDs (for example `.localhost`), which recent Vite versions read. If you see the error anyway (an older Vite, LAN mode's `.local` names, or a server Porch didn't launch), add the host:

```js
// vite.config.js
export default { server: { allowedHosts: [".localhost", ".local"] } };
```

### Hot reload doesn't connect (WebSocket errors in the console)

**Cause.** The proxy forwards WebSocket upgrades on HTTP/1.1 and HTTP/2, so HMR normally works unchanged. It breaks when the dev server tells the browser to connect straight to its own port or host, for example a hard-coded `hmr.port`, `hmr.host` or `hmr.clientPort`.

**Fix.** Remove those settings so the client connects to the page's origin. If your setup needs them, set `server.hmr.clientPort` to the proxy port (7462, or 443 with `--port 443`). For webpack-dev-server, set `client.webSocketURL` to `auto://0.0.0.0:0/ws`.

### Next.js: "Blocked cross-origin request from web.localhost"

**Cause.** Next.js 15+ warns about or blocks dev-only requests from origins it doesn't know.

**Fix.** Add the Porch hosts to `next.config.js`: `allowedDevOrigins: ["*.localhost"]`.

### Django: `DisallowedHost`; Rails: "Blocked hosts"

**Cause.** The framework's host allow list doesn't include the Porch name.

**Fix.** Django: `ALLOWED_HOSTS = [".localhost"]` in development. Rails allows `.localhost` in development by default; for a custom TLD add `config.hosts << ".local.example.com"`.

### Expo or React Native on a phone

Use `--lan`. Porch keeps Expo's own LAN mode and doesn't force `HOST` for it.

## Other machines and the tray

### "No machine is showing a code. Show one there first, or enter its address."

**Cause.** The joining machine didn't find the other one over LAN mDNS or `tailscale status`: multicast is blocked, the machines are on different subnets, or the Tailscale CLI isn't installed.

**Fix.** Make sure `porch pair` is still showing the code on the other machine, then name it: `porch pair 482913 --host mac-mini` or `--host 100.101.102.103`.

### "pairing code expired; run porch pair again"

Codes last 5 minutes, work once, and are cancelled after 5 wrong guesses. Show a new one.

### "… did not prove it knows the pairing code; it may be impersonating the machine showing it. Nothing was shared"

**Cause.** A wrong code, or the address you reached isn't the machine showing the code.

**Fix.** Check the code and the `--host` value. Nothing was shared, so retrying is safe.

### Pairing hangs or times out

**Cause.** Usually a firewall. Pairing and remote apps use the discovery agent on TCP port 7463; LAN discovery uses mDNS on UDP 5353.

**Fix.** Allow incoming TCP 7463 on the machine running the apps (on macOS, allow `porch` when the firewall asks). Or pair over Tailscale, which doesn't need multicast. Both machines need Porch 0.4.0 or later.

### Remote app stopped working after a change on the other machine

**Cause.** The other machine's certificate changed (for example after its Porch state was removed), and Porch pins the certificate it saw when you paired.

**Fix.** Pair again. `porch list --all` shows each paired machine and any connection error.

### An app on another machine loads much more slowly than locally

**Cause.** Every request crosses the network. Over plain HTTP a browser fetches at most six files at a time from one host, so a dev server that serves hundreds of separate modules (Vite, for example) waits for that many round trips to the other machine.

**Fix.** Turn on HTTPS on the machine you browse from: `porch setup --https` (no sudo). Over HTTPS the browser uses HTTP/2 and fetches the modules in parallel, so the page waits for a few round trips instead of dozens. Paired-machine URLs then start with `https://`.

### Linux: the tray icon doesn't appear

**Cause.** The tray needs GTK 3 and an AppIndicator host. GNOME doesn't ship one by default.

**Fix.** Install the GNOME "AppIndicator and KStatusNotifierItem Support" extension (or your desktop's equivalent). If the tray exits with "AppIndicator is unavailable. Install libayatana-appindicator3-1, or use `porch list --all`", install that package. Without a host Porch says "This desktop has no system tray host. On GNOME enable an AppIndicator extension, or use `porch list --all`". The CLI (`porch list --all`, `porch open`, `porch copy`) works without the tray.

## Platforms

### Firewall prompts or LAN devices can't connect

`--lan` makes the proxy accept connections from your network, so phones can open `web.local:7462`. Allow incoming TCP on the proxy port (7462 by default) and mDNS (UDP 5353). Without `--lan`, the proxy only listens on loopback and needs no firewall rule. To end LAN mode, run `porch proxy stop && porch proxy start --no-lan`.

### WSL

Run Porch inside WSL and open URLs in your Windows browser. `porch trust` inside WSL also installs the CA into the Windows user store (it uses `certutil.exe` when `WSL_DISTRO_NAME` is set), so Windows browsers trust it. The browser reaches the proxy through WSL's localhost forwarding, which is on by default; if `http://web.localhost:7462` doesn't load from Windows, check that `localhostForwarding` isn't disabled in `.wslconfig`, or use mirrored networking.

### Windows

Windows support is a preview. Proxy, apps, trust (current user's Root store), pairing and the tray work; `porch service install` adds a Task Scheduler task that starts the proxy when you log in, as you. Report anything that behaves differently from macOS or Linux.

## Still stuck

- `porch list --json` and `porch doctor` output are the most useful things to include in an issue.
- `porch prune --force` removes routes whose apps are gone and stops what they left behind.
- `porch clean` removes everything Porch installed (trust, services, hosts entries, pairing, logs, state). Use it as a last resort; you'll need `porch trust` again for HTTPS, and `porch pair` again to share with other machines.
