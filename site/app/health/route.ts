// The health route, and the only thing in the running container that can say what it is.
//
// Two jobs in one endpoint. Railway's `healthcheckPath` needs something to poll — without
// it a container that boots broken is served rather than failing the deploy. And nothing
// deployed in this estate is traceable to a commit: no Railway service has a repo trigger,
// every deploy is a CLI upload, and Railway records no commit for one. Twelve services,
// zero provenance.
//
// So it reports the SHA it was built from. `.git` is excluded by .dockerignore, which is
// correct — the image should not carry a repo — so the value has to be passed in as a
// build ARG at deploy time, and `tools/railway-deploy.sh` is what passes it.
//
// It reports `unknown` when nobody did. That is deliberate: a deploy that skipped the
// wrapper says it does not know, rather than asserting a SHA it cannot stand behind. The
// nearby precedent is a stamp that existed, was never compared to anything, and let a
// six-commit-stale build sit in production unnoticed — so the wrapper polls this route
// after deploying and fails if the SHA is not the one it just shipped. A stamp nobody
// compares is decoration.

// A build-time constant would bake the fallback into the bundle at `next build`, which
// runs in a stage that has no SHA to know. Read at request time, from the runtime stage's
// environment, where the ARG actually lands.
export const dynamic = "force-dynamic";

export function GET(): Response {
  return Response.json(
    {
      status: "ok",
      // Not `?? "unknown"` on an empty string: `--build-arg GIT_SHA=` yields "", and an
      // empty SHA reported as a SHA is the failure this route exists to prevent.
      sha: process.env.GRIDWORK_GIT_SHA || "unknown",
    },
    {
      // A cached health check answers for the container that served it last, which is the
      // one question it must never answer.
      headers: { "cache-control": "no-store" },
    },
  );
}
