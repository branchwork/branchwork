import { useEffect, useMemo, useState } from "react";
import { useLearningsStore, type PromotionCandidate } from "../../stores/learnings-store.js";
import { useProjectsStore, type ProjectRow } from "../../stores/projects-store.js";
import { useCredentialsStore, type CredentialRow } from "../../stores/credentials-store.js";
import { Button } from "../ui/Button.js";
import { Banner } from "../ui/Banner.js";
import { Select } from "../ui/Select.js";
import { errorMessage, httpErrorBody } from "../../lib/error.js";
import { formatRelative } from "../../lib/time.js";

/// Promotions tab (Phase 4, Task 4.2). Lists learnings that crossed the
/// promotion threshold — each with a diff preview of exactly what would
/// be appended under `## Other rules` in the project's `CLAUDE.md`.
/// Approving a candidate opens a real PR (server-side, via the configured
/// GitHub PAT credential) and stamps the learning with the PR URL.
///
/// Universally visible (standalone + SaaS) like Diagnostics — promotion
/// is a single-tenant-friendly operator action, not an org-admin gate.
export function PromotionsTab() {
  const candidates = useLearningsStore((s) => s.candidates);
  const loaded = useLearningsStore((s) => s.candidatesLoaded);
  const loading = useLearningsStore((s) => s.candidatesLoading);
  const fetchCandidates = useLearningsStore((s) => s.fetchCandidates);

  const projects = useProjectsStore((s) => s.projects);
  const fetchProjects = useProjectsStore((s) => s.fetchProjects);
  const credentials = useCredentialsStore((s) => s.credentials);
  const fetchCredentials = useCredentialsStore((s) => s.fetchCredentials);

  useEffect(() => {
    fetchCandidates().catch(() => {});
    fetchProjects().catch(() => {});
    fetchCredentials().catch(() => {});
  }, [fetchCandidates, fetchProjects, fetchCredentials]);

  // Only GitHub projects can receive a PR today; only PAT credentials can
  // drive the API (SSH keys can't open PRs).
  const githubProjects = useMemo(() => projects.filter((p) => p.host === "github"), [projects]);
  const patCredentials = useMemo(
    () => credentials.filter((c) => c.kind === "gh_pat"),
    [credentials],
  );

  return (
    <section data-testid="promotions-tab" aria-labelledby="promotions-heading">
      <div className="mb-4">
        <h2 id="promotions-heading" className="text-base font-semibold text-gray-100">
          Promote learnings to CLAUDE.md
        </h2>
        <p className="text-xs text-gray-500 mt-1">
          Learnings that have been surfaced into enough agent sessions to earn a place in the
          project rules. Approving one opens a pull request appending the rule under{" "}
          <code>## Other rules</code>.
        </p>
      </div>

      {githubProjects.length === 0 && (
        <Banner kind="warn" className="mb-4">
          No GitHub project is configured. Add one under{" "}
          <a className="underline" href="/new-project">
            New Project
          </a>{" "}
          and a GitHub PAT credential to promote learnings.
        </Banner>
      )}

      {!loaded && loading && <p className="text-sm text-gray-400">Loading promotion candidates…</p>}
      {loaded && candidates.length === 0 && (
        <p data-testid="promotions-empty" className="text-sm text-gray-400">
          No promotion candidates yet. A learning is flagged once it has been rendered into{" "}
          {/* threshold is server-driven; copy stays generic */}
          enough distinct agent sessions.
        </p>
      )}

      <ul className="space-y-4">
        {candidates.map((c) => (
          <CandidateCard
            key={c.learningId}
            candidate={c}
            projects={githubProjects}
            credentials={patCredentials}
          />
        ))}
      </ul>
    </section>
  );
}

