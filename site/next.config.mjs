// @ts-check
import { createMDX } from "fumadocs-mdx/next";

// A static export, served by Workers static assets (site/wrangler.jsonc). There is no
// server at request time, so the response headers live in public/_headers, which the
// asset handler applies to every response, and /health is rendered at build time.
/** @type {import('next').NextConfig} */
const config = {
  output: "export",
  // The live site answers /docs and 308s /docs/ to it; the export keeps that shape.
  trailingSlash: false,
  // No image optimizer runs at request time. The site uses no next/image today; this keeps
  // one added later from failing the build.
  images: { unoptimized: true },
  reactStrictMode: true,
  experimental: {
    // TypeScript 7 is the Go port. It ships no JavaScript compiler API, so every
    // consumer that imported `typescript` as a library — Next included — has to be
    // told to shell out to the CLI instead. Without this, `next typegen` refuses
    // outright: "TypeScript 7.0.2 does not provide the compiler API required by
    // Next.js".
    //
    // Refusing is the good case. The same removal is what silently took another
    // project's module graph from 151 modules to 0 with exit 0 and no message,
    // which is why the typecheck gate counts the files it read rather than
    // trusting the exit code. `tsc --noEmit` itself is unaffected — the CLI is
    // exactly what the Go port still provides.
    useTypeScriptCli: true,
  },
};

export default createMDX()(config);
