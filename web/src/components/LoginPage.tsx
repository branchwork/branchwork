import { useEffect, useState } from "react";
import { useAuthStore } from "../stores/auth-store.js";
import type { SsoLoginOption } from "../stores/auth-store.js";
import { fetchJson } from "../api.js";

type Mode = "login" | "signup";

const ERROR_MESSAGES: Record<string, string> = {
  invalid_credentials: "Wrong email or password.",
  email_taken: "An account with that email already exists.",
  invalid_email: "Please enter a valid email address.",
  password_too_short: "Password must be at least 8 characters.",
  password_too_long: "Password must be 72 characters or fewer.",
  // SSO errors (set as ?sso_error= query param by the server callback)
  idp_error: "Your identity provider returned an error.",
  invalid_state: "SSO session expired. Please try again.",
  invalid_token: "Identity verification failed.",
  missing_email: "Your identity provider did not return an email address.",
  provisioning_failed: "Failed to create your account. Please contact an admin.",
  issuer_mismatch: "Identity provider mismatch.",
  saml_not_success: "SAML authentication was not successful.",
};

function humanize(code: string | null): string | null {
  if (!code) return null;
  return ERROR_MESSAGES[code] ?? code.replace(/_/g, " ");
}

export function LoginPage() {
  const [mode, setMode] = useState<Mode>("login");
  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");
  const [submitting, setSubmitting] = useState(false);
  const [ssoProviders, setSsoProviders] = useState<SsoLoginOption[]>([]);
  const error = useAuthStore((s) => s.error);
  const login = useAuthStore((s) => s.login);
  const signup = useAuthStore((s) => s.signup);

  // Check for SSO error from callback redirect.
  const [ssoError] = useState(() => {
    const params = new URLSearchParams(window.location.search);
    const err = params.get("sso_error");
    if (err) {
      // Clean up the URL so the error doesn't persist on refresh.
      window.history.replaceState({}, "", window.location.pathname);
    }
    return err;
  });

  // Discover SSO providers when the user enters an email with a domain.
  useEffect(() => {
    if (!email.includes("@") || mode !== "login") {
      setSsoProviders([]);
      return;
    }
    const timer = setTimeout(async () => {
      try {
        const providers = await fetchJson<SsoLoginOption[]>(
          `/api/auth/sso/providers?email=${encodeURIComponent(email)}`
        );
        setSsoProviders(providers);
      } catch {
        setSsoProviders([]);
      }
    }, 400);
    return () => clearTimeout(timer);
  }, [email, mode]);

  async function handleSubmit(e: React.FormEvent) {
    e.preventDefault();
    if (submitting) return;
    setSubmitting(true);
    try {
      if (mode === "login") {
        await login(email, password);
      } else {
        await signup(email, password);
      }
    } catch {
      // error state is already on the store; swallow here so the form stays
      // mounted instead of crashing the tree.
    } finally {
      setSubmitting(false);
    }
  }

  const title = mode === "login" ? "Sign in to Branchwork" : "Create an Branchwork account";
  const submitLabel = mode === "login" ? "Sign in" : "Sign up";
  const toggleLabel =
    mode === "login" ? "Need an account? Sign up" : "Already have an account? Sign in";

  const displayError = ssoError || error;

  return (
    <div className="flex h-screen items-center justify-center bg-gray-950 text-gray-100">
      <form
        onSubmit={handleSubmit}
        className="w-full max-w-sm bg-gray-900 border border-gray-800 rounded-lg p-6 shadow-xl"
      >
        <h1 className="text-lg font-semibold mb-1">{title}</h1>
        <p className="text-xs text-gray-500 mb-5">
          {mode === "login"
            ? "Use your email and password."
            : "Pick an email and a password (at least 8 characters)."}
        </p>

        <label className="block text-xs font-medium text-gray-400 mb-1">Email</label>
        <input
          type="email"
          autoComplete="email"
          required
          value={email}
          onChange={(e) => setEmail(e.target.value)}
          className="w-full bg-gray-800 border border-gray-700 rounded px-3 py-2 text-sm text-gray-200 placeholder:text-gray-600 focus:outline-none focus:border-indigo-600 mb-3"
          placeholder="you@example.com"
        />

        <label className="block text-xs font-medium text-gray-400 mb-1">Password</label>
        <input
          type="password"
          autoComplete={mode === "login" ? "current-password" : "new-password"}
          required
          minLength={8}
          value={password}
          onChange={(e) => setPassword(e.target.value)}
          className="w-full bg-gray-800 border border-gray-700 rounded px-3 py-2 text-sm text-gray-200 placeholder:text-gray-600 focus:outline-none focus:border-indigo-600 mb-4"
          placeholder="********"
        />

        {displayError && (
          <div className="mb-3 text-xs text-red-400 bg-red-900/20 border border-red-900/40 rounded px-2 py-1.5">
            {humanize(displayError)}
          </div>
        )}

        <button
          type="submit"
          disabled={submitting || !email || !password}
          className="w-full bg-indigo-600 hover:bg-indigo-500 disabled:bg-indigo-800 disabled:text-gray-400 text-white text-sm font-medium rounded py-2 transition"
        >
          {submitting ? "\u2026" : submitLabel}
        </button>

        {ssoProviders.length > 0 && (
          <div className="mt-4 space-y-2">
            <div className="flex items-center gap-2">
              <div className="flex-1 border-t border-gray-800" />
              <span className="text-xs text-gray-600">or</span>
              <div className="flex-1 border-t border-gray-800" />
            </div>
            {ssoProviders.map((p) => (
              <a
                key={p.id}
                href={p.loginUrl}
                className="block w-full bg-gray-800 hover:bg-gray-700 border border-gray-700 text-gray-200 text-sm font-medium rounded py-2 text-center transition"
              >
                Sign in with {p.name}
              </a>
            ))}
          </div>
        )}

        <button
          type="button"
          onClick={() => setMode(mode === "login" ? "signup" : "login")}
          className="w-full text-xs text-gray-500 hover:text-gray-300 mt-3"
        >
          {toggleLabel}
        </button>
      </form>
    </div>
  );
}
