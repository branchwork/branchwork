import { useCallback, useEffect, useState } from "react";
import { deleteJson, fetchJson, postJson, putJson } from "../../api.js";
import { errorMessage } from "../../lib/error.js";
import { toastError } from "../../lib/toast.js";
import { Banner } from "../ui/Banner.js";
import { Button } from "../ui/Button.js";

/// `SsoProviderDto` from auth/sso.rs:67. The server intentionally
/// strips client_secret + idp_certificate from this DTO; the UI only
/// asks for those fields when creating or updating, never when reading
/// back.
interface SsoProviderDto {
  id: string;
  orgId: string;
  protocol: "oidc" | "saml" | string;
  name: string;
  enabled: boolean;
  emailDomains: string | null;
  issuerUrl: string | null;
  clientId: string | null;
  idpEntityId: string | null;
  idpSsoUrl: string | null;
  spEntityId: string | null;
  groupsClaim: string | null;
  groupRoleMapping: unknown | null;
  createdAt: string;
  updatedAt: string;
}

interface SsoTabProps {
  orgSlug: string;
}

interface NewProviderForm {
  protocol: "oidc" | "saml";
  name: string;
  emailDomains: string;
  // OIDC
  issuerUrl: string;
  clientId: string;
  clientSecret: string;
  // SAML
  idpEntityId: string;
  idpSsoUrl: string;
  idpCertificate: string;
}

const EMPTY_FORM: NewProviderForm = {
  protocol: "oidc",
  name: "",
  emailDomains: "",
  issuerUrl: "",
  clientId: "",
  clientSecret: "",
  idpEntityId: "",
  idpSsoUrl: "",
  idpCertificate: "",
};

