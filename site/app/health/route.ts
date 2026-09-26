// The health route, and the only thing on the deployed site that can say what it is.
//
// The site is a static export served by Workers static assets, so this route renders once,
// at `next build`, into an extensionless `health` file. A deployed asset is traceable to a
// commit only through what the build wrote into it, so it reports the SHA it was built
// from: .github/workflows/site.yml sets GRIDWORK_GIT_SHA to the pushed commit before the
// build, and the CI site job does the same and reads the value back.
//
// It reports `unknown` when the variable was not set, as in a local build. That is
// deliberate: a build that was not handed a SHA says it does not know, rather than
// asserting one it cannot stand behind. The nearby precedent is a stamp that existed, was
// never compared to anything, and let a six-commit-stale build sit in production
// unnoticed — so the CI smoke requires the reported sha to equal the commit it built.
//
// Content-Type and Cache-Control come from public/_headers: an asset has no response
// object to set them on.
export const dynamic = "force-static";

export function GET(): Response {
  return Response.json({
    status: "ok",
    // Not `?? "unknown"` on an empty string: an empty SHA reported as a SHA is the failure
    // this route exists to prevent.
    sha: process.env.GRIDWORK_GIT_SHA || "unknown",
  });
}
