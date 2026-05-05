# Dashboard UI audit (2026-05-05 baseline)

This document is the verbatim capture of the audit that drove the
`dashboard-ui-overhaul` plan. The audit reviewed `web/` and produced
60+ findings across the 20 areas below, plus a "Top 10 prioritised
fixes" list at the end.

Every finding stays in its original area with a severity tag and
`path/file.tsx:LINE` reference. The capture is intentionally
flat — no re-prioritisation, no rewriting, no live links to plan
tasks. Downstream plans cite findings as "audit §3 finding 4".

Severity tags:

- `blocker` — user-visible regression or data-correctness risk in a
  flow the README claims as primary.
- `major` — user-visible defect, but the flow still completes (or
  has a workaround).
- `minor` — quality issue without an immediate user-visible
  consequence.
- `gap` — missing UI for a backend capability that already ships.

## §1 Component structure

- `minor` `web/src/components/PhaseColumn.tsx` is exported but has
  zero importers in `web/src`. Either delete or fold its
  "Start Phase" action into `PhaseCard.tsx`.
- `minor` Eight different inline error-banner markups across
  components (`PlanBoard.tsx:227`, `Sidebar.tsx:205`,
  `TaskCard.tsx:691`, `PhaseColumn.tsx:150`, `PhaseCard.tsx:38`,
  `AgentPanel.tsx:721`, `AdminPage.tsx:122`, `LoginPage.tsx:130`,
  `AuditLog.tsx:402`, `NewPlanForm.tsx:282`) — none of them share a
  primitive.
- `minor` Twelve-plus duplications of the indigo-primary button
  Tailwind class string (`bg-indigo-600 hover:bg-indigo-500
  disabled:bg-gray-700 …`) in the same components — at least
  `PlanBoard.tsx:631`, `:684`, `:825`, `NewPlanForm.tsx:241`,
  `AdminPage.tsx:108`.
- `minor` `confirm()` and `alert()` browser dialogs at
  `PhaseCard.tsx:38`, `PlanBoard.tsx:54`, `:73` instead of the
  same Modal primitive.

## §2 State management

- `major` `useAuthStore.logout()` (`auth-store.ts:80`) clears
  `user` only. `plan-store`, `agent-store`, `settings-store`, and
  `ws-store` keep their data after logout. Switching users in the
  same tab leaks the previous org's plans, agents, and settings
  until a hard refresh.
- `minor` `errorMessage` is defined privately at
  `auth-store.ts:31` and not exported, so 15+ inline
  `e instanceof Error ? e.message : String(e)` repetitions live
  across components.
- `minor` Selectors return new references on every store change.
  `useAgentStore((s) => s.agents)` re-renders every consumer on
  any agent-store mutation (Zustand v5 default `Object.is`
  equality). Same for plan-store consumers.

## §3 API surface coverage

- `gap` `AdminPage` exposes ~3 of the ~12 server-side configurable
  surfaces. Endpoints with no UI today:
  - `gap` `/api/orgs/{slug}/members` GET / POST / DELETE / PUT
    (role).
  - `gap` `/api/orgs/{slug}/kill-switch` PUT.
  - `gap` `/api/orgs/{slug}/budget` PUT and
    `/api/orgs/{slug}/usage` GET (current-usage chart).
  - `gap` `/api/orgs/{slug}/user-quotas` GET / PUT.
  - `gap` `/api/orgs/{slug}/sso` and per-provider CRUD.
- `gap` `/api/plans/{name}` DELETE has no UI.
- `gap` `/api/plans/{name}/tasks/{task_number}/learnings` PUT has
  no UI in `TaskCard`.
- `gap` `effort` and `skip_permissions` settings appear in both
  `AdminPage` (server-wide treatment) and `Sidebar` (user-scoped
  treatment) with no ownership cue. Persistence at
  `<claude-dir>/branchwork-settings.json` is server-wide today.

## §4 WebSocket event handling

- `major` Pervasive `as { … }` casts in `ws-store.ts` for every
  event payload. A schema mismatch is silent — message swallowed in
  catch (`ws-store.ts:94`) or, worse, partially applied to the
  store.
- `major` Six unchecked `JSON.parse` sites:
  `AgentPanel.tsx:288`, `:291`, `:308`; `AuditLog.tsx:115`,
  `:307`; `ws-store.ts:92`. A malformed payload throws past the
  WS listener.
- `major` Seven server-broadcast events have no UI handler today:
  - `phase_advanced`
  - `task_cost_reported`
  - `plan_reset`
  - `ci_run_dismissed`
  - `agent_branch_cleared`
  - `auto_finish_triggered`
  - `runner_connected` / `runner_disconnected` / `runner_drivers`
- `minor` `hook_event` lands a TODO branch in `ws-store.ts` that
  silently swallows the message.
