// decode.test.ts — the TypeScript half of the golden round trip.
//
// Each committed golden (Rust-serialized) is decoded HERE at runtime: the
// contract-critical facts are checked against the wire discipline (decimal
// strings, absent-not-null, snake_case values, tagged results), the value is
// used in TYPED positions against the generated bindings, and the decoded
// value is re-emitted to goldens-ts/ — which `cargo run -p xtask -- contract
// --check` reads back into the Rust types to close the loop.

import { expect, test } from "bun:test";
import type * as B from "./bindings.ts";

const DECIMAL = /^(0|[1-9][0-9]*)$/;

async function golden(name: string): Promise<unknown> {
  return JSON.parse(await Bun.file(`${import.meta.dir}/goldens/${name}`).text());
}

async function reemit(name: string, value: unknown): Promise<void> {
  await Bun.write(`${import.meta.dir}/goldens-ts/${name}`, `${JSON.stringify(value)}\n`);
}

function isRecord(v: unknown): v is Record<string, unknown> {
  return typeof v === "object" && v !== null && !Array.isArray(v);
}

// ---- compile-time contract: mutual structural assignability ----
// A hand-written expectation of the envelope's serialize shape; if either
// direction stops extending, the contract moved and this file goes red.
type ExpectedEventEnvelope = {
  event_id: string;
  project_id: string;
  aggregate_type: string;
  aggregate_id: string;
  aggregate_version: number;
  event_type: string;
  schema_version: number;
  global_sequence: string;
  occurred_at: string;
  appended_at: string;
  actor: { kind: string; id?: string | null };
  origin: { system: string; ref?: string | null };
  causation_id?: string | null;
  correlation_id?: string | null;
  idempotency_key?: string | null;
  payload: B.JsonValue;
  payload_ref?: {
    digest: string;
    media_type: string;
    byte_size: string;
    retention_class?: string | null;
    evidence_pin?: boolean | null;
  } | null;
};
type Extends<A, B_> = A extends B_ ? true : never;
const _generatedExtendsExpected: Extends<B.EventEnvelope_Serialize, ExpectedEventEnvelope> = true;
const _expectedExtendsGenerated: Extends<ExpectedEventEnvelope, B.EventEnvelope_Serialize> = true;

// ---- compile-time contract: tri-state omission under exactOptionalPropertyTypes ----
// Omitting every optional field must COMPILE; that is the `?:` proof.
const _minimalCheckpoint: B.OrchestratorCheckpoint_Serialize = { seq: "7" };

test("event-envelope-full: decimal-string counters, ref key, typed use", async () => {
  const raw = await golden("event-envelope-full.json");
  if (!isRecord(raw)) throw new Error("golden is not an object");
  expect(raw["global_sequence"]).toMatch(DECIMAL);
  // > 2^53: the string survives exactly; a number column would have rounded.
  expect(raw["global_sequence"]).toBe("9007199254740993");
  const payloadRef = raw["payload_ref"];
  if (!isRecord(payloadRef)) throw new Error("payload_ref missing");
  expect(payloadRef["byte_size"]).toMatch(DECIMAL);
  // Rust's r#ref serializes as plain `ref`.
  const origin = raw["origin"];
  if (!isRecord(origin)) throw new Error("origin missing");
  expect(origin["ref"]).toBe("node-a");
  const env = raw as B.EventEnvelope_Serialize;
  const seq: string = env.global_sequence; // typed position: string, not number
  expect(BigInt(seq) > 2n ** 53n).toBe(true);
  await reemit("event-envelope-full.json", env);
});

test("event-envelope-minimal: absent optionals are ABSENT, not null", async () => {
  const raw = await golden("event-envelope-minimal.json");
  if (!isRecord(raw)) throw new Error("golden is not an object");
  for (const key of ["causation_id", "correlation_id", "idempotency_key", "payload_ref"]) {
    expect(key in raw).toBe(false);
  }
  const actor = raw["actor"];
  if (!isRecord(actor)) throw new Error("actor missing");
  expect("id" in actor).toBe(false);
  await reemit("event-envelope-minimal.json", raw as B.EventEnvelope_Serialize);
});

