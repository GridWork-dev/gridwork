// Parity between the live gridwork.sh and a candidate deploy of the static export.
//
//   bun scripts/parity.ts --target http://127.0.0.1:8787            # before merge: wrangler dev of out/
//   bun scripts/parity.ts --target https://gridwork-site.<sub>.workers.dev  # after merge
//   bun scripts/parity.ts --target https://gridwork.sh              # after the DNS cutover
//
// A one-shot tool run by hand at the cutover, not a CI gate: its reference is the live site,
// which CI must not depend on. It reads the LIVE sitemap, so the route list is the one
// visitors can reach today, not the one the candidate claims. For every route, plus the
// health, robots, sitemap and not-found paths, it compares the status code, the media type
// and each security header's exact value, and requires uncached JSON from /health on both
// sides. The SHA itself differs by construction and is not compared. Any mismatch exits 1.
//
// Redirect handling differs between the two origins on trailing-slash variants (the old
// server answers 308, the asset handler 307), so redirects are not followed and slash
// variants are out of scope: the sitemap has none.

// A module, for the top-level awaits below.
export {};

const SECURITY_HEADERS = [
  "strict-transport-security",
  "x-content-type-options",
  "x-frame-options",
  "referrer-policy",
  "content-security-policy",
] as const;
const EXTRA_PATHS = ["/health", "/robots.txt", "/sitemap.xml", "/this-path-does-not-exist"];
const TIMEOUT_MS = 15_000;

function arg(name: string, fallback?: string): string {
  const i = process.argv.indexOf(name);
  const value = i >= 0 ? process.argv[i + 1] : fallback;
  if (!value) throw new Error(`missing ${name}`);
  return value.replace(/\/+$/, "");
}

// A fetch that cannot hang the run: every request is bounded.
async function fetchWithTimeout(url: string): Promise<Response> {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), TIMEOUT_MS);
  try {
    return await fetch(url, { redirect: "manual", signal: controller.signal });
  } finally {
    clearTimeout(timer);
  }
}

type Snapshot = { status: number; type: string; headers: Record<string, string> };

async function snapshot(base: string, path: string): Promise<Snapshot> {
  const response = await fetchWithTimeout(`${base}${path}`);
  await response.arrayBuffer();
  const headers: Record<string, string> = {};
  for (const name of [...SECURITY_HEADERS, "cache-control"]) {
    headers[name] = (response.headers.get(name) ?? "").replace(/\s+/g, " ").trim();
  }
  const type = (response.headers.get("content-type") ?? "").split(";")[0]!.trim().toLowerCase();
  return { status: response.status, type, headers };
}

function compare(path: string, live: Snapshot, target: Snapshot): string[] {
  const problems: string[] = [];
  if (live.status !== target.status) problems.push(`status ${live.status} → ${target.status}`);
  if (live.type !== target.type)
    problems.push(`content-type ${live.type || "none"} → ${target.type || "none"}`);
  for (const name of SECURITY_HEADERS) {
    if (live.headers[name] !== target.headers[name]) {
      problems.push(
        `${name} "${live.headers[name] || "absent"}" → "${target.headers[name] || "absent"}"`,
      );
    }
  }
  if (path === "/health") {
    for (const [side, snap] of [
      ["live", live],
      ["target", target],
    ] as const) {
      if (!/\bno-store\b/.test(snap.headers["cache-control"] ?? ""))
        problems.push(`${side} /health is cacheable`);
    }
  }
  return problems.map((p) => `${path}: ${p}`);
}

const live = arg("--live", "https://gridwork.sh");
const target = arg("--target");

const sitemap = await (await fetchWithTimeout(`${live}/sitemap.xml`)).text();
const routes = [...sitemap.matchAll(/<loc>([^<]+)<\/loc>/g)].map((m) => new URL(m[1]!).pathname);
if (routes.length === 0) {
  process.stderr.write(`parity: FAIL: ${live}/sitemap.xml listed no routes\n`);
  process.exit(1);
}

const problems: string[] = [];
const paths = [...new Set([...routes, ...EXTRA_PATHS])];
for (const path of paths) {
  const [a, b] = await Promise.all([snapshot(live, path), snapshot(target, path)]);
  problems.push(...compare(path, a, b));
}

if (problems.length > 0) {
  process.stderr.write(
    `parity: FAIL: ${problems.length} mismatch(es) across ${paths.length} paths\n`,
  );
  for (const p of problems) process.stderr.write(`  ${p}\n`);
  process.exitCode = 1;
} else {
  process.stdout.write(
    `parity: ${paths.length} paths (${routes.length} from the live sitemap) match ${live}\n`,
  );
}
