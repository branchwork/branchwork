import { useState } from "react";
import { useAuthStore } from "../stores/auth-store.js";

type Mode = "login" | "signup";

const ERROR_MESSAGES: Record<string, string> = {
  invalid_credentials: "Wrong email or password.",
  email_taken: "An account with that email already exists.",
  invalid_email: "Please enter a valid email address.",
  password_too_short: "Password must be at least 8 characters.",
  password_too_long: "Password must be 72 characters or fewer.",
};

function humanize(code: string | null): string | null {
  if (!code) return null;
  return ERROR_MESSAGES[code] ?? code;
}

export function LoginPage() {
  const [mode, setMode] = useState<Mode>("login");
  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");
  const [submitting, setSubmitting] = useState(false);
  const error = useAuthStore((s) => s.error);
  const login = useAuthStore((s) => s.login);
  const signup = useAuthStore((s) => s.signup);

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

  const title = mode === "login" ? "Sign in to orchestrAI" : "Create an orchestrAI account";
  const submitLabel = mode === "login" ? "Sign in" : "Sign up";
  const toggleLabel =
    mode === "login" ? "Need an account? Sign up" : "Already have an account? Sign in";

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

        {error && (
          <div className="mb-3 text-xs text-red-400 bg-red-900/20 border border-red-900/40 rounded px-2 py-1.5">
            {humanize(error)}
          </div>
        )}

        <button
          type="submit"
          disabled={submitting || !email || !password}
          className="w-full bg-indigo-600 hover:bg-indigo-500 disabled:bg-indigo-800 disabled:text-gray-400 text-white text-sm font-medium rounded py-2 transition"
        >
          {submitting ? "…" : submitLabel}
        </button>

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
