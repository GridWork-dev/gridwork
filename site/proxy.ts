import { NextResponse, type NextRequest } from "next/server";
import { matchesOriginSecret, originVerificationDisabledFor } from "@/lib/origin-verification";

const ORIGIN_SECRET_HEADER = "X-Gridwork-Origin-Secret";

export function proxy(request: NextRequest): NextResponse {
  const mutationBypassEnabled = originVerificationDisabledFor(
    process.env.NODE_ENV,
    process.env.GRIDWORK_ORIGIN_SECRET_MODE,
  );

  if (
    !mutationBypassEnabled &&
    !matchesOriginSecret(request.headers.get(ORIGIN_SECRET_HEADER), [
      process.env.GRIDWORK_ORIGIN_SECRET_CURRENT,
      process.env.GRIDWORK_ORIGIN_SECRET_NEXT,
    ])
  ) {
    return NextResponse.json(
      { error: "Forbidden" },
      { status: 403, headers: { "cache-control": "no-store" } },
    );
  }

  return NextResponse.next();
}

// Deliberately no matcher: the origin gate must run for routes, metadata, and assets.
