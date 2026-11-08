export const metadata = { title: "OAuth on a Porch URL" };

export default function RootLayout({ children }) {
  return (
    <html lang="en">
      <body style={{ fontFamily: "system-ui, sans-serif", maxWidth: 640, margin: "10vh auto", padding: "0 16px" }}>
        {children}
      </body>
    </html>
  );
}
