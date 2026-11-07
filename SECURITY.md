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

- **No root by default.** The proxy runs as you. It uses a privileged port only if you ask for one the OS reserves, and the system trust store only with `porch trust --system`.
- **Loopback only.** Outside LAN mode the proxy answers only clients on this computer. Where it must bind the wildcard address to use port 80 without sudo (macOS), it drops every non-loopback connection before reading a byte of it. LAN clients can't set `X-Forwarded-*` headers or hop counts, and never see the targets behind your routes.
- **A narrow local CA.** New CAs carry X.509 name constraints (`localhost`, `local`, `test`, `internal`, `home.arpa` and your configured domains), so even a stolen CA key can't impersonate a real website. Porch signs only names under the domains it serves, never a route's raw name, rate-limits issuance, and trusts the CA for your user only.
- **Verified upstreams.** Remote aliases and `--upstream https` check the backend's certificate. Only an alias you register with `--insecure` skips the check.
- **Pairing.** Codes are authenticated with SPAKE2 before any credential moves, the other machine's certificate is pinned, and paired machines can reach only apps on this computer that aren't `--private`, never other hosts it can see.
- **Bounded resources.** The proxy caps open connections and times out clients that don't send a request head.
- **Supply chain.** Release archives carry signed build-provenance attestations that `porch self-update` and the install scripts verify. `cargo deny` checks every change for known vulnerabilities, unmaintained crates and licenses.

**Shared computers.** Any local user can connect to loopback ports, so on a machine shared with people you don't trust, other users could open your apps, and apps on your paired machines, through your proxy. The same is true of any dev server on loopback. Use a computer of your own, or don't pair it.
