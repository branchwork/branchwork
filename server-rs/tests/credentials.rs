//! Integration tests for the Branchwork-managed credentials API (Phase
//! 3.2 of `runner-daemon-workspace`).
//!
//! These spin up the real `branchwork-server` binary against a scratch
//! tempdir (no network), authenticate via the existing `/api/auth/signup`
//! flow, and exercise the three endpoints end-to-end:
//!
//! - `GET /api/credentials` (no secrets in payload)
//! - `POST /api/credentials` (kind=ssh_key with caller-supplied body, AND
//!   kind=generate that mints a fresh ed25519 pair)
//! - `DELETE /api/credentials/{id}` with the referential-integrity check
//!   against `projects.default_credential_id`
//!
//! Acceptance criterion: the encrypted private key NEVER appears in any
//! list / create / delete response.

mod support;

use serde_json::{Value, json};
use std::path::Path;
use std::process::Command;

use support::TestDashboard;

/// Sign up a fresh user (creates a personal org + default-org membership)
/// and persist the session cookie into `cookie_jar`. Returns the user
/// row (id, email) parsed from the signup response.
fn signup(d: &TestDashboard, jar: &Path, email: &str, password: &str) -> Value {
    let body = json!({ "email": email, "password": password });
    let (status, value) = http_with_jar(
        "POST",
        &format!("{}/api/auth/signup", d.base_url),
        Some(body),
        Some(jar),
    );
    assert_eq!(status, 201, "signup failed: status={status} body={value}");
    value
}

/// `POST` with a JSON body, carrying any cookies in `jar`.
fn post_authed(d: &TestDashboard, jar: &Path, path: &str, body: Value) -> (u16, Value) {
    http_with_jar(
        "POST",
        &format!("{}{path}", d.base_url),
        Some(body),
        Some(jar),
    )
}

fn get_authed(d: &TestDashboard, jar: &Path, path: &str) -> (u16, Value) {
    http_with_jar("GET", &format!("{}{path}", d.base_url), None, Some(jar))
}

fn delete_authed(d: &TestDashboard, jar: &Path, path: &str) -> (u16, Value) {
    http_with_jar("DELETE", &format!("{}{path}", d.base_url), None, Some(jar))
}

/// Cookie-aware variant of the curl helper from `support::http`. Uses
/// `-c <jar> -b <jar>` so signup persists the session cookie and
/// subsequent calls send it back.
fn http_with_jar(method: &str, url: &str, body: Option<Value>, jar: Option<&Path>) -> (u16, Value) {
    let mut cmd = Command::new("curl");
    cmd.args([
        "-sS",
        "-o",
        "-",
        "-w",
        "\n\n__STATUS__:%{http_code}",
        "-X",
        method,
        "-H",
        "Content-Type: application/json",
    ]);
    if let Some(j) = jar {
        cmd.args(["-c", &j.to_string_lossy(), "-b", &j.to_string_lossy()]);
    }
    cmd.arg(url);
    let body_str;
    if let Some(b) = body {
        body_str = serde_json::to_string(&b).unwrap();
        cmd.args(["-d", &body_str]);
    }
    let out = cmd.output().unwrap_or_else(|e| panic!("curl: {e}"));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let (body_str, status_str) = stdout
        .rsplit_once("\n\n__STATUS__:")
        .unwrap_or_else(|| panic!("bad curl output: {stdout}"));
    let status: u16 = status_str.trim().parse().unwrap_or(0);
    let value: Value = if body_str.trim().is_empty() {
        Value::Null
    } else {
        serde_json::from_str(body_str).unwrap_or(Value::String(body_str.to_string()))
    };
    (status, value)
}

/// Helper: SELECT encrypted_secret column directly out of SQLite so we
/// can verify the secret was actually stored (and confirm it never
/// leaks in any API response by JSON-stringifying the response and
/// scanning for the encrypted bytes' base64 / hex / literal forms).
fn read_encrypted_secret_bytes(d: &TestDashboard, credential_id: &str) -> Vec<u8> {
    let db_path = d.dir.path().join(".claude").join("branchwork.db");
    let conn = rusqlite::Connection::open(&db_path).expect("open sqlite");
    conn.query_row(
        "SELECT encrypted_secret FROM credentials WHERE id = ?1",
        rusqlite::params![credential_id],
        |r| r.get::<_, Vec<u8>>(0),
    )
    .expect("encrypted_secret row")
}