export function SsoTab({ orgSlug }: SsoTabProps) {
  const [providers, setProviders] = useState<SsoProviderDto[] | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [showForm, setShowForm] = useState(false);
  const [form, setForm] = useState<NewProviderForm>(EMPTY_FORM);
  const [saving, setSaving] = useState(false);
  const [busyId, setBusyId] = useState<string | null>(null);

  const refresh = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const list = await fetchJson<SsoProviderDto[]>(
        `/api/orgs/${encodeURIComponent(orgSlug)}/sso`,
      );
      setProviders(list);
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setLoading(false);
    }
  }, [orgSlug]);

  useEffect(() => {
    refresh();
  }, [refresh]);

  async function create() {
    if (form.name.trim() === "") {
      toastError("Provider name is required");
      return;
    }
    setSaving(true);
    try {
      const body: Record<string, unknown> = {
        protocol: form.protocol,
        name: form.name.trim(),
      };
      if (form.emailDomains.trim() !== "")
        body.emailDomains = form.emailDomains.trim();
      if (form.protocol === "oidc") {
        body.issuerUrl = form.issuerUrl.trim();
        body.clientId = form.clientId.trim();
        body.clientSecret = form.clientSecret;
      } else {
        body.idpSsoUrl = form.idpSsoUrl.trim();
        if (form.idpEntityId.trim() !== "")
          body.idpEntityId = form.idpEntityId.trim();
        if (form.idpCertificate.trim() !== "")
          body.idpCertificate = form.idpCertificate;
      }
      await postJson(`/api/orgs/${encodeURIComponent(orgSlug)}/sso`, body);
      setForm(EMPTY_FORM);
      setShowForm(false);
      await refresh();
    } catch (e) {
      toastError(e, "Create failed");
    } finally {
      setSaving(false);
    }
  }

  async function toggleEnabled(p: SsoProviderDto) {
    setBusyId(p.id);
    try {
      await putJson(
        `/api/orgs/${encodeURIComponent(orgSlug)}/sso/${encodeURIComponent(p.id)}`,
        { enabled: !p.enabled },
      );
      await refresh();
    } catch (e) {
      toastError(e, "Update failed");
    } finally {
      setBusyId(null);
    }
  }

  async function remove(p: SsoProviderDto) {
    if (!confirmDelete(p.name)) return;
    setBusyId(p.id);
    try {
      await deleteJson(
        `/api/orgs/${encodeURIComponent(orgSlug)}/sso/${encodeURIComponent(p.id)}`,
      );
      await refresh();
    } catch (e) {
      toastError(e, "Delete failed");
    } finally {
      setBusyId(null);
    }
  }

  return (
    <div className="space-y-6">
      <div className="flex items-baseline justify-between">
        <h2 className="text-sm font-semibold text-gray-200">
          Identity providers
        </h2>
        {!showForm && (
          <Button
            onClick={() => setShowForm(true)}
            variant="primary"
            size="sm"
            data-testid="sso-add"
          >
            Add provider
          </Button>
        )}
      </div>

      {loading && (
        <p className="text-xs text-gray-500" data-testid="sso-loading">
          Loading…
        </p>
      )}
      {error && <Banner>{error}</Banner>}

      {!loading && !error && providers && providers.length === 0 && !showForm && (
        <p className="text-xs text-gray-500">
          No SSO providers configured. Users sign in with email + password
          until you add one.
        </p>
      )}

      {!loading && !error && providers && providers.length > 0 && (
        <div
          className="rounded border border-gray-800 divide-y divide-gray-800"
          data-testid="sso-list"
        >
          {providers.map((p) => {
            const busy = busyId === p.id;
            return (
              <div
                key={p.id}
                className="flex items-center gap-3 px-3 py-2"
                data-testid={`sso-row-${p.id}`}
              >
                <div className="flex-1 min-w-0">
                  <div className="text-sm text-gray-200 truncate">
                    {p.name}{" "}
                    <span className="ml-1 text-[10px] uppercase text-gray-500">
                      {p.protocol}
                    </span>
                  </div>
                  <div className="text-[11px] text-gray-500 truncate">
                    {p.protocol === "oidc"
                      ? (p.issuerUrl ?? "—")
                      : (p.idpSsoUrl ?? "—")}
                    {p.emailDomains && (
                      <>
                        {" · domains: "}
                        <span className="font-mono">{p.emailDomains}</span>
                      </>
                    )}
                  </div>
                </div>
                <button
                  type="button"
                  onClick={() => toggleEnabled(p)}
                  disabled={busy}
                  data-testid={`sso-toggle-${p.id}`}
                  className={`text-[11px] px-2 py-0.5 rounded border transition disabled:opacity-50 ${
                    p.enabled
                      ? "border-emerald-700/50 bg-emerald-900/30 text-emerald-200"
                      : "border-gray-700 bg-gray-800 text-gray-400"
                  }`}
                >
                  {p.enabled ? "Enabled" : "Disabled"}
                </button>
                <Button
                  onClick={() => remove(p)}
                  disabled={busy}
                  loading={busy}
                  variant="danger"
                  size="sm"
                  data-testid={`sso-delete-${p.id}`}
                >
                  Delete
                </Button>
              </div>
            );
          })}
        </div>
      )}

      {showForm && (
        <div
          className="rounded border border-gray-800 bg-gray-900/40 p-4 max-w-2xl space-y-3"
          data-testid="sso-form"
        >
          <h3 className="text-sm font-semibold text-gray-200">New provider</h3>

          <Field label="Protocol">
            <select
              value={form.protocol}
              onChange={(e) =>
                setForm({
                  ...form,
                  protocol: e.target.value as "oidc" | "saml",
                })
              }
              data-testid="sso-protocol"
              className="bg-gray-800 border border-gray-700 rounded px-2 py-1.5 text-sm text-gray-200 focus:outline-none focus:border-indigo-600"
            >
              <option value="oidc">OIDC</option>
              <option value="saml">SAML</option>
            </select>
          </Field>

          <Field label="Name">
            <input
              type="text"
              value={form.name}
              onChange={(e) => setForm({ ...form, name: e.target.value })}
              placeholder="Acme Okta"
              data-testid="sso-name"
              className="w-full bg-gray-800 border border-gray-700 rounded px-3 py-1.5 text-sm text-gray-200 focus:outline-none focus:border-indigo-600"
            />
          </Field>

          <Field label="Email domains (optional, comma-separated)">
            <input
              type="text"
              value={form.emailDomains}
              onChange={(e) =>
                setForm({ ...form, emailDomains: e.target.value })
              }
              placeholder="acme.com, acme.io"
              data-testid="sso-domains"
              className="w-full bg-gray-800 border border-gray-700 rounded px-3 py-1.5 text-sm text-gray-200 focus:outline-none focus:border-indigo-600"
            />
          </Field>

          {form.protocol === "oidc" ? (
            <>
              <Field label="Issuer URL">
                <input
                  type="text"
                  value={form.issuerUrl}
                  onChange={(e) =>
                    setForm({ ...form, issuerUrl: e.target.value })
                  }
                  placeholder="https://acme.okta.com/.well-known/openid-configuration"
                  data-testid="sso-issuer"
                  className="w-full bg-gray-800 border border-gray-700 rounded px-3 py-1.5 text-sm text-gray-200 focus:outline-none focus:border-indigo-600"
                />
              </Field>
              <Field label="Client ID">
                <input
                  type="text"
                  value={form.clientId}
                  onChange={(e) =>
                    setForm({ ...form, clientId: e.target.value })
                  }
                  data-testid="sso-client-id"
                  className="w-full bg-gray-800 border border-gray-700 rounded px-3 py-1.5 text-sm text-gray-200 focus:outline-none focus:border-indigo-600"
                />
              </Field>
              <Field label="Client secret">
                <input
                  type="password"
                  value={form.clientSecret}
                  onChange={(e) =>
                    setForm({ ...form, clientSecret: e.target.value })
                  }
                  data-testid="sso-client-secret"
                  className="w-full bg-gray-800 border border-gray-700 rounded px-3 py-1.5 text-sm text-gray-200 focus:outline-none focus:border-indigo-600"
                />
              </Field>
            </>
          ) : (
            <>
              <Field label="IdP SSO URL">
                <input
                  type="text"
                  value={form.idpSsoUrl}
                  onChange={(e) =>
                    setForm({ ...form, idpSsoUrl: e.target.value })
                  }
                  data-testid="sso-idp-url"
                  className="w-full bg-gray-800 border border-gray-700 rounded px-3 py-1.5 text-sm text-gray-200 focus:outline-none focus:border-indigo-600"
                />
              </Field>
              <Field label="IdP entity ID (optional)">
                <input
                  type="text"
                  value={form.idpEntityId}
                  onChange={(e) =>
                    setForm({ ...form, idpEntityId: e.target.value })
                  }
                  data-testid="sso-idp-entity"
                  className="w-full bg-gray-800 border border-gray-700 rounded px-3 py-1.5 text-sm text-gray-200 focus:outline-none focus:border-indigo-600"
                />
              </Field>
              <Field label="IdP certificate (PEM, optional)">
                <textarea
                  value={form.idpCertificate}
                  onChange={(e) =>
                    setForm({ ...form, idpCertificate: e.target.value })
                  }
                  rows={4}
                  placeholder="-----BEGIN CERTIFICATE-----..."
                  data-testid="sso-idp-cert"
                  className="w-full bg-gray-800 border border-gray-700 rounded px-3 py-1.5 text-xs text-gray-200 font-mono focus:outline-none focus:border-indigo-600"
                />
              </Field>
            </>
          )}

          <div className="flex gap-2 pt-2">
            <Button
              onClick={create}
              loading={saving}
              disabled={saving}
              variant="primary"
              size="sm"
              data-testid="sso-create"
            >
              Create
            </Button>
            <Button
              onClick={() => {
                setShowForm(false);
                setForm(EMPTY_FORM);
              }}
              disabled={saving}
              variant="ghost"
              size="sm"
            >
              Cancel
            </Button>
          </div>
        </div>
      )}
    </div>
  );
}

function Field({
  label,
  children,
}: {
  label: string;
  children: React.ReactNode;
}) {
  return (
    <label className="block">
      <span className="text-[11px] uppercase tracking-wide text-gray-500">
        {label}
      </span>
      <div className="mt-1">{children}</div>
    </label>
  );
}

/// `confirm()` is the prevailing destructive-action prompt across this
/// codebase (e.g. discardAgentBranch in agent-store, BulkDeleteModal's
/// raw window.confirm fallback). Wrapped in a helper so a future
/// refactor to `Modal` only touches one site.
function confirmDelete(name: string): boolean {
  if (typeof window === "undefined") return true;
  return window.confirm(
    `Delete SSO provider "${name}"? Existing user accounts linked to this provider remain but can no longer sign in.`,
  );
}
