# OAuth on a Porch URL (Next.js + NextAuth)

A Next.js app with NextAuth sign-in at a stable URL, `http://oauth.localhost:7462`. The callback URL you register with the provider stays the same across restarts, because Porch's URL doesn't depend on which port the app got.

It uses one idea: Porch passes the app its public URL as `PORCH_URL`, and `auth.js` copies it into `NEXTAUTH_URL` when that isn't set. NextAuth then builds every callback URL from the Porch URL instead of `localhost:<port>`.

## GitHub (works with the default `.localhost` URL)

1. Create an OAuth app at [GitHub > Settings > Developer settings > OAuth Apps](https://github.com/settings/developers):
   - Homepage URL: `http://oauth.localhost:7462`
   - Authorization callback URL: `http://oauth.localhost:7462/api/auth/callback/github`
2. Configure and run:

   ```sh
   cd examples/oauth-nextauth
   cp .env.example .env.local     # fill in GITHUB_ID, GITHUB_SECRET, NEXTAUTH_SECRET
   npm install
   porch                          # runs "dev" at http://oauth.localhost:7462
   ```

3. Open http://oauth.localhost:7462 and sign in. The page lists the exact callback URL for each configured provider, so you can copy it into the provider's console.

The name `oauth` comes from `"porch": { "name": "oauth" }` in `package.json`. URLs here assume the default proxy on port 7462. If yours uses another port or HTTPS, or you're in a linked Git worktree (which adds a branch prefix such as `login.oauth.localhost`), the URL changes to match. `porch get oauth` prints the URL this checkout will use; register that one.

## Google (needs a real domain and HTTPS)

Google rejects redirect URIs on `.localhost` subdomains and requires HTTPS for anything other than plain `localhost`. Serve the app under a domain you control instead:

```sh
porch proxy stop                                       # only if a proxy with other settings is running
porch proxy start --https --tld local.example.com      # replace with a domain you own
porch trust                                            # once; trusts Porch's CA for your user
porch                                                  # https://oauth.local.example.com:7462
```

Make the name resolve to your machine, with either:

- a DNS record for a domain you own: `*.local.example.com  A  127.0.0.1` (works for every developer, no local setup), or
- `sudo porch hosts sync` after the app has started. It writes entries for the routes that exist at that moment, so run it again when you add names.

In [Google Cloud Console > Credentials](https://console.cloud.google.com/apis/credentials), create an OAuth client ID of type Web application with:

- Authorized JavaScript origin: `https://oauth.local.example.com:7462`
- Authorized redirect URI: `https://oauth.local.example.com:7462/api/auth/callback/google`

To drop `:7462` from the URL, start the proxy with `--https --port 443` instead (binding port 443 needs sudo).

Don't use a bare public TLD such as `--tld dev`: `oauth.dev` is someone else's real domain, and your hosts entry would hide it.

## Without Porch

`npm run dev` still works on `http://localhost:3000`. Set `NEXTAUTH_URL=http://localhost:3000` in `.env.local` and register that callback URL too.

## Notes

- Auth.js v5 reads `AUTH_URL` instead of `NEXTAUTH_URL`. Copy `PORCH_URL` into it the same way.
- Another option is to trust forwarded headers (`AUTH_TRUST_HOST=true`). Porch sends `X-Forwarded-Host` and `X-Forwarded-Proto`, so NextAuth derives the right origin from the request. Setting the URL explicitly, as this example does, is easier to reason about.
- [`skills/oauth`](../../skills/oauth/SKILL.md) covers other providers and frameworks.
