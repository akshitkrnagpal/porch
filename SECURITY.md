# Security policy

Porch runs a local proxy and, once you pair machines, an authenticated discovery service on your private network. Please report security problems in either.

## Reporting a vulnerability

Please report vulnerabilities privately through [GitHub security advisories](https://github.com/akshitkrnagpal/porch/security/advisories/new), not in a public issue. Include the Porch version (`porch --version`), your platform, and steps to reproduce.

You can expect an acknowledgement within a few days. Fixes ship in a new release, and the advisory is published once the release is out.

## Supported versions

Only the latest release receives security fixes. Update with `porch self-update` or `brew upgrade akshitkrnagpal/tap/porch`.

## Scope

In scope: the proxy, the discovery agent and pairing, browser relays, and the hosts-file and trust-store integrations. The [discovery guide](docs/discovery.md) describes the security model these are meant to uphold.

## Security model at a glance

- **No root by default.** The proxy runs as you, and so does the startup service (`porch service install`, or "Start at login" in `porch setup`): a login item in your own account. It uses a privileged port only if you ask for one the OS reserves, and the system trust store only with `porch trust --system`. On Linux a port below 1024 can need a system unit, which still runs as you with only the right to bind that port, from a root-owned copy of porch. Porch 0.5.2 and earlier installed a startup service that ran the proxy as root from a binary you could overwrite; `porch service status` and `porch doctor` flag it, and `porch service install` replaces it.
- **Loopback only.** Outside LAN mode the proxy answers only clients on this computer. A port below 1024 without sudo is bound on loopback where the OS allows it (Linux, Windows). Where it must bind the wildcard address instead (macOS), it drops every non-loopback connection before reading a byte of it. Other devices can still complete a TCP handshake on that port; `PORCH_CLEAN_URLS=0` keeps the proxy on a loopback-only high port instead. LAN clients can't set forwarding headers (`X-Forwarded-*`, `Forwarded`, `X-Real-IP`, `X-Original-*` and similar) or hop counts, and never see the targets behind your routes. LAN mode turned on by an app's `--lan` ends when the proxy restarts; only `porch proxy start --lan` saves it. `porch list` and `porch doctor` warn while it's on.
- **A narrow local CA.** New CAs carry X.509 name constraints (`localhost`, `local`, `test`, `internal`, `home.arpa` and your configured domains, never a public suffix such as `com` or `co.uk`, checked against the Public Suffix List, and no IP addresses), so a stolen CA key can't impersonate a website outside those names. Name constraints don't cover email (S/MIME) or code-signing certificates, so since Porch 0.5.3 a new CA can also sign TLS server certificates only (an extended key usage), and `porch trust` limits macOS trust to TLS. Linux per-user trust (NSS) has always been TLS-only. A CA made by an older Porch has no such limit, and macOS trust installed by an older Porch applies to every purpose: someone who stole that CA's key could sign an email or code-signing certificate your account would accept. `porch doctor` points these out; [replace the CA](docs/reference.md#https-and-trust), and run `porch trust` again on macOS. Porch signs only names under the domains it serves, never a route's raw name, and rate-limits issuance. Porch keeps no private key for a retired CA, and trusts the CA for your user only unless you ask for `porch trust --system`.
- **Verified upstreams.** Remote aliases and `--upstream https` check the backend's certificate. Only an alias you register with `--insecure` skips the check.
- **Pairing.** Codes are authenticated with SPAKE2 before any credential moves, the other machine's certificate is pinned, a machine can't take over a paired machine's identity by claiming its ID, and paired machines can reach only apps on this computer that aren't `--private`, never other hosts it can see. Relayed requests reach your apps from 127.0.0.1, so an app that trusts every loopback client (a notebook server without a token, a debug console) is open to every paired machine: start it with `--private`.
- **Bounded resources.** The proxy raises its open-file limit and sizes one connection cap, shared by every listener and WebSocket, to fit it. It times out clients that don't start a request or send a request head. In LAN mode each other device gets a share of the connections, its idle connections are closed after a minute, a request body that stalls for a minute is cut off, and TCP keepalive closes connections from devices that left the network. The discovery agent caps connections per address, closes connections that never authenticate, and keeps room for paired machines.
- **Supply chain.** Release archives carry signed build-provenance attestations that `porch self-update` and the install scripts verify. `cargo deny` checks every change for known vulnerabilities, unmaintained crates and licenses.

**Shared computers.** Any local user can connect to loopback ports, so on a machine shared with people you don't trust, other users could open your apps, and apps on your paired machines, through your proxy. The same is true of any dev server on loopback. Use a computer of your own, or don't pair it. Porch does avoid running other users' programs: apps get `node_modules/.bin` on their PATH only from inside the project, never from shared directories such as `/tmp` or `C:\`, and on macOS and Linux not from directories another user owns.
