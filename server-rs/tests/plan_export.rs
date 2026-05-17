//! E2E tests for `GET /api/plans/:name/export` (Phase 1, Task 1.1).
//!
//! Covers:
//! - The exported body starts with the canonical
//!   `# branchwork-plan-export: v1\n# exported-at: <iso8601>\n` header.
//! - `Content-Type: application/yaml` and `Content-Disposition: attachment;
//!   filename="<name>.plan.yaml"` are set.
//! - The body re-parses through `parse_plan_file` without modification
//!   (the headline acceptance criterion).
//! - Markdown-source plans are emitted via the canonical YAML serializer
//!   and still round-trip.
//! - Unknown plans 404 with `{"error": "Plan not found"}`.
//!
//! The dashboard's `http()` helper in `tests/support/mod.rs` parses
//! responses as JSON, so this file shells out to `curl -i` directly when it
//! needs to inspect response headers.

mod support;

use std::process::Command;
use support::TestDashboard;

fn minimal_yaml_plan(name: &str, project_dir: &std::path::Path) -> String {
    format!(
        concat!(
            "title: {name}\n",
            "project: {project}\n",
            "context: |\n",
            "  Sample plan for the export round-trip test.\n",
            "phases:\n",
            "  - number: 1\n",
            "    title: Phase 1\n",
            "    description: ''\n",
            "    tasks:\n",
            "      - number: '1.1'\n",
            "        title: Task 1.1\n",
            "        description: First task\n",
            "        acceptance: It works.\n",
        ),
        name = name,
        project = project_dir.display(),
    )
}

/// Fetch `url` with `curl -i` and split into (status, headers, body).
fn curl_with_headers(url: &str) -> (u16, Vec<(String, String)>, String) {
    let out = Command::new("curl")
        .args(["-sS", "-i", "-X", "GET", url])
        .output()
        .unwrap_or_else(|e| panic!("curl: {e}"));
    let raw = String::from_utf8_lossy(&out.stdout).to_string();

    // Split header block from body. curl emits headers terminated by either
    // `\r\n\r\n` (canonical HTTP) or `\n\n` depending on platform; tolerate
    // both.
    let (header_block, body) = raw
        .split_once("\r\n\r\n")
        .or_else(|| raw.split_once("\n\n"))
        .unwrap_or((raw.as_str(), ""));

    let mut lines = header_block.lines();
    let status_line = lines.next().unwrap_or("");
    // "HTTP/1.1 200 OK" — take the second whitespace-separated token.
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let mut headers = Vec::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_lowercase(), v.trim().to_string()));
        }
    }
    (status, headers, body.to_string())
}

fn header(headers: &[(String, String)], name: &str) -> Option<String> {
    let needle = name.to_lowercase();
    headers
        .iter()
        .find(|(k, _)| *k == needle)
        .map(|(_, v)| v.clone())
}