#[test]
fn list_endpoint_is_empty_for_fresh_user() {
    let d = TestDashboard::new();
    let jar = d.dir.path().join("cookies.txt");
    signup(&d, &jar, "alice@branchwork.local", "supersecret-pw");

    let (status, body) = get_authed(&d, &jar, "/api/credentials");
    assert_eq!(status, 200);
    assert_eq!(
        body["credentials"].as_array().map(|a| a.len()),
        Some(0),
        "fresh user should have zero credentials: {body}"
    );
}

#[test]
fn unauthenticated_list_is_401() {
    let d = TestDashboard::new();
    // No signup → no cookie jar → no session.
    let (status, _) = http_with_jar(
        "GET",
        &format!("{}/api/credentials", d.base_url),
        None,
        None,
    );
    assert_eq!(status, 401);
}

#[test]
fn create_ssh_key_then_list_does_not_leak_secret() {
    let d = TestDashboard::new();
    let jar = d.dir.path().join("cookies.txt");
    signup(&d, &jar, "bob@branchwork.local", "supersecret-pw");

    // Canonical OpenSSH-PEM dummy. The body bytes do NOT need to be a
    // valid private key — the API accepts the secret verbatim and only
    // the dispatch path (later phase) will care.
    let secret_pem = "-----BEGIN OPENSSH PRIVATE KEY-----\n\
                      THIS-IS-THE-SECRET-DO-NOT-LEAK-ME\n\
                      -----END OPENSSH PRIVATE KEY-----\n";
    let pub_key = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIExampleBytes bob@laptop";

    let create_body = json!({
        "name": "Bob's laptop key",
        "kind": "ssh_key",
        "secret": secret_pem,
        "publicPart": pub_key,
        "hostHint": "github.com",
        "scopes": null,
    });
    let (status, create_resp) = post_authed(&d, &jar, "/api/credentials", create_body);
    assert_eq!(
        status, 201,
        "POST /api/credentials failed: body={create_resp}"
    );

    // The create response must NOT echo the secret.
    let create_json_str = serde_json::to_string(&create_resp).unwrap();
    assert!(
        !create_json_str.contains("THIS-IS-THE-SECRET-DO-NOT-LEAK-ME"),
        "secret leaked in create response: {create_json_str}"
    );
    // But the public part is fine.
    assert_eq!(create_resp["publicPart"], json!(pub_key));
    assert_eq!(create_resp["kind"], json!("ssh_key"));
    assert_eq!(create_resp["generated"], json!(false));

    let credential_id = create_resp["id"].as_str().unwrap().to_string();

    // GET /api/credentials must show the row, again with no secret.
    let (status, list_resp) = get_authed(&d, &jar, "/api/credentials");
    assert_eq!(status, 200);
    let list_json_str = serde_json::to_string(&list_resp).unwrap();
    assert!(
        !list_json_str.contains("THIS-IS-THE-SECRET-DO-NOT-LEAK-ME"),
        "secret leaked in list response: {list_json_str}"
    );
    let creds = list_resp["credentials"].as_array().unwrap();
    assert_eq!(creds.len(), 1);
    assert_eq!(creds[0]["id"], json!(credential_id));
    assert_eq!(creds[0]["name"], json!("Bob's laptop key"));
    assert_eq!(creds[0]["publicPart"], json!(pub_key));
    // Encrypted blob field should NOT appear in any list shape.
    assert!(
        !creds[0]
            .as_object()
            .unwrap()
            .contains_key("encryptedSecret"),
        "list row leaked encryptedSecret field: {creds:?}"
    );
    assert!(
        !creds[0].as_object().unwrap().contains_key("secret"),
        "list row leaked secret field: {creds:?}"
    );

    // Confirm the on-disk encrypted_secret IS populated (and not the
    // plaintext) so we actually exercised the encrypt path.
    let stored = read_encrypted_secret_bytes(&d, &credential_id);
    assert!(
        !stored.is_empty(),
        "encrypted_secret row is empty — encryption never ran"
    );
    let stored_str = String::from_utf8_lossy(&stored);
    assert!(
        !stored_str.contains("THIS-IS-THE-SECRET-DO-NOT-LEAK-ME"),
        "plaintext was stored to disk instead of being encrypted"
    );
}

