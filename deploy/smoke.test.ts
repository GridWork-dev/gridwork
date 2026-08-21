import { describe, expect, test } from "bun:test";

import { readSmokeConfig, runSmoke, type SmokeConfig } from "./smoke";

const sha = "a".repeat(40);
const originSecret = "A".repeat(43);
const canaryUrl = "https://r123---gridwork-site-example-ue.a.run.app";
const securityHeaders = {
  "strict-transport-security": "max-age=31536000",
  "x-content-type-options": "nosniff",
  "x-frame-options": "DENY",
  "referrer-policy": "strict-origin-when-cross-origin",
  "content-security-policy": "default-src 'self'",
};

function responseFor(url: string | URL | Request, shaValue = sha): Response {
  const parsed = new URL(String(url));
  if (parsed.pathname === "/health") {
    return Response.json(
      { status: "ok", sha: shaValue },
      { headers: { ...securityHeaders, "cache-control": "no-store" } },
    );
  }
  return new Response("ok", { status: 200, headers: securityHeaders });
}

function config(overrides: Partial<SmokeConfig> = {}): SmokeConfig {
  return {
    environment: "production",
    mode: "post-deploy",
    expectedSha: sha,
    ...overrides,
  };
}

describe("public deployment smoke", () => {
  test("defaults a staging template invocation to post-deploy mode", () => {
    expect(
      readSmokeConfig({
        ENVIRONMENT: "staging",
        CF_ACCESS_CLIENT_ID: "client-id-name",
        CF_ACCESS_CLIENT_SECRET: "secret-value",
      }),
    ).toEqual({
      environment: "staging",
      mode: "post-deploy",
      canaryTag: undefined,
      canaryUrls: undefined,
      originSecrets: undefined,
      expectedSha: undefined,
      accessClientId: "client-id-name",
      accessClientSecret: "secret-value",
    });
  });

  test("checks health and a page through the production Cloudflare hostname", async () => {
    const calls: Array<{ url: string; headers: Headers }> = [];
    await runSmoke(config(), async (url, init) => {
      calls.push({ url: String(url), headers: new Headers(init?.headers) });
      return responseFor(url);
    });

    expect(calls.map((call) => call.url)).toEqual([
      "https://gridwork.sh/health",
      "https://gridwork.sh/",
    ]);
    for (const call of calls) {
      expect(call.url.includes("run.app")).toBe(false);
      expect(call.headers.has("x-gridwork-origin-secret")).toBe(false);
    }
  });

  test("sends Access service credentials only to the staging public hostname", async () => {
    const calls: Array<{ url: string; headers: Headers }> = [];
    await runSmoke(
      config({
        environment: "staging",
        accessClientId: "client-id-name",
        accessClientSecret: "secret-value",
      }),
      async (url, init) => {
        calls.push({ url: String(url), headers: new Headers(init?.headers) });
        return responseFor(url);
      },
    );

    expect(calls[0]?.url).toBe("https://gridwork.gwstg.dev/health");
    expect(calls[0]?.headers.get("cf-access-client-id")).toBe("client-id-name");
    expect(calls[0]?.headers.get("cf-access-client-secret")).toBe("secret-value");
    await expect(
      runSmoke(config({ environment: "staging", accessClientId: undefined })),
    ).rejects.toThrow("staging smoke requires Cloudflare Access service credentials");
  });

  test("smokes the zero-traffic tag URL directly with the gridwork origin header", async () => {
    const calls: Array<{ url: string; headers: Headers }> = [];
    await runSmoke(
      config({
        mode: "pre-migration",
        canaryTag: "r123",
        canaryUrls: JSON.stringify({ "gridwork-site": canaryUrl }),
        originSecrets: JSON.stringify({ "gridwork-site": originSecret }),
        expectedSha: undefined,
      }),
      async (url, init) => {
        calls.push({ url: String(url), headers: new Headers(init?.headers) });
        return responseFor(url);
      },
    );

    expect(calls.map((call) => call.url)).toEqual([`${canaryUrl}/health`, `${canaryUrl}/`]);
    for (const call of calls) {
      expect(call.headers.get("x-gridwork-origin-secret")).toBe(originSecret);
      expect(call.headers.has("cf-access-client-id")).toBe(false);
      expect(call.headers.has("cf-access-client-secret")).toBe(false);
    }
  });

  test("fails closed when canary URL or secret maps are absent, malformed, or unsafe", async () => {
    const base = {
      mode: "post-migration",
      canaryTag: "r123",
      expectedSha: undefined,
    } as const;
    await expect(runSmoke(config(base))).rejects.toThrow("CANARY_URLS is required");
    await expect(
      runSmoke(config({ ...base, canaryUrls: "{}", originSecrets: "{}" })),
    ).rejects.toThrow("CANARY_URLS is missing gridwork-site");
    await expect(
      runSmoke(
        config({
          ...base,
          canaryUrls: JSON.stringify({ "gridwork-site": "https://example.com" }),
          originSecrets: JSON.stringify({ "gridwork-site": originSecret }),
        }),
      ),
    ).rejects.toThrow("CANARY_URLS.gridwork-site must be a Cloud Run tag URL");
    await expect(
      runSmoke(
        config({
          ...base,
          canaryUrls: JSON.stringify({ "gridwork-site": canaryUrl }),
          originSecrets: JSON.stringify({ "gridwork-site": "not-a-secret" }),
        }),
      ),
    ).rejects.toThrow("ORIGIN_SECRETS.gridwork-site is invalid");
  });

  test("fails on status, provenance, cache, or browser-header regressions", async () => {
    await expect(
      runSmoke(config(), async () => new Response("bad", { status: 503 })),
    ).rejects.toThrow("public health returned 503");
    await expect(
      runSmoke(config(), async (url) => responseFor(url, "b".repeat(40))),
    ).rejects.toThrow("public health reported an unexpected revision");
    await expect(
      runSmoke(config({ expectedSha: undefined }), async (url) => {
        const response = responseFor(url);
        response.headers.delete("strict-transport-security");
        return response;
      }),
    ).rejects.toThrow("missing strict-transport-security");
  });

  test("requires a rollout tag for the pre/post-migration workflow modes", async () => {
    await expect(
      runSmoke(config({ mode: "pre-migration", canaryTag: undefined }), async (url) =>
        responseFor(url),
      ),
    ).rejects.toThrow("CANARY_TAG is required");
    await expect(
      runSmoke(config({ mode: "pre-migration", canaryTag: "bad tag" }), async (url) =>
        responseFor(url),
      ),
    ).rejects.toThrow("CANARY_TAG is invalid");
  });
});
