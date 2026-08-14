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

function bytesFromHex(hex: string): Uint8Array {
  if (!/^(?:[0-9a-f]{2})+$/.test(hex)) throw new Error("wire golden is not lowercase hex");
  return Uint8Array.from(hex.match(/../g) ?? [], (byte) => Number.parseInt(byte, 16));
}

function readWireFrame(
  wire: Uint8Array,
  offset: number,
): { kind: number; body: Uint8Array; next: number } {
  if (wire.length - offset < 5) throw new Error("truncated frame header");
  const length = new DataView(wire.buffer, wire.byteOffset, wire.byteLength).getUint32(
    offset,
    false,
  );
  if (length < 1) throw new Error("frame length omitted the kind byte");
  const next = offset + 4 + length;
  if (next > wire.length) throw new Error("truncated frame body");
  return {
    kind: wire[offset + 4]!,
    body: wire.slice(offset + 5, next),
    next,
  };
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

// ---- compile-time contract: a unit request carries ONLY its tag ----
// If `health` ever grew a required field, this stops compiling.
const _unitRequest: B.KernelRequest_Serialize = { type: "health" };

// ---- compile-time contract: the protocol major is a NUMBER, counters are STRINGS ----
// The one place the decimal-string rule does not apply is the small bounded
// version; getting that backwards in either direction fails here.
const _helloShape: B.ClientControl_Serialize = {
  type: "hello",
  protocol_major: 1,
  protocol_minor: 0,
  capabilities: ["event_subscribe", "pty_input"],
};
const _batchShape: Pick<Extract<B.ServerControl_Serialize, { type: "event_batch" }>, "cursor"> = {
  cursor: "9007199254740993",
};
const _gateActorShape: Pick<B.Gate_Serialize, "decided_by"> = {
  decided_by: { kind: "operator" },
};
const _openPtyShape: Extract<B.KernelCommand_Serialize, { type: "open_pty_session" }> = {
  type: "open_pty_session",
  pty_session_id: "pty-1",
  generation: "gen-1",
  engine_session_id: "session-1",
};

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

test("workspace node: durable tree structure and pane binding", async () => {
  const raw = await golden("workspace-node.json");
  if (!isRecord(raw)) throw new Error("golden is not an object");
  const node = raw as B.WorkspaceNode_Serialize;
  const kind: B.WorkspaceNodeKind = node.kind;
  expect(kind).toBe("pane");
  expect(node.parent_id).toBe("tab-0001");
  expect(node.session_id).toBe("pty-1");
  for (const transient of ["split_size", "focus", "z_order"]) {
    expect(transient in raw).toBe(false);
  }
  await reemit("workspace-node.json", node);
});

test("workflow run: an open-string step and a terminal close that keeps the row", async () => {
  const raw = await golden("workflow-run.json");
  if (!isRecord(raw)) throw new Error("golden is not an object");
  const run = raw as B.WorkflowRun_Serialize;
  // `step` is template vocabulary, typed as a plain string — a union here
  // would mean the kernel grew an act taxonomy (decision 17 forbids it).
  const step: string | null | undefined = run.step;
  expect(step).toBe("ship");
  expect(run.state).toBe("completed");
  expect(run.template_ref).toBe("seven-act@1");
  expect(run.closed_at).toBe("2026-08-09T13:00:00Z");
  await reemit("workflow-run.json", run);
});

test("pty session: lifecycle counters and a terminal close that keeps the row", async () => {
  const raw = await golden("pty-session.json");
  if (!isRecord(raw)) throw new Error("golden is not an object");
  const session = raw as B.PtySession_Serialize;
  expect(session.state).toBe("closed");
  // The generation names one host lifetime — distinct generations are how
  // the S8 receipt counts host restarts.
  expect(session.generation).toBe("gen-0001");
  expect(session.engine_session_id).toBe("session-0001");
  expect(session.attach_count).toBe(3);
  expect(session.detach_count).toBe(3);
  expect(session.closed_at).toBe("2026-08-09T18:00:00Z");
  await reemit("pty-session.json", session);
});

test("pty session template: executable data lives in the catalog", async () => {
  const raw = await golden("pty-session-template.json");
  if (!isRecord(raw)) throw new Error("golden is not an object");
  const template = raw as B.PtySessionTemplate_Serialize;
  expect(template.name).toBe("review");
  expect(template.command).toBe("/usr/bin/review-agent");
  expect(template.args).toEqual(["--interactive"]);
  expect(template.env).toEqual({ TERM: "env:TERM" });
  expect([template.cols, template.rows]).toEqual([100, 30]);
  await reemit("pty-session-template.json", template);
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

test("client control: tagged frames, number major, decimal-string cursor", async () => {
  const raw = await golden("kernel-client-control.json");
  if (!Array.isArray(raw)) throw new Error("golden is not an array");
  const frames = raw as B.ClientControl_Serialize[];
  expect(frames.map((f) => f.type)).toEqual([
    "hello",
    "request",
    "request",
    "request",
    "request",
    "request",
    "request",
    "request",
    "request",
    "request",
    "request",
    "request",
    "pty_raw_publish_snapshot",
    "pty_raw_publish_output",
    "request",
    "request",
    "request",
    "request",
    "request",
    "pty_delivery_ack",
  ]);

  for (const frame of frames) {
    // Exhaustive narrow: any unhandled ClientControl variant fails the `never` arm.
    switch (frame.type) {
      case "hello": {
        // The version is a bounded JSON NUMBER — the decimal-string rule is
        // for 64-bit counters, not for this.
        const major: number = frame.protocol_major;
        expect(major).toBe(1);
        expect(frame.capabilities).toEqual([
          "event_subscribe",
          "blob",
          "pty_raw",
          "pty_input",
          "pty_control",
          "pty_start",
        ]);
        for (const name of frame.capabilities) expect(name).toMatch(/^[a-z][a-z0-9_]*$/);
        break;
      }
      case "request": {
        expect(typeof frame.request_id).toBe("string");
        break;
      }
      case "pty_raw_publish_snapshot":
      case "pty_raw_publish_output": {
        expect(typeof frame.request_id).toBe("string");
        expect(frame.byte_size).toMatch(DECIMAL);
        break;
      }
      case "pty_delivery_ack": {
        expect(typeof frame.delivery_id).toBe("string");
        expect(frame.result.status).toBe("applied");
        break;
      }
      default: {
        const unreachable: never = frame;
        throw new Error(`unknown frame: ${JSON.stringify(unreachable)}`);
      }
    }
  }

  // A unit request is exactly its tag — no null-filled fields.
  const verifySealed = raw[1];
  if (!isRecord(verifySealed)) throw new Error("frame 1 missing");
  expect(verifySealed["request"]).toEqual({ type: "verify_sealed" });

  // The typed command travels AS the envelope payload; command_type names the
  // same variant the payload's own tag does.
  const submit = raw[2];
  if (!isRecord(submit)) throw new Error("frame 2 missing");
  const request = submit["request"];
  if (!isRecord(request)) throw new Error("request missing");
  const envelope = request["envelope"];
  if (!isRecord(envelope)) throw new Error("envelope missing");
  const payload = envelope["payload"];
  if (!isRecord(payload)) throw new Error("payload missing");
  expect(envelope["command_type"]).toBe("activate_kernel");
  expect(payload["type"]).toBe("activate_kernel");
  expect(payload["archive_manifest_sha256"]).toMatch(/^[0-9a-f]{64}$/);

  const read = raw[3];
  if (!isRecord(read)) throw new Error("frame 3 missing");
  const readRequest = read["request"];
  if (!isRecord(readRequest)) throw new Error("read request missing");
  expect(readRequest["cursor"]).toMatch(DECIMAL);
  expect(BigInt(String(readRequest["cursor"])) > 2n ** 53n).toBe(true);
  // Absent optionals stay ABSENT inside a tagged variant too.
  const list = raw[4];
  if (!isRecord(list)) throw new Error("frame 4 missing");
  const listRequest = list["request"];
  if (!isRecord(listRequest)) throw new Error("list request missing");
  expect("cursor" in listRequest).toBe(false);

  // A reattach: `cursor` present, resuming after a prior frame revision.
  const ptyAttach = raw[6];
  if (!isRecord(ptyAttach)) throw new Error("frame 6 missing");
  const attachRequest = ptyAttach["request"];
  if (!isRecord(attachRequest)) throw new Error("pty attach request missing");
  expect(attachRequest["type"]).toBe("pty_attach");
  expect(attachRequest["generation"]).toBe("pty-life-1");
  expect(attachRequest["cursor"]).toMatch(DECIMAL);

  // A fresh, un-attached snapshot request carries no cursor at all.
  const ptySnapshot = raw[7];
  if (!isRecord(ptySnapshot)) throw new Error("frame 7 missing");
  const snapshotRequest = ptySnapshot["request"];
  if (!isRecord(snapshotRequest)) throw new Error("pty snapshot request missing");
  expect(snapshotRequest["type"]).toBe("pty_snapshot");
  expect("cursor" in snapshotRequest).toBe(false);

  // The host's publish half: a sequenced reseed carrying a full frame, a
  // delta batch on the same decimal-string axis, and the explicit retire.
  const publishSnapshot = raw[8];
  if (!isRecord(publishSnapshot)) throw new Error("frame 8 missing");
  const publishRequest = publishSnapshot["request"];
  if (!isRecord(publishRequest)) throw new Error("pty publish request missing");
  expect(publishRequest["type"]).toBe("pty_publish_snapshot");
  expect(publishRequest["seq"]).toMatch(DECIMAL);
  const publishedFrame = publishRequest["frame"];
  if (!isRecord(publishedFrame)) throw new Error("published frame missing");
  expect(Array.isArray(publishedFrame["styles"])).toBe(true);
  expect(Array.isArray(publishedFrame["rows"])).toBe(true);

  const publishDeltas = raw[9];
  if (!isRecord(publishDeltas)) throw new Error("frame 9 missing");
  const deltasRequest = publishDeltas["request"];
  if (!isRecord(deltasRequest)) throw new Error("pty deltas request missing");
  expect(deltasRequest["type"]).toBe("pty_publish_deltas");
  expect(deltasRequest["seq"]).toMatch(DECIMAL);
  expect(Array.isArray(deltasRequest["deltas"])).toBe(true);

  const retire = raw[10];
  if (!isRecord(retire)) throw new Error("frame 10 missing");
  const retireRequest = retire["request"];
  if (!isRecord(retireRequest)) throw new Error("pty retire request missing");
  expect(retireRequest["type"]).toBe("pty_retire");

  const rawAttach = raw[11];
  if (!isRecord(rawAttach)) throw new Error("frame 11 missing");
  const rawAttachRequest = rawAttach["request"];
  if (!isRecord(rawAttachRequest)) throw new Error("raw attach request missing");
  expect(rawAttachRequest["type"]).toBe("pty_raw_attach");
  expect(rawAttachRequest["cursor"]).toMatch(DECIMAL);

  const rawSnapshot = raw[12];
  if (!isRecord(rawSnapshot)) throw new Error("frame 12 missing");
  expect(rawSnapshot["type"]).toBe("pty_raw_publish_snapshot");
  expect(rawSnapshot["byte_size"]).toMatch(DECIMAL);
  expect("seq" in rawSnapshot).toBe(false);

  const rawOutput = raw[13];
  if (!isRecord(rawOutput)) throw new Error("frame 13 missing");
  expect(rawOutput["type"]).toBe("pty_raw_publish_output");
  expect(rawOutput["seq"]).toMatch(DECIMAL);

  const rawResize = raw[14];
  if (!isRecord(rawResize)) throw new Error("frame 14 missing");
  const rawResizeRequest = rawResize["request"];
  if (!isRecord(rawResizeRequest)) throw new Error("raw resize request missing");
  expect(rawResizeRequest["type"]).toBe("pty_publish_raw_resize");
  expect(rawResizeRequest["seq"]).toMatch(DECIMAL);

  const ptyInput = raw[15];
  if (!isRecord(ptyInput)) throw new Error("frame 15 missing");
  const ptyInputRequest = ptyInput["request"];
  if (!isRecord(ptyInputRequest)) throw new Error("pty input request missing");
  expect(ptyInputRequest["type"]).toBe("send_pty_input");
  expect(ptyInputRequest["data_base64"]).toBe("AP8K");
  const ptyInputEnvelope = ptyInputRequest["envelope"];
  if (!isRecord(ptyInputEnvelope)) throw new Error("pty input envelope missing");
  const ptyInputPayload = ptyInputEnvelope["payload"];
  if (!isRecord(ptyInputPayload)) throw new Error("pty input payload missing");
  expect(ptyInputPayload["type"]).toBe("send_pty_input");
  expect(ptyInputPayload["byte_count"]).toBe("3");
  expect("data_base64" in ptyInputPayload).toBe(false);

  for (const [index, type] of [
    [16, "resize_pty_session"],
    [17, "stop_pty_session"],
    [18, "start_pty_session"],
  ] as const) {
    const frame = raw[index];
    if (!isRecord(frame) || !isRecord(frame["request"])) throw new Error(`frame ${index} missing`);
    expect(frame["request"]["type"]).toBe(type);
    const envelope = frame["request"]["envelope"];
    if (!isRecord(envelope) || !isRecord(envelope["payload"]))
      throw new Error("control envelope missing");
    expect(envelope["payload"]["command"]).toBeUndefined();
  }

  await reemit("kernel-client-control.json", frames);
});

test("server control: refusals are values, cursors survive a disconnect", async () => {
  const raw = await golden("kernel-server-control.json");
  if (!Array.isArray(raw)) throw new Error("golden is not an array");
  const frames = raw as B.ServerControl_Serialize[];
  expect(frames.map((f) => f.type)).toEqual([
    "hello_ack",
    "hello_refusal",
    "response",
    "response",
    "response",
    "event_batch",
    "stream_closed",
    "response",
    "response",
    "pty_delta_batch",
    "pty_stream_closed",
    "response",
    "response",
    "response",
    "pty_raw_snapshot",
    "pty_raw_chunk",
    "pty_raw_resized",
    "pty_raw_stream_closed",
    "response",
    "pty_input",
    "pty_resize",
    "pty_stop",
    "pty_start",
    "pty_delivery_settled",
  ]);

  for (const frame of frames) {
    // Exhaustive narrow over every server frame kind.
    switch (frame.type) {
      case "hello_ack": {
        // The INTERSECTION: the client asked for three capabilities and was
        // granted one. A client must not assume either absent capability.
        expect(frame.capabilities).toEqual(["pty_raw", "pty_input", "pty_control", "pty_start"]);
        expect(frame.sealed).toBe(true);
        break;
      }
      case "hello_refusal": {
        const code: B.KernelErrorCode = frame.code;
        expect(code).toBe("unsupported_version");
        break;
      }
      case "response": {
        expect(typeof frame.request_id).toBe("string");
        break;
      }
      case "pty_input": {
        expect(frame.generation).toBe("pty-life-1");
        expect(frame.byte_size).toMatch(DECIMAL);
        expect(frame.data_base64).toBe("AP8K");
        break;
      }
      case "pty_resize": {
        expect([frame.cols, frame.rows]).toEqual([120, 40]);
        break;
      }
      case "pty_stop": {
        expect(frame.generation).toBe("pty-life-1");
        break;
      }
      case "pty_start": {
        expect(frame.template_name).toBe("review");
        expect(frame.session_id).toBe("review-7");
        break;
      }
      case "pty_delivery_settled": {
        expect(frame.delivery_id).toBe("pty_session:pty-1:2");
        break;
      }
      case "event_batch": {
        expect(frame.cursor).toMatch(DECIMAL);
        expect(frame.events).toHaveLength(1);
        break;
      }
      case "stream_closed": {
        expect(frame.code).toBe("slow_consumer");
        // The last delivered cursor rides the disconnect, so the consumer
        // resumes instead of replaying from the start.
        expect(frame.last_cursor).toMatch(DECIMAL);
        break;
      }
      case "pty_delta_batch": {
        expect(frame.generation).toBe("pty-life-1");
        expect(frame.seq).toMatch(DECIMAL);
        expect(frame.deltas.length).toBeGreaterThan(0);
        break;
      }
      case "pty_stream_closed": {
        expect(frame.generation).toBe("pty-life-1");
        expect(frame.code).toBe("slow_consumer");
        // The PTY analogue of `stream_closed.last_cursor`, on its own
        // sequence axis — a reattach resumes from here, not from `Seq`.
        expect(frame.last_seq).toMatch(DECIMAL);
        break;
      }
      case "pty_raw_snapshot": {
        expect(frame.generation).toBe("pty-life-1");
        expect(frame.byte_size).toMatch(DECIMAL);
        expect(frame.seq).toBeUndefined();
        break;
      }
      case "pty_raw_chunk": {
        expect(frame.generation).toBe("pty-life-1");
        expect(frame.byte_size).toMatch(DECIMAL);
        expect(frame.seq).toMatch(DECIMAL);
        break;
      }
      case "pty_raw_resized": {
        expect(frame.generation).toBe("pty-life-1");
        expect(frame.seq).toMatch(DECIMAL);
        break;
      }
      case "pty_raw_stream_closed": {
        expect(frame.generation).toBe("pty-life-1");
        expect(frame.code).toBe("slow_consumer");
        expect(frame.last_seq).toMatch(DECIMAL);
        break;
      }
      default: {
        const unreachable: never = frame;
        throw new Error(`unknown frame: ${JSON.stringify(unreachable)}`);
      }
    }
  }

  const sealed = raw[2];
  if (!isRecord(sealed)) throw new Error("frame 2 missing");
  const sealedResult = sealed["result"];
  if (!isRecord(sealedResult)) throw new Error("sealed result missing");
  expect(sealedResult["type"]).toBe("sealed_verification");
  expect(sealedResult["event_count"]).toBe("1");
  // The genesis sequence is DATABASE-assigned: certification must never
  // assume the numeral 1.
  expect(sealedResult["genesis_watermark"]).toMatch(DECIMAL);
  expect(sealedResult["genesis_watermark"]).not.toBe("1");

  // An error is an ordinary response, not an out-of-band condition.
  const refused = raw[3];
  if (!isRecord(refused)) throw new Error("frame 3 missing");
  const errorResult = refused["result"];
  if (!isRecord(errorResult)) throw new Error("error result missing");
  expect(errorResult["type"]).toBe("error");
  expect(errorResult["code"]).toBe("idempotency_conflict");

  const blob = raw[4];
  if (!isRecord(blob)) throw new Error("frame 4 missing");
  const blobResult = blob["result"];
  if (!isRecord(blobResult)) throw new Error("blob result missing");
  const descriptor = blobResult["descriptor"];
  if (!isRecord(descriptor)) throw new Error("descriptor missing");
  expect(descriptor["address"]).toMatch(/^sha256:[0-9a-f]{64}$/);
  expect(descriptor["byte_size"]).toMatch(DECIMAL);
  expect(blobResult["deduplicated"]).toBe(false);

  const attached = raw[7];
  if (!isRecord(attached)) throw new Error("frame 7 missing");
  const attachedResult = attached["result"];
  if (!isRecord(attachedResult)) throw new Error("pty attached result missing");
  expect(attachedResult["type"]).toBe("pty_attached");
  expect(attachedResult["generation"]).toBe("pty-life-1");
  expect(attachedResult["cursor"]).toMatch(DECIMAL);

  // The full styled frame: every color tier appears somewhere in the grid.
  const snapshot = raw[8];
  if (!isRecord(snapshot)) throw new Error("frame 8 missing");
  const snapshotResult = snapshot["result"];
  if (!isRecord(snapshotResult)) throw new Error("pty snapshot result missing");
  expect(snapshotResult["type"]).toBe("pty_snapshot");
  expect(snapshotResult["generation"]).toBe("pty-life-1");
  expect(snapshotResult["seq"]).toMatch(DECIMAL);
  const frame = snapshotResult["frame"];
  if (!isRecord(frame)) throw new Error("pty frame missing");
  const styles = frame["styles"];
  const rows = frame["rows"];
  const cursor = frame["cursor"];
  if (!Array.isArray(styles) || !Array.isArray(rows)) {
    throw new Error("interned frame halves missing");
  }
  if (!isRecord(cursor)) throw new Error("pty cursor missing");
  expect(cursor["row"]).toBe(1);
  expect(cursor["col"]).toBe(1);
  // Every run names its style by index into the frame's own table — no index
  // space outlives the message.
  for (const row of rows) {
    if (!Array.isArray(row)) throw new Error("row is not a run list");
    for (const run of row) {
      if (!isRecord(run)) throw new Error("run missing");
      expect(typeof run["style"]).toBe("number");
      expect((run["style"] as number) < styles.length).toBe(true);
    }
  }
  const frameJson = JSON.stringify(frame);
  for (const tier of ["ansi16", "xterm256", "truecolor"]) {
    expect(frameJson).toContain(`"type":"${tier}"`);
  }

  // The publish acknowledgements: one for either publish, one for retire.
  const published = raw[11];
  if (!isRecord(published)) throw new Error("frame 11 missing");
  const publishedResult = published["result"];
  if (!isRecord(publishedResult)) throw new Error("pty published result missing");
  expect(publishedResult["type"]).toBe("pty_published");

  const retired = raw[12];
  if (!isRecord(retired)) throw new Error("frame 12 missing");
  const retiredResult = retired["result"];
  if (!isRecord(retiredResult)) throw new Error("pty retired result missing");
  expect(retiredResult["type"]).toBe("pty_retired");

  const rawAttached = raw[13];
  if (!isRecord(rawAttached)) throw new Error("frame 13 missing");
  const rawAttachedResult = rawAttached["result"];
  if (!isRecord(rawAttachedResult)) throw new Error("raw attached result missing");
  expect(rawAttachedResult["type"]).toBe("pty_raw_attached");
  expect("cursor" in rawAttachedResult).toBe(false);

  const rawSnapshot = raw[14];
  if (!isRecord(rawSnapshot)) throw new Error("frame 14 missing");
  expect(rawSnapshot["type"]).toBe("pty_raw_snapshot");
  expect(rawSnapshot["byte_size"]).toMatch(DECIMAL);
  expect("seq" in rawSnapshot).toBe(false);

  const rawChunk = raw[15];
  if (!isRecord(rawChunk)) throw new Error("frame 15 missing");
  expect(rawChunk["type"]).toBe("pty_raw_chunk");
  expect(rawChunk["seq"]).toMatch(DECIMAL);

  const rawClosed = raw[17];
  if (!isRecord(rawClosed)) throw new Error("frame 17 missing");
  expect(rawClosed["type"]).toBe("pty_raw_stream_closed");
  expect(rawClosed["code"]).toBe("slow_consumer");

  const ptyInput = raw[19];
  if (!isRecord(ptyInput)) throw new Error("frame 19 missing");
  expect(ptyInput["type"]).toBe("pty_input");
  expect(ptyInput["command_id"]).toBe("input-1");
  expect(ptyInput["byte_size"]).toBe("3");
  expect(ptyInput["data_base64"]).toBe("AP8K");

  await reemit("kernel-server-control.json", frames);
});

test("raw PTY wire goldens pair each JSON header with one byte-exact kind-0x02 frame", async () => {
  const raw = await golden("pty-raw-wire.json");
  if (!isRecord(raw)) throw new Error("raw wire golden is not an object");

  for (const direction of ["publish", "delivery"] as const) {
    const pair = raw[direction];
    if (!isRecord(pair) || typeof pair["wire_hex"] !== "string") {
      throw new Error(`${direction} raw wire pair missing`);
    }
    const wire = bytesFromHex(pair["wire_hex"]);
    const headerFrame = readWireFrame(wire, 0);
    const payloadFrame = readWireFrame(wire, headerFrame.next);
    expect(headerFrame.kind).toBe(0x01);
    expect(payloadFrame.kind).toBe(0x02);
    expect(payloadFrame.next).toBe(wire.length);

    const header = JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(headerFrame.body));
    if (direction === "publish") {
      const typed = header as B.ClientControl_Serialize;
      expect(typed.type).toBe("pty_raw_publish_output");
    } else {
      const typed = header as B.ServerControl_Serialize;
      expect(typed.type).toBe("pty_raw_chunk");
    }
    if (!isRecord(header)) throw new Error(`${direction} header is not an object`);
    expect(header["byte_size"]).toBe(String(payloadFrame.body.length));
    expect(Array.from(payloadFrame.body)).toEqual([0x00, 0xff, 0x1b, 0x5b, 0xc3]);
  }

  await reemit("pty-raw-wire.json", raw);
});

test("kernel checkpoint: decimal-string sequence, lowercase digest", async () => {
  const raw = await golden("kernel-checkpoint.json");
  if (!isRecord(raw)) throw new Error("golden is not an object");
  const checkpoint = raw as B.Checkpoint_Serialize;
  expect(checkpoint.schema_version).toBe(1);
  expect(checkpoint.through_sequence).toMatch(DECIMAL);
  expect(BigInt(checkpoint.through_sequence) > 2n ** 53n).toBe(true);
  expect(checkpoint.projection_hash).toMatch(/^[0-9a-f]{64}$/);
  expect(checkpoint.records_ref.byte_size).toMatch(DECIMAL);
  await reemit("kernel-checkpoint.json", checkpoint);
});

test("signal theme: 15 tokens, crate order, hex values", async () => {
  const raw = JSON.parse(await Bun.file(`${import.meta.dir}/signal-theme.json`).text());
  if (!Array.isArray(raw)) throw new Error("theme is not an array");
  const tokens = raw as B.Token[];
  expect(tokens).toHaveLength(15);
  expect(tokens[0]).toEqual({
    name: "bg",
    value: "#070B10",
    role: "canvas background",
    index256: 232,
    tier16: "NotAColor",
  });
  for (const token of tokens) {
    expect(token.value).toMatch(/^#[0-9A-F]{6}$/i);
    expect(token.name).toMatch(/^[a-z0-9_]+$/);
  }
});
