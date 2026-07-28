# Railway builds this with the REPOSITORY ROOT as the Docker build context and
# `build.dockerfilePath` (railway.json) resolved relative to it — so COPY sources
# are repo-root-relative. Copying explicitly rather than `COPY . /usr/share/caddy`
# keeps the rest of the repository out of the web root.
FROM caddy:2-alpine

COPY site/Caddyfile /etc/caddy/Caddyfile
COPY site/index.html /usr/share/caddy/index.html

RUN caddy validate --config /etc/caddy/Caddyfile