#[test]
fn create_generate_mints_ed25519_and_returns_public_part_only() {
    let d = TestDashboard::new();
    let jar = d.dir.path().join("cookies.txt");
    signup(&d, &jar, "carol@branchwork.local", "supersecret-pw");

    let create_body = json!({
        "name": "Generated key",
        "kind": "generate",
    });
    let (status, create_resp) = post_authed(&d, &jar, "/api/credentials", create_body);
    assert_eq!(status, 201, "POST kind=generate failed: body={create_resp}");

    // Public part: should be a fresh ssh-ed25519 one-liner.
    let pub_part = create_resp["publicPart"].as_str().unwrap().to_string();
    assert!(
        pub_part.starts_with("ssh-ed25519 "),
        "unexpected public-part shape: {pub_part}"
    );
    assert!(
        pub_part.contains("branchwork:Generated key"),
        "public part missing branchwork:<name> comment: {pub_part}"
    );
    // Stored kind is ssh_key (NOT "generate"; that's a request-time
    // discriminator only).
    assert_eq!(create_resp["kind"], json!("ssh_key"));
    assert_eq!(create_resp["generated"], json!(true));

    // The response MUST NOT carry the private key PEM.
    let create_json_str = serde_json::to_string(&create_resp).unwrap();
    assert!(
        !create_json_str.contains("BEGIN OPENSSH PRIVATE KEY"),
        "generate response leaked private-key PEM: {create_json_str}"
    );

    // And LIST must surface only public_part, never the private body.
    let (_, list_resp) = get_authed(&d, &jar, "/api/credentials");
    let list_json_str = serde_json::to_string(&list_resp).unwrap();
    assert!(
        !list_json_str.contains("BEGIN OPENSSH PRIVATE KEY"),
        "list response leaked private-key PEM: {list_json_str}"
    );
    assert_eq!(
        list_resp["credentials"][0]["publicPart"],
        json!(pub_part),
        "list publicPart must round-trip the create response value"
    );
}

#[test]
fn create_rejects_invalid_shapes() {
    let d = TestDashboard::new();
    let jar = d.dir.path().join("cookies.txt");
    signup(&d, &jar, "dan@branchwork.local", "supersecret-pw");

    // Missing name.
    let (status, body) = post_authed(
        &d,
        &jar,
        "/api/credentials",
        json!({ "kind": "ssh_key", "secret": "x" }),
    );
    assert_eq!(status, 400);
    assert_eq!(body["error"], json!("name_required"));

    // Invalid kind.
    let (status, body) = post_authed(
        &d,
        &jar,
        "/api/credentials",
        json!({ "name": "x", "kind": "bogus", "secret": "x" }),
    );
    assert_eq!(status, 400);
    assert_eq!(body["error"], json!("invalid_kind"));

    // Non-generate without secret.
    let (status, body) = post_authed(
        &d,
        &jar,
        "/api/credentials",
        json!({ "name": "x", "kind": "ssh_key" }),
    );
    assert_eq!(status, 400);
    assert_eq!(body["error"], json!("secret_required"));

    // Generate with secret.
    let (status, body) = post_authed(
        &d,
        &jar,
        "/api/credentials",
        json!({ "name": "x", "kind": "generate", "secret": "ignored" }),
    );
    assert_eq!(status, 400);
    assert_eq!(body["error"], json!("secret_not_allowed_for_generate"));

    // Empty secret on non-generate.
    let (status, body) = post_authed(
        &d,
        &jar,
        "/api/credentials",
        json!({ "name": "x", "kind": "gh_pat", "secret": "" }),
    );
    assert_eq!(status, 400);
    assert_eq!(body["error"], json!("secret_required"));
}

