//! E2E tests for `POST /api/plans/export-bundle` (Phase 1, Task 1.3).
//!
//! Covers:
//! - The headline acceptance: pick 3 plans, POST the bundle, extract the
//!   zip, every .plan.yaml round-trips through 1.1's loader (GET
//!   /api/plans/<new>) AND the manifest's sha256 per-entry matches a
//!   freshly-computed digest of the unzipped bytes.
//! - `Content-Type: application/zip` + `Content-Disposition: attachment;
//!   filename="branchwork-plans-<utc-stamp>.zip"`.
//! - 400 on empty `names` array, duplicate names, missing-name plan
//!   (404).
//! - A bundle entry's `# branchwork-plan-export: v1` header line is
//!   byte-identical to what `GET /api/plans/:name/export` would have
//!   emitted (excluding `exported_at`, which advances per call).

mod support;

use std::fmt::Write as _;
use std::io::Read;
use std::process::Command;
use support::TestDashboard;

fn minimal_yaml_plan(name: &str, project_dir: &std::path::Path) -> String {
    format!(
        concat!(
            "title: {name}\n",
            "project: {project}\n",
            "context: |\n",
            "  Sample plan for the export bundle test.\n",
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

/// POST `url` with JSON body, save raw response body to disk (binary-safe),
/// and parse `-w` status + headers from a sidecar file. Returns
/// `(status, headers, body_bytes)`.
///
/// We can't reuse `support::http()` here because that helper assumes a JSON
/// body and would corrupt zip bytes during the lossy UTF-8 conversion.
fn curl_post_binary(url: &str, json_body: &str) -> (u16, Vec<(String, String)>, Vec<u8>) {
    let dir = tempfile::tempdir().expect("tempdir");
    let body_path = dir.path().join("body.bin");
    let headers_path = dir.path().join("headers.txt");

    let out = Command::new("curl")
        .args([
            "-sS",
            "-X",
            "POST",
            "-H",
            "Content-Type: application/json",
            "-o",
        ])
        .arg(&body_path)
        .args(["-D"])
        .arg(&headers_path)
        .args(["-w", "%{http_code}", "-d", json_body, url])
        .output()
        .unwrap_or_else(|e| panic!("curl: {e}"));
    let status_str = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let status: u16 = status_str.parse().unwrap_or(0);
    let body = std::fs::read(&body_path).unwrap_or_default();
    let header_text = std::fs::read_to_string(&headers_path).unwrap_or_default();

    let mut headers = Vec::new();
    for line in header_text.lines().skip(1) {
        // skip the "HTTP/x 200 OK" status line
        if line.is_empty() {
            continue;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_lowercase(), v.trim().to_string()));
        }
    }
    (status, headers, body)
}

fn header_value(headers: &[(String, String)], name: &str) -> Option<String> {
    let needle = name.to_lowercase();
    headers
        .iter()
        .find(|(k, _)| *k == needle)
        .map(|(_, v)| v.clone())
}

/// Read a zip archive from memory into a {filename -> bytes} map. Forces an
/// owned `Vec<u8>` per entry so the archive can be dropped before the test
/// asserts on the bytes.
fn unzip_to_map(bytes: &[u8]) -> std::collections::BTreeMap<String, Vec<u8>> {
    let cursor = std::io::Cursor::new(bytes.to_vec());
    let mut z =
        zip::ZipArchive::new(cursor).unwrap_or_else(|e| panic!("ZipArchive::new failed: {e}"));
    let mut out = std::collections::BTreeMap::new();
    for i in 0..z.len() {
        let mut entry = z.by_index(i).expect("zip by_index");
        let name = entry.name().to_string();
        let mut data = Vec::new();
        entry.read_to_end(&mut data).expect("zip read");
        out.insert(name, data);
    }
    out
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    let mut h = sha2::Sha256::new();
    h.update(bytes);
    let digest = h.finalize();
    let mut s = String::with_capacity(digest.len() * 2);
    for b in digest {
        let _ = write!(&mut s, "{b:02x}");
    }
    s
}

#[test]
fn three_plan_bundle_round_trips_yamls_and_manifest_hashes_match() {
    let d = TestDashboard::new();
    let names = ["bundle-alpha", "bundle-bravo", "bundle-charlie"];
    for n in &names {
        d.create_plan(n, &minimal_yaml_plan(n, &d.project));
    }

    let body = serde_json::json!({"names": names.to_vec()}).to_string();
    let (status, headers, zip_bytes) =
        curl_post_binary(&format!("{}/api/plans/export-bundle", d.base_url), &body);
    assert_eq!(status, 200, "non-200: {status}");

    let ct = header_value(&headers, "content-type").unwrap_or_default();
    assert!(
        ct.starts_with("application/zip"),
        "Content-Type was {ct:?}, expected application/zip"
    );

    let disp = header_value(&headers, "content-disposition").unwrap_or_default();
    assert!(
        disp.starts_with("attachment"),
        "Content-Disposition must be attachment, got {disp:?}"
    );
    assert!(
        disp.contains("filename=\"branchwork-plans-"),
        "expected dated filename, got {disp:?}"
    );
    assert!(
        disp.contains(".zip\""),
        "filename must end .zip, got {disp:?}"
    );

    // Extract — 3 YAML entries + 1 manifest.yaml = 4 total.
    let entries = unzip_to_map(&zip_bytes);
    assert_eq!(
        entries.len(),
        4,
        "expected 4 entries, got: {:?}",
        entries.keys().collect::<Vec<_>>()
    );
    for n in &names {
        let key = format!("{n}.plan.yaml");
        assert!(entries.contains_key(&key), "missing entry: {key}");
    }
    assert!(
        entries.contains_key("manifest.yaml"),
        "missing manifest.yaml"
    );

    // Manifest shape + hash equality.
    let manifest_yaml =
        std::str::from_utf8(&entries["manifest.yaml"]).expect("manifest.yaml is utf8");
    let manifest: serde_yaml::Value = serde_yaml::from_str(manifest_yaml)
        .unwrap_or_else(|e| panic!("manifest parse: {e}\n{manifest_yaml}"));
    let mapping = manifest.as_mapping().expect("manifest is a mapping");
    assert_eq!(
        mapping
            .get(serde_yaml::Value::String(
                "branchwork_plan_export".to_string()
            ))
            .and_then(|v| v.as_str()),
        Some("v1"),
        "manifest version mismatch: {manifest_yaml}"
    );
    let exported_at = mapping
        .get(serde_yaml::Value::String("exported_at".to_string()))
        .and_then(|v| v.as_str())
        .expect("manifest.exported_at missing");
    chrono::DateTime::parse_from_rfc3339(exported_at)
        .unwrap_or_else(|e| panic!("manifest exported_at not RFC3339 ({exported_at:?}): {e}"));

    let entries_node = mapping
        .get(serde_yaml::Value::String("entries".to_string()))
        .and_then(|v| v.as_sequence())
        .expect("manifest.entries missing");
    assert_eq!(
        entries_node.len(),
        3,
        "manifest.entries should list 3 plans"
    );

    for entry in entries_node {
        let m = entry.as_mapping().expect("entry is mapping");
        let name = m
            .get(serde_yaml::Value::String("name".to_string()))
            .and_then(|v| v.as_str())
            .expect("entry.name");
        let filename = m
            .get(serde_yaml::Value::String("filename".to_string()))
            .and_then(|v| v.as_str())
            .expect("entry.filename");
        let sha = m
            .get(serde_yaml::Value::String("sha256".to_string()))
            .and_then(|v| v.as_str())
            .expect("entry.sha256");
        assert_eq!(
            filename,
            format!("{name}.plan.yaml"),
            "filename derived from name"
        );

        // Acceptance: sha256 in manifest matches the unzipped bytes.
        let yaml_bytes = entries
            .get(filename)
            .unwrap_or_else(|| panic!("manifest cites missing entry: {filename}"));
        assert_eq!(
            sha,
            sha256_hex(yaml_bytes),
            "manifest sha256 ({sha}) does not match unzipped bytes for {filename}"
        );

        // Per-entry header echoes the bundle wire shape.
        let body = std::str::from_utf8(yaml_bytes).expect("yaml utf8");
        assert!(
            body.starts_with("# branchwork-plan-export: v1\n"),
            "entry missing version header line: {filename}"
        );
        let second = body.lines().nth(1).unwrap_or("");
        assert!(
            second.starts_with("# exported-at: "),
            "entry second line wrong: {second:?}"
        );
    }

    // Acceptance (1.1 round-trip): write each unzipped YAML back into the
    // plans dir under a fresh name and fetch via GET /api/plans/<new>.
    for n in &names {
        let yaml_bytes = entries
            .get(&format!("{n}.plan.yaml"))
            .expect("entry present");
        let new_name = format!("{n}-loaded");
        std::fs::write(d.plans_dir.join(format!("{new_name}.yaml")), yaml_bytes).unwrap();
        let (status, value) = d.get(&format!("/api/plans/{new_name}"));
        assert_eq!(
            status, 200,
            "reload of unzipped YAML failed for {n}: status={status} body={value}"
        );
        assert_eq!(value["title"], *n, "title mismatch: {value}");
        assert_eq!(value["phases"].as_array().unwrap().len(), 1);
        assert_eq!(value["phases"][0]["tasks"].as_array().unwrap().len(), 1);
        assert_eq!(value["phases"][0]["tasks"][0]["number"], "1.1");
    }
}

#[test]
fn bundle_entry_yaml_matches_single_plan_export_body() {
    // The bundle's per-plan bytes (modulo the exported-at timestamp) MUST
    // equal what GET /api/plans/:name/export emits. This is the canonical
    // "byte-identical to 1.1" property — bundle readers can re-run 1.1's
    // loader on any individual entry and get the same result as if a user
    // had downloaded that plan alone.
    let d = TestDashboard::new();
    let name = "bundle-parity";
    d.create_plan(name, &minimal_yaml_plan(name, &d.project));

    // Single-plan export.
    let single = Command::new("curl")
        .args(["-sS", "-X", "GET"])
        .arg(format!("{}/api/plans/{name}/export", d.base_url))
        .output()
        .expect("curl single");
    let single_body = String::from_utf8(single.stdout).expect("single utf8");

    // Bundle export, single entry.
    let body = serde_json::json!({"names": [name]}).to_string();
    let (status, _, zip_bytes) =
        curl_post_binary(&format!("{}/api/plans/export-bundle", d.base_url), &body);
    assert_eq!(status, 200);
    let entries = unzip_to_map(&zip_bytes);
    let bundle_entry = entries
        .get(&format!("{name}.plan.yaml"))
        .expect("entry present");
    let bundle_body = std::str::from_utf8(bundle_entry).expect("entry utf8");

    // The exported-at timestamps differ between the two calls; everything
    // else must match line-for-line.
    let stripped_single = strip_exported_at(&single_body);
    let stripped_bundle = strip_exported_at(bundle_body);
    assert_eq!(
        stripped_single, stripped_bundle,
        "bundle entry diverged from single-plan export (excluding timestamp)"
    );
}

fn strip_exported_at(body: &str) -> String {
    body.lines()
        .filter(|l| !l.starts_with("# exported-at: "))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn empty_names_returns_400_with_names_required() {
    let d = TestDashboard::new();
    let (status, value) = d.post("/api/plans/export-bundle", serde_json::json!({"names": []}));
    assert_eq!(status, 400, "body: {value}");
    assert_eq!(
        value["error"], "names_required",
        "unexpected error: {value}"
    );
}

#[test]
fn duplicate_name_returns_400_with_duplicate_name() {
    let d = TestDashboard::new();
    let name = "bundle-dupe";
    d.create_plan(name, &minimal_yaml_plan(name, &d.project));

    let (status, value) = d.post(
        "/api/plans/export-bundle",
        serde_json::json!({"names": [name, name]}),
    );
    assert_eq!(status, 400, "body: {value}");
    assert_eq!(value["error"], "duplicate_name");
    assert_eq!(value["name"], name);
}

#[test]
fn unknown_plan_in_bundle_returns_404() {
    let d = TestDashboard::new();
    let known = "bundle-known";
    d.create_plan(known, &minimal_yaml_plan(known, &d.project));

    let (status, value) = d.post(
        "/api/plans/export-bundle",
        serde_json::json!({"names": [known, "does-not-exist"]}),
    );
    assert_eq!(status, 404, "body: {value}");
    assert_eq!(value["error"], "Plan not found");
    assert_eq!(value["name"], "does-not-exist");
}

#[test]
fn missing_names_field_returns_400() {
    // serde_default makes names: Vec::new(), which falls through to the
    // names_required guard.
    let d = TestDashboard::new();
    let (status, value) = d.post("/api/plans/export-bundle", serde_json::json!({}));
    assert_eq!(status, 400, "body: {value}");
    assert_eq!(value["error"], "names_required");
}
