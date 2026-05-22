import { useEffect, useId, useRef, useState } from "react";
import { HttpError } from "../api.js";
import {
  useCredentialsStore,
  type CreateCredentialBody,
  type CreateCredentialResponse,
  type CredentialCreateKind,
} from "../stores/credentials-store.js";
import { Button } from "./ui/Button.js";
import { Modal } from "./ui/Modal.js";
import { Banner } from "./ui/Banner.js";
import { errorMessage } from "../lib/error.js";

interface AddCredentialModalProps {
  onClose: () => void;
}

/// The two "shapes" the modal supports — distinct from the server's
/// `kind` because `paste` is a UX bucket that covers all three
/// caller-supplies-secret kinds (ssh_key / gh_pat / gitlab_pat).
type ModalMode = "paste" | "generate";

/// `kind` choices the operator can pick from inside the `paste` mode.
const PASTE_KIND_OPTIONS: { value: Exclude<CredentialCreateKind, "generate">; label: string }[] = [
  { value: "ssh_key", label: "SSH private key" },
  { value: "gh_pat", label: "GitHub personal access token" },
  { value: "gitlab_pat", label: "GitLab personal access token" },
];

/// `AddCredentialModal` drives both server-supported create flows:
///
/// 1. `paste`: operator supplies the secret body verbatim. Server
///    seals it with the master key. Public-half is optional (only
///    really applicable for `ssh_key` rows; PATs use it as a
///    fingerprint/label hint).
/// 2. `generate`: server mints a fresh ed25519 keypair, returns the
///    public half + a deep link to `github.com/settings/ssh/new`.
///    The private half is sealed server-side and NEVER reaches the
///    wire — even on this single response.
///
/// The modal stays open after a successful `generate` until the
/// operator clicks Done — gives them time to copy the public half
/// and paste it into GitHub (this is the acceptance criterion).
export function AddCredentialModal({ onClose }: AddCredentialModalProps) {
  const createCredential = useCredentialsStore((s) => s.createCredential);

  const [mode, setMode] = useState<ModalMode>("paste");
  const [pasteKind, setPasteKind] = useState<Exclude<CredentialCreateKind, "generate">>("ssh_key");
  const [name, setName] = useState("");
  const [secret, setSecret] = useState("");
  const [publicPart, setPublicPart] = useState("");
  const [hostHint, setHostHint] = useState("");
  const [scopes, setScopes] = useState("");
  const [submitting, setSubmitting] = useState(false);
  const [submitError, setSubmitError] = useState<string | null>(null);
  /// Set after a successful `kind=generate` response so the modal can
  /// switch into the "copy your public key now" confirmation view.
  /// Null otherwise.
  const [generated, setGenerated] = useState<CreateCredentialResponse | null>(null);

  const nameInputRef = useRef<HTMLInputElement>(null);

  // Default host hint to github.com for the common case.
  useEffect(() => {
    if (!hostHint && (mode === "generate" || pasteKind === "ssh_key" || pasteKind === "gh_pat")) {
      setHostHint("github.com");
    } else if (mode === "paste" && pasteKind === "gitlab_pat" && hostHint === "github.com") {
      setHostHint("gitlab.com");
    }
    // Effect deliberately only runs when mode/pasteKind change — once
    // the user has typed anything custom into hostHint we leave it.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [mode, pasteKind]);

  async function handleSubmit() {
    setSubmitError(null);
    if (!name.trim()) {
      setSubmitError("Name is required.");
      return;
    }
    if (mode === "paste" && !secret.trim()) {
      setSubmitError("Secret is required for this credential kind.");
      return;
    }
    setSubmitting(true);
    try {
      const body: CreateCredentialBody =
        mode === "generate"
          ? {
              name: name.trim(),
              kind: "generate",
              hostHint: hostHint.trim() || undefined,
              scopes: scopes.trim() || undefined,
            }
          : {
              name: name.trim(),
              kind: pasteKind,
              secret,
              publicPart: publicPart.trim() || undefined,
              hostHint: hostHint.trim() || undefined,
              scopes: scopes.trim() || undefined,
            };
      const res = await createCredential(body);
      if (res.generated) {
        // Stay open so the operator can copy + paste into GitHub.
        setGenerated(res);
      } else {
        onClose();
      }
    } catch (e) {
      if (e instanceof HttpError) {
        const body = e.body as { error?: string; message?: string } | undefined;
        setSubmitError(body?.message ?? body?.error ?? errorMessage(e));
      } else {
        setSubmitError(errorMessage(e));
      }
    } finally {
      setSubmitting(false);
    }
  }

  // After successful generate, the modal collapses to a copy-public-key view.
  if (generated) {
    return <GeneratedKeyConfirmation generated={generated} onClose={onClose} />;
  }

  return (
    <Modal
      open={true}
      onClose={() => (submitting ? undefined : onClose())}
      title="Add credential"
      description="Branchwork seals the private half with the master key. Public halves are safe to share."
      initialFocusRef={nameInputRef}
      size="2xl"
    >
      <div className="mt-4 space-y-4">
        {/* Mode picker */}
        <fieldset className="space-y-2" aria-label="Credential creation mode">
          <legend className="sr-only">How to add this credential</legend>
          <ModeRadio
            value="paste"
            checked={mode === "paste"}
            onChange={() => setMode("paste")}
            label="I have a private key"
            description="Paste a PEM-encoded private key or a personal access token. The server seals it at rest."
          />
          <ModeRadio
            value="generate"
            checked={mode === "generate"}
            onChange={() => setMode("generate")}
            label="Generate a new ed25519 keypair"
            description="Server mints a fresh keypair and returns the public half so you can paste it into GitHub. The private half never leaves the server."
          />
        </fieldset>

        <hr className="border-gray-800" />

        {/* Common fields */}
        <Field id="cred-name" label="Name">
          <input
            id="cred-name"
            ref={nameInputRef}
            type="text"
            value={name}
            onChange={(e) => setName(e.target.value)}
            placeholder="e.g. work-laptop key"
            className={inputCls}
            required
            disabled={submitting}
            data-testid="credential-name"
          />
        </Field>

        {mode === "paste" && (
          <>
            <Field id="cred-kind" label="Kind">
              <select
                id="cred-kind"
                value={pasteKind}
                onChange={(e) =>
                  setPasteKind(e.target.value as Exclude<CredentialCreateKind, "generate">)
                }
                className={inputCls}
                disabled={submitting}
                data-testid="credential-kind"
              >
                {PASTE_KIND_OPTIONS.map((o) => (
                  <option key={o.value} value={o.value}>
                    {o.label}
                  </option>
                ))}
              </select>
            </Field>

            <Field
              id="cred-secret"
              label="Secret"
              hint={
                pasteKind === "ssh_key"
                  ? "PEM body (e.g. -----BEGIN OPENSSH PRIVATE KEY-----…)."
                  : "Token string (ghp_… for GitHub, glpat-… for GitLab)."
              }
            >
              <textarea
                id="cred-secret"
                value={secret}
                onChange={(e) => setSecret(e.target.value)}
                rows={pasteKind === "ssh_key" ? 8 : 2}
                className={`${inputCls} font-mono text-xs`}
                placeholder={
                  pasteKind === "ssh_key"
                    ? "-----BEGIN OPENSSH PRIVATE KEY-----\n…\n-----END OPENSSH PRIVATE KEY-----"
                    : "ghp_…"
                }
                disabled={submitting}
                required
                data-testid="credential-secret"
              />
            </Field>

            {pasteKind === "ssh_key" && (
              <Field
                id="cred-public"
                label="Public key (optional)"
                hint="If you have the matching ssh-ed25519 / ssh-rsa one-liner, paste it here so the dashboard can display it."
              >
                <textarea
                  id="cred-public"
                  value={publicPart}
                  onChange={(e) => setPublicPart(e.target.value)}
                  rows={2}
                  className={`${inputCls} font-mono text-xs`}
                  placeholder="ssh-ed25519 AAAA… user@host"
                  disabled={submitting}
                  data-testid="credential-public-part"
                />
              </Field>
            )}
          </>
        )}

        <Field
          id="cred-host"
          label="Host hint (optional)"
          hint="Free-form label like github.com or gitlab.acme.io. Used by the credentials list to disambiguate rows."
        >
          <input
            id="cred-host"
            type="text"
            value={hostHint}
            onChange={(e) => setHostHint(e.target.value)}
            placeholder="github.com"
            className={inputCls}
            disabled={submitting}
            data-testid="credential-host-hint"
          />
        </Field>

        <Field
          id="cred-scopes"
          label="Scopes (optional)"
          hint="Comma-separated tags, e.g. repo,workflow."
        >
          <input
            id="cred-scopes"
            type="text"
            value={scopes}
            onChange={(e) => setScopes(e.target.value)}
            placeholder="repo, workflow"
            className={inputCls}
            disabled={submitting}
            data-testid="credential-scopes"
          />
        </Field>

        {submitError && <Banner kind="error">{submitError}</Banner>}

        <div className="flex justify-end gap-2 pt-2">
          <Button variant="secondary" size="sm" onClick={onClose} disabled={submitting}>
            Cancel
          </Button>
          <Button
            variant="primary"
            size="sm"
            onClick={handleSubmit}
            loading={submitting}
            data-testid="credential-submit"
          >
            {mode === "generate" ? "Generate keypair" : "Add credential"}
          </Button>
        </div>
      </div>
    </Modal>
  );
}

interface GeneratedKeyConfirmationProps {
  generated: CreateCredentialResponse;
  onClose: () => void;
}

/// Post-generate view: the public half is shown once with a copy
/// button + deep link to `github.com/settings/ssh/new`. We do NOT
/// have the private half in this response (the server intentionally
/// only emits the public half on `kind=generate`), so dismiss is
/// a no-data-loss action.
function GeneratedKeyConfirmation({ generated, onClose }: GeneratedKeyConfirmationProps) {
  const [copied, setCopied] = useState(false);

  async function handleCopy() {
    if (!generated.publicPart) return;
    try {
      await navigator.clipboard.writeText(generated.publicPart);
      setCopied(true);
      window.setTimeout(() => setCopied(false), 2000);
    } catch {
      // Fallback: select the textarea contents so the operator can
      // ctrl/cmd-C manually. Clipboard API rejection is normal in some
      // headless test envs and over insecure HTTP, so we should never
      // crash the modal.
      const el = document.getElementById("generated-public-key") as HTMLTextAreaElement | null;
      el?.select();
    }
  }

  // Hint to the operator about pasting into GitHub. We deep-link to
  // the SSH-key add page rather than the PAT page because every
  // `kind=generate` row is an ed25519 SSH key.
  const githubUrl = "https://github.com/settings/ssh/new";

  return (
    <Modal
      open={true}
      onClose={onClose}
      title="Keypair generated"
      description="Copy the public half and paste it into your git host now. The private half stays sealed on the server."
      size="2xl"
    >
      <div className="mt-4 space-y-4">
        <Banner kind="warn">
          <strong>Copy this public key now.</strong> The server will not show it again, though you
          can always view it in the credentials list (the private half never leaves the server).
        </Banner>

        <Field id="generated-public-key" label="Public key">
          <textarea
            id="generated-public-key"
            readOnly
            value={generated.publicPart ?? ""}
            rows={3}
            className={`${inputCls} font-mono text-xs`}
            data-testid="generated-public-key"
          />
        </Field>

        <div className="flex flex-wrap items-center gap-2">
          <Button variant="primary" size="sm" onClick={handleCopy} data-testid="copy-public-key">
            {copied ? "Copied!" : "Copy public key"}
          </Button>
          <a
            href={githubUrl}
            target="_blank"
            rel="noreferrer noopener"
            className="inline-flex items-center rounded border border-indigo-700/50 bg-indigo-900/30 px-3 py-1.5 text-xs text-indigo-200 hover:bg-indigo-900/50 transition"
            data-testid="github-deep-link"
          >
            Open GitHub SSH settings ↗
          </a>
        </div>

        <p className="text-xs text-gray-500">
          On GitHub, give the key any title and paste the contents above into the "Key" field. Make
          sure "Authentication Key" is selected if you want this credential to work for `git clone`.
          After saving on GitHub, click Done below to dismiss this dialog.
        </p>

        <div className="flex justify-end pt-2">
          <Button variant="primary" size="sm" onClick={onClose} data-testid="generated-done">
            Done
          </Button>
        </div>
      </div>
    </Modal>
  );
}

interface ModeRadioProps {
  value: string;
  checked: boolean;
  onChange: () => void;
  label: string;
  description: string;
}

function ModeRadio({ value, checked, onChange, label, description }: ModeRadioProps) {
  return (
    <label
      className={`block rounded border px-3 py-2 cursor-pointer transition ${
        checked
          ? "border-indigo-600 bg-indigo-900/30"
          : "border-gray-700 bg-gray-900/40 hover:border-gray-600"
      }`}
    >
      <div className="flex items-start gap-2">
        <input
          type="radio"
          name="credential-mode"
          value={value}
          checked={checked}
          onChange={onChange}
          className="mt-0.5"
          data-testid={`mode-${value}`}
        />
        <div className="min-w-0">
          <div className="text-sm font-medium text-gray-100">{label}</div>
          <div className="text-xs text-gray-400 mt-0.5">{description}</div>
        </div>
      </div>
    </label>
  );
}

interface FieldProps {
  id: string;
  label: string;
  hint?: string;
  children: React.ReactNode;
}

function Field({ id, label, hint, children }: FieldProps) {
  const hintId = useId();
  return (
    <div>
      <label htmlFor={id} className="block text-xs font-medium text-gray-200 mb-1">
        {label}
      </label>
      {children}
      {hint && (
        <p id={hintId} className="text-[11px] text-gray-500 mt-1">
          {hint}
        </p>
      )}
    </div>
  );
}

const inputCls =
  "w-full px-2 py-1.5 text-sm bg-gray-800 border border-gray-700 rounded text-gray-100 placeholder-gray-600 outline-none focus:border-indigo-600 transition disabled:opacity-50";
