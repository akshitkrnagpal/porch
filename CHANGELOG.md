# Changelog

All notable changes to Porch. Versions follow [semantic versioning](https://semver.org/).

## Unreleased

### Security
- Requests from other devices (LAN mode) can no longer set `X-Forwarded-*` headers, so they can't change the scheme or host your app sees.
- Paired machines can only open apps that run on this machine. An alias to another host is never relayed to a peer.
- The 404 page shows other devices app names only, not the ports and URLs behind them.

### Improved
- The proxy keeps routes in memory between requests and re-reads them only when they change. With many routes it handles noticeably more requests per second.
- Every command and flag has help text, and `porch --help` includes examples.
- A redesigned 404 page in light and dark mode lists running apps, with a filter when there are more than five. When an app isn't listening yet, its tab shows a "waiting for the app" page and reloads by itself as soon as the app answers.
- `porch alias` prints the URL it registered, `porch proxy stop` confirms what it did, and `porch workspace` prints a readable table.
- An out-of-range alias port (`porch alias db 99999`) explains the problem instead of failing with a URL parse error.

## 0.4.2 (2026-11-07)
- The tray menu groups apps under a header for each machine.
- Clicking an app in the menu opens it right away, at the URL its row shows.
- App URLs show at the top level of the tray menu.

## 0.4.1 (2026-11-07)
- The test suite is deterministic. Fixed the product races behind its flakes.
- Release archives are 64% smaller: Porch.app executables are deduplicated, the build profile favors size, and native-tls is gone.

## 0.4.0 (2026-11-07)
- Windows support for the CLI, the tray and releases.
- Fixed CA cleanup and Windows trust.
- Code pairing authenticates with SPAKE2 before any credentials are exchanged.
- Every loopback spelling is blocked in proxied redirects.
- `**` works as a recursive glob in workspace patterns.
- HTTPS without sudo: the proxy keeps its high port and trusts its CA for your user only.
- The proxy toggle in the menu header is a switch.

## 0.3.2 (2026-11-07)
- Launch preparation: README, MIT license and security policy.
- The menu bar app and peer catalogs show each app's real health.

## 0.3.1 (2026-11-07)
- Discovery can start at login, and each paired machine is listed once.

## 0.3.0 (2026-11-06)
- New tray and app icon.
- One-command code pairing, with readable URLs for apps on other machines.
- The first run needs no sudo and uses port 7462. Adds a listen check, stale-route cleanup, `porch setup` and `porch doctor --fix`.
- Tagged releases, a Homebrew formula and `porch self-update`.
