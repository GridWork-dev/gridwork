#!/usr/bin/env bash
# Smoke the static export as the Workers asset handler serves it.
#
#   scripts/smoke.sh <base-url> <expected-sha>
#
# Run against `wrangler dev` of out/ (the CI site job) or a deployed URL. Every route in the
# built sitemap must answer 200 with the full security-header set; an unknown path must be a
# 404; /health must report the expected SHA as uncached JSON. The route count is compared to
# the sitemap's, so a sitemap that parsed to nothing cannot pass.
set -euo pipefail
base="${1:?usage: smoke.sh <base-url> <expected-sha>}"
want_sha="${2:?usage: smoke.sh <base-url> <expected-sha>}"
cd "$(dirname "$0")/.."

mapfile -t paths < <(grep -oE '<loc>[^<]+</loc>' out/sitemap.xml | sed -E 's#</?loc>##g; s#^https?://[^/]+##; s#^$#/#')
if [ "${#paths[@]}" -lt 10 ]; then
  echo "smoke: out/sitemap.xml lists ${#paths[@]} routes — expected at least 10" >&2
  exit 1
fi

check() {
  local path="$1" status="$2" response
  response="$(curl -sS -D - -o /dev/null "${base}${path}" | tr -d '\r')"
  if ! grep -qE "^HTTP/[0-9.]+ ${status}( |$)" <<<"$response"; then
    echo "smoke: ${path} answered '$(head -1 <<<"$response")', expected ${status}" >&2
    return 1
  fi
  for header in Strict-Transport-Security X-Content-Type-Options X-Frame-Options Referrer-Policy Content-Security-Policy; do
    if ! grep -qi "^${header}:" <<<"$response"; then
      echo "smoke: ${path} is missing ${header}" >&2
      return 1
    fi
  done
  if grep -qi '^X-Powered-By:' <<<"$response"; then
    echo "smoke: X-Powered-By leaked on ${path}" >&2
    return 1
  fi
}

checked=0
for path in "${paths[@]}"; do
  check "$path" 200
  checked=$((checked + 1))
done
check /this-path-does-not-exist 404
echo "smoke: ${checked}/${#paths[@]} sitemap routes answer 200 with every security header; unknown path 404"

health="$(curl -sS -D - "${base}/health" | tr -d '\r')"
reported="$(sed -n '/^$/,$p' <<<"$health" | tail -n +2 | jq -r '.sha')"
if [ "$reported" != "$want_sha" ]; then
  echo "smoke: /health reports '${reported}', expected '${want_sha}'" >&2
  exit 1
fi
grep -qi '^content-type: application/json' <<<"$health" || { echo "smoke: /health is not served as application/json" >&2; exit 1; }
grep -qi '^cache-control:.*no-store' <<<"$health" || { echo "smoke: /health is missing cache-control: no-store" >&2; exit 1; }
echo "smoke: /health reports ${want_sha} as uncached JSON"