#[test]
fn export_returns_yaml_body_with_header_and_attachment_headers() {
    let d = TestDashboard::new();
    let name = "export-headers";
    d.create_plan(name, &minimal_yaml_plan(name, &d.project));

    let (status, headers, body) =
        curl_with_headers(&format!("{}/api/plans/{name}/export", d.base_url));
    assert_eq!(status, 200, "body: {body}");

    let ct = header(&headers, "content-type").unwrap_or_default();
    assert!(
        ct.starts_with("application/yaml"),
        "Content-Type was {ct:?}, expected application/yaml"
    );

    let disp = header(&headers, "content-disposition").unwrap_or_default();
    assert!(
        disp.contains(&format!(r#"filename="{name}.plan.yaml""#)),
        "Content-Disposition was {disp:?}"
    );
    assert!(
        disp.starts_with("attachment"),
        "Content-Disposition must be attachment, got {disp:?}"
    );

    // Header preamble: two comment lines, in order.
    assert!(
        body.starts_with("# branchwork-plan-export: v1\n"),
        "body must start with version line, got first 80 chars: {:?}",
        &body.chars().take(80).collect::<String>()
    );
    let second_line = body.lines().nth(1).unwrap_or("");
    assert!(
        second_line.starts_with("# exported-at: "),
        "second line was {second_line:?}, expected # exported-at: <iso8601>"
    );

    // Timestamp must be a parseable RFC3339 / ISO8601 string.
    let ts = second_line.trim_start_matches("# exported-at: ");
    chrono::DateTime::parse_from_rfc3339(ts)
        .unwrap_or_else(|e| panic!("exported-at not RFC3339: {ts:?}: {e}"));
}

#[test]
fn exported_body_reparses_through_plan_loader_without_modification() {
    // The headline acceptance criterion: the YAML the endpoint emits must
    // load again through the existing plan loader. We assert this by
    // writing the exported body into the plans dir under a new name and
    // re-fetching it via GET /api/plans/<new>, which exercises
    // `parse_plan_file` end-to-end.
    let d = TestDashboard::new();
    let name = "export-roundtrip";
    d.create_plan(name, &minimal_yaml_plan(name, &d.project));

    let (status, _, body) = curl_with_headers(&format!("{}/api/plans/{name}/export", d.base_url));
    assert_eq!(status, 200, "export status: {status}, body: {body}");

    // Round-trip: write the body verbatim as a new plan and load it.
    let new_name = "export-roundtrip-loaded";
    let plans_dir = d.plans_dir.clone();
    std::fs::write(plans_dir.join(format!("{new_name}.yaml")), &body).unwrap();

    let (status, value) = d.get(&format!("/api/plans/{new_name}"));
    assert_eq!(
        status, 200,
        "reload of exported YAML failed: status={status} body={value}"
    );
    assert_eq!(value["title"], name, "title mismatch: {value}");
    assert_eq!(value["phases"].as_array().unwrap().len(), 1);
    assert_eq!(value["phases"][0]["tasks"].as_array().unwrap().len(), 1);
    assert_eq!(value["phases"][0]["tasks"][0]["number"], "1.1");

    // Also verify the raw body is a parseable YAML mapping at the serde
    // level — catches header-comment encoding regressions independent of
    // the dashboard loader's tolerance.
    let v: serde_yaml::Value = serde_yaml::from_str(&body)
        .unwrap_or_else(|e| panic!("exported body is not valid YAML: {e}\nbody:\n{body}"));
    assert!(v.is_mapping(), "exported body is not a YAML mapping");
    assert_eq!(v.get("title").and_then(|t| t.as_str()), Some(name));
}

#[test]
fn export_markdown_plan_emits_canonical_yaml() {
    // Plans authored as .md files have no on-disk YAML, so the handler
    // re-serializes via `serialize_plan_yaml`. Confirm the markdown source
    // still survives the export + reload round-trip.
    let d = TestDashboard::new();
    let name = "export-md-source";
    let md = format!(
        "# Sample Markdown Plan\n\
         \n\
         project: {project}\n\
         \n\
         ## Phase 1: Phase One\n\
         \n\
         ### Task 1.1: First task\n\
         \n\
         Acceptance: It works.\n",
        project = d.project.display(),
    );
    std::fs::write(d.plans_dir.join(format!("{name}.md")), &md).unwrap();

    let (status, headers, body) =
        curl_with_headers(&format!("{}/api/plans/{name}/export", d.base_url));
    assert_eq!(status, 200, "body: {body}");

    let disp = header(&headers, "content-disposition").unwrap_or_default();
    assert!(
        disp.contains(&format!(r#"filename="{name}.plan.yaml""#)),
        "Content-Disposition was {disp:?}"
    );

    assert!(
        body.starts_with("# branchwork-plan-export: v1\n"),
        "markdown export missing header line, first 80 chars: {:?}",
        &body.chars().take(80).collect::<String>()
    );

    let v: serde_yaml::Value = serde_yaml::from_str(&body)
        .unwrap_or_else(|e| panic!("markdown-export body is not valid YAML: {e}\nbody:\n{body}"));
    assert!(v.is_mapping());
    assert!(
        v.get("phases").map(|p| p.is_sequence()).unwrap_or(false),
        "phases not a sequence in markdown export: {body}"
    );
}

#[test]
fn export_unknown_plan_returns_404() {
    let d = TestDashboard::new();
    let (status, _, body) =
        curl_with_headers(&format!("{}/api/plans/does-not-exist/export", d.base_url));
    assert_eq!(status, 404, "body: {body}");
    let parsed: serde_json::Value = serde_json::from_str(&body)
        .unwrap_or_else(|e| panic!("404 body is not JSON: {e} (body: {body})"));
    assert_eq!(parsed["error"], "Plan not found");
}
