# Security policy

Porch runs a local proxy and, once you pair machines, an authenticated discovery service on your private network. Please report security problems in either.

## Reporting a vulnerability

Please report vulnerabilities privately through [GitHub security advisories](https://github.com/akshitkrnagpal/porch/security/advisories/new), not in a public issue. Include the Porch version (`porch --version`), your platform, and steps to reproduce.

You can expect an acknowledgement within a few days. Fixes ship in a new release, and the advisory is published once the release is out.

## Supported versions

Only the latest release receives security fixes. Update with `porch self-update` or `brew upgrade akshitkrnagpal/tap/porch`.

## Scope

In scope: the proxy, the discovery agent and pairing, browser relays, and the hosts-file and trust-store integrations. The [discovery guide](docs/discovery.md) describes the security model these are meant to uphold.
