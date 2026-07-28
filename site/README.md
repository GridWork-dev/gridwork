# site/

The public site at [gridwork.dev](https://gridwork.dev) — one self-contained static
page. No build step, no dependencies, no external requests: `index.html` carries its own
inline CSS and a `data:` URI favicon, and there is no JavaScript at all.

| File | What |
|---|---|
| `index.html` | The whole site |
| `Caddyfile` | Static serve on `$PORT`, plus the response-header floor (HSTS, nosniff, DENY, CSP) |
| `Dockerfile` | `caddy:2-alpine` + the two files above |

## Editing

Open `index.html` in a browser. That is the whole loop — there is nothing to compile.

Content is sourced only from the repository's own public documents (`README.md`,
`ROADMAP.md`, `CONTRIBUTING.md`, `CLEANROOM.md`); when a claim on the page changes,
change it there first. `tools/leak-scan.sh` runs over this directory in CI like every
other tracked file.

## Deploying

Railway builds `site/Dockerfile` and serves the result; `gridwork.dev` DNS points at that
service. Both are operator-managed — there is no deploy step in CI.

Build config lives in `railway.json` at the repository root:

```json
{ "build": { "builder": "DOCKERFILE", "dockerfilePath": "site/Dockerfile", "watchPatterns": ["site/**"] } }
```

`watchPatterns` keeps commits that do not touch `site/**` from triggering a rebuild.

**One knob worth knowing:** the Docker build context is the repository root, so the
`COPY` sources in `site/Dockerfile` are repo-root-relative (`site/index.html`, not
`index.html`). If a Railway **Root Directory** is ever set on the service, that becomes
the context and those paths need to lose their `site/` prefix. The failure mode is a
loud `COPY failed` at build time, not a bad deploy.

To check the image locally:

```bash
docker build -f site/Dockerfile -t gridwork-site .   # from the repo root
docker run --rm -p 8080:80 gridwork-site             # then open http://localhost:8080
```
