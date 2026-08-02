import type { Metadata } from "next";
import { JetBrains_Mono } from "next/font/google";
import type { ReactNode } from "react";
import { RootProvider } from "fumadocs-ui/provider/next";

import "./globals.css";

const mono = JetBrains_Mono({
  display: "swap",
  subsets: ["latin"],
  variable: "--font-signal",
});

const title = "GridWork — an agent operating system for the terminal";
const description =
  "GridWork is an open, pre-1.0 agent operating system for the terminal. Its Rust contract and event-sourced kernel ship today; engines and TUI are in progress.";

export const metadata: Metadata = {
  metadataBase: new URL("https://gridwork.dev"),
  title,
  description,
  openGraph: {
    type: "website",
    url: "https://gridwork.dev",
    siteName: "GridWork",
    title,
    description,
    images: [{ url: "/og.png", width: 1200, height: 630, alt: title }],
  },
  twitter: {
    card: "summary_large_image",
    title,
    description,
    images: ["/og.png"],
  },
};

export default function RootLayout({ children }: { children: ReactNode }) {
  return (
    <html
      lang="en"
      className={`dark ${mono.variable}`}
      style={{ colorScheme: "dark" }}
      suppressHydrationWarning
    >
      <body className="flex min-h-screen flex-col">
        <RootProvider theme={{ enabled: false }} search={{ enabled: false }}>
          {children}
        </RootProvider>
      </body>
    </html>
  );
}