function CandidateCard({
  candidate,
  projects,
  credentials,
}: {
  candidate: PromotionCandidate;
  projects: ProjectRow[];
  credentials: CredentialRow[];
}) {
  const promote = useLearningsStore((s) => s.promote);
  const [projectId, setProjectId] = useState<string>(projects[0]?.id ?? "");
  const [credentialId, setCredentialId] = useState<string>("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // Seed the project selection once projects arrive (the store fetch may
  // resolve after first render).
  useEffect(() => {
    if (!projectId && projects.length > 0) setProjectId(projects[0].id);
  }, [projects, projectId]);

  const promoted = candidate.promotedPrUrl != null;
  const title = humanizeSlug(candidate.slug);

  async function onPromote() {
    if (!projectId) {
      setError("Pick a project to open the PR against.");
      return;
    }
    setBusy(true);
    setError(null);
    try {
      await promote(candidate.learningId, projectId, credentialId || undefined);
    } catch (e) {
      const body = httpErrorBody<{ error?: string; message?: string }>(e);
      setError(body?.message ?? errorMessage(e));
    } finally {
      setBusy(false);
    }
  }

  return (
    <li
      data-testid={`promotion-candidate-${candidate.learningId}`}
      className="rounded border border-gray-800 bg-gray-900/40 p-3"
    >
      <div className="flex items-baseline justify-between gap-3 flex-wrap">
        <h3 className="font-medium text-gray-100">{title}</h3>
        <span className="text-xs text-gray-500">
          {candidate.category} · seen in {candidate.hitCount} session
          {candidate.hitCount === 1 ? "" : "s"} (≥ {candidate.threshold} in {candidate.windowDays}d)
          · flagged {formatRelative(candidate.createdAt)}
        </span>
      </div>

      <DiffPreview preview={candidate.appendPreview} />

      {promoted ? (
        <p className="mt-3 text-sm text-emerald-300">
          Promoted —{" "}
          <a
            data-testid={`promotion-pr-link-${candidate.learningId}`}
            href={candidate.promotedPrUrl ?? "#"}
            target="_blank"
            rel="noopener noreferrer"
            className="underline hover:text-emerald-200"
          >
            view pull request &#x2197;
          </a>
        </p>
      ) : (
        <div className="mt-3 flex items-end gap-2 flex-wrap">
          <label className="flex flex-col gap-1 text-xs text-gray-400">
            Project repo
            <Select
              aria-label={`Project for ${title}`}
              data-testid={`promotion-project-${candidate.learningId}`}
              value={projectId}
              onChange={(e) => setProjectId(e.target.value)}
              disabled={busy || projects.length === 0}
            >
              {projects.length === 0 && <option value="">No GitHub project</option>}
              {projects.map((p) => (
                <option key={p.id} value={p.id}>
                  {p.name}
                </option>
              ))}
            </Select>
          </label>
          <label className="flex flex-col gap-1 text-xs text-gray-400">
            Credential
            <Select
              aria-label={`Credential for ${title}`}
              data-testid={`promotion-credential-${candidate.learningId}`}
              value={credentialId}
              onChange={(e) => setCredentialId(e.target.value)}
              disabled={busy}
            >
              <option value="">Project default</option>
              {credentials.map((c) => (
                <option key={c.id} value={c.id}>
                  {c.name}
                </option>
              ))}
            </Select>
          </label>
          <Button
            data-testid={`promotion-approve-${candidate.learningId}`}
            onClick={onPromote}
            disabled={busy || projects.length === 0}
          >
            {busy ? "Opening PR…" : "Promote → open PR"}
          </Button>
        </div>
      )}

      {error && (
        <Banner kind="error" className="mt-3">
          {error}
        </Banner>
      )}
    </li>
  );
}

/// Render the rule block as a unified-diff-style preview: a single gray
/// context line for the `## Other rules` anchor, then every line of the
/// appended block prefixed with `+` in green. Communicates "here is what
/// would be appended to CLAUDE.md" without fetching the live file.
function DiffPreview({ preview }: { preview: string }) {
  const addedLines = preview.replace(/\n+$/, "").split("\n");
  return (
    <pre className="mt-2 max-h-72 overflow-auto rounded bg-gray-950 border border-gray-800 p-2 font-mono text-[11px] leading-snug whitespace-pre-wrap">
      <span className="text-gray-500"> ## Other rules</span>
      {"\n"}
      {addedLines.map((line, i) => (
        <span key={i} className="text-emerald-300">
          {`+ ${line}`}
          {"\n"}
        </span>
      ))}
    </pre>
  );
}

/// Mirror of `api/learnings.rs::humanize_slug` for display: split on
/// `-`/`_`, capitalise the first word.
function humanizeSlug(slug: string): string {
  const words = slug
    .split(/[-_]/)
    .filter((w) => w.length > 0)
    .join(" ");
  if (words.length === 0) return slug;
  return words.charAt(0).toUpperCase() + words.slice(1);
}
