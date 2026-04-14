//! Statically-defined plan skeletons the New Plan form can pick from.

use axum::{Json, response::IntoResponse};
use serde::Serialize;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Template {
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub placeholder: &'static str,
    pub skeleton: &'static str,
}

pub const TEMPLATES: &[Template] = &[
    Template {
        id: "add-rest-endpoint",
        name: "Add REST endpoint",
        description: "New HTTP route with handler, types, tests, and wiring.",
        placeholder: "Add a GET /api/widgets endpoint that returns all widgets for the current user.",
        skeleton: "Produce a plan with these phases:\n\
            - Phase 0: Design — request/response types, auth, error cases.\n\
            - Phase 1: Implementation — handler function, route registration, DB access.\n\
            - Phase 2: Tests — unit tests for the handler, integration test for the route.\n\
            - Phase 3: Docs — update API reference or OpenAPI schema if present.",
    },
    Template {
        id: "write-tests-for-module",
        name: "Write tests for a module",
        description: "Raise test coverage on an existing module with unit and integration tests.",
        placeholder: "Write tests for the `plan_parser` module, covering parse errors and edge cases.",
        skeleton: "Produce a plan with these phases:\n\
            - Phase 0: Survey — read the module, list public functions and branches, note missing coverage.\n\
            - Phase 1: Unit tests — one task per public function, covering happy path and edge cases.\n\
            - Phase 2: Integration tests — cover how the module is used from callers.\n\
            - Phase 3: CI — ensure new tests run in the existing pipeline.",
    },
    Template {
        id: "refactor-extract-service",
        name: "Refactor to extract service",
        description: "Pull scattered logic into a dedicated service module with a clean interface.",
        placeholder: "Extract notification-sending logic from api/plans.rs into a NotificationService.",
        skeleton: "Produce a plan with these phases:\n\
            - Phase 0: Survey — locate all current call sites and the shapes of data they pass.\n\
            - Phase 1: Design — define the service interface (methods, errors, dependencies).\n\
            - Phase 2: Implementation — create the new module and move logic in.\n\
            - Phase 3: Migration — update call sites, delete old code, verify no dead imports.\n\
            - Phase 4: Tests — unit tests on the service; update existing tests to use it.",
    },
    Template {
        id: "add-database-migration",
        name: "Add database migration",
        description: "Schema change with migration script, model updates, and rollout plan.",
        placeholder: "Add a `notes` TEXT column to the `tasks` table so agents can leave freeform notes.",
        skeleton: "Produce a plan with these phases:\n\
            - Phase 0: Design — columns, types, nullability, indexes, backfill strategy.\n\
            - Phase 1: Migration — write the migration file (up + down if supported).\n\
            - Phase 2: Code — update models, queries, and serialization.\n\
            - Phase 3: Tests — ensure migrations apply cleanly and queries still work.\n\
            - Phase 4: Rollout — note any follow-ups (data backfill, deprecation).",
    },
    Template {
        id: "bug-fix-investigation",
        name: "Investigate and fix a bug",
        description: "Reproduce, diagnose, fix, and add a regression test.",
        placeholder: "Users report that merging an agent branch sometimes leaves the agent in 'running' state.",
        skeleton: "Produce a plan with these phases:\n\
            - Phase 0: Reproduce — minimal steps that trigger the bug locally.\n\
            - Phase 1: Diagnose — read the relevant code paths; identify root cause.\n\
            - Phase 2: Fix — the smallest change that addresses the root cause.\n\
            - Phase 3: Regression test — a test that fails before the fix and passes after.",
    },
    Template {
        id: "add-frontend-component",
        name: "Add frontend component",
        description: "New React component with state, styles, and integration into a page.",
        placeholder: "Add a CostChart component to the plan detail page that shows cost by phase.",
        skeleton: "Produce a plan with these phases:\n\
            - Phase 0: Design — component props, data source, states (loading, empty, error).\n\
            - Phase 1: Implementation — component file, styles, any new store bindings.\n\
            - Phase 2: Integration — mount it in the target page, wire real data.\n\
            - Phase 3: Polish — accessibility, empty states, responsive behavior.",
    },
];

pub fn find(id: &str) -> Option<&'static Template> {
    TEMPLATES.iter().find(|t| t.id == id)
}

// ── GET /api/templates ──────────────────────────────────────────────────────

pub async fn list_templates() -> impl IntoResponse {
    Json(TEMPLATES)
}
