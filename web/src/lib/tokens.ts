/// Semantic design tokens for the dashboard primitives.
///
/// Each token is a small Tailwind class-string bundle keyed by intent
/// (BRAND, SUCCESS, WARN, DANGER, MUTED). Primitives import these so a
/// future light theme can swap palettes in one place — the alternative
/// (12+ duplications of `bg-indigo-600 hover:bg-indigo-500 …` flagged
/// in the dashboard audit §1/§14) makes a theme change touch every
/// component.
///
/// Scope is deliberately narrow: only colours that recur across
/// primitives are extracted. Layout, typography, and one-off accents
/// stay inline. The goal is to unify drift, not over-abstract.

export const BRAND = {
  /// Solid filled surface — primary buttons, selected templates.
  solidBg: "bg-indigo-600",
  solidBgHover: "hover:bg-indigo-500",
  solidText: "text-white",
  /// Soft pill / badge surface.
  softBg: "bg-indigo-900/30",
  softBorder: "border-indigo-700/50",
  softText: "text-indigo-200",
  softDot: "bg-indigo-400",
} as const;

export const SUCCESS = {
  solidBg: "bg-emerald-600",
  solidBgHover: "hover:bg-emerald-500",
  solidText: "text-white",
  softBg: "bg-emerald-900/20",
  softBorder: "border-emerald-700/40",
  softText: "text-emerald-300",
  softDot: "bg-emerald-500",
} as const;

export const WARN = {
  solidBg: "bg-amber-600",
  solidBgHover: "hover:bg-amber-500",
  solidText: "text-white",
  softBg: "bg-amber-900/30",
  softBorder: "border-amber-700/50",
  softText: "text-amber-200",
  softDot: "bg-amber-400",
} as const;

export const DANGER = {
  solidBg: "bg-red-700",
  solidBgHover: "hover:bg-red-600",
  solidText: "text-white",
  softBg: "bg-red-900/30",
  softBorder: "border-red-700/50",
  softText: "text-red-200",
  softDot: "bg-red-400",
} as const;

export const MUTED = {
  /// Secondary surface tone — gray buttons, neutral chips.
  solidBg: "bg-gray-800",
  solidBgHover: "hover:bg-gray-700",
  solidText: "text-gray-200",
  softBg: "bg-gray-900/40",
  softBorder: "border-gray-700",
  softText: "text-gray-400",
  softDot: "bg-gray-500",
  /// Disabled state shared by every solid-surface primitive. Pulled out
  /// of the variant strings because every button drift site flagged in
  /// audit §1 used the same `disabled:bg-gray-700 disabled:text-gray-500`
  /// pair.
  disabledBg: "disabled:bg-gray-700",
  disabledText: "disabled:text-gray-500",
} as const;

export type SemanticToken =
  | typeof BRAND
  | typeof SUCCESS
  | typeof WARN
  | typeof DANGER
  | typeof MUTED;
