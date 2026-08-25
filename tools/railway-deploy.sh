#!/usr/bin/env bash
# railway-deploy.sh — the only supported way to deploy this site.
#
# `railway up` tars the working TREE and uploads it. Not a git ref, not HEAD — whatever is
# on disk when you press enter. Ignored paths stay out, but tracked-and-modified files ride
# along, and this is the repo where that bites hardest: commits here routinely touch site/
# and crates/ together, so a deploy during unrelated crate work ships uncommitted changes to
# production.
#
# It is worse than that on this machine, and the refusals below are in the order they were
# found to matter.
set -euo pipefail

cd "$(dirname "$0")/.."

PUBLIC_URL="https://github.com/GridWork-dev/gridwork.git"
EXPECT_PROJECT="gridwork"
EXPECT_SERVICE="site"
HEALTH_URL="https://gridwork.sh/health"

# ---------------------------------------------------------------------------------------
# 1. The linked directory does not decide which project you deploy to.
#
# Railway's CLI reads RAILWAY_PROJECT_ID / RAILWAY_ENVIRONMENT_ID from the environment and
# they OUTRANK the per-directory link. Measured from this repo root, on a box where those
# are exported for another project:
#
#     $ railway status --json | jq -r .name
#     caisson-prod
#     $ env -u RAILWAY_PROJECT_ID -u RAILWAY_ENVIRONMENT_ID ... railway status --json | jq -r .name
#     gridwork
#
# So the naive `railway up` from this directory uploads this site to a different product's
# production service, silently, with a correct-looking link on disk. Stripping the pins is
# not optional and it is not defensive coding — it is the difference between the two runs
# above. RAILWAY_API_TOKEN is deliberately NOT stripped: that is the credential, not a
# target.
#
# Resolved to a path before the wrapper shadows the name: `env` execs a program, and
# `command` is a shell builtin it cannot run.
RAILWAY_BIN="$(command -v railway || true)"
if [ -z "$RAILWAY_BIN" ]; then
  echo "deploy: the railway CLI is not on PATH" >&2
  exit 2
fi
railway() {
  env -u RAILWAY_PROJECT_ID \
      -u RAILWAY_PROJECT_NAME \
      -u RAILWAY_ENVIRONMENT \
      -u RAILWAY_ENVIRONMENT_ID \
      -u RAILWAY_ENVIRONMENT_NAME \
      -u RAILWAY_SERVICE_ID \
      -u RAILWAY_SERVICE_NAME \
      "$RAILWAY_BIN" "$@"
}

# ---------------------------------------------------------------------------------------
# 2. …and neither does the directory you happen to be standing in.
#
# Four directories on this box are Railway-linked to this same service — the repo root,
# site/, and two scratch paths. `cd`-ing into the wrong one and deploying is a real path to
# production, so the target is asserted rather than assumed. This is also the check that
# catches a stripped-env run that still resolves somewhere unexpected.
#
# Parsed with jq, not a regex over the JSON. The first `"name"` in that payload is the
# ENVIRONMENT's — "production" — so a sed-and-head version reads the wrong field and
# refuses every legitimate deploy while looking like it works.
project="$(railway status --json | jq -r '.name')"
if [ "$project" != "$EXPECT_PROJECT" ]; then
  echo "deploy: linked project is '${project}', expected '${EXPECT_PROJECT}'" >&2
  echo "deploy: refusing — this directory is not linked to the site you are deploying" >&2
  exit 1
fi

# ---------------------------------------------------------------------------------------
# 3. A dirty tree is an unreviewable deploy.
#
# The uploaded tar is the working directory. If `git status` is not empty, what reaches
# production is not any commit, and nothing afterwards can reconstruct what shipped.
dirty="$(git status --porcelain)"
if [ -n "$dirty" ]; then
  echo "deploy: the working tree is dirty — refusing" >&2
  printf '%s\n' "$dirty" >&2
  echo "deploy: railway up uploads this tree verbatim, so these changes would ship" >&2
  exit 1
