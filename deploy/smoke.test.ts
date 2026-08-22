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
  test("reads the explicit staging mode supplied by the corrected template", () => {
    expect(
      readSmokeConfig({
        ENVIRONMENT: "staging",
        MODE: "staging",
        CF_ACCESS_CLIENT_ID: "client-id-name",
        CF_ACCESS_CLIENT_SECRET: "secret-value",
      }),
    ).toEqual({
      environment: "staging",
      mode: "staging",
      service: undefined,
      canaryTag: undefined,
      canaryUrls: undefined,
      originSecrets: undefined,
      expectedSha: undefined,
      accessClientId: "client-id-name",
      accessClientSecret: "secret-value",
    });
  });

  test("does not bind a delayed promotion to the workflow checkout", () => {
    const workflowSha = "b".repeat(40);
    expect(
      readSmokeConfig({
        ENVIRONMENT: "production",
        MODE: "post-deploy",
        GITHUB_SHA: workflowSha,
      }).expectedSha,
    ).toBeUndefined();
    expect(
      readSmokeConfig({
        ENVIRONMENT: "production",
        MODE: "post-deploy",
        EXPECTED_SHA: sha,
        GITHUB_SHA: workflowSha,
      }).expectedSha,
    ).toBe(sha);
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
        mode: "staging",
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

    await expect(
      runSmoke(
        config({
          environment: "staging",
          mode: "staging",
          accessClientId: "client-id-name",
          accessClientSecret: "secret-value",
        }),
        async () =>
          new Response(null, {
            status: 302,
            headers: { location: "https://example.cloudflareaccess.com/cdn-cgi/access/login" },
          }),
      ),
    ).rejects.toThrow("public health returned 302");
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
        const headers = new Headers(init?.headers);
        calls.push({ url: String(url), headers });
        if (new URL(String(url)).searchParams.has("origin-gate")) {
          return Response.json({ error: "Forbidden" }, { status: 403 });
        }
        return responseFor(url);
      },
    );

    expect(calls.map((call) => call.url)).toEqual([
      `${canaryUrl}/health?origin-gate=missing`,
      `${canaryUrl}/health?origin-gate=incorrect`,
      `${canaryUrl}/health`,
      `${canaryUrl}/`,
    ]);
    expect(calls[0]?.headers.has("x-gridwork-origin-secret")).toBe(false);
    expect(calls[1]?.headers.get("x-gridwork-origin-secret")).not.toBe(originSecret);
    for (const call of calls.slice(2)) {
      expect(call.headers.get("x-gridwork-origin-secret")).toBe(originSecret);
      expect(call.headers.has("cf-access-client-id")).toBe(false);
      expect(call.headers.has("cf-access-client-secret")).toBe(false);
    }
  });

  test("rejects a canary whose direct origin accepts missing or incorrect gate credentials", async () => {
    await expect(
      runSmoke(
        config({
          mode: "pre-migration",
          canaryTag: "r123",
          canaryUrls: JSON.stringify({ "gridwork-site": canaryUrl }),
          originSecrets: JSON.stringify({ "gridwork-site": originSecret }),
          expectedSha: undefined,
        }),
        async (url) => responseFor(url),
      ),
    ).rejects.toThrow("canary origin gate accepted a request without the origin secret");
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

  test("reads SERVICE and scopes rollback smoke to the requested owned service", async () => {
    const rollback = readSmokeConfig({
      ENVIRONMENT: "production",
      MODE: "rollback",
      SERVICE: "gridwork-site",
    });
    expect(rollback).toMatchObject({ mode: "rollback", service: "gridwork-site" });
    await expect(runSmoke(rollback, async (url) => responseFor(url))).resolves.toBeUndefined();

    await expect(
      runSmoke({ ...config({ mode: "rollback" }), service: undefined } as SmokeConfig),
    ).rejects.toThrow("rollback smoke requires SERVICE");
    await expect(
      runSmoke({ ...config({ mode: "rollback" }), service: "other-site" } as SmokeConfig),
    ).rejects.toThrow("rollback smoke received unknown service other-site");
  });
});