- `major` WS-before-fetch race: `App.tsx:44` fires `connect()` and
  `fetchPlans()/fetchAgents()` simultaneously. An early
  `agent_started` event arrives, fires `agentStore.fetchAgents()`
  (`ws-store.ts:144`), and whichever promise resolves second wins.
- `major` `AuditLog.tsx:300` adds its own
  `addEventListener("message")` directly on the raw WebSocket. On
  `ws-store` reconnect the new socket has no listener until
  AuditLog remounts — silent data drop.
- `minor` Reconnect refetches cover plans+agents only; settings,
  drivers, and per-plan auto-mode-config can drift.

## §5 Routing / view state

- `major` `react-router-dom@7.5.2` is in `package.json` but
  unused. The app routes via a hand-rolled `useState<View>` in
  `App.tsx`.
- `major` Plan and agent selection live in stores, not URL
  params. Refresh loses the selected plan; a deep-link to a
  specific plan or agent is impossible.
- `major` Audit-log filters (kind, time range, plan filter,
  search) live entirely in component state. URL is unsharable;
  copy-paste does not reproduce a filtered view.
- `minor` Unrouted URLs fall into the `"plans"` default and
  silently rewrite the user's intent. No 404 page.

## §6 Loading / empty / error states

- `major` Visiting `/plans/does-not-exist` (or any unknown plan
  via the URL) shows an empty `PlanBoard` instead of a "plan not
  found" UI. Same for `/agents/<unknown>`.
- `minor` No shared loading-state primitive — most pages render
  `null` or a bare "Loading…" string until their store finishes
  fetching.
- `minor` Empty states differ per page (e.g. `AgentTree` empty
  vs. `ProjectDashboard` empty vs. `RunnersPage` doesn't exist
  yet).

## §7 Error UX

- `major` Eight different inline error-banner markups across
  forms and panels (call sites listed in §1). Visual drift makes
  errors easy to miss.
- `major` No global toast/notification surface for transient
  errors; every callsite renders inline.
- `major` No mid-session 401 recovery (see §13). Per-component
  error rendering forces the user to manually refresh.
- `minor` `e instanceof Error ? e.message : String(e)` repeated
  15+ times — see §2.

## §8 Accessibility

- `major` `EditableText.tsx:50` is a `<span onClick>`. Keyboard
  users cannot enter edit mode. Used pervasively for plan title,
  plan context, task title / description / acceptance.
- `major` Status menu (`TaskCard.tsx:288`) is a hand-rolled
  right-click `<div>` — no ARIA roles, no keyboard nav, hidden
  from non-mouse users.
- `major` Driver dropdown (`TaskCard.tsx:443`), merge-target
  dropdown (`AgentPanel.tsx:658`), and tab bar
  (`AgentPanel.tsx:101`) are hand-rolled menus without ARIA roles
  or keyboard navigation.
- `major` StaleBranchesButton modal at `PlanBoard.tsx:624` is a
  bare `<div role="…">` with no focus trap, no Esc, no
  return-focus on close.
- `minor` Icon-only buttons missing `aria-label`: dismiss `x` at
  `Sidebar.tsx:208`, `:262`; `⚙ Admin` button at
  `Sidebar.tsx:229`; multiple kebab/close buttons elsewhere.
- `minor` Animated status dots at `AgentTree.tsx:69` and
  `AgentPanel.tsx:46` convey state by colour only — no
  screen-reader text alternative.

## §9 Mobile / responsive

- `major` README claims "any screen, including phone". No mobile
  media queries beyond a single grid breakpoint exist.
- `major` `AgentPanel` mounts as a fixed `w-[600px]` right rail
  (`App.tsx:96`). On `<lg` viewports, sidebar + content + panel
  overflow horizontally.
- `major` xterm.js `FitAddon` fits to the 600px parent
  regardless of viewport — terminal renders with horizontal
  scroll on narrow screens.
- `major` Sidebar is not collapsible on mobile.
- `minor` Tap targets fail 44/48px minimum on touch:
  - `Sidebar.tsx:99` plan-list rows are 1.5px-y vertical
    padding.
  - `PlanBoard.tsx:259` status filter pills are
    `text-[10px] px-2 py-0.5`.
  - `TaskCard.tsx:417` CI dismiss `x` is icon-sized.
  - `TaskCard.tsx:450` driver `<select>` is 10px font.

## §10 Performance

- `major` `agentOutput[id]` is unbounded. `appendOutput` clones
  the array per WS line at `ws-store.ts:148`. A long-running
  agent grows the array unbounded.
- `major` `xterm` `scrollback: 10000` (`AgentPanel.tsx:181`) is
  per-agent and never trimmed. Each unselected agent retains its
  buffer for the lifetime of the tab.
