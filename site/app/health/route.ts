// The health route, and the only thing in the running container that can say what it is.
//
// Two jobs in one endpoint. A platform healthcheck needs something to poll — without it a
// container that boots broken is served rather than failing the deploy. And nothing
// deployed here is traceable to a commit on its own: the image carries no repo, and on the
// Railway half no service has a repo trigger, so every deploy there is a CLI upload for
// which Railway records no commit at all.
//
// So it reports the SHA it was built from. `.git` is excluded by .dockerignore, which is
// correct — the image should not carry a repo — so the value has to arrive as a build ARG.
// TWO paths deliver it now, and adding a third build path without wiring the ARG is how
// this quietly goes back to reporting nothing:
//
//   - tools/railway-deploy.sh sets it as a Railway SERVICE VARIABLE, because `railway up`
//     has no --build-arg and Railway feeds service variables to a Dockerfile build;
//   - .github/workflows/publish-image.yml passes `build-args: GIT_SHA`, for the image
//     pushed to the GCP registry that this service's `cloud-run-service` runtime
//     (deploy/services.json) consumes.
//
// It reports `unknown` when neither did. That is deliberate: a deploy that skipped both
// says it does not know, rather than asserting a SHA it cannot stand behind. The nearby
// precedent is a stamp that existed, was never compared to anything, and let a
// six-commit-stale build sit in production unnoticed — so `unknown` is not a shrug the
// pipeline tolerates. deploy/smoke.ts requires the reported sha to match /^[0-9a-f]{40}$/
// ("did not report an immutable revision" otherwise, which is what `unknown` trips) and,
// when handed an expected sha, to equal it exactly; the Railway wrapper polls this route
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