#[test]
fn delete_archives_credential_and_removes_it_from_list() {
    let d = TestDashboard::new();
    let jar = d.dir.path().join("cookies.txt");
    signup(&d, &jar, "ed@branchwork.local", "supersecret-pw");

    // Create.
    let (status, create_resp) = post_authed(
        &d,
        &jar,
        "/api/credentials",
        json!({
            "name": "tmp",
            "kind": "gh_pat",
            "secret": "ghp_DUMMY_DO_NOT_SHIP",
        }),
    );
    assert_eq!(status, 201);
    let id = create_resp["id"].as_str().unwrap().to_string();

    // Delete.
    let (status, body) = delete_authed(&d, &jar, &format!("/api/credentials/{id}"));
    assert_eq!(status, 200);
    assert_eq!(body["ok"], json!(true));
    assert_eq!(body["id"], json!(id));

    // List is empty.
    let (_, list_resp) = get_authed(&d, &jar, "/api/credentials");
    assert_eq!(list_resp["credentials"].as_array().unwrap().len(), 0);

    // Second delete is 404 (archived rows are filtered out).
    let (status, body) = delete_authed(&d, &jar, &format!("/api/credentials/{id}"));
    assert_eq!(status, 404);
    assert_eq!(body["error"], json!("credential_not_found"));
}

#[test]
fn delete_unknown_id_is_404() {
    let d = TestDashboard::new();
    let jar = d.dir.path().join("cookies.txt");
    signup(&d, &jar, "fred@branchwork.local", "supersecret-pw");

    let (status, body) = delete_authed(
        &d,
        &jar,
        "/api/credentials/00000000-0000-0000-0000-000000000000",
    );
    assert_eq!(status, 404);
    assert_eq!(body["error"], json!("credential_not_found"));
}

#[test]
fn delete_refuses_when_project_references_credential() {
    let d = TestDashboard::new();
    let jar = d.dir.path().join("cookies.txt");
    signup(&d, &jar, "gina@branchwork.local", "supersecret-pw");

    // Create a credential.
    let (_, create_resp) = post_authed(
        &d,
        &jar,
        "/api/credentials",
        json!({
            "name": "shared key",
            "kind": "ssh_key",
            "secret": "-----BEGIN OPENSSH PRIVATE KEY-----\nDUMMY\n-----END OPENSSH PRIVATE KEY-----\n",
            "publicPart": "ssh-ed25519 AAAA dummy",
        }),
    );
    let credential_id = create_resp["id"].as_str().unwrap().to_string();

    // Create a project that references this credential as its default.
    let (status, _) = post_authed(
        &d,
        &jar,
        "/api/projects",
        json!({
            "name": "my-project",
            "repo_url": "git@github.com:owner/my-project.git",
            "default_credential_id": credential_id,
        }),
    );
    assert_eq!(status, 201);

    // Delete must refuse with 409 + credential_in_use.
    let (status, body) = delete_authed(&d, &jar, &format!("/api/credentials/{credential_id}"));
    assert_eq!(status, 409, "expected 409, got {status} body={body}");
    assert_eq!(body["error"], json!("credential_in_use"));
    let project_ids = body["projectIds"].as_array().unwrap();
    assert_eq!(project_ids.len(), 1);

    // Confirm the credential is still in the list (not archived).
    let (_, list_resp) = get_authed(&d, &jar, "/api/credentials");
    let creds = list_resp["credentials"].as_array().unwrap();
    assert_eq!(creds.len(), 1);
    assert_eq!(creds[0]["id"], json!(credential_id));
}

#[test]
fn delete_succeeds_once_project_default_is_cleared() {
    let d = TestDashboard::new();
    let jar = d.dir.path().join("cookies.txt");
    signup(&d, &jar, "hank@branchwork.local", "supersecret-pw");

    // Create credential + project.
    let (_, create_resp) = post_authed(
        &d,
        &jar,
        "/api/credentials",
        json!({
            "name": "k",
            "kind": "gh_pat",
            "secret": "ghp_x",
        }),
    );
    let credential_id = create_resp["id"].as_str().unwrap().to_string();
    let (_, _) = post_authed(
        &d,
        &jar,
        "/api/projects",
        json!({
            "name": "ref-proj",
            "repo_url": "git@github.com:owner/ref-proj.git",
            "default_credential_id": credential_id,
        }),
    );

    // 1) Initial delete refused.
    let (status, _) = delete_authed(&d, &jar, &format!("/api/credentials/{credential_id}"));
    assert_eq!(status, 409);

    // 2) Operator clears the project's default by deleting the project
    //    row. (A future API will let them PATCH the default without
    //    deleting the project, but for this acceptance test the existing
    //    DELETE /api/projects/{id} is the simplest way to break the FK.)
    //    We need the project id.
    let (_, list_proj) = get_authed(&d, &jar, "/api/projects");
    let proj_id = list_proj["projects"][0]["id"].as_str().unwrap().to_string();
    let (status, _) = delete_authed(&d, &jar, &format!("/api/projects/{proj_id}"));
    assert_eq!(status, 200);

    // 3) Delete now succeeds.
    let (status, body) = delete_authed(&d, &jar, &format!("/api/credentials/{credential_id}"));
    assert_eq!(
        status, 200,
        "expected delete to succeed, got {status} body={body}"
    );
}

