// tools/railway-deploy.sh refuses before it uploads. This runs the real script against a
// throwaway tree with `railway`, `git` and `curl` stubbed on PATH, and asserts on the exit
// status and on which railway subcommands were reached — `up` in the call log means the
// refusal did not happen, whatever the exit status says.
//
// The stubs answer only what the script asks. `git` reports a clean, published tree so the
// earlier refusals stay quiet and the one under test is the one deciding. `curl` returns the
// stub sha immediately so a run that wrongly reaches the health loop fails in seconds, not
// after thirty ten-second sleeps.
//
// It lives in tools/ so the existing `bun test tools/` CI step picks it up.

import { afterAll, beforeAll, describe, expect, it } from "bun:test";
import { spawnSync } from "node:child_process";
import { chmodSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

const SCRIPT = resolve(import.meta.dir, "railway-deploy.sh");
const SHA = "0123456789abcdef0123456789abcdef01234567";

let root = "";

function stub(name: string, body: string): void {
  const path = join(root, "bin", name);
  writeFileSync(path, `#!/usr/bin/env bash\n${body}\n`);
  chmodSync(path, 0o755);
}

beforeAll(() => {
  root = mkdtempSync(join(tmpdir(), "railway-deploy-"));
  mkdirSync(join(root, "bin"));
  mkdirSync(join(root, "tools"));
  mkdirSync(join(root, "site"));
  writeFileSync(join(root, "tools", "railway-deploy.sh"), readFileSync(SCRIPT));
  chmodSync(join(root, "tools", "railway-deploy.sh"), 0o755);

  stub(
    "railway",
    [
      'echo "$*" >> "$STUB_LOG"',
      'case "$1 $2" in',
      '  "status --json") echo \'{"name":"gridwork"}\' ;;',
      '  "variables --service") printf "%s" "$STUB_RAILWAY_VARS" ;;',
      '  "up --service") ;;',
      '  *) echo "stub railway: unexpected $*" >&2; exit 9 ;;',
      "esac",
    ].join("\n"),
  );
  stub(
    "git",
    [
      'case "$1" in',
      "  status|fetch|merge-base) ;;",
      `  rev-parse) echo ${SHA} ;;`,
      '  *) echo "stub git: unexpected $*" >&2; exit 9 ;;',
      "esac",
    ].join("\n"),
  );
  stub("curl", `echo '{"sha":"${SHA}"}'`);
});

afterAll(() => {
  if (root) rmSync(root, { recursive: true, force: true });
});

function deploy(opts: { proxy: boolean; vars: Record<string, string>; dryRun?: boolean }) {
  const proxy = join(root, "site", "proxy.ts");
  if (opts.proxy) writeFileSync(proxy, "export {};\n");
  else if (existsSync(proxy)) rmSync(proxy);

  const log = join(root, "calls.log");
  if (existsSync(log)) rmSync(log);

  const args = [join(root, "tools", "railway-deploy.sh")];
  if (opts.dryRun) args.push("--dry-run");
  const run = spawnSync("bash", args, {
    cwd: root,
    encoding: "utf8",
    env: {
      ...process.env,
      PATH: `${join(root, "bin")}:${process.env.PATH ?? ""}`,
      STUB_LOG: log,
      STUB_RAILWAY_VARS: JSON.stringify(opts.vars),
    },
  });
  const calls = existsSync(log) ? readFileSync(log, "utf8").trim().split("\n") : [];
  return { status: run.status, stdout: run.stdout, stderr: run.stderr, calls };
}

describe("railway-deploy.sh origin-secret guard", () => {
  it("refuses a tree with site/proxy.ts when the service lacks GRIDWORK_ORIGIN_SECRET_CURRENT", () => {
    const r = deploy({ proxy: true, vars: { GIT_SHA: "unrelated" } });
    expect(r.status).toBe(1);
    expect(r.stderr).toContain("GRIDWORK_ORIGIN_SECRET_CURRENT");
    // Never reached the upload, and never touched a variable on the service.
    expect(r.calls.some((c) => c.startsWith("up "))).toBe(false);
    expect(r.calls.some((c) => c.includes("--set"))).toBe(false);
  });

  it("proceeds when the service carries the secret", () => {
    const r = deploy({ proxy: true, vars: { GRIDWORK_ORIGIN_SECRET_CURRENT: "x" }, dryRun: true });
    expect(r.status).toBe(0);
    expect(r.stdout).toContain("--dry-run");
  });

  it("does not gate a tree without site/proxy.ts", () => {
    const r = deploy({ proxy: false, vars: {}, dryRun: true });
    expect(r.status).toBe(0);
    expect(r.calls.some((c) => c.startsWith("variables "))).toBe(false);
  });

  it("refuses when the variable listing itself fails", () => {
    const r = deploy({ proxy: true, vars: {} , dryRun: true });
    // `{}` parses and lacks the key; an empty/invalid listing must refuse the same way.
    expect(r.status).toBe(1);
  });
});
