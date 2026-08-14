# Railway and CI use the repository root as build context and select this file
# through railway.json / docker build -f site/Dockerfile .
FROM oven/bun:1.3.14-alpine@sha256:5acc90a93e91ff07bf72aa90a7c9f0fa189765aec90b47bdbf2152d2196383c0 AS deps
WORKDIR /repo/site
COPY site/package.json site/bun.lock site/source.config.ts ./
RUN mkdir -p content/docs && bun install --frozen-lockfile

FROM oven/bun:1.3.14-alpine@sha256:5acc90a93e91ff07bf72aa90a7c9f0fa189765aec90b47bdbf2152d2196383c0 AS build
WORKDIR /repo/site
COPY --from=deps /repo/site/node_modules ./node_modules
COPY site/ ./
COPY README.md ROADMAP.md CLEANROOM.md SECURITY.md /repo/
COPY docs/architecture.md docs/protocol.md docs/PARITY.md /repo/docs/
COPY docs/contract/NAMING.md /repo/docs/contract/
COPY docs/security/THREAT_MODEL.md /repo/docs/security/
COPY docs/derivation/SPECS.md docs/derivation/CAPTURES.md /repo/docs/derivation/
COPY docs/derivation/reviews/ /repo/docs/derivation/reviews/
ENV NEXT_TELEMETRY_DISABLED=1
# The deps-stage postinstall proves the install layer is valid but sees an empty content
# directory. Regenerate after the real curated content arrives; never copy stale .source.
RUN bun run postinstall && bun run build

FROM node:22-alpine@sha256:c610fcdfb1d5b4740dd70c284ed3cb16bb857e0f7166196e36a5501df7a3aa32 AS runtime
WORKDIR /app

# Deploy provenance. `railway up` uploads the local working TREE, not a git ref, so
# nothing in the image or in any response could say what is running — twelve services in
# this estate, none traceable to a commit. /health reports this value.
#
# `.git` is excluded by .dockerignore, correctly: the image should not carry a repo. So
# the SHA cannot be read at build time, it has to be passed in, and
# tools/railway-deploy.sh is what passes it.
#
# The default is `unknown` on purpose. A deploy that skipped the wrapper reports that it
# does not know, which is the honest answer, and never states a SHA it cannot stand behind.
ARG GIT_SHA=unknown
ENV NODE_ENV=production \
    NEXT_TELEMETRY_DISABLED=1 \
    HOSTNAME=0.0.0.0 \
    GRIDWORK_GIT_SHA=${GIT_SHA}

COPY --from=build /repo/site/public ./public
COPY --from=build /repo/site/.next/standalone ./
COPY --from=build /repo/site/.next/static ./.next/static

RUN mkdir -p .next/cache && chown -R 10001:10001 .next
USER 10001:10001

EXPOSE 3000
CMD ["node", "server.js"]