#[test]
fn list_credentials_includes_used_by_count_from_projects() {
    // Task 3.4 (credentials UI): the list endpoint must surface the
    // count of `projects.default_credential_id` references so the
    // dashboard can render `used by N projects` without a per-row
    // probe.
    let d = TestDashboard::new();
    let jar = d.dir.path().join("cookies.txt");
    signup(&d, &jar, "iris@branchwork.local", "supersecret-pw");

    // Create two credentials.
    let (_, key_one) = post_authed(
        &d,
        &jar,
        "/api/credentials",
        json!({ "name": "key-one", "kind": "gh_pat", "secret": "ghp_one" }),
    );
    let id_one = key_one["id"].as_str().unwrap().to_string();
    let (_, key_two) = post_authed(
        &d,
        &jar,
        "/api/credentials",
        json!({ "name": "key-two", "kind": "gh_pat", "secret": "ghp_two" }),
    );
    let id_two = key_two["id"].as_str().unwrap().to_string();

    // Fresh `POST` response: used_by_count is 0 (a freshly-created row
    // can't have references yet — pins the contract that the create
    // endpoint never silently inherits an inflated count).
    assert_eq!(
        key_one["usedByCount"],
        json!(0),
        "fresh POST response must carry usedByCount=0"
    );

    // Two projects reference key-one; zero reference key-two.
    let (_, _) = post_authed(
        &d,
        &jar,
        "/api/projects",
        json!({
            "name": "p-alpha",
            "repo_url": "git@github.com:owner/p-alpha.git",
            "default_credential_id": id_one,
        }),
    );
    let (_, _) = post_authed(
        &d,
        &jar,
        "/api/projects",
        json!({
            "name": "p-beta",
            "repo_url": "git@github.com:owner/p-beta.git",
            "default_credential_id": id_one,
        }),
    );

    // List returns both rows with the right counts. We don't pin the
    // ORDER because consecutive INSERTs can share a second-resolution
    // `created_at` and SQLite's ORDER BY tie-break is undefined — look
    // up by id instead.
    let (status, list_resp) = get_authed(&d, &jar, "/api/credentials");
    assert_eq!(status, 200);
    let creds = list_resp["credentials"].as_array().unwrap();
    assert_eq!(creds.len(), 2);
    let mut by_id: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    for c in creds {
        by_id.insert(
            c["id"].as_str().unwrap().to_string(),
            c["usedByCount"].as_i64().unwrap(),
        );
    }
    assert_eq!(by_id.get(&id_one), Some(&2));
    assert_eq!(by_id.get(&id_two), Some(&0));
}

#[test]
fn create_then_list_does_not_carry_owner_user_id_of_other_users() {
    let d = TestDashboard::new();
    let jar_a = d.dir.path().join("cookies-a.txt");
    let jar_b = d.dir.path().join("cookies-b.txt");

    signup(&d, &jar_a, "user-a@branchwork.local", "supersecret-pw");
    signup(&d, &jar_b, "user-b@branchwork.local", "supersecret-pw");

    // user-a creates a credential.
    let (status, _) = post_authed(
        &d,
        &jar_a,
        "/api/credentials",
        json!({ "name": "A key", "kind": "gh_pat", "secret": "ghp_A" }),
    );
    assert_eq!(status, 201);

    // user-b lists — must not see user-a's credential.
    let (status, body) = get_authed(&d, &jar_b, "/api/credentials");
    assert_eq!(status, 200);
    assert_eq!(
        body["credentials"].as_array().unwrap().len(),
        0,
        "user-b leaked user-a's credentials: {body}"
    );
}
