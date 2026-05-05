import * as v from "valibot";

/// Schemas for the Claude Code stream-JSON envelopes the StreamJsonView
/// renderer in `AgentPanel.tsx` actually understands. Every other line
/// (non-JSON, or a JSON envelope whose `type` we have no opinion on)
/// passes through as `{ type: "raw", raw: string }` so the renderer can
/// fall back to a plain-text rendering instead of pretending a partial
/// shape is present.

const TextBlock = v.object({
  type: v.literal("text"),
  text: v.string(),
});

const ToolUseBlock = v.object({
  type: v.literal("tool_use"),
  name: v.string(),
});

/// Catch-all block: Claude ships a growing set of content block types
/// (`thinking`, `image`, `tool_result`, …) that the renderer doesn't
/// surface. Accept them as `{ type: string }` so the validator doesn't
/// reject the whole envelope when one shows up.
const OtherBlock = v.object({
  type: v.string(),
});

const ContentBlockSchema = v.union([TextBlock, ToolUseBlock, OtherBlock]);

const AssistantEnvelope = v.object({
  type: v.literal("assistant"),
  message: v.object({
    content: v.array(ContentBlockSchema),
  }),
});

const ResultEnvelope = v.object({
  type: v.literal("result"),
  result: v.optional(v.string()),
  duration_ms: v.optional(v.number()),
  num_turns: v.optional(v.number()),
  total_cost_usd: v.optional(v.number()),
});

/// Three envelopes the renderer explicitly skips. They're listed here
/// so a malformed `system` envelope still validates as `system` rather
/// than falling through to the raw branch — the test catalog
/// exercises this.
const SystemEnvelope = v.object({ type: v.literal("system") });
const RateLimitEnvelope = v.object({ type: v.literal("rate_limit_event") });
const UserEnvelope = v.object({ type: v.literal("user") });

const KnownEnvelopeSchema = v.variant("type", [
  AssistantEnvelope,
  ResultEnvelope,
  SystemEnvelope,
  RateLimitEnvelope,
  UserEnvelope,
]);

export type StreamJsonEnvelope = v.InferOutput<typeof KnownEnvelopeSchema>;

export type ParsedStreamJsonLine =
  | StreamJsonEnvelope
  | { type: "raw"; raw: string };

/// Best-effort parse of one stream-JSON content line. Never throws:
/// non-JSON, or a JSON value that doesn't fit any known envelope,
/// returns `{ type: "raw", raw: content }` so the renderer can decide
/// whether to surface or skip it.
export function parseStreamJsonLine(content: string): ParsedStreamJsonLine {
  let value: unknown;
  try {
    value = JSON.parse(content);
  } catch {
    return { type: "raw", raw: content };
  }
  const result = v.safeParse(KnownEnvelopeSchema, value);
  if (result.success) return result.output;
  return { type: "raw", raw: content };
}

/// Verdict embedded inside a Check-Plan agent's final `result` envelope.
/// The agent emits a `{ "status": "...", "reason": "..." }` object as
/// part of a longer free-form `result` string; the StreamJsonView
/// regex-matches the JSON object and feeds it here to surface the
/// banner. `reason` is optional because some early Check-Plan prompts
/// emit only `status`.
export const VerdictSchema = v.object({
  status: v.string(),
  reason: v.optional(v.string()),
});

export type Verdict = v.InferOutput<typeof VerdictSchema>;

export function parseVerdictJson(raw: string): Verdict | null {
  let value: unknown;
  try {
    value = JSON.parse(raw);
  } catch {
    return null;
  }
  const result = v.safeParse(VerdictSchema, value);
  return result.success ? result.output : null;
}