fi

# ---------------------------------------------------------------------------------------
# 4. And a clean tree still is not necessarily a published one.
#
# Clean only says the tree matches HEAD; HEAD can be a local commit nobody has seen. This
# checkout has no git remote by design, so the published branch is fetched by URL into
# FETCH_HEAD rather than read from a tracking ref that could be stale or absent.
git fetch --quiet "$PUBLIC_URL" main
if ! git merge-base --is-ancestor HEAD FETCH_HEAD; then
  echo "deploy: HEAD is not an ancestor of the published main — refusing" >&2
  echo "deploy: $(git rev-parse --short HEAD) is local-only or diverged; push and merge it first" >&2
  exit 1
fi

# ---------------------------------------------------------------------------------------
# 4b. A tree that gates every request on a secret must not ship to a service without one.
#
# site/proxy.ts (Wave 3) answers 403 to any request whose X-Gridwork-Origin-Secret header
# does not match GRIDWORK_ORIGIN_SECRET_CURRENT/NEXT, and it has no production bypass.
# Deploying it to a service whose environment lacks the variable takes the whole site down
# while every check above passes — the tree is clean, published, and wrong for this target.
# Only the variable NAMES are inspected: the listing goes straight into jq and nothing
# prints a value. A failed listing refuses too; an unreadable environment is not a passing one.
if [ -e site/proxy.ts ]; then
  if ! railway variables --service "$EXPECT_SERVICE" --json | jq -e 'has("GRIDWORK_ORIGIN_SECRET_CURRENT")' >/dev/null; then
    echo "deploy: site/proxy.ts gates every request on GRIDWORK_ORIGIN_SECRET_CURRENT and the '${EXPECT_SERVICE}' service does not carry it — refusing" >&2
    echo "deploy: this tree is deploy-frozen until the origin secret is set on the service (T34a)" >&2
    exit 1
  fi
fi

sha="$(git rev-parse HEAD)"
echo "deploy: ${EXPECT_PROJECT}/${EXPECT_SERVICE} at ${sha}"

if [ "${1:-}" = "--dry-run" ]; then
  echo "deploy: --dry-run, stopping before the upload"
  exit 0
fi

# ---------------------------------------------------------------------------------------
# 5. Carry the SHA into the image.
#
# `railway up` has no --build-arg. Railway passes SERVICE VARIABLES to a Dockerfile build
# as build args, so the variable is what feeds `ARG GIT_SHA` in site/Dockerfile.
# --skip-deploys keeps setting it from triggering a deploy of its own, which would race the
# one below and build the previous tree with the new SHA — the exact lie this is here to
# prevent.
railway variables --service "$EXPECT_SERVICE" --set "GIT_SHA=${sha}" --skip-deploys >/dev/null
railway up --service "$EXPECT_SERVICE" --ci

# ---------------------------------------------------------------------------------------
# 6. Then read the stamp back, because an unread stamp is decoration.
#
# The nearby precedent: a sibling repo has carried this exact provenance stamp for months
# and still let a six-commit-stale build sit in production unnoticed, because nothing ever
# compared it to anything. This is that comparison. It is the whole reason the route
# returns the SHA rather than just "ok".
echo "deploy: waiting for ${HEALTH_URL} to report ${sha:0:7}"
for _ in $(seq 1 30); do
  reported="$(curl -fsS --max-time 10 "$HEALTH_URL" 2>/dev/null | jq -r '.sha // empty' || true)"
  if [ "$reported" = "$sha" ]; then
    echo "deploy: verified — ${HEALTH_URL} reports ${sha:0:7}"
    exit 0
  fi
  sleep 10
done

echo "deploy: the upload succeeded but ${HEALTH_URL} still reports '${reported:-<no response>}'" >&2
echo "deploy: expected ${sha}. Either the deploy has not cut over yet, or it did not build this tree." >&2
exit 1
