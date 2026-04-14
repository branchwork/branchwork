import { useState, useEffect } from "react";
import { fetchJson, postJson } from "../api.js";
import { useAgentStore } from "../stores/agent-store.js";
import { usePlanStore } from "../stores/plan-store.js";

interface Folder {
  name: string;
  path: string;
}

interface Template {
  id: string;
  name: string;
  description: string;
  placeholder: string;
  skeleton: string;
}

interface Props {
  onClose: () => void;
}

export function NewPlanForm({ onClose }: Props) {
  const [description, setDescription] = useState("");
  const [folder, setFolder] = useState("");
  const [folders, setFolders] = useState<Folder[]>([]);
  const [templates, setTemplates] = useState<Template[]>([]);
  const [templateId, setTemplateId] = useState<string>("");
  const [showSuggestions, setShowSuggestions] = useState(false);
  const [creating, setCreating] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [confirmCreate, setConfirmCreate] = useState<string | null>(null);
  const selectAgent = useAgentStore((s) => s.selectAgent);
  const fetchPlans = usePlanStore((s) => s.fetchPlans);

  useEffect(() => {
    fetchJson<Folder[]>("/api/folders").then(setFolders).catch(() => {});
    fetchJson<Template[]>("/api/templates").then(setTemplates).catch(() => {});
  }, []);

  const selectedTemplate = templates.find((t) => t.id === templateId) ?? null;

  function handleTemplateChange(id: string) {
    setTemplateId(id);
    const tpl = templates.find((t) => t.id === id);
    if (tpl && !description.trim()) {
      setDescription(tpl.placeholder);
    }
  }

  const filtered = folder.trim()
    ? folders.filter(
        (f) =>
          f.name.toLowerCase().includes(folder.toLowerCase()) ||
          f.path.toLowerCase().includes(folder.toLowerCase())
      )
    : folders;

  async function handleSubmit() {
    setError(null);
    setCreating(true);
    try {
      const res = await postJson<
        { agentId: string; folder: string } | { error: string; resolvedFolder?: string }
      >("/api/plans/create", {
        description,
        folder,
        createFolder: !!confirmCreate,
        templateId: templateId || undefined,
      });

      if ("error" in res) {
        if (res.error === "folder_not_found" && res.resolvedFolder) {
          setConfirmCreate(res.resolvedFolder);
          setCreating(false);
          return;
        }
        setError((res as { error: string; message?: string }).message ?? res.error);
        setCreating(false);
        return;
      }

      selectAgent(res.agentId);
      // Plan will appear via file watcher once the agent writes it
      setTimeout(() => fetchPlans(), 5000);
      setTimeout(() => fetchPlans(), 15000);
      setTimeout(() => fetchPlans(), 30000);
      onClose();
    } catch (e) {
      setError(String(e));
    } finally {
      setCreating(false);
    }
  }

  async function handleConfirmCreate() {
    setConfirmCreate(null);
    await handleSubmit();
  }

  function handleKeyDown(e: React.KeyboardEvent) {
    if (e.key === "Enter" && e.metaKey) {
      e.preventDefault();
      handleSubmit();
    }
  }

  return (
    <div className="p-6 max-w-2xl">
      <div className="flex items-center justify-between mb-6">
        <h2 className="text-xl font-bold">New Plan</h2>
        <button
          onClick={onClose}
          className="text-gray-500 hover:text-gray-300 text-sm"
        >
          Cancel
        </button>
      </div>

      {/* Folder */}
      <div className="mb-4">
        <label className="block text-sm font-medium text-gray-400 mb-1.5">
          Project folder
        </label>
        <div className="relative">
          <input
            type="text"
            value={folder}
            onChange={(e) => {
              setFolder(e.target.value);
              setConfirmCreate(null);
              setError(null);
            }}
            onFocus={() => setShowSuggestions(true)}
            onBlur={() => setTimeout(() => setShowSuggestions(false), 200)}
            placeholder="~/my-project or /home/cpo/project"
            className="w-full bg-gray-800 border border-gray-700 rounded px-3 py-2 text-sm text-gray-200 placeholder:text-gray-600 focus:outline-none focus:border-indigo-600"
          />
          {showSuggestions && filtered.length > 0 && (
            <div className="absolute top-full left-0 right-0 mt-1 bg-gray-800 border border-gray-700 rounded-md shadow-lg max-h-48 overflow-auto z-20">
              {filtered.map((f) => (
                <button
                  key={f.path}
                  onMouseDown={(e) => e.preventDefault()}
                  onClick={() => {
                    setFolder(f.path);
                    setShowSuggestions(false);
                  }}
                  className="w-full text-left px-3 py-1.5 text-sm text-gray-300 hover:bg-gray-700 truncate"
                >
                  <span className="text-indigo-400">{f.name}</span>
                  <span className="text-gray-600 ml-2 text-xs">{f.path}</span>
                </button>
              ))}
            </div>
          )}
        </div>
        {confirmCreate && (
          <div className="mt-2 p-2 bg-amber-900/20 border border-amber-700/50 rounded text-xs text-amber-400">
            <p>
              Directory does not exist: <code className="text-amber-300">{confirmCreate}</code>
            </p>
            <div className="flex gap-2 mt-2">
              <button
                onClick={handleConfirmCreate}
                className="px-2 py-1 bg-amber-700 hover:bg-amber-600 text-white rounded transition"
              >
                Create folder and continue
              </button>
              <button
                onClick={() => setConfirmCreate(null)}
                className="px-2 py-1 text-gray-400 hover:text-gray-200 transition"
              >
                Cancel
              </button>
            </div>
          </div>
        )}
      </div>

      {/* Template */}
      {templates.length > 0 && (
        <div className="mb-4">
          <label className="block text-sm font-medium text-gray-400 mb-1.5">
            Template <span className="text-gray-600">(optional)</span>
          </label>
          <div className="grid grid-cols-2 gap-2">
            <TemplateButton
              selected={templateId === ""}
              title="From scratch"
              description="No skeleton — describe anything."
              onClick={() => setTemplateId("")}
            />
            {templates.map((t) => (
              <TemplateButton
                key={t.id}
                selected={templateId === t.id}
                title={t.name}
                description={t.description}
                onClick={() => handleTemplateChange(t.id)}
              />
            ))}
          </div>
          {selectedTemplate && (
            <pre className="mt-2 p-2 bg-gray-900 border border-gray-800 rounded text-[11px] text-gray-500 whitespace-pre-wrap font-mono">
              {selectedTemplate.skeleton}
            </pre>
          )}
        </div>
      )}

      {/* Description */}
      <div className="mb-4">
        <label className="block text-sm font-medium text-gray-400 mb-1.5">
          {selectedTemplate ? "Specifics" : "What do you want to build?"}
        </label>
        <textarea
          value={description}
          onChange={(e) => setDescription(e.target.value)}
          onKeyDown={handleKeyDown}
          rows={8}
          placeholder={
            selectedTemplate?.placeholder ??
            "Describe the feature, project, or task you want to plan..."
          }
          className="w-full bg-gray-800 border border-gray-700 rounded px-3 py-2 text-sm text-gray-200 placeholder:text-gray-600 focus:outline-none focus:border-indigo-600 resize-y"
        />
        <p className="text-[10px] text-gray-600 mt-1">
          An agent will explore the folder and create a structured plan with phases and tasks.
        </p>
      </div>

      {/* Error */}
      {error && (
        <div className="mb-4 p-2 bg-red-900/20 border border-red-700/50 rounded text-xs text-red-400">
          {error}
        </div>
      )}

      {/* Submit */}
      <button
        onClick={handleSubmit}
        disabled={creating || !description.trim() || !folder.trim()}
        className="px-4 py-2 bg-indigo-600 hover:bg-indigo-500 disabled:bg-gray-700 disabled:text-gray-500 text-white text-sm rounded transition"
      >
        {creating ? "Starting agent..." : "Create Plan"}
      </button>
    </div>
  );
}

interface TemplateButtonProps {
  selected: boolean;
  title: string;
  description: string;
  onClick: () => void;
}

function TemplateButton({ selected, title, description, onClick }: TemplateButtonProps) {
  const base = "text-left px-3 py-2 rounded border text-sm transition";
  const state = selected
    ? "bg-indigo-600/20 border-indigo-500 text-indigo-200"
    : "bg-gray-800 border-gray-700 text-gray-300 hover:border-gray-600";
  return (
    <button type="button" onClick={onClick} className={`${base} ${state}`}>
      <div className="font-medium">{title}</div>
      <div className="text-[11px] text-gray-500 mt-0.5 line-clamp-2">{description}</div>
    </button>
  );
}
