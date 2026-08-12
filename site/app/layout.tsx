import type { Metadata } from "next";
import { JetBrains_Mono } from "next/font/google";
import Script from "next/script";
import type { ReactNode } from "react";
import { RootProvider } from "fumadocs-ui/provider/next";

import { SITE_ORIGIN } from "@/lib/site-url";
import { StructuredData } from "./structured-data";

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
  metadataBase: new URL(SITE_ORIGIN),
  title,
  description,
  alternates: { canonical: "/" },
  openGraph: {
    type: "website",
    url: SITE_ORIGIN,
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
      <head>
        {/* Cookieless, aggregate-only. data-domain is the canonical host;
            gridwork.dev 301s at the edge, so the origin only ever sees this one. */}
        <Script
          defer
          data-domain="gridwork.sh"
          src="https://plausible.io/js/script.js"
          strategy="afterInteractive"
        />
        <StructuredData />
      </head>
      <body className="flex min-h-screen flex-col font-mono">
        <RootProvider theme={{ enabled: false }} search={{ enabled: false }}>
          {children}
        </RootProvider>
      </body>
    </html>
  );
}