test("command-envelope: required idempotency key", async () => {
  const raw = await golden("command-envelope.json");
  if (!isRecord(raw)) throw new Error("golden is not an object");
  const env = raw as B.CommandEnvelope_Serialize;
  expect(env.idempotency_key).toBe("cancel-once");
  expect(env.command_type).toBe("cancel_attempt");
  await reemit("command-envelope.json", env);
});

test("task: snake_case state value", async () => {
  const raw = await golden("task.json");
  if (!isRecord(raw)) throw new Error("golden is not an object");
  const task = raw as B.Task_Serialize;
  const state: B.TaskState = task.state;
  expect(state).toBe("input_required");
  expect(task.tracker_ref).toBe("tracker://issue/42");
  await reemit("task.json", task);
});

test("attempt: model_lane + open engine + budget cost as decimal string", async () => {
  const raw = await golden("attempt.json");
  if (!isRecord(raw)) throw new Error("golden is not an object");
  const attempt = raw as B.Attempt_Serialize;
  expect(attempt.state).toBe("blocked");
  expect(attempt.model_lane).toBe("standard");
  expect(typeof attempt.engine).toBe("string");
  expect(attempt.budget?.max_cost_micros).toMatch(DECIMAL);
  await reemit("attempt.json", attempt);
});

test("message: delivery_refs is a channel map", async () => {
  const raw = await golden("message.json");
  if (!isRecord(raw)) throw new Error("golden is not an object");
  const message = raw as B.Message_Serialize;
  expect(message.state).toBe("applied");
  expect(message.delivery_refs).toEqual({ chat: "chat-msg-77", inbox: "inbox-3" });
  await reemit("message.json", message);
});

test("command terminal: outcome present exactly at verification_complete", async () => {
  const raw = await golden("command-verification-complete.json");
  if (!isRecord(raw)) throw new Error("golden is not an object");
  const command = raw as B.Command_Serialize;
  expect(command.state).toBe("verification_complete");
  const outcome: B.Outcome | null | undefined = command.outcome;
  expect(outcome).toBe("clean");
  await reemit("command-verification-complete.json", command);
});

test("transition results: tagged snake_case kinds, exhaustive", async () => {
  const raw = await golden("transition-results.json");
  if (!Array.isArray(raw)) throw new Error("golden is not an array");
  const results = raw as B.TransitionResult<B.TaskState>[];
  const kinds = results.map((r) => r.kind);
  expect(kinds).toEqual(["applied", "illegal_edge", "stale_version", "unauthorized_actor"]);
  for (const result of results) {
    // Exhaustive narrow: a fifth kind would fail the `never` arm at compile time.
    switch (result.kind) {
      case "applied":
        expect(result.version).toBe(2);
        break;
      case "illegal_edge":
        expect(result.from).toBe("completed");
        break;
      case "stale_version":
        expect(result.actual).toBe(3);
        break;
      case "unauthorized_actor":
        expect(result.reason).toContain("liveness_producer");
        break;
      default: {
        const unreachable: never = result;
        throw new Error(`unknown kind: ${JSON.stringify(unreachable)}`);
      }
    }
  }
  await reemit("transition-results.json", results);
});

test("checkpoint: tri-state — absent omitted, empty list kept", async () => {
  const raw = await golden("orchestrator-checkpoint.json");
  if (!isRecord(raw)) throw new Error("golden is not an object");
  expect("native_session_ref" in raw).toBe(false);
  expect("leases" in raw).toBe(false);
  expect(raw["open_attempts"]).toEqual([]);
  const checkpoint = raw as B.OrchestratorCheckpoint_Serialize;
  expect(checkpoint.seq).toMatch(DECIMAL);
  expect(checkpoint.pending_approvals?.[0]?.kind).toBe("design_fork");
  await reemit("orchestrator-checkpoint.json", checkpoint);
});

test("signal theme: 12 tokens, crate order, hex values", async () => {
  const raw = JSON.parse(await Bun.file(`${import.meta.dir}/signal-theme.json`).text());
  if (!Array.isArray(raw)) throw new Error("theme is not an array");
  const tokens = raw as B.Token[];
  expect(tokens).toHaveLength(12);
  expect(tokens[0]).toEqual({ name: "bg", value: "#070B10", role: "canvas background" });
  for (const token of tokens) {
    expect(token.value).toMatch(/^#[0-9A-F]{6}$/i);
    expect(token.name).toMatch(/^[a-z0-9_]+$/);
  }
});
