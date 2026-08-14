// The docs mirror freshness gate.
//
// WHY THE SOURCE HASHES LIVE INSIDE THE MIRRORS AND NOT IN THE MAP. The first
// version of this gate kept every source digest in source-map.json and checked
// only that the map agreed with the current sources — the mirrored page itself
// got an existence check and nothing more. That let a stale mirror pass two
// ways, and one of them happened: a canonical doc moved, the map digest was
// refreshed (the red gate's own error message invites exactly that edit), the
// mirror was never re-curated, and the gate stayed green until a later commit
// had to catch the mirror up by hand. The digest the gate checked did not
// cover the thing it vouched for.
//
// So the freshness claim now lives where the curation lives: each curated page
// embeds `{/* curated-from: <path> sha256=<hex> */}` for every canonical
// source it mirrors, written at curation time. The gate verifies those against
// the CURRENT source bytes, which makes "the source moved and the mirror did
// not" mechanically red — going green again requires editing the mirror file
// itself, in the same place a curator updates the content. There is no third
// artifact left whose edit can reconcile the other two in silence.
//
// source-map.json keeps two jobs: the page footer's canonical-source links,
// and the sha256 of each curated page's own bytes — so corrupted or
// hand-drifted mirror content is red too, and a re-curation is an explicit
// two-sided act (update the page's curated-from lines, re-register its bytes).
// Like every gate in this repository, what this proves is that the claim was
// made against these exact bytes and is attributable; a curator who updates a
// hash without re-reading has still signed the file that says so.
//
// A source may be a FILE or a DIRECTORY. A directory binds the recursive
// listing digest (sorted `<sha256>  <relpath>` lines), so a file added,
// removed, or edited anywhere under it moves the digest. The reviews mirror is
// why: it describes every record in docs/derivation/reviews/, and a map that
// enumerated the records one by one silently stopped covering the tree the
// day the next record landed.
import { createHash } from "node:crypto";
import { readFile, readdir, stat } from "node:fs/promises";
import { dirname, isAbsolute, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";
import { z } from "zod";

const siteRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const repoRoot = resolve(siteRoot, "..");
const docsRoot = resolve(siteRoot, "content/docs");

const safeRelativePath = z
  .string()
  .trim()
  .min(1)
  .max(240)
  .refine(
    (value) => !isAbsolute(value) && !value.includes("\0") && !value.split(/[\\/]/).includes(".."),
    "path must be a bounded relative path without parent traversal",
  );

const sourceMapSchema = z
  .object({
    version: z.literal(2),
    pages: z
      .array(
        z
          .object({
            destination: safeRelativePath.regex(/^content\/docs\/.+\.mdx$/),
            sha256: z.string().regex(/^[a-f0-9]{64}$/),
            sources: z.array(z.object({ path: safeRelativePath }).strict()).min(1),
          })
          .strict(),
      )
      .min(1),
  })
  .strict();

// One binding per line, anywhere in the page (curation puts them right after
// the frontmatter). MDX renders the comment as nothing.
const bindingPattern = /\{\/\*\s*curated-from:\s+(\S+)\s+sha256=([a-f0-9]{64})\s*\*\/\}/g;

function resolveInside(root: string, relativePath: string): string {
  const absolutePath = resolve(root, relativePath);
  if (!absolutePath.startsWith(`${root}${sep}`)) {
    throw new Error(`path escapes allowed root: ${relativePath}`);
  }
  return absolutePath;
}

function sha256Bytes(bytes: Uint8Array): string {
  return createHash("sha256").update(bytes).digest("hex");
}

async function directoryDigest(root: string): Promise<string> {
  const entries = await readdir(root, { recursive: true, encoding: "utf8" });
  const files: string[] = [];
  for (const entry of entries) {
    const absolutePath = resolve(root, entry);
    if ((await stat(absolutePath)).isFile()) {
      files.push(entry.split(sep).join("/"));
    }
  }
  files.sort();
  const digest = createHash("sha256");
  for (const relativePath of files) {
    const fileHash = sha256Bytes(await readFile(resolve(root, relativePath)));
    digest.update(`${fileHash}  ${relativePath}\n`);
  }
  return digest.digest("hex");
}

async function sourceDigest(absolutePath: string): Promise<string> {
  const info = await stat(absolutePath);
  if (info.isDirectory()) {
    return directoryDigest(absolutePath);
  }
  return sha256Bytes(await readFile(absolutePath));
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

    const destinationBytes = await readFile(resolveInside(siteRoot, page.destination));
    const destinationHash = sha256Bytes(destinationBytes);
    if (destinationHash !== page.sha256) {
      throw new Error(
        `mirror content drifted from its registration: ${page.destination} ` +
          `hashes to ${destinationHash}, the map registered ${page.sha256} — ` +
          `if this is a deliberate re-curation, update the page's sha256 in ` +
          `source-map.json; otherwise the mirror was edited without one`,
      );
    }

    const bindings = [...destinationBytes.toString("utf8").matchAll(bindingPattern)].map(
      (match) => ({
        path: safeRelativePath.parse(match[1]),
        sha256: match[2] as string,
      }),
    );
    if (bindings.length === 0) {
      throw new Error(
        `${page.destination} declares no curated-from binding — every mirror ` +
          `names the source bytes it was curated from`,
      );
    }

    const boundPaths = new Set(bindings.map((binding) => binding.path));
    const mappedPaths = new Set(page.sources.map((source) => source.path));
    const unbound = [...mappedPaths].filter((path) => !boundPaths.has(path));
    const unmapped = [...boundPaths].filter((path) => !mappedPaths.has(path));
    if (unbound.length > 0 || unmapped.length > 0) {
      throw new Error(
        `${page.destination} and its source-map row disagree about its sources; ` +
          `in map only: ${unbound.join(",") || "none"}; ` +
          `in page only: ${unmapped.join(",") || "none"}`,
      );
    }

    for (const binding of bindings) {
      const actual = await sourceDigest(resolveInside(repoRoot, binding.path));
      if (actual !== binding.sha256) {
        throw new Error(
          `stale curated docs: ${page.destination} was curated from ` +
            `${binding.path}@${binding.sha256}, which now digests to ${actual} — ` +
            `re-curate the mirror, update its curated-from line, and ` +
            `re-register the page's sha256 in source-map.json`,
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
