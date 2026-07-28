# Railway builds this with the REPOSITORY ROOT as the Docker build context and
# `build.dockerfilePath` (railway.json) resolved relative to it — so COPY sources
# are repo-root-relative. Copying explicitly rather than `COPY . /usr/share/caddy`
# keeps the rest of the repository out of the web root.
# Pinned by digest: the tag rides point releases, the digest is what we
# reviewed. Bump deliberately (re-pin) — dependabot tracks the tag.
FROM caddy:2-alpine@sha256:5f5c8640aae01df9654968d946d8f1a56c497f1dd5c5cda4cf95ab7c14d58648

COPY site/Caddyfile /etc/caddy/Caddyfile
COPY site/index.html /usr/share/caddy/index.html
COPY site/og.png /usr/share/caddy/og.png

RUN caddy validate --config /etc/caddy/Caddyfile

# The platform injects PORT above 1024 and TLS terminates at the edge, so root
# buys nothing. Fixed unprivileged uid:gid (no passwd entry needed); caddy's
# config/data homes point at /tmp (1777) so the non-root user can write them.
ENV XDG_CONFIG_HOME=/tmp/caddy-config \
    XDG_DATA_HOME=/tmp/caddy-data
USER 10001:10001
