import { getServerSession } from "next-auth";
import { authOptions } from "../auth";

export default async function Home() {
  const session = await getServerSession(authOptions);
  const base = process.env.NEXTAUTH_URL;

  return (
    <main>
      <h1>OAuth on a Porch URL</h1>
      <p>
        This app: <code>{base ?? "(NEXTAUTH_URL not set)"}</code>
      </p>
      {authOptions.providers.length === 0 ? (
        <p>No providers configured. Copy <code>.env.example</code> to <code>.env.local</code> and add one.</p>
      ) : (
        <>
          <p>Register these callback URLs with your provider:</p>
          <ul>
            {authOptions.providers.map((p) => (
              <li key={p.id}>
                <code>{`${base}/api/auth/callback/${p.id}`}</code>
              </li>
            ))}
          </ul>
        </>
      )}
      {session ? (
        <p>
          Signed in as <strong>{session.user?.email ?? session.user?.name}</strong>.{" "}
          <a href="/api/auth/signout">Sign out</a>
        </p>
      ) : (
        <p>
          <a href="/api/auth/signin">Sign in</a>
        </p>
      )}
    </main>
  );
}