- `major` `PlanBoard` renders all phases and all tasks
  (`PhaseCard.tsx:147`); `AgentTree` iterates every agent
  (`AgentTree.tsx:174`). Plans with 100+ tasks or accounts with
  200+ historical agents degrade.
- `minor` `Sidebar.grouped`, `App.activeCount`, and
  PhaseCard/TaskCard `completedSet` recompute on every render.
- `minor` Plan-wide computations like `completedSet` are not
  memoised at the PlanBoard level and instead recompute per
  TaskCard.

## §11 Tests

- `major` Server has 210 tests; `web/` has 34. The gap is on
  user-visible flows.
- `major` No Playwright (or any other) e2e test for the README
  golden path: signup → plan create → task agent → CI green →
  merge → task completed.
- `major` Untested components: `AdminPage`, `AgentTree`,
  `ProjectDashboard`, `LoginPage`, `EditableText`.
- `major` Untested stores: `agent-store`, `settings-store`,
  `auth-store`.
- `minor` No SaaS-specific e2e (runner registration + same
  golden path).

## §12 Type safety

- `major` WS payloads parsed via `as { … }` casts swallow schema
  drift across server changes (cross-listed with §4).
- `major` Six unchecked `JSON.parse` sites (cross-listed with
  §4). A malformed payload throws past the listener.
- `minor` `Agent`, `PlanConfig`, `AuditEntry` types are
  redeclared across multiple files instead of imported from a
  shared module. Drift is possible but has not bitten yet.
- `minor` Server response types are hand-rolled with no runtime
  validation. Schema coverage there is a separate concern.

## §13 Auth UX

- `blocker` No mid-session 401 recovery. After session expiry
  every API call shows a per-component error and the user must
  manually refresh. `web/src/api.ts` has no response interceptor.
- `major` Logout does not reset stores (cross-listed with §2).
- `minor` Typo: "Create **an** Branchwork account" at
  `LoginPage.tsx:87`.
- `minor` No "Forgot your password?" link below the password
  field.
- `minor` No "Sign out" link on `LoginPage` when an SSO
  redirect leaves the user in partial-auth state (token cookie
  present but invalid).
- `minor` SSO discovery has no fallback when the typed email is
  a personal email — no "Sign in with email + password instead"
  option that flips the form back to password mode without
  losing the email value.

## §14 Theme / styling

- `minor` 12+ duplications of the indigo-primary button class
  string (cross-listed with §1) — no design tokens.
- `minor` Dark-mode-only. Light theme would require touching
  every component because semantic colours are not extracted to
  tokens.
- `minor` Eight different inline error markups (cross-listed
  with §1, §7) — visual drift across the same product.
- `minor` Bottom-right cluster (`App.tsx:101` area) holds the
  connection indicator, driver-auth chip, and (eventually)
  runner-status — but the layout is ad-hoc, not a primitive.

## §15 Connection-loss UX

- `major` WS disconnect indicator is a 2px coloured dot in the
  bottom-right. Users do not notice the disconnect; data appears
  fresh while events are being lost.
- `major` No prominent banner during disconnect (>2s).
- `major` No stale-data marker next to plan or agent timestamps
  when WS has been disconnected for a noticeable interval.
- `minor` On reconnect, only plans+agents refetch (cross-listed
  with §4). Settings, drivers, and per-plan auto-mode-config
  can drift.

## §16 Notifications

- `major` Every event triggers a desktop notification if
  permission was granted. No opt-out, no per-event-class
  preferences.
- `major` No batching: 5 `agent_stopped` events in 2s = 5
  separate notifications.
- `minor` `Notification.requestPermission()` fires unsolicited
  on the first WS connect — not on a user gesture.
- `gap` Missing notification class for `phase_advanced` (the
  event itself is unhandled — see §4).

## §17 SaaS-specific gaps

- `blocker` `<a href="/runners">` in `NewPlanForm.tsx:331` is a
  404 — the route is not registered. The single user-facing
  cross-link to runner management is broken.
- `major` No runner-status indicator anywhere in the app
  chrome. SaaS users cannot tell whether their runner is online
  without leaving the dashboard.
- `major` `runner_connected`, `runner_disconnected`, and
  `runner_drivers` WS events are broadcast by the server and
  ignored by the UI (cross-listed with §4).
- `major` Org name is shown nowhere except a hardcoded URL
  fragment in `AuditLog.tsx:255`.
- `gap` No org switcher for users belonging to >1 org.
- `gap` No member-count chip on the org name.
- `gap` No deployment-mode discriminator on the client. The
  same UI ships in standalone and SaaS, with no way to hide
  SaaS-only affordances when no runner concept applies.

## §18 Admin / settings completeness

- `gap` `AdminPage` exposes 3 of ~12 configurable server-side
  surfaces (cross-listed with §3).
- `gap` No admin tabs for members, roles, kill-switch, budget,
  user-quotas, or SSO providers.
