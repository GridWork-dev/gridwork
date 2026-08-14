// Proof that the vendored `gridwork` oxlint plugin actually LOADS and fires here.
//
// This is not a copy of the canonical mutation-check suite — that suite lives with the
// plugin upstream and cannot be vendored into this public repo yet (see the note in
// tools/vendored-config-drift.test.ts). This covers the one question the canonical suite
// cannot answer from upstream: does plugin.js load under THIS repo's oxlint build, from
// THIS repo's config layout, where the binary is installed under site/ and the config is
// reached through two levels of `extends`?
//
// It matters because the failure is silent. `jsPlugins` pointing at a file oxlint declines
// to load does not error — the two security rules simply never fire, and `tools/lint.sh`
// exits 0 having enforced no security floor at all. A green gate that checks nothing is
// the failure mode this whole lane exists to prevent, so the check runs the REAL binary
// against a real fixture and asserts on its diagnostics rather than importing plugin.js
// and calling into it, which would pass even if oxlint never loaded it.
import { afterAll, beforeAll, describe, expect, it } from "bun:test";
import { spawnSync } from "node:child_process";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

const REPO_ROOT = resolve(import.meta.dir, "..");
// The repo-root config, i.e. the one tools/lint.sh resolves for contracts/ — reached
// through `extends` into the vendored base, which is where `jsPlugins` is declared.
const CONFIG = join(REPO_ROOT, ".oxlintrc.json");
const OXLINT = join(REPO_ROOT, "site/node_modules/.bin/oxlint");

type Diagnostic = { code?: string };

let dir: string;
beforeAll(() => {
  dir = mkdtempSync(join(tmpdir(), "gw-oxlint-load-"));
});
afterAll(() => {
  rmSync(dir, { recursive: true, force: true });
});

/** Write `source` to a fixture and return oxlint's rule ids for it. */
function lint(name: string, source: string): string[] {
  const file = join(dir, `${name}.ts`);
  writeFileSync(file, source);
  const run = spawnSync(OXLINT, ["-c", CONFIG, "--no-ignore", "--format=json", file], {
    encoding: "utf8",
  });
  // A binary that never launched leaves `status` undefined and `stderr` null, and the
  // message that reports it says "oxlint exited undefined: null" — which is what it said
  // the first time site/node_modules was absent. The reason is in `error`; check it first.
  if (run.error) {
    throw new Error(`could not run ${OXLINT}: ${run.error.message}`);
  }
  // oxlint exits 1 when it finds anything; only a crash (>1, or unparseable stdout) is a
  // harness failure. Surfacing stderr turns "the binary moved" into a readable error
  // instead of a JSON.parse stack.
  if (run.status !== 0 && run.status !== 1) {
    throw new Error(`oxlint exited ${String(run.status)}: ${run.stderr}`);
  }
  const parsed = JSON.parse(run.stdout) as { diagnostics: Diagnostic[] };
  return parsed.diagnostics.flatMap((d) => (d.code === undefined ? [] : [d.code]));
}

describe("the vendored gridwork plugin loads in this repo", () => {
  it("fires no-interpolated-exec through the real binary", () => {
    const found = lint(
      "exec",
      "import { execSync } from 'node:child_process';\n" +
        "export const f = (u: string) => execSync(`echo ${u}`);\n",
    );
    expect(found).toContain("gridwork(no-interpolated-exec)");
  });

  it("fires no-abortsignal-timeout through the real binary", () => {
    const found = lint(
      "signal",
      "export const f = (u: string) => fetch(u, { signal: AbortSignal.timeout(1000) });\n",
    );
    expect(found).toContain("gridwork(no-abortsignal-timeout)");
  });

  it("still runs the default plugins the `plugins` key would have dropped", () => {
    // The override-not-merge trap: setting `plugins` REPLACES oxlint's defaults rather
    // than adding to them, and it does so across `extends`. If the vendored base ever
    // trims that array to only what the house adds, `typescript` goes with it and this
    // goes red — here, rather than in whichever app first shipped an `any`.
    expect(lint("defaults", "export const f = (x: any) => x;\n")).toContain(
      "typescript(no-explicit-any)",
    );
  });

  it("does not flag the shapes a bare selector would false-positive on", () => {
    // The negative case carries the weight: a rule that flags everything passes every
    // positive test above. `db.exec(sql)` and `regex.exec(s)` are the shapes that made a
    // bare-`exec` selector unusable.
    const found = lint(
      "negatives",
      "declare const db: { exec: (s: string) => void };\n" +
        "export const a = (sql: string) => db.exec(sql);\n" +
        "export const b = (s: string) => /x/.exec(s);\n",
    );
    expect(found).not.toContain("gridwork(no-interpolated-exec)");
  });
});
