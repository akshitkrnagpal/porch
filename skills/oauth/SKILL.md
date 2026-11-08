---
name: oauth
description: Make OAuth and social sign-in work on Porch dev URLs (http://app.localhost:7462). Use when registering redirect or callback URIs for Google, GitHub, Microsoft, Apple, Facebook or another provider, when fixing "redirect_uri_mismatch", "invalid redirect URI" or "must end with a public top-level domain", when a callback lands on localhost:3000 instead of the Porch URL, or when setting NEXTAUTH_URL, AUTH_URL or a Passport callbackURL for local development with Porch.
---

# OAuth with Porch

Porch gives each app a URL that doesn't change between restarts, so you register a callback once. Two things decide whether it works:

1. **The provider accepts the URL.** Some reject `.localhost` subdomains or plain HTTP.
2. **The app sends the same URL.** Auth libraries must build callbacks from the Porch URL, not `localhost:<port>`.

## 1. Get the exact URL

```sh
porch get web          # http://web.localhost:7462 (includes worktree prefix, TLD, scheme, port)
```

Inside the app, the same value is in `PORCH_URL`. The callback is that plus the library's path, for example `http://web.localhost:7462/api/auth/callback/github`. Register it exactly: scheme, host, port and path all have to match.

In a linked Git worktree the host gets a branch prefix (`login.web.localhost`). Either register that URL too, or run that worktree with `--prefix ''` while testing sign-in (two checkouts then can't run under the same name at once).

## 2. Pick a URL the provider accepts

| Provider | `http://web.localhost:7462` | What to do |
| --- | --- | --- |
| GitHub | Accepted | Register the Porch URL as is |
| Microsoft Entra ID | Usually accepted for `http://localhost`; subdomains vary by app type | Try the Porch URL; if refused, use a custom domain (below) |
| Google | Rejected: `.localhost` subdomains aren't a public TLD, and non-localhost URIs must be HTTPS | Custom domain + HTTPS |
| Apple | Rejected: no localhost or IPs; the return URL needs a real, registered domain | Custom domain + HTTPS, with the domain listed in your Services ID configuration |
| Facebook / Meta | Exact match only, HTTPS enforced for most apps | Custom domain + HTTPS |

Provider rules change. If the console rejects a URL, read its error message and match it against this table rather than retrying variations.

### Custom domain + HTTPS

Use a subdomain of a domain you own, so nothing collides with a real site:

```sh
porch proxy stop                                   # if a proxy with other settings is running
porch proxy start --https --tld local.example.com
porch trust                                        # once, per user, no sudo
porch                                              # -> https://web.local.example.com:7462
```

The name must resolve to 127.0.0.1. Browsers only do that automatically for `.localhost`, so pick one:

- **DNS (best for teams):** `*.local.example.com  A  127.0.0.1` at your DNS provider. Every developer gets the same redirect URIs with no local setup. Some routers and resolvers block public names that resolve to 127.0.0.1 (DNS rebinding protection); allow the domain there if lookups fail.
- **Hosts file:** `sudo porch hosts sync` after the app has started. It writes entries for the routes that exist now, so rerun it after adding names. `porch hosts clean` removes Porch's block.

To drop `:7462` from the URL, use `porch proxy start --https --tld local.example.com --port 443` (binding 443 needs sudo).

Avoid bare public TLDs (`--tld dev`, `--tld app`): `web.dev` is a real domain owned by someone else, and those TLDs are HSTS-preloaded so they only work over HTTPS anyway.

Porch issues certificates only for configured TLDs and registered names, from its local CA. Apps that Porch launches trust that CA through `NODE_EXTRA_CA_CERTS`, so server-side calls back to your own HTTPS URL work. Tools Porch didn't launch need `porch trust --system` or the CA file at `~/.porch/ca.pem`.

## 3. Make the app send the Porch URL

| Library | Setting |
| --- | --- |
| NextAuth v4 | `NEXTAUTH_URL` |
| Auth.js v5 | `AUTH_URL`, or `AUTH_TRUST_HOST=true` to trust Porch's `X-Forwarded-Host`/`X-Forwarded-Proto` |
| Passport | `callbackURL: process.env.PORCH_URL + "/auth/google/callback"` |
| Better Auth | `baseURL: process.env.PORCH_URL` |
| Anything else | Read `PORCH_URL` |

Copy `PORCH_URL` in code, so you don't hardcode a URL that changes with the worktree or TLD:

```js
// Runs before the auth handler; an explicit NEXTAUTH_URL still wins.
process.env.NEXTAUTH_URL ??= process.env.PORCH_URL;
```

Or in the script: `"dev": "NEXTAUTH_URL=$(porch get web) next dev"`.

## Troubleshooting

| Symptom | Cause | Fix |
| --- | --- | --- |
| `redirect_uri_mismatch` and the URI in the error says `localhost:4xxx` | Library built the callback from the app's real port | Set `NEXTAUTH_URL`/`AUTH_URL`/`callbackURL` from `PORCH_URL` |
| Mismatch, URI looks right | Scheme, port or worktree prefix differs from what's registered | Compare with `porch get NAME` character by character |
| "Must end with a public top-level domain" (Google) | `.localhost` subdomain | Custom domain + HTTPS |
| "Must use HTTPS" | Proxy runs plain HTTP | `porch proxy stop && porch proxy start --https ...` |
| Browser: "can't reach" the custom domain | Name doesn't resolve to 127.0.0.1 | DNS record or `sudo porch hosts sync` |
| Certificate warning on the custom domain | CA not trusted | `porch trust` |
| Server-side token exchange fails with a certificate error | A process Porch didn't launch calls the HTTPS URL | `NODE_EXTRA_CA_CERTS=~/.porch/ca.pem` or `porch trust --system` |
| Session cookie lost after callback | App sets a cookie `Domain` for another host, or mixes http/https | Leave `Domain` unset; use one scheme |

## Example

`examples/oauth-nextauth` in the Porch repo: Next.js + NextAuth with GitHub on `http://oauth.localhost:7462` and Google on a custom HTTPS domain.
