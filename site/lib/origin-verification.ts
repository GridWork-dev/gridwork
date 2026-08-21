import { Buffer } from "node:buffer";
import { timingSafeEqual } from "node:crypto";

const ORIGIN_SECRET_BYTES = 32;
const ORIGIN_SECRET_BASE64URL_LENGTH = 43;
const BASE64URL_32_BYTES = /^[A-Za-z0-9_-]{42}[AEIMQUYcgkosw048]$/;

export function originVerificationDisabledFor(
  nodeEnvironment: string | undefined,
  mode: string | undefined,
): boolean {
  return mode === "disabled" && (nodeEnvironment === "development" || nodeEnvironment === "test");
}

function decodeOriginSecret(value: string | undefined): Buffer | undefined {
  if (
    value === undefined ||
    value.length !== ORIGIN_SECRET_BASE64URL_LENGTH ||
    !BASE64URL_32_BYTES.test(value)
  ) {
    return undefined;
  }

  const decoded = Buffer.from(value, "base64url");
  return decoded.length === ORIGIN_SECRET_BYTES ? decoded : undefined;
}

export function matchesOriginSecret(
  presentedValue: string | null,
  configuredValues: readonly (string | undefined)[],
): boolean {
  if (presentedValue === null) return false;

  const presented = decodeOriginSecret(presentedValue);
  const current = decodeOriginSecret(configuredValues[0]);
  if (presented === undefined || current === undefined) return false;

  let matched = timingSafeEqual(presented, current);
  for (const configuredValue of configuredValues.slice(1)) {
    const configured = decodeOriginSecret(configuredValue);
    if (configured !== undefined) {
      matched = timingSafeEqual(presented, configured) || matched;
    }
  }

  return matched;
}
