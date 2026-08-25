import { output, readManifestForRepository, requireService, type ServiceManifest } from "./manifest";
import { reportCliError } from "./manifest";

const COMMIT = /^[0-9a-f]{40}$/;
const SERVICE_KEY = /^[a-z][a-z0-9-]{0,62}$/;
const ROOT_BUILD_INPUTS = [
  ".dockerignore",
  "deploy/services.json",
  "README.md",
  "ROADMAP.md",
  "CLEANROOM.md",
  "SECURITY.md",
  "docs/architecture.md",
  "docs/protocol.md",
  "docs/PARITY.md",
  "docs/contract/NAMING.md",
  "docs/security/THREAT_MODEL.md",
  "docs/derivation/SPECS.md",
  "docs/derivation/CAPTURES.md",
  "docs/derivation/reviews/**",
] as const;

function globExpression(pattern: string): RegExp {
  let source = "^";
  for (let index = 0; index < pattern.length; index++) {
    const character = pattern[index]!;
    if (character === "*") {
      if (pattern[index + 1] === "*") {
        index++;
        if (pattern[index + 1] === "/") {
          index++;
          source += "(?:.*/)?";
        } else {
          source += ".*";
        }
      } else {
        source += "[^/]*";
      }
    } else if (character === "?") {
      source += "[^/]";
    } else {
      source += character.replace(/[\\^$.[\]{}()+|]/g, "\\$&");
    }
  }
  return new RegExp(`${source}$`);
}

export function selectServiceKeys(
  manifest: ServiceManifest,
  changedPaths: string[] | null,
  only?: string,
): string[] {
  if (only) {
    assertServiceKey(only);
    requireService(manifest, only);
    return [only];
  }

  const keys = Object.keys(manifest.services).sort();
  for (const key of keys) assertServiceKey(key);
  if (changedPaths === null) return keys;
  return keys.filter((key) => matchesWatchedPath(keyPathList(manifest, key), changedPaths));
}

function assertServiceKey(key: string): void {
  if (!SERVICE_KEY.test(key)) throw new Error(`invalid service key ${key}`);
}

function keyPathList(manifest: ServiceManifest, key: string): string[] {
  const service = requireService(manifest, key);
  return [
    ...service.watched_paths,
    service.dockerfile,
    ...(service.build_context === "." && service.dockerfile === "site/Dockerfile"
      ? ROOT_BUILD_INPUTS
      : []),
  ];
}

function matchesWatchedPath(patterns: string[], paths: string[]): boolean {
  return paths.some((path) => patterns.some((pattern) => globExpression(pattern).test(path)));
}

export function parseBefore(value: string | undefined): string | null {
  const before = value?.trim() ?? "";
  if (before === "" || before === "0".repeat(40)) return null;
  if (!COMMIT.test(before)) throw new Error("BEFORE must be a full lowercase Git commit SHA");
  return before;
}

export function changedPathsSince(before: string): string[] {
  // A deletion under a watched path changes the image just as surely as an addition;
  // keep the full diff instead of filtering deleted paths out of the rebuild matrix.
  const result = Bun.spawnSync([
    "git",
    "diff",
    "--name-only",
    "-z",
    before,
    "HEAD",
    "--",
  ]);
  if (result.exitCode !== 0) {
    const detail = new TextDecoder().decode(result.stderr).trim();
    throw new Error(`git diff failed${detail ? `: ${detail}` : ""}`);
  }
  return new TextDecoder()
    .decode(result.stdout)
    .split("\0")
    .filter((path) => path.length > 0);
}

async function main(): Promise<void> {
  const manifest = await readManifestForRepository(process.env["GITHUB_REPOSITORY"]);
  const only = process.env["ONLY"]?.trim() || undefined;
  const before = parseBefore(process.env["BEFORE"]);
  const selected = selectServiceKeys(manifest, before === null ? null : changedPathsSince(before), only);
  output("matrix", JSON.stringify(selected));
  output("any", selected.length > 0 ? "true" : "false");
}

if (import.meta.main) {
  main().catch(reportCliError);
}
