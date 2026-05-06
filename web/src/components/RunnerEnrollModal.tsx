import { useEffect, useRef, useState } from "react";
import { Modal } from "./ui/Modal.js";
import { Button } from "./ui/Button.js";
import { Banner } from "./ui/Banner.js";
import { useRunnerStore } from "../stores/runner-store.js";
import { errorMessage } from "../lib/error.js";

interface Props {
  open: boolean;
  onClose: () => void;
}

interface Issued {
  token: string;
  runner_name: string;
}

/// Build the install command shown to the operator. The token is the
/// only secret material — the runner binary defaults `--saas-url` to the
/// dashboard origin and `--cwd` to the operator's home, so name+token
/// is all the modal needs to surface. Single-quoting the token is
/// belt-and-braces (32-byte hex never needs quoting today, but a future
/// token format with shell metacharacters wouldn't break the line).
export function buildInstallCommand(token: string): string {
  return `branchwork-runner --token '${token}'`;
}

/// Modal-driven enrolment flow for `/runners`. Two phases:
///
/// 1. Name input + Submit. POSTs `/api/runners/tokens`, surfaces server
///    errors inline (Banner) — empty `runner_name` is rejected
///    server-side as a 400.
/// 2. Issued state. Shows the token verbatim plus the
///    `branchwork-runner --token <…>` command, both inside read-only
///    inputs with Copy buttons. The token is shown exactly once: the
///    server only persists its SHA-256, so closing the modal without
///    copying is a permanent loss. The "Done" button closes; reopening
///    starts a fresh phase 1.
export function RunnerEnrollModal({ open, onClose }: Props) {
  const createRunnerToken = useRunnerStore((s) => s.createRunnerToken);
  const [name, setName] = useState("");
  const [busy, setBusy] = useState(false);
  const [issued, setIssued] = useState<Issued | null>(null);
  const [error, setError] = useState<string | null>(null);
  const nameInputRef = useRef<HTMLInputElement>(null);

  // Reset state every time the modal opens — leaving stale issued
  // tokens around between opens would let an operator copy yesterday's
  // token without realising it was already revoked / replaced.
  useEffect(() => {
    if (open) {
      setName("");
      setBusy(false);
      setIssued(null);
      setError(null);
    }
  }, [open]);

  async function handleSubmit() {
    if (!name.trim()) {
      setError("Runner name is required.");
      return;
    }
    setBusy(true);
    setError(null);
    try {
      const res = await createRunnerToken(name.trim());
      setIssued(res);
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy(false);
    }
  }

  function handleKey(e: React.KeyboardEvent) {
    if (e.key === "Enter" && !issued) {
      e.preventDefault();
      void handleSubmit();
    }
  }

  return (
    <Modal
      open={open}
      onClose={onClose}
      title={issued ? "Runner enrolled" : "Enrol a runner"}
      description={
        issued
          ? "Copy the install command and run it on the host that should serve as your runner. The token is shown once — closing this dialog discards it."
          : "Pick a short, recognisable name (e.g. laptop, ci-runner). The dashboard uses it in the runner list and audit log."
      }
      initialFocusRef={nameInputRef}
    >
      {!issued ? (
        <div className="mt-4 space-y-3">
          <label className="block text-xs text-gray-400">
            Runner name
            <input
              ref={nameInputRef}
              type="text"
              value={name}
              onChange={(e) => setName(e.target.value)}
              onKeyDown={handleKey}
              disabled={busy}
              placeholder="laptop"
              data-testid="runner-name-input"
              className="mt-1 w-full bg-gray-800 border border-gray-700 rounded px-3 py-2 text-sm text-gray-200 placeholder:text-gray-600 focus:outline-none focus:border-indigo-600 disabled:opacity-60"
            />
          </label>
          {error && <Banner>{error}</Banner>}
          <div className="flex justify-end gap-2 pt-2">
            <Button variant="ghost" size="sm" onClick={onClose} disabled={busy}>
              Cancel
            </Button>
            <Button
              variant="primary"
              size="sm"
              onClick={handleSubmit}
              disabled={busy || !name.trim()}
              loading={busy}
            >
              {busy ? "Issuing…" : "Issue token"}
            </Button>
          </div>
        </div>
      ) : (
        <IssuedView issued={issued} onClose={onClose} />
      )}
    </Modal>
  );
}

function IssuedView({
  issued,
  onClose,
}: {
  issued: Issued;
  onClose: () => void;
}) {
  const command = buildInstallCommand(issued.token);
  return (
    <div className="mt-4 space-y-3">
      <div>
        <label className="block text-xs text-gray-400 mb-1">Token</label>
        <CopyableField value={issued.token} testId="enroll-token" />
      </div>
      <div>
        <label className="block text-xs text-gray-400 mb-1">
          Install command
        </label>
        <CopyableField value={command} testId="enroll-command" multiline />
        <p className="mt-1 text-[11px] text-gray-500">
          Run this on the host you want to enrol. The runner registers via
          WebSocket; once it&rsquo;s online the row in the list flips emerald.
        </p>
      </div>
      <Banner kind="warn">
        This is the only time the dashboard will show this token. Save it now
        if you need to re-run the install command later.
      </Banner>
      <div className="flex justify-end pt-2">
        <Button variant="primary" size="sm" onClick={onClose}>
          Done
        </Button>
      </div>
    </div>
  );
}

function CopyableField({
  value,
  testId,
  multiline,
}: {
  value: string;
  testId: string;
  multiline?: boolean;
}) {
  const [copied, setCopied] = useState(false);
  async function handleCopy() {
    try {
      await navigator.clipboard.writeText(value);
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    } catch {
      // Best-effort — some browsers / contexts (file://, insecure
      // origins) deny clipboard access. The value is still visible
      // and selectable in the field, so the operator can fall back
      // to manual copy.
    }
  }
  return (
    <div className="flex gap-2">
      {multiline ? (
        <textarea
          readOnly
          value={value}
          rows={2}
          data-testid={testId}
          onFocus={(e) => e.currentTarget.select()}
          className="flex-1 bg-gray-800 border border-gray-700 rounded px-2 py-1.5 text-xs font-mono text-gray-200 resize-none focus:outline-none focus:border-indigo-600"
        />
      ) : (
        <input
          readOnly
          type="text"
          value={value}
          data-testid={testId}
          onFocus={(e) => e.currentTarget.select()}
          className="flex-1 bg-gray-800 border border-gray-700 rounded px-2 py-1.5 text-xs font-mono text-gray-200 focus:outline-none focus:border-indigo-600"
        />
      )}
      <Button
        variant="secondary"
        size="sm"
        onClick={handleCopy}
        aria-label={`Copy ${testId.replace("enroll-", "")}`}
      >
        {copied ? "Copied" : "Copy"}
      </Button>
    </div>
  );
}
