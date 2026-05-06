import {
  useEffect,
  useRef,
  useState,
  type FormEvent,
  type KeyboardEvent,
  type FocusEvent as ReactFocusEvent,
  type ChangeEvent,
} from "react";

interface Props {
  value: string;
  onSave: (value: string) => void;
  /// Used to derive the trigger button's accessible name (`Edit ${label}`)
  /// and the edit form's accessible name. Examples: "plan title",
  /// "task description". Without it the editable region has no
  /// accessible name (audit §8 fix).
  label: string;
  multiline?: boolean;
  className?: string;
  editClassName?: string;
  placeholder?: string;
}

export function EditableText({
  value,
  onSave,
  label,
  multiline = false,
  className = "",
  editClassName = "",
  placeholder = "Click to edit",
}: Props) {
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState(value);
  const inputRef = useRef<HTMLInputElement | HTMLTextAreaElement>(null);
  const triggerRef = useRef<HTMLButtonElement>(null);
  const wasEditing = useRef(false);

  useEffect(() => {
    setDraft(value);
  }, [value]);

  useEffect(() => {
    if (editing) {
      wasEditing.current = true;
      inputRef.current?.focus();
      inputRef.current?.select();
    } else if (wasEditing.current) {
      wasEditing.current = false;
      triggerRef.current?.focus();
    }
  }, [editing]);

  function commit() {
    const trimmed = draft.trim();
    if (trimmed !== value) {
      onSave(trimmed);
    }
    setEditing(false);
  }

  function cancel() {
    setDraft(value);
    setEditing(false);
  }

  function handleSubmit(e: FormEvent<HTMLFormElement>) {
    e.preventDefault();
    commit();
  }

  function handleKeyDown(
    e: KeyboardEvent<HTMLInputElement | HTMLTextAreaElement>,
  ) {
    if (e.key === "Escape") {
      e.preventDefault();
      cancel();
      return;
    }
    if (e.key === "Enter" && multiline && (e.metaKey || e.ctrlKey)) {
      e.preventDefault();
      commit();
    }
    // Single-line Enter is handled by <form> natural submit.
    // Multi-line Enter (no modifier) inserts a newline (default).
  }

  function handleBlur(
    e: ReactFocusEvent<HTMLInputElement | HTMLTextAreaElement>,
  ) {
    // If focus is moving to another control inside the same form (e.g.
    // the sr-only Save button via Shift+Tab), don't save yet — the
    // explicit submit handler will run.
    const next = e.relatedTarget as Node | null;
    if (next && e.currentTarget.form?.contains(next)) {
      return;
    }
    commit();
  }

  if (!editing) {
    return (
      <button
        ref={triggerRef}
        type="button"
        onClick={() => setEditing(true)}
        aria-label={`Edit ${label}`}
        className={`inline-block text-left cursor-pointer hover:bg-gray-800/50 rounded px-0.5 -mx-0.5 transition focus-visible:outline focus-visible:outline-2 focus-visible:outline-indigo-500 ${className}`}
        title="Click to edit"
      >
        {value || <span className="text-gray-600 italic">{placeholder}</span>}
      </button>
    );
  }

  const sharedProps = {
    value: draft,
    onChange: (e: ChangeEvent<HTMLInputElement | HTMLTextAreaElement>) =>
      setDraft(e.target.value),
    onBlur: handleBlur,
    onKeyDown: handleKeyDown,
    "aria-label": label,
    className: `bg-gray-800 border border-indigo-600/50 rounded px-1.5 py-0.5 outline-none text-gray-100 w-full ${editClassName}`,
  };

  return (
    <form
      onSubmit={handleSubmit}
      aria-label={`Edit ${label}`}
      className="contents"
    >
      {multiline ? (
        <textarea
          ref={inputRef as React.RefObject<HTMLTextAreaElement>}
          rows={Math.max(3, draft.split("\n").length)}
          {...sharedProps}
        />
      ) : (
        <input
          ref={inputRef as React.RefObject<HTMLInputElement>}
          type="text"
          {...sharedProps}
        />
      )}
      {/* Hidden submit/cancel for screen readers and as the canonical
          form-submit affordance. Sighted users use Enter (single-line),
          Cmd/Ctrl+Enter (multi-line), Esc to cancel, or click outside
          to save (onBlur). tabIndex=-1 keeps Tab order tight. */}
      <button type="submit" className="sr-only" tabIndex={-1}>
        Save {label}
      </button>
      <button
        type="button"
        onClick={cancel}
        className="sr-only"
        tabIndex={-1}
      >
        Cancel editing {label}
      </button>
    </form>
  );
}
