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
  //
  // The design-token contract and its validator (Lane E item 2, acceptance 7). Vendored
  // under site/ rather than tools/ because index.ts imports postcss and Bun resolves bare
  // specifiers by walking up from the imported file — there is no root package.json here,
  // so a copy beside this test would find no node_modules at all. The canonical path is
  // unchanged; only the local one moves.
  //
  // These two are why the site declares all 24 roles literally in all three theme scopes
  // instead of generating them: keeping the validator byte-identical is what makes this a
  // plain equality check. A local relaxation would have made it a fork with a diff to
  // maintain, which is the thing D3 exists to prevent.
  ["site/tools/design-tokens/contract.ts", "tools/design-tokens/contract.ts"],
  ["site/tools/design-tokens/index.ts", "tools/design-tokens/index.ts"],
] as const;

/** The vendored bytes as of the commit that recorded them, keyed by the same repo-relative
 *  path as VENDORED.
 *
 *  These exist because the comparison below cannot run on a CI runner — there is no
 *  canonical checkout to compare against — and CI is the only place a merge is actually
 *  gated. Without them the whole of D3 is a local courtesy: a vendored config edited on a
 *  branch reaches main with every check green.
 *
 *  The two halves catch different things and neither subsumes the other. A digest catches
 *  an edit made HERE, anywhere it runs. The canonical comparison catches this repo falling
 *  behind an upstream change, which a digest recorded from these same bytes never can.
 *
 *  Changing a vendored config is therefore two edits — the file and its digest — and the
 *  second is the one a reviewer sees. That is the point, not an inconvenience. */
const RECORDED: Readonly<Record<string, string>> = {
  "tools/oxlint-config/base.oxlintrc.json":
    "50702202ed9ba3721c520c2f540077429afc722269ef3cd49a186b35c1d0a563",
  "tools/oxlint-config/react.oxlintrc.json":
    "2baac8b48862d86183c422aca30966e60236d652a7ecd62b20aca60da5c97ac1",
  "tools/oxlint-config/plugin.js":
    "c1f050b5b2b4ce9586776163f6d519e15cff51c85bd5b73762e2cfdb9ce045a0",
  ".oxfmtrc.json": "e02c4a19a29b71af404771d5d57f10f4f97f42cbbdfc4256c831ff1832a5eb80",
  "site/tools/design-tokens/contract.ts":
    "da35d2b648a26aa132934a11c6704d02b827673d1354beeff1f358e882a26ffc",
  "site/tools/design-tokens/index.ts":
    "e8ecbdb20459df57e7f3afc508ffc98f3851b37072e701b3893455f54a39296c",
};

/** SHA-256 of the file's raw bytes. Byte-for-byte, not parsed-and-compared: a reordered
 *  key or a reworded comment is still drift from a shared floor. */
function sha256(path: string): string {
  return createHash("sha256").update(readFileSync(path)).digest("hex");
}

// Runs everywhere, including CI. Deliberately a separate describe from the canonical
// comparison below, which skips when there is nothing to compare against: one of these two
// blocks always executes, so "all green" can never mean "nothing ran".
describe("vendored house configs match their recorded bytes (SPEC D3)", () => {
  // Asserted against VENDORED rather than a literal, so a file added to one list and
  // forgotten in the other fails here instead of going unwatched by the half that runs in
  // CI. `expect(...).toHaveLength` on an empty VENDORED would pass a `for` loop silently.
  it("records a digest for every vendored file", () => {
    expect(Object.keys(RECORDED).sort()).toEqual(VENDORED.map(([local]) => local).sort());
    expect(VENDORED.length).toBeGreaterThan(0);
  });

  for (const [local] of VENDORED) {
    it(`${local} matches its recorded digest`, () => {
      const localPath = join(REPO_ROOT, local);
      expect(existsSync(localPath), `${local} missing from this repo`).toBe(true);
      expect(
        sha256(localPath),
        `${local} was edited here — re-vendor from canonical, or record the new bytes if ` +
          `the change already landed upstream`,
      ).toBe(RECORDED[local]);
    });
  }
});

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
    expect(VENDORED).toHaveLength(6);
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