- `gap` No diagnostics tab (server version, uptime, runner
  health, WS connection count).
- `minor` `effort` and `skip_permissions` duplicated in both
  `AdminPage` and `Sidebar` (cross-listed with §3) with no
  ownership cue.

## §19 Internationalisation

- `gap` ~200 user-facing strings hardcoded in English. No i18n
  library; no extraction process.
- `minor` Hardcoded `"en-US"` locale in 4 files
  (`Sidebar.tsx:7`, `ProjectDashboard.tsx:5`, `TaskCard.tsx:59`,
  `NewPlanForm.tsx:33`, `AuditLog.tsx:93` formatters) instead
  of `navigator.language`.

## §20 Dependencies

- `major` `react-router-dom@7.5.2` in `package.json` but unused
  (cross-listed with §5).
- `major` No ESLint or Prettier config in `web/`.
  `eslint-plugin-react-hooks` would have caught the empty-deps
  `useEffect` bugs at `AgentTree.tsx:19` and
  `AgentPanel.tsx:275` / `:281`.
- `minor` Missing `eslint-plugin-jsx-a11y` for the
  accessibility regressions Phase 6 tasks will mitigate.
- `minor` `AgentPanel` ships `@xterm/xterm` and addons eagerly
  even though only one of N agents is PTY at a time. No code
  splitting around the terminal.
- `minor` No bundle-size budget check in CI.
- `minor` No runtime schema-validation library
  (`valibot`/`zod`) for WS payloads or `JSON.parse` outputs.
- `minor` No virtualisation library
  (`@tanstack/react-virtual`/`react-window`) for the long
  lists in §10.
- `minor` Four duplicated time-formatters and one private
  `errorMessage` in `auth-store.ts:31` instead of a `web/src/lib/`
  utilities folder (which does not exist yet).

## Top 10 prioritised fixes

The audit ranked the following ten fixes by user-visible impact and
implementation leverage. The plan executes them in roughly this
order; deviations are noted per-task.

1. **Foundation primitives** — design-system Button / IconButton /
   Modal / Switch / Select / Badge under `web/src/components/ui/`,
   plus a centralised toast store. Replaces the 8 error markups,
   12+ button duplications, and 15+ inline error-message snippets
   in §1, §2, §7, §14.
2. **Global 401 handler in `web/src/api.ts`** — wrap
   `fetchJson/postJson/putJson` with a response interceptor that
   logs out + toasts on 401, and rejects with a sentinel error.
   Resolves the §13 blocker.
3. **Reset every store on logout** — `useAuthStore.logout()` calls
   each store's `reset()` so user-switching in the same tab does
   not leak the previous org's data. Resolves the §2 / §13
   leakage.
4. **WebSocket schema validation + missing handlers** — `valibot`
   schemas for every event in §4; handlers for the seven ignored
   events; replace `JSON.parse` casts with validated parses.
   Resolves §4 / §12.
5. **URL routing via `react-router-dom`** — convert `view` enum
   to routes; plan, agent, audit filter, admin tab become
   deep-linkable. 404 page for unknown URLs. Resolves §5 / §6.
6. **Runner status + `/runners` page** — runner indicator in the
   chrome, real `/runners` route with list + enrol + diagnostics.
   Resolves the §17 blocker (`NewPlanForm.tsx:331` 404 link).
7. **Connection-loss UX** — top-of-viewport disconnect banner,
   stale-data chip on plan/agent timestamps. Resolves §15.
8. **Mobile + accessibility pass** — collapsible sidebar,
   AgentPanel-as-drawer on `<lg`, tap-target minimums; modal
   focus trap, real keyboard support for status menu / driver
   dropdown / merge dropdown / tab bar / `EditableText`;
   icon-only `aria-label` cleanup. Resolves §8 / §9.
9. **Performance: cap output buffer, virtualise long lists,
   memoise selectors** — `agentOutput[id]` ring-buffer at 5000;
   xterm dispose on unselect with `/replay` rehydrate;
   `@tanstack/react-virtual` on PhaseCard tasks and AgentTree;
   `useShallow` selectors. Resolves §10.
10. **Test coverage closure** — Playwright e2e for the README
    golden path (standalone + SaaS); component tests for
    `AdminPage`, `AgentTree`, `ProjectDashboard`, `LoginPage`,
    `EditableText`; store tests for `agent-store`,
    `settings-store`, `auth-store`; ESLint with
    `react-hooks` + `jsx-a11y`. Resolves §11 / §20.

Items beyond the Top 10 that the plan still ships: org admin
tabs (§3 / §18), notification opt-out + batching (§16), settings
ownership split (§3 / §18), missing plan operations (§3),
PhaseColumn dead-code removal (§1), bundle-size budget +
lazy xterm (§20). Internationalisation (§19) is documented but
intentionally out of scope for this plan.
