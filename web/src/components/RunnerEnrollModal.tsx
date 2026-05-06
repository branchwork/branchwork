import { useEffect, useRef, useState } from "react";
import { Modal } from "./ui/Modal.js";
import { Button } from "./ui/Button.js";
import { Banner } from "./ui/Banner.js";
import {
  useRunnerStore,
  type RunnerInstallCommand,
} from "../stores/runner-store.js";
import { errorMessage } from "../lib/error.js";

interface Props {
  open: boolean;
  onClose: () => void;
}

/// Build the legacy single-binary install command. Kept exported because
/// existing callers + tests still rely on it for the manual fallback path
/// (e.g. operators who want to skip `curl|sh` and run the binary they
/// already have). Token is single-quoted so a future format with shell
/// metacharacters wouldn't break the line.
export function buildInstallCommand(token: string): string {
  return `branchwork-runner --token '${token}'`;
}

/// Modal-driven enrolment flow for `/runners`. Three phases:
///
/// 1. Name input + Submit. POSTs `/api/runners/install-command`,
///    surfaces server errors inline (Banner) — empty `runner_name` is
///    rejected server-side as a 400.
/// 2. Issued state. Shows the one-line `curl … | sh -s -- '<TOKEN>'`
///    install command in a code block with a one-click copy button.
///    Subscribes to `runner_connected` WS events keyed by the runner
///    name and flips to phase 3 when one arrives. The token is shown
///    exactly once: the server only persists its SHA-256, so closing
///    the modal without copying is a permanent loss.
/// 3. Connected state. Replaces the install copy with a "Connected!"
///    success message + a Done button.
///
/// The listener attaches at issue time and detaches on unmount or
/// modal close — re-opening after closing starts a fresh phase 1.
export function RunnerEnrollModal({ open, onClose }: Props) {
  const fetchInstallCommand = useRunnerStore((s) => s.fetchInstallCommand);
  const runners = useRunnerStore((s) => s.runners);
  const [name, setName] = useState("");
  const [busy, setBusy] = useState(false);
  const [issued, setIssued] = useState<RunnerInstallCommand | null>(null);
  const [connected, setConnected] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const nameInputRef = useRef<HTMLInputElement>(null);

  // Reset state every time the modal opens — leaving a stale issued
  // token / connected flag around between opens would let an operator
  // copy yesterday's command without realising it was already revoked
  // or already tied to a runner.
  useEffect(() => {
    if (open) {
      setName("");
      setBusy(false);
      setIssued(null);
      setConnected(false);
      setError(null);
    }
  }, [open]);

  // Pivot to "Connected!" the moment a `runner_connected` WS event
  // (delivered to `useRunnerStore.applyConnected`, which inserts/updates
  // a row in `runners`) shows a row whose `name` matches the runner the
  // operator just enrolled. We don't have the runner_id at issue time —
  // the runner generates it locally — so name-matching is the only
  // contract available. The store already de-duplicates runner_name
  // collisions per org, so a stale row from a previous enrolment cannot
  // mask the new one.
  useEffect(() => {
    if (!issued || connected) return;
    const target = issued.runner_name;
    const match = runners.find(
      (r) => r.name === target && r.status === "online",
    );
    if (match) setConnected(true);
  }, [issued, connected, runners]);

  async function handleSubmit() {
    if (!name.trim()) {
      setError("Runner name is required.");
      return;
    }
    setBusy(true);
    setError(null);
    try {
      const res = await fetchInstallCommand(name.trim());
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
      title={
        connected
          ? "Runner connected"
          : issued
            ? "Install runner"
            : "Enrol a runner"
      }
      description={
        connected
          ? `${issued?.runner_name ?? "The runner"} is online and ready to take work.`
          : issued
            ? "Run the command below on the host you want to enrol. Once the runner connects, this dialog updates automatically."
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
              {busy ? "Issuing…" : "Issue command"}
            </Button>
          </div>
        </div>
      ) : connected ? (
        <ConnectedView issued={issued} onClose={onClose} />
      ) : (
        <InstallView issued={issued} onClose={onClose} />
      )}
    </Modal>
  );
}

function InstallView({
  issued,
  onClose,
}: {
  issued: RunnerInstallCommand;
  onClose: () => void;
}) {
  return (
    <div className="mt-4 space-y-3">
      <div>
        <label className="block text-xs text-gray-400 mb-1">
          Install command
        </label>
        <CopyableField value={issued.command} testId="enroll-command" multiline />
        <p className="mt-1 text-[11px] text-gray-500">
          Paste this into a terminal on the host you want to enrol. The
          script downloads <code>branchwork-runner</code> and starts it
          in the background.
        </p>
      </div>
      <div>
        <label className="block text-xs text-gray-400 mb-1">
          Token (raw)
        </label>
        <CopyableField value={issued.token} testId="enroll-token" />
        <p className="mt-1 text-[11px] text-gray-500">
          For manual installs (e.g. you already have the binary), run{" "}
          <code className="text-gray-400">
            branchwork-runner --token &lsquo;{issued.token}&rsquo; --saas-url{" "}
            {issued.saas_url}
          </code>
          .
        </p>
      </div>
      <Banner kind="warn">
        This is the only time the dashboard will show this token. It is
        single-use: after the first runner connects, the token is bound
        to that runner — re-pasting on another host will fail.
      </Banner>
      <div
        className="flex items-center gap-2 text-[11px] text-gray-500"
        data-testid="enroll-waiting"
      >
        <span className="inline-block w-2 h-2 rounded-full bg-amber-500 animate-pulse" />
        Waiting for the runner to connect…
      </div>
      <div className="flex justify-end pt-2">
        <Button variant="ghost" size="sm" onClick={onClose}>
          Close
        </Button>
      </div>
    </div>
  );
}

function ConnectedView({
  issued,
  onClose,
}: {
  issued: RunnerInstallCommand;
  onClose: () => void;
}) {
  // The shared `Banner` only ships `error`/`warn` kinds today; rather
  // than widen its surface for one site we render an inline success
  // panel with the same role="status" semantics screen readers expect.
  return (
    <div className="mt-4 space-y-3" data-testid="enroll-connected">
      <div
        role="status"
        className="rounded border px-3 py-2 text-xs border-emerald-700/50 bg-emerald-900/30 text-emerald-200"
      >
        <span className="font-mono">{issued.runner_name}</span> is online.
        The dashboard can now dispatch agents to it.
      </div>
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
