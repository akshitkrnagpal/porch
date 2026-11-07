import GitHub from "next-auth/providers/github";
import Google from "next-auth/providers/google";

// Porch passes the app's public URL as PORCH_URL, for example
// http://oauth.localhost:7462. NextAuth builds callback URLs from NEXTAUTH_URL,
// so point it there. An explicit NEXTAUTH_URL still wins.
if (!process.env.NEXTAUTH_URL && process.env.PORCH_URL) {
  process.env.NEXTAUTH_URL = process.env.PORCH_URL;
}

const providers = [];
if (process.env.GITHUB_ID) {
  providers.push(
    GitHub({ clientId: process.env.GITHUB_ID, clientSecret: process.env.GITHUB_SECRET }),
  );
}
if (process.env.GOOGLE_CLIENT_ID) {
  providers.push(
    Google({
      clientId: process.env.GOOGLE_CLIENT_ID,
      clientSecret: process.env.GOOGLE_CLIENT_SECRET,
    }),
  );
}

export const authOptions = { providers, secret: process.env.NEXTAUTH_SECRET };
