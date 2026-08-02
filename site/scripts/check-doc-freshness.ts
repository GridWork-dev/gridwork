import { createHash } from "node:crypto";
import { access, readFile, readdir } from "node:fs/promises";
import { dirname, isAbsolute, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";
import { z } from "zod";

const siteRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const repoRoot = resolve(siteRoot, "..");
const docsRoot = resolve(siteRoot, "content/docs");

const safeRelativePath = z.string().trim().min(1).max(240).refine(
  (value) =>
    !isAbsolute(value) &&
    !value.includes("\0") &&
    !value.split(/[\\/]/).includes(".."),
  "path must be a bounded relative path without parent traversal",
);

const sourceMapSchema = z.object({
  version: z.literal(1),
  pages: z.array(z.object({
    destination: safeRelativePath.regex(/^content\/docs\/.+\.mdx$/),
    sources: z.array(z.object({
      path: safeRelativePath,
      sha256: z.string().regex(/^[a-f0-9]{64}$/),
    }).strict()).min(1),
  }).strict()).min(1),
}).strict();

function resolveInside(root: string, relativePath: string): string {
  const absolutePath = resolve(root, relativePath);
  if (!absolutePath.startsWith(`${root}${sep}`)) {
    throw new Error(`path escapes allowed root: ${relativePath}`);
  }
  return absolutePath;
}

async function sha256(path: string): Promise<string> {
  return createHash("sha256").update(await readFile(path)).digest("hex");
}

async function main(): Promise<void> {
  const raw: unknown = JSON.parse(
    await readFile(resolve(siteRoot, "content/source-map.json"), "utf8"),
  );
  const sourceMap = sourceMapSchema.parse(raw);
  const mappedDestinations = new Set<string>();
  let sourceBindings = 0;

  for (const page of sourceMap.pages) {
    if (mappedDestinations.has(page.destination)) {
      throw new Error(`duplicate destination: ${page.destination}`);
    }
    mappedDestinations.add(page.destination);
    await access(resolveInside(siteRoot, page.destination));

    for (const source of page.sources) {
      const actual = await sha256(resolveInside(repoRoot, source.path));
      if (actual !== source.sha256) {
        throw new Error(
          `stale curated docs: ${source.path} expected ${source.sha256}, received ${actual}`,
        );
      }
      sourceBindings += 1;
    }
  }

  const entries = await readdir(docsRoot, { recursive: true, encoding: "utf8" });
  const curatedPages = entries
    .filter((entry) => entry.endsWith(".mdx"))
    .map((entry) => `content/docs/${entry.split(sep).join("/")}`)
    .sort();

  const unmapped = curatedPages.filter((page) => !mappedDestinations.has(page));
  const missing = [...mappedDestinations].filter((page) => !curatedPages.includes(page));
  if (unmapped.length > 0 || missing.length > 0) {
    throw new Error(
      `source-map coverage mismatch; unmapped=${unmapped.join(",") || "none"}; missing=${missing.join(",") || "none"}`,
    );
  }

  process.stdout.write(
    `docs-freshness: ${curatedPages.length} pages, ${sourceBindings} source bindings clean\n`,
  );
}

main().catch((error: unknown) => {
  const message = error instanceof Error ? error.message : String(error);
  process.stderr.write(`docs-freshness: FAIL: ${message}\n`);
  process.exitCode = 1;
});
