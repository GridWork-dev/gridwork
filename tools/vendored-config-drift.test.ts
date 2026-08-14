// Vendored-config drift gate (house-standards SPEC D3, Lane C).
//
// D3 says COPY the shared configs, do not depend on them. Two independent reasons: bun
// cannot reliably install them from GitHub Packages, and — SPEC acceptance 15 — a config
// that extends by URL fails outright in a public repo, which this one is. Copying is the
// reversible choice. What makes a copy safe rather than a fork is this test: it hashes
// every vendored file against the canonical original and fails on any divergence.
//
// So the vendored files are NOT editable here. A change goes into the canonical copy and
// is re-vendored; editing the local one turns a shared floor into several private ones,
// silently, which is the exact failure D3 exists to prevent.
//
// WHY THE CANONICAL LOCATION IS DISCOVERED RATHER THAN WRITTEN DOWN. This repo is public
// and tools/leak-scan.sh rejects any tracked file naming a private-estate repo or carrying
// an absolute home path. The canonical checkout is both. So it is located by shape — the
// sibling directory that contains the config tree — with an env override for anything
// else. Nothing here discloses where it lives.
import { describe, expect, it } from "bun:test";
import { createHash } from "node:crypto";
import { existsSync, readdirSync, readFileSync } from "node:fs";
import { join, resolve } from "node:path";

const REPO_ROOT = resolve(import.meta.dir, "..");

// The marker is the config's own package.json, NOT base.oxlintrc.json. Every consumer
// vendors the config files, so the config files identify a consumer just as well as the
// canonical — on this machine four sibling checkouts carry them, and picking the
// first-sorted one silently compared this repo against another consumer and failed every
// assertion. Only the canonical PUBLISHES the configs as a package.
const MARKER = "tools/oxlint-config/package.json";

/** The canonical checkout: `$HOUSE_CONFIG_ROOT`, else the sibling that carries MARKER. */
function findCanonical(): string | null {
  const override = process.env.HOUSE_CONFIG_ROOT;
  if (override) return existsSync(join(override, MARKER)) ? override : null;

  const parent = resolve(REPO_ROOT, "..");
  let entries: string[];
  try {
    entries = readdirSync(parent);
  } catch {
    return null;
  }
  const found = entries
    .sort()
    .map((name) => join(parent, name))
    .filter((candidate) => candidate !== REPO_ROOT && existsSync(join(candidate, MARKER)));

  // Ambiguity is an error, not a coin flip. Two matching siblings means the marker stopped
  // identifying the canonical, and quietly picking one would compare this repo against an
  // arbitrary checkout — reporting drift that is really just "wrong reference", or worse,
  // agreement with a copy that has itself drifted. Set HOUSE_CONFIG_ROOT to disambiguate.
  if (found.length > 1) {
    throw new Error(
      `drift gate: ${String(found.length)} sibling checkouts carry ${MARKER} — ` +
        `set HOUSE_CONFIG_ROOT to name the canonical one`,
    );
  }
  return found[0] ?? null;
}

/** vendored path (repo-relative) → canonical path (canonical-repo-relative) */
const VENDORED: readonly (readonly [string, string])[] = [
  ["tools/oxlint-config/base.oxlintrc.json", "tools/oxlint-config/base.oxlintrc.json"],
  ["tools/oxlint-config/react.oxlintrc.json", "tools/oxlint-config/react.oxlintrc.json"],
  ["tools/oxlint-config/plugin.js", "tools/oxlint-config/plugin.js"],
  // `.oxfmtrc.json` is byte-vendored like the rest, which is why every path this repo must
  // keep the formatter away from lives in `.prettierignore` instead. oxfmt has no
  // `extends`: uniformity across the estate is identical bytes or nothing.
  [".oxfmtrc.json", ".oxfmtrc.json"],
  // DELIBERATELY ABSENT: tools/oxlint-config/plugin.test.ts, the canonical mutation checks
  // for plugin.js. It carries a private-estate repo name in a comment, so leak-scan.sh
  // refuses it in this public repo and it cannot be vendored until that is reworded
  // upstream — the same defect already fixed once in react.oxlintrc.json and missed in its
  // sibling. Editing the copy here is not the workaround; that IS the fork D3 forbids.
  // What covers the gap meanwhile is tools/oxlint-plugin-load.test.ts, which proves the
  // plugin loads and both security rules fire against THIS repo's oxlint build.
] as const;

/** SHA-256 of the file's raw bytes. Byte-for-byte, not parsed-and-compared: a reordered
 *  key or a reworded comment is still drift from a shared floor. */
function sha256(path: string): string {
  return createHash("sha256").update(readFileSync(path)).digest("hex");
}

const CANONICAL_ROOT = findCanonical();

describe("vendored house configs match canonical (SPEC D3)", () => {
  // CI runners have no canonical checkout, so cross-repo drift is only verifiable locally.
  // Skipping LOUDLY beats a gate that silently passes because its input was absent — an
  // absent canonical is "not checked", never "checked and identical".
  if (CANONICAL_ROOT === null) {
    it.skip("canonical checkout not found — drift NOT verified here", () => {});
    return;
  }

  // The count is asserted separately from the per-file loop on purpose. A `for` over an
  // array proves every listed file matches; it says nothing about a file that was vendored
  // and never listed, which is the drift that actually happens — someone copies a fifth
  // config in and the gate stays green because it was never told to look.
  it("checks every vendored file", () => {
    expect(VENDORED).toHaveLength(4);
  });

  for (const [local, canonical] of VENDORED) {
    it(`${local} is byte-identical to canonical`, () => {
      const localPath = join(REPO_ROOT, local);
      const canonicalPath = join(CANONICAL_ROOT, canonical);
      expect(existsSync(localPath), `${local} missing from this repo`).toBe(true);
      expect(existsSync(canonicalPath), `${canonical} missing from canonical`).toBe(true);
      expect(sha256(localPath), `${local} has drifted — re-vendor, do not edit`).toBe(
        sha256(canonicalPath),
      );
    });
  }
});
