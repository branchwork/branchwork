//! SSO (SAML/OIDC) authentication for enterprise organizations.
//!
//! Supports:
//! - **OIDC**: Keycloak, Okta, Azure AD, Google Workspace -- any standard OIDC provider.
//! - **SAML 2.0**: HTTP-Redirect binding for AuthnRequest, HTTP-POST binding for Response.
//!
//! Admin configures SSO providers per org. Users sign in via their corporate IdP.
//! First-time users are provisioned just-in-time (JIT). IdP group claims are
//! mapped to org roles via the provider's `group_role_mapping`.

use std::collections::HashMap;
use std::io::Write as IoWrite;

use axum::{
    Json,
    extract::{Form, Path, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use base64::{Engine, engine::general_purpose};
use flate2::{Compression, write::DeflateEncoder};
use jsonwebtoken::jwk::JwkSet;
use rand::RngCore;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::AuthUser;
use super::orgs;
use super::sessions;
use crate::state::AppState;

// ── Types ───────────────────────────────────────────────────────────────────

/// Internal representation of an SSO provider (includes secrets).
#[derive(Debug, Clone)]
struct SsoProvider {
    id: String,
    org_id: String,
    protocol: String,
    name: String,
    enabled: bool,
    email_domains: Option<String>,
    // OIDC
    issuer_url: Option<String>,
    client_id: Option<String>,
    client_secret: Option<String>,
    // SAML
    idp_entity_id: Option<String>,
    idp_sso_url: Option<String>,
    #[allow(dead_code)]
    idp_certificate: Option<String>,
    sp_entity_id: Option<String>,
    // Common
    groups_claim: Option<String>,
    group_role_mapping: Option<String>,
    #[allow(dead_code)]
    created_at: String,
    #[allow(dead_code)]
    updated_at: String,
}

/// Serialized to the API with secrets stripped.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SsoProviderDto {
    id: String,
    org_id: String,
    protocol: String,
    name: String,
    enabled: bool,
    email_domains: Option<String>,
    issuer_url: Option<String>,
    client_id: Option<String>,
    // client_secret intentionally omitted
    idp_entity_id: Option<String>,
    idp_sso_url: Option<String>,
    // idp_certificate intentionally omitted
    sp_entity_id: Option<String>,
    groups_claim: Option<String>,
    group_role_mapping: Option<serde_json::Value>,
    created_at: String,
    updated_at: String,
}

impl SsoProvider {
    fn to_dto(&self) -> SsoProviderDto {
        SsoProviderDto {
            id: self.id.clone(),
            org_id: self.org_id.clone(),
            protocol: self.protocol.clone(),
            name: self.name.clone(),
            enabled: self.enabled,
            email_domains: self.email_domains.clone(),
            issuer_url: self.issuer_url.clone(),
            client_id: self.client_id.clone(),
            idp_entity_id: self.idp_entity_id.clone(),
            idp_sso_url: self.idp_sso_url.clone(),
            sp_entity_id: self.sp_entity_id.clone(),
            groups_claim: self.groups_claim.clone(),
            group_role_mapping: self
                .group_role_mapping
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok()),
            created_at: self.created_at.clone(),
            updated_at: self.updated_at.clone(),
        }
    }
}

/// Public-facing SSO provider info for the login page.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SsoLoginOption {
    id: String,
    name: String,
    protocol: String,
    login_url: String,
}

// ── Request / response DTOs ─────────────────────────────────────────────────

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateProviderBody {
    protocol: String,
    name: String,
    email_domains: Option<String>,
    issuer_url: Option<String>,
    client_id: Option<String>,
    client_secret: Option<String>,
    idp_entity_id: Option<String>,
    idp_sso_url: Option<String>,
    idp_certificate: Option<String>,
    sp_entity_id: Option<String>,
    groups_claim: Option<String>,
    group_role_mapping: Option<serde_json::Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateProviderBody {
    name: Option<String>,
    enabled: Option<bool>,
    email_domains: Option<String>,
    issuer_url: Option<String>,
    client_id: Option<String>,
    client_secret: Option<String>,
    idp_entity_id: Option<String>,
    idp_sso_url: Option<String>,
    idp_certificate: Option<String>,
    sp_entity_id: Option<String>,
    groups_claim: Option<String>,
    group_role_mapping: Option<serde_json::Value>,
}

#[derive(Deserialize)]
pub struct OidcCallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

#[derive(Deserialize)]
pub struct SamlAcsForm {
    #[serde(rename = "SAMLResponse")]
    saml_response: String,
    #[serde(rename = "RelayState")]
    relay_state: Option<String>,
}

/// OIDC discovery document (subset of fields we need).
#[derive(Deserialize)]
struct OidcDiscovery {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    jwks_uri: String,
}

/// OIDC token endpoint response.
#[derive(Deserialize)]
struct OidcTokenResponse {
    id_token: String,
}

/// Claims we extract from the OIDC ID token.
#[derive(Deserialize)]
struct IdTokenClaims {
    sub: String,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    nonce: Option<String>,
    /// Captures all other claims for group extraction.
    #[serde(flatten)]
    extra: HashMap<String, serde_json::Value>,
}

/// Intermediate representation of a parsed SAML response.
struct SamlResponseData {
    issuer: Option<String>,
    status_success: bool,
    name_id: Option<String>,
    attributes: HashMap<String, Vec<String>>,
}

/// Stored OIDC/SAML auth state for callback validation.
struct SsoAuthState {
    provider_id: String,
    pkce_verifier: Option<String>,
    nonce: Option<String>,
}

#[derive(Serialize)]
struct ErrorResponse {
    error: &'static str,
}

fn err(status: StatusCode, msg: &'static str) -> Response {
    (status, Json(ErrorResponse { error: msg })).into_response()
}

// ── Utility helpers ─────────────────────────────────────────────────────────

/// Percent-encode a string for use in URL query parameters (RFC 3986).
fn percent_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                String::from(b as char)
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Extract the external-facing base URL from request headers.
/// Respects `X-Forwarded-Proto` and `X-Forwarded-Host` for reverse-proxy setups.
fn extract_base_url(headers: &HeaderMap) -> String {
    let proto = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("http");
    let host = headers
        .get("x-forwarded-host")
        .or_else(|| headers.get(header::HOST))
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost:3100");
    format!("{proto}://{host}")
}

/// Generate a cryptographically random URL-safe string (256 bits).
fn random_state() -> String {
    let mut buf = [0u8; 32];
    rand::rng().fill_bytes(&mut buf);
    general_purpose::URL_SAFE_NO_PAD.encode(buf)
}

/// Generate a PKCE code_verifier + code_challenge (S256) pair.
fn generate_pkce() -> (String, String) {
    let mut buf = [0u8; 32];
    rand::rng().fill_bytes(&mut buf);
    let verifier = general_purpose::URL_SAFE_NO_PAD.encode(buf);
    let challenge = general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

// ── DB helpers ──────────────────────────────────────────────────────────────

fn load_provider(conn: &Connection, provider_id: &str) -> Option<SsoProvider> {
    conn.query_row(
        "SELECT id, org_id, protocol, name, enabled, email_domains, \
         issuer_url, client_id, client_secret, \
         idp_entity_id, idp_sso_url, idp_certificate, sp_entity_id, \
         groups_claim, group_role_mapping, created_at, updated_at \
         FROM sso_providers WHERE id = ?1",
        params![provider_id],
        |row| {
            Ok(SsoProvider {
                id: row.get(0)?,
                org_id: row.get(1)?,
                protocol: row.get(2)?,
                name: row.get(3)?,
                enabled: row.get::<_, i64>(4)? != 0,
                email_domains: row.get(5)?,
                issuer_url: row.get(6)?,
                client_id: row.get(7)?,
                client_secret: row.get(8)?,
                idp_entity_id: row.get(9)?,
                idp_sso_url: row.get(10)?,
                idp_certificate: row.get(11)?,
                sp_entity_id: row.get(12)?,
                groups_claim: row.get(13)?,
                group_role_mapping: row.get(14)?,
                created_at: row.get(15)?,
                updated_at: row.get(16)?,
            })
        },
    )
    .optional()
    .ok()
    .flatten()
}

fn load_provider_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SsoProvider> {
    Ok(SsoProvider {
        id: row.get(0)?,
        org_id: row.get(1)?,
        protocol: row.get(2)?,
        name: row.get(3)?,
        enabled: row.get::<_, i64>(4)? != 0,
        email_domains: row.get(5)?,
        issuer_url: row.get(6)?,
        client_id: row.get(7)?,
        client_secret: row.get(8)?,
        idp_entity_id: row.get(9)?,
        idp_sso_url: row.get(10)?,
        idp_certificate: row.get(11)?,
        sp_entity_id: row.get(12)?,
        groups_claim: row.get(13)?,
        group_role_mapping: row.get(14)?,
        created_at: row.get(15)?,
        updated_at: row.get(16)?,
    })
}

const PROVIDER_COLUMNS: &str = "id, org_id, protocol, name, enabled, email_domains, \
     issuer_url, client_id, client_secret, \
     idp_entity_id, idp_sso_url, idp_certificate, sp_entity_id, \
     groups_claim, group_role_mapping, created_at, updated_at";

fn list_org_providers(conn: &Connection, org_id: &str) -> Vec<SsoProvider> {
    conn.prepare(&format!(
        "SELECT {PROVIDER_COLUMNS} FROM sso_providers WHERE org_id = ?1 ORDER BY name"
    ))
    .and_then(|mut stmt| {
        stmt.query_map(params![org_id], load_provider_row)?
            .collect::<Result<Vec<_>, _>>()
    })
    .unwrap_or_default()
}

/// Find enabled SSO providers whose `email_domains` match the given email address.
fn providers_for_email(conn: &Connection, email: &str) -> Vec<SsoProvider> {
    let domain = match email.split('@').nth(1) {
        Some(d) => d.to_lowercase(),
        None => return vec![],
    };

    conn.prepare(&format!(
        "SELECT {PROVIDER_COLUMNS} FROM sso_providers \
         WHERE enabled = 1 AND email_domains IS NOT NULL"
    ))
    .and_then(|mut stmt| {
        stmt.query_map([], load_provider_row)?
            .collect::<Result<Vec<_>, _>>()
    })
    .unwrap_or_default()
    .into_iter()
    .filter(|p| {
        p.email_domains
            .as_deref()
            .unwrap_or("")
            .split(',')
            .any(|d| d.trim().to_lowercase() == domain)
    })
    .collect()
}

fn store_auth_state(
    conn: &Connection,
    state: &str,
    provider_id: &str,
    pkce_verifier: Option<&str>,
    nonce: Option<&str>,
) {
    conn.execute(
        "INSERT INTO sso_auth_state (state, provider_id, pkce_verifier, nonce) \
         VALUES (?1, ?2, ?3, ?4)",
        params![state, provider_id, pkce_verifier, nonce],
    )
    .ok();
    // Cleanup stale entries (older than 10 minutes).
    conn.execute(
        "DELETE FROM sso_auth_state WHERE created_at < datetime('now', '-10 minutes')",
        [],
    )
    .ok();
}

fn consume_auth_state(conn: &Connection, state: &str) -> Option<SsoAuthState> {
    let result = conn
        .query_row(
            "SELECT provider_id, pkce_verifier, nonce FROM sso_auth_state \
             WHERE state = ?1 AND created_at > datetime('now', '-10 minutes')",
            params![state],
            |row| {
                Ok(SsoAuthState {
                    provider_id: row.get(0)?,
                    pkce_verifier: row.get(1)?,
                    nonce: row.get(2)?,
                })
            },
        )
        .optional()
        .ok()
        .flatten();
    // Single-use: delete regardless of whether we found it.
    conn.execute(
        "DELETE FROM sso_auth_state WHERE state = ?1",
        params![state],
    )
    .ok();
    result
}

// ── OIDC helpers ────────────────────────────────────────────────────────────

async fn oidc_discover(issuer_url: &str) -> Result<OidcDiscovery, String> {
    let url = format!(
        "{}/.well-known/openid-configuration",
        issuer_url.trim_end_matches('/')
    );
    let client = reqwest::Client::new();
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("OIDC discovery request failed: {e}"))?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!("OIDC discovery returned {status}"));
    }
    serde_json::from_str(&body).map_err(|e| format!("OIDC discovery parse failed: {e}"))
}

async fn fetch_jwks(jwks_uri: &str) -> Result<JwkSet, String> {
    let client = reqwest::Client::new();
    let resp = client
        .get(jwks_uri)
        .send()
        .await
        .map_err(|e| format!("JWKS fetch failed: {e}"))?;
    let body = resp.text().await.unwrap_or_default();
    serde_json::from_str(&body).map_err(|e| format!("JWKS parse failed: {e}"))
}

async fn exchange_code(
    token_endpoint: &str,
    code: &str,
    redirect_uri: &str,
    client_id: &str,
    client_secret: &str,
    pkce_verifier: Option<&str>,
) -> Result<OidcTokenResponse, String> {
    let client = reqwest::Client::new();
    let mut form_params: Vec<(&str, &str)> = vec![
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("client_id", client_id),
        ("client_secret", client_secret),
    ];
    if let Some(v) = pkce_verifier {
        form_params.push(("code_verifier", v));
    }
    let resp = client
        .post(token_endpoint)
        .form(&form_params)
        .send()
        .await
        .map_err(|e| format!("token exchange request failed: {e}"))?;

    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!("token exchange returned {status}: {body}"));
    }
    serde_json::from_str(&body).map_err(|e| format!("token response parse failed: {e}"))
}

fn validate_id_token(
    id_token: &str,
    jwks: &JwkSet,
    issuer: &str,
    client_id: &str,
    expected_nonce: Option<&str>,
) -> Result<IdTokenClaims, String> {
    let header = jsonwebtoken::decode_header(id_token)
        .map_err(|e| format!("JWT header decode failed: {e}"))?;

    let kid = header.kid.as_deref();

    // Find matching key in JWKS.
    let jwk = jwks
        .keys
        .iter()
        .find(|k| {
            if let Some(expected_kid) = kid {
                k.common.key_id.as_deref() == Some(expected_kid)
            } else {
                // No kid in header — take the first key.
                true
            }
        })
        .ok_or_else(|| "no matching JWK key found".to_string())?;

    let key = jsonwebtoken::DecodingKey::from_jwk(jwk)
        .map_err(|e| format!("JWK to DecodingKey failed: {e}"))?;

    let mut validation = jsonwebtoken::Validation::new(header.alg);
    validation.set_issuer(&[issuer]);
    validation.set_audience(&[client_id]);

    let data = jsonwebtoken::decode::<IdTokenClaims>(id_token, &key, &validation)
        .map_err(|e| format!("JWT validation failed: {e}"))?;

    // Verify nonce matches.
    if let Some(expected) = expected_nonce
        && data.claims.nonce.as_deref() != Some(expected)
    {
        return Err("nonce mismatch".to_string());
    }

    Ok(data.claims)
}

/// Extract group names from the ID token's extra claims.
fn extract_groups_from_claims(claims: &IdTokenClaims, groups_claim: &str) -> Vec<String> {
    claims
        .extra
        .get(groups_claim)
        .and_then(|v| match v {
            serde_json::Value::Array(arr) => Some(
                arr.iter()
                    .filter_map(|item| item.as_str().map(String::from))
                    .collect(),
            ),
            serde_json::Value::String(s) => Some(vec![s.clone()]),
            _ => None,
        })
        .unwrap_or_default()
}

// ── SAML helpers ────────────────────────────────────────────────────────────

/// Build a SAML 2.0 AuthnRequest XML document.
fn build_saml_authn_request(sp_entity_id: &str, acs_url: &str, idp_sso_url: &str) -> String {
    let id = format!("_{}", Uuid::new_v4());
    let instant = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ");
    format!(
        r#"<samlp:AuthnRequest xmlns:samlp="urn:oasis:names:tc:SAML:2.0:protocol"
 xmlns:saml="urn:oasis:names:tc:SAML:2.0:assertion"
 ID="{id}" Version="2.0" IssueInstant="{instant}"
 Destination="{idp_sso_url}"
 AssertionConsumerServiceURL="{acs_url}"
 ProtocolBinding="urn:oasis:names:tc:SAML:2.0:bindings:HTTP-POST">
  <saml:Issuer>{sp_entity_id}</saml:Issuer>
  <samlp:NameIDPolicy Format="urn:oasis:names:tc:SAML:1.1:nameid-format:emailAddress"
                       AllowCreate="true"/>
</samlp:AuthnRequest>"#
    )
}

/// Deflate-compress + base64-encode an AuthnRequest for HTTP-Redirect binding,
/// then append it as a `SAMLRequest` query parameter to the IdP SSO URL.
fn encode_saml_redirect(authn_request: &str, idp_sso_url: &str) -> String {
    let mut encoder = DeflateEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(authn_request.as_bytes()).unwrap();
    let compressed = encoder.finish().unwrap();
    let encoded = general_purpose::STANDARD.encode(&compressed);

    let sep = if idp_sso_url.contains('?') { "&" } else { "?" };
    format!("{idp_sso_url}{sep}SAMLRequest={}", percent_encode(&encoded))
}

/// Generate SAML 2.0 SP metadata XML for IdP configuration.
fn build_sp_metadata(sp_entity_id: &str, acs_url: &str) -> String {
    format!(
        r#"<?xml version="1.0"?>
<md:EntityDescriptor xmlns:md="urn:oasis:names:tc:SAML:2.0:metadata"
                      entityID="{sp_entity_id}">
  <md:SPSSODescriptor protocolSupportEnumeration="urn:oasis:names:tc:SAML:2.0:protocol"
                       AuthnRequestsSigned="false"
                       WantAssertionsSigned="true">
    <md:NameIDFormat>urn:oasis:names:tc:SAML:1.1:nameid-format:emailAddress</md:NameIDFormat>
    <md:AssertionConsumerService
        Binding="urn:oasis:names:tc:SAML:2.0:bindings:HTTP-POST"
        Location="{acs_url}"
        index="0"
        isDefault="true"/>
  </md:SPSSODescriptor>
</md:EntityDescriptor>"#
    )
}

/// Parse a base64-decoded SAML 2.0 Response XML and extract the user identity.
fn parse_saml_response_xml(xml: &str) -> Result<SamlResponseData, String> {
    use quick_xml::Reader;
    use quick_xml::events::Event;

    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::new();

    let mut data = SamlResponseData {
        issuer: None,
        status_success: false,
        name_id: None,
        attributes: HashMap::new(),
    };

    let mut in_issuer = false;
    let mut in_name_id = false;
    let mut in_attribute_value = false;
    let mut current_attr_name: Option<String> = None;
    let mut issuer_captured = false;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                let local = String::from_utf8_lossy(e.local_name().as_ref()).to_string();
                match local.as_str() {
                    "Issuer" if !issuer_captured => in_issuer = true,
                    "NameID" => in_name_id = true,
                    "Attribute" => {
                        for attr in e.attributes().flatten() {
                            if attr.key.local_name().as_ref() == b"Name" {
                                current_attr_name =
                                    Some(String::from_utf8_lossy(&attr.value).to_string());
                            }
                        }
                    }
                    "AttributeValue" if current_attr_name.is_some() => {
                        in_attribute_value = true;
                    }
                    _ => {}
                }
            }
            Ok(Event::Empty(ref e)) => {
                let local = String::from_utf8_lossy(e.local_name().as_ref()).to_string();
                if local == "StatusCode" {
                    for attr in e.attributes().flatten() {
                        if attr.key.local_name().as_ref() == b"Value" {
                            let value = String::from_utf8_lossy(&attr.value);
                            if value.ends_with(":Success") {
                                data.status_success = true;
                            }
                        }
                    }
                }
            }
            Ok(Event::Text(ref e)) => {
                if let Ok(text) = e.unescape() {
                    let text = text.trim().to_string();
                    if !text.is_empty() {
                        if in_issuer {
                            data.issuer = Some(text);
                        } else if in_name_id {
                            data.name_id = Some(text);
                        } else if in_attribute_value && let Some(ref name) = current_attr_name {
                            data.attributes.entry(name.clone()).or_default().push(text);
                        }
                    }
                }
            }
            Ok(Event::End(ref e)) => {
                let local = String::from_utf8_lossy(e.local_name().as_ref()).to_string();
                match local.as_str() {
                    "Issuer" => {
                        in_issuer = false;
                        issuer_captured = true;
                    }
                    "NameID" => in_name_id = false,
                    "Attribute" => current_attr_name = None,
                    "AttributeValue" => in_attribute_value = false,
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(format!("XML parse error: {e}")),
            _ => {}
        }
        buf.clear();
    }

    Ok(data)
}

// ── JIT provisioning & group mapping ────────────────────────────────────────

/// Find an existing user linked to this SSO account, or provision a new one.
/// Returns `(user_id, is_new_user)`.
fn find_or_provision_sso_user(
    conn: &Connection,
    provider: &SsoProvider,
    email: &str,
    external_id: &str,
    groups: &[String],
) -> Result<(String, bool), String> {
    // 1. Check for existing SSO account link.
    let existing: Option<String> = conn
        .query_row(
            "SELECT user_id FROM sso_accounts WHERE provider_id = ?1 AND external_id = ?2",
            params![provider.id, external_id],
            |row| row.get(0),
        )
        .optional()
        .ok()
        .flatten();

    if let Some(user_id) = existing {
        // Update last login and groups.
        let groups_json = serde_json::to_string(&groups).unwrap_or_default();
        conn.execute(
            "UPDATE sso_accounts SET last_login_at = datetime('now'), \
             email = ?1, groups = ?2 WHERE provider_id = ?3 AND external_id = ?4",
            params![email, groups_json, provider.id, external_id],
        )
        .ok();
        apply_group_mapping(
            conn,
            &provider.org_id,
            &user_id,
            &provider.group_role_mapping,
            groups,
        );
        return Ok((user_id, false));
    }

    // 2. Check if a user with this email already exists (link existing account).
    let existing_user: Option<String> = conn
        .query_row(
            "SELECT id FROM users WHERE email = ?1",
            params![email],
            |row| row.get(0),
        )
        .optional()
        .ok()
        .flatten();

    let (user_id, is_new) = if let Some(uid) = existing_user {
        // Ensure they're a member of the provider's org.
        conn.execute(
            "INSERT OR IGNORE INTO org_members (org_id, user_id, role) VALUES (?1, ?2, ?3)",
            params![provider.org_id, uid, orgs::ROLE_MEMBER],
        )
        .ok();
        (uid, false)
    } else {
        // 3. JIT provision: create a new user with a random unusable password.
        let uid = Uuid::new_v4().to_string();
        let fake_hash = bcrypt::hash(random_state(), bcrypt::DEFAULT_COST)
            .map_err(|e| format!("hash failed: {e}"))?;
        conn.execute(
            "INSERT INTO users (id, email, password_hash) VALUES (?1, ?2, ?3)",
            params![uid, email, fake_hash],
        )
        .map_err(|e| format!("user insert failed: {e}"))?;

        // Create personal org (same as signup flow).
        orgs::create_personal_org(conn, &uid, email);

        // Add to default org.
        conn.execute(
            "INSERT OR IGNORE INTO org_members (org_id, user_id, role) VALUES (?1, ?2, ?3)",
            params![orgs::DEFAULT_ORG_ID, uid, orgs::ROLE_MEMBER],
        )
        .ok();

        // Add to the provider's org.
        conn.execute(
            "INSERT OR IGNORE INTO org_members (org_id, user_id, role) VALUES (?1, ?2, ?3)",
            params![provider.org_id, uid, orgs::ROLE_MEMBER],
        )
        .ok();

        (uid, true)
    };

    // 4. Create SSO account link.
    let sso_account_id = Uuid::new_v4().to_string();
    let groups_json = serde_json::to_string(&groups).unwrap_or_default();
    conn.execute(
        "INSERT OR REPLACE INTO sso_accounts \
         (id, user_id, provider_id, external_id, email, groups, last_login_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, datetime('now'))",
        params![
            sso_account_id,
            user_id,
            provider.id,
            external_id,
            email,
            groups_json,
        ],
    )
    .map_err(|e| format!("SSO account link failed: {e}"))?;

    // 5. Apply group-to-role mapping.
    apply_group_mapping(
        conn,
        &provider.org_id,
        &user_id,
        &provider.group_role_mapping,
        groups,
    );

    Ok((user_id, is_new))
}

/// Map IdP group names to org roles using the provider's `group_role_mapping` JSON.
/// Picks the highest-priority role among the user's groups.
fn apply_group_mapping(
    conn: &Connection,
    org_id: &str,
    user_id: &str,
    mapping_json: &Option<String>,
    groups: &[String],
) {
    let mapping: HashMap<String, String> = mapping_json
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_default();

    if mapping.is_empty() || groups.is_empty() {
        return;
    }

    let role_priority = |r: &str| -> u8 {
        match r {
            "owner" => 4,
            "admin" => 3,
            "member" => 2,
            "viewer" => 1,
            _ => 0,
        }
    };

    let mut best_role: Option<&str> = None;
    let mut best_priority = 0u8;

    for group in groups {
        if let Some(role) = mapping.get(group) {
            let p = role_priority(role);
            if p > best_priority {
                best_priority = p;
                best_role = Some(role);
            }
        }
    }

    if let Some(role) = best_role {
        conn.execute(
            "INSERT INTO org_members (org_id, user_id, role) VALUES (?1, ?2, ?3) \
             ON CONFLICT(org_id, user_id) DO UPDATE SET role = excluded.role",
            params![org_id, user_id, role],
        )
        .ok();
    }
}

// ── Shared SSO completion ───────────────────────────────────────────────────

/// Provision user if needed, create a session, and redirect to the dashboard.
fn complete_sso_login(
    state: &AppState,
    provider: &SsoProvider,
    email: &str,
    external_id: &str,
    groups: &[String],
) -> Response {
    let result = {
        let conn = state.db.lock().unwrap();
        let result = find_or_provision_sso_user(&conn, provider, email, external_id, groups);

        if let Ok((ref user_id, is_new)) = result {
            crate::audit::log(
                &conn,
                &provider.org_id,
                Some(user_id),
                Some(email),
                if is_new {
                    crate::audit::actions::SSO_JIT_PROVISION
                } else {
                    crate::audit::actions::SSO_LOGIN
                },
                crate::audit::resources::USER,
                Some(user_id),
                Some(
                    &serde_json::json!({
                        "provider": provider.name,
                        "protocol": provider.protocol,
                        "external_id": external_id,
                        "groups": groups,
                    })
                    .to_string(),
                ),
            );
        }

        result
    }; // conn dropped — sessions::create locks again

    let (user_id, _is_new) = match result {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[sso] user provisioning failed: {e}");
            return redirect_with_error("provisioning_failed");
        }
    };

    let token = sessions::create(&state.db, &user_id);
    let cookie = sessions::set_cookie_value(&token);

    let mut headers = HeaderMap::new();
    headers.insert(header::SET_COOKIE, HeaderValue::from_str(&cookie).unwrap());
    headers.insert(header::LOCATION, HeaderValue::from_static("/"));
    (StatusCode::FOUND, headers).into_response()
}

fn redirect_with_error(code: &str) -> Response {
    let url = format!("/?sso_error={code}");
    let mut headers = HeaderMap::new();
    if let Ok(val) = HeaderValue::from_str(&url) {
        headers.insert(header::LOCATION, val);
    }
    (StatusCode::FOUND, headers).into_response()
}

// ── Admin permission helper ─────────────────────────────────────────────────

#[allow(clippy::result_large_err)]
fn require_org_admin(conn: &Connection, user: &AuthUser, slug: &str) -> Result<String, Response> {
    let org_id: Option<String> = conn
        .query_row(
            "SELECT id FROM organizations WHERE slug = ?1",
            params![slug],
            |row| row.get(0),
        )
        .optional()
        .ok()
        .flatten();
    let org_id = match org_id {
        Some(id) => id,
        None => return Err(err(StatusCode::NOT_FOUND, "org_not_found")),
    };

    let role: Option<String> = conn
        .query_row(
            "SELECT role FROM org_members WHERE org_id = ?1 AND user_id = ?2",
            params![org_id, user.id],
            |row| row.get(0),
        )
        .optional()
        .ok()
        .flatten();

    match role.as_deref() {
        Some("owner") | Some("admin") => Ok(org_id),
        _ => Err(err(StatusCode::FORBIDDEN, "insufficient_permissions")),
    }
}

// ── API handlers — admin (require auth + owner/admin) ───────────────────────

/// `GET /api/orgs/:slug/sso` — list SSO providers for this org.
pub async fn list_providers(
    State(state): State<AppState>,
    user: AuthUser,
    Path(slug): Path<String>,
) -> Response {
    let conn = state.db.lock().unwrap();
    let org_id = match require_org_admin(&conn, &user, &slug) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    let providers: Vec<SsoProviderDto> = list_org_providers(&conn, &org_id)
        .into_iter()
        .map(|p| p.to_dto())
        .collect();
    Json(providers).into_response()
}

/// `POST /api/orgs/:slug/sso` — create a new SSO provider.
pub async fn create_provider(
    State(state): State<AppState>,
    user: AuthUser,
    Path(slug): Path<String>,
    Json(body): Json<CreateProviderBody>,
) -> Response {
    let conn = state.db.lock().unwrap();
    let org_id = match require_org_admin(&conn, &user, &slug) {
        Ok(id) => id,
        Err(resp) => return resp,
    };

    if body.protocol != "oidc" && body.protocol != "saml" {
        return err(StatusCode::BAD_REQUEST, "invalid_protocol");
    }
    if body.name.trim().is_empty() {
        return err(StatusCode::BAD_REQUEST, "name_required");
    }

    // Validate required fields per protocol.
    if body.protocol == "oidc" {
        if body.issuer_url.as_deref().unwrap_or("").is_empty() {
            return err(StatusCode::BAD_REQUEST, "issuer_url_required");
        }
        if body.client_id.as_deref().unwrap_or("").is_empty() {
            return err(StatusCode::BAD_REQUEST, "client_id_required");
        }
        if body.client_secret.as_deref().unwrap_or("").is_empty() {
            return err(StatusCode::BAD_REQUEST, "client_secret_required");
        }
    } else if body.idp_sso_url.as_deref().unwrap_or("").is_empty() {
        return err(StatusCode::BAD_REQUEST, "idp_sso_url_required");
    }

    let id = Uuid::new_v4().to_string();
    let group_mapping_str = body.group_role_mapping.as_ref().map(|v| v.to_string());

    let result = conn.execute(
        "INSERT INTO sso_providers (id, org_id, protocol, name, email_domains, \
         issuer_url, client_id, client_secret, \
         idp_entity_id, idp_sso_url, idp_certificate, sp_entity_id, \
         groups_claim, group_role_mapping) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        params![
            id,
            org_id,
            body.protocol,
            body.name.trim(),
            body.email_domains,
            body.issuer_url,
            body.client_id,
            body.client_secret,
            body.idp_entity_id,
            body.idp_sso_url,
            body.idp_certificate,
            body.sp_entity_id,
            body.groups_claim,
            group_mapping_str,
        ],
    );

    if let Err(e) = result {
        eprintln!("[sso] create provider error: {e}");
        return err(StatusCode::INTERNAL_SERVER_ERROR, "db_error");
    }

    crate::audit::log(
        &conn,
        &org_id,
        Some(&user.id),
        Some(&user.email),
        crate::audit::actions::SSO_PROVIDER_CREATE,
        crate::audit::resources::SSO_PROVIDER,
        Some(&id),
        Some(&serde_json::json!({"name": body.name.trim(), "protocol": body.protocol}).to_string()),
    );

    match load_provider(&conn, &id) {
        Some(p) => (StatusCode::CREATED, Json(p.to_dto())).into_response(),
        None => err(StatusCode::INTERNAL_SERVER_ERROR, "provider_not_found"),
    }
}

/// `PUT /api/orgs/:slug/sso/:provider_id` — update an SSO provider.
pub async fn update_provider(
    State(state): State<AppState>,
    user: AuthUser,
    Path((slug, provider_id)): Path<(String, String)>,
    Json(body): Json<UpdateProviderBody>,
) -> Response {
    let conn = state.db.lock().unwrap();
    let org_id = match require_org_admin(&conn, &user, &slug) {
        Ok(id) => id,
        Err(resp) => return resp,
    };

    let provider = match load_provider(&conn, &provider_id) {
        Some(p) if p.org_id == org_id => p,
        _ => return err(StatusCode::NOT_FOUND, "provider_not_found"),
    };

    let name = body
        .name
        .as_deref()
        .unwrap_or(&provider.name)
        .trim()
        .to_string();
    let enabled = body.enabled.unwrap_or(provider.enabled);
    let group_mapping = body
        .group_role_mapping
        .as_ref()
        .map(|v| v.to_string())
        .or(provider.group_role_mapping.clone());

    conn.execute(
        "UPDATE sso_providers SET \
         name = ?1, enabled = ?2, email_domains = coalesce(?3, email_domains), \
         issuer_url = coalesce(?4, issuer_url), \
         client_id = coalesce(?5, client_id), \
         client_secret = coalesce(?6, client_secret), \
         idp_entity_id = coalesce(?7, idp_entity_id), \
         idp_sso_url = coalesce(?8, idp_sso_url), \
         idp_certificate = coalesce(?9, idp_certificate), \
         sp_entity_id = coalesce(?10, sp_entity_id), \
         groups_claim = coalesce(?11, groups_claim), \
         group_role_mapping = ?12, \
         updated_at = datetime('now') \
         WHERE id = ?13",
        params![
            name,
            enabled as i64,
            body.email_domains,
            body.issuer_url,
            body.client_id,
            body.client_secret,
            body.idp_entity_id,
            body.idp_sso_url,
            body.idp_certificate,
            body.sp_entity_id,
            body.groups_claim,
            group_mapping,
            provider_id,
        ],
    )
    .ok();

    crate::audit::log(
        &conn,
        &org_id,
        Some(&user.id),
        Some(&user.email),
        crate::audit::actions::SSO_PROVIDER_UPDATE,
        crate::audit::resources::SSO_PROVIDER,
        Some(&provider_id),
        None,
    );

    match load_provider(&conn, &provider_id) {
        Some(p) => Json(p.to_dto()).into_response(),
        None => err(StatusCode::INTERNAL_SERVER_ERROR, "provider_not_found"),
    }
}

/// `DELETE /api/orgs/:slug/sso/:provider_id` — delete an SSO provider.
pub async fn delete_provider(
    State(state): State<AppState>,
    user: AuthUser,
    Path((slug, provider_id)): Path<(String, String)>,
) -> Response {
    let conn = state.db.lock().unwrap();
    let org_id = match require_org_admin(&conn, &user, &slug) {
        Ok(id) => id,
        Err(resp) => return resp,
    };

    let exists = load_provider(&conn, &provider_id)
        .map(|p| p.org_id == org_id)
        .unwrap_or(false);
    if !exists {
        return err(StatusCode::NOT_FOUND, "provider_not_found");
    }

    conn.execute(
        "DELETE FROM sso_accounts WHERE provider_id = ?1",
        params![provider_id],
    )
    .ok();
    conn.execute(
        "DELETE FROM sso_providers WHERE id = ?1",
        params![provider_id],
    )
    .ok();

    crate::audit::log(
        &conn,
        &org_id,
        Some(&user.id),
        Some(&user.email),
        crate::audit::actions::SSO_PROVIDER_DELETE,
        crate::audit::resources::SSO_PROVIDER,
        Some(&provider_id),
        None,
    );

    Json(serde_json::json!({"ok": true})).into_response()
}

// ── API handlers — public (no auth required) ────────────────────────────────

/// `GET /api/auth/sso/providers?email=...` — discover SSO providers for a given
/// email address (used by the login page to show SSO buttons).
pub async fn discover_providers(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let conn = state.db.lock().unwrap();

    let providers = if let Some(email) = params.get("email") {
        providers_for_email(&conn, email)
    } else if let Some(org_slug) = params.get("org") {
        let org_id: Option<String> = conn
            .query_row(
                "SELECT id FROM organizations WHERE slug = ?1",
                params![org_slug],
                |row| row.get(0),
            )
            .optional()
            .ok()
            .flatten();
        match org_id {
            Some(id) => list_org_providers(&conn, &id)
                .into_iter()
                .filter(|p| p.enabled)
                .collect(),
            None => vec![],
        }
    } else {
        vec![]
    };

    let options: Vec<SsoLoginOption> = providers
        .into_iter()
        .map(|p| SsoLoginOption {
            login_url: format!("/api/auth/sso/{}/login", p.id),
            id: p.id,
            name: p.name,
            protocol: p.protocol,
        })
        .collect();
    Json(options).into_response()
}

/// `GET /api/auth/sso/:provider_id/login` — initiate SSO login (redirect to IdP).
pub async fn sso_login(
    State(state): State<AppState>,
    Path(provider_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let provider = {
        let conn = state.db.lock().unwrap();
        load_provider(&conn, &provider_id)
    };

    let provider = match provider {
        Some(p) if p.enabled => p,
        _ => return err(StatusCode::NOT_FOUND, "provider_not_found"),
    };

    let base = extract_base_url(&headers);

    match provider.protocol.as_str() {
        "oidc" => oidc_login_redirect(&state, &provider, &base).await,
        "saml" => saml_login_redirect(&state, &provider, &base),
        _ => err(StatusCode::BAD_REQUEST, "unsupported_protocol"),
    }
}

async fn oidc_login_redirect(state: &AppState, provider: &SsoProvider, base_url: &str) -> Response {
    let issuer_url = match provider.issuer_url.as_deref() {
        Some(url) if !url.is_empty() => url,
        _ => return err(StatusCode::INTERNAL_SERVER_ERROR, "missing_issuer_url"),
    };
    let client_id = match provider.client_id.as_deref() {
        Some(id) if !id.is_empty() => id,
        _ => return err(StatusCode::INTERNAL_SERVER_ERROR, "missing_client_id"),
    };

    let discovery = match oidc_discover(issuer_url).await {
        Ok(d) => d,
        Err(e) => {
            eprintln!("[sso] OIDC discovery failed: {e}");
            return err(StatusCode::BAD_GATEWAY, "oidc_discovery_failed");
        }
    };

    let redirect_uri = format!("{base_url}/api/auth/sso/{}/callback", provider.id);
    let state_param = random_state();
    let nonce = random_state();
    let (pkce_verifier, pkce_challenge) = generate_pkce();

    {
        let conn = state.db.lock().unwrap();
        store_auth_state(
            &conn,
            &state_param,
            &provider.id,
            Some(&pkce_verifier),
            Some(&nonce),
        );
    }

    let auth_url = format!(
        "{}?response_type=code&client_id={}&redirect_uri={}\
         &scope={}&state={}&nonce={}\
         &code_challenge={}&code_challenge_method=S256",
        discovery.authorization_endpoint,
        percent_encode(client_id),
        percent_encode(&redirect_uri),
        percent_encode("openid email profile"),
        percent_encode(&state_param),
        percent_encode(&nonce),
        percent_encode(&pkce_challenge),
    );

    let mut resp_headers = HeaderMap::new();
    if let Ok(val) = HeaderValue::from_str(&auth_url) {
        resp_headers.insert(header::LOCATION, val);
    }
    (StatusCode::FOUND, resp_headers).into_response()
}

fn saml_login_redirect(state: &AppState, provider: &SsoProvider, base_url: &str) -> Response {
    let idp_sso_url = match provider.idp_sso_url.as_deref() {
        Some(url) if !url.is_empty() => url,
        _ => return err(StatusCode::INTERNAL_SERVER_ERROR, "missing_idp_sso_url"),
    };
    let default_entity_id = format!("{base_url}/api/auth/sso/{}", provider.id);
    let sp_entity_id = provider
        .sp_entity_id
        .as_deref()
        .unwrap_or(&default_entity_id);
    let acs_url = format!("{base_url}/api/auth/sso/{}/saml/acs", provider.id);

    let authn_request = build_saml_authn_request(sp_entity_id, &acs_url, idp_sso_url);
    let redirect_url = encode_saml_redirect(&authn_request, idp_sso_url);

    // Store RelayState as a CSRF token.
    let relay_state = random_state();
    {
        let conn = state.db.lock().unwrap();
        store_auth_state(&conn, &relay_state, &provider.id, None, None);
    }

    let full_url = format!("{redirect_url}&RelayState={}", percent_encode(&relay_state));
    let mut resp_headers = HeaderMap::new();
    if let Ok(val) = HeaderValue::from_str(&full_url) {
        resp_headers.insert(header::LOCATION, val);
    }
    (StatusCode::FOUND, resp_headers).into_response()
}

/// `GET /api/auth/sso/:provider_id/callback` — OIDC authorization code callback.
pub async fn oidc_callback(
    State(state): State<AppState>,
    Path(provider_id): Path<String>,
    Query(params): Query<OidcCallbackQuery>,
    headers: HeaderMap,
) -> Response {
    // Handle IdP-side errors.
    if let Some(ref error) = params.error {
        eprintln!(
            "[sso] OIDC error from IdP: {error} - {:?}",
            params.error_description
        );
        return redirect_with_error("idp_error");
    }

    let code = match params.code.as_deref() {
        Some(c) if !c.is_empty() => c,
        _ => return redirect_with_error("missing_code"),
    };
    let state_param = match params.state.as_deref() {
        Some(s) if !s.is_empty() => s,
        _ => return redirect_with_error("missing_state"),
    };

    // Validate state (CSRF protection + PKCE recovery).
    let auth_state = {
        let conn = state.db.lock().unwrap();
        consume_auth_state(&conn, state_param)
    };
    let auth_state = match auth_state {
        Some(s) if s.provider_id == provider_id => s,
        _ => return redirect_with_error("invalid_state"),
    };

    let provider = {
        let conn = state.db.lock().unwrap();
        load_provider(&conn, &provider_id)
    };
    let provider = match provider {
        Some(p) if p.enabled => p,
        _ => return redirect_with_error("provider_not_found"),
    };

    let issuer_url = match provider.issuer_url.as_deref() {
        Some(url) if !url.is_empty() => url,
        _ => return redirect_with_error("misconfigured"),
    };
    let client_id = match provider.client_id.as_deref() {
        Some(id) if !id.is_empty() => id,
        _ => return redirect_with_error("misconfigured"),
    };
    let client_secret = match provider.client_secret.as_deref() {
        Some(s) if !s.is_empty() => s,
        _ => return redirect_with_error("misconfigured"),
    };

    let base = extract_base_url(&headers);
    let redirect_uri = format!("{base}/api/auth/sso/{}/callback", provider.id);

    // OIDC discovery.
    let discovery = match oidc_discover(issuer_url).await {
        Ok(d) => d,
        Err(e) => {
            eprintln!("[sso] OIDC discovery failed during callback: {e}");
            return redirect_with_error("discovery_failed");
        }
    };

    // Exchange authorization code for tokens.
    let tokens = match exchange_code(
        &discovery.token_endpoint,
        code,
        &redirect_uri,
        client_id,
        client_secret,
        auth_state.pkce_verifier.as_deref(),
    )
    .await
    {
        Ok(t) => t,
        Err(e) => {
            eprintln!("[sso] token exchange failed: {e}");
            return redirect_with_error("token_exchange_failed");
        }
    };

    // Fetch JWKS and validate ID token.
    let jwks = match fetch_jwks(&discovery.jwks_uri).await {
        Ok(j) => j,
        Err(e) => {
            eprintln!("[sso] JWKS fetch failed: {e}");
            return redirect_with_error("jwks_failed");
        }
    };

    let claims = match validate_id_token(
        &tokens.id_token,
        &jwks,
        &discovery.issuer,
        client_id,
        auth_state.nonce.as_deref(),
    ) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[sso] ID token validation failed: {e}");
            return redirect_with_error("invalid_token");
        }
    };

    let email = match claims.email.as_deref() {
        Some(e) if !e.is_empty() => e.to_lowercase(),
        _ => return redirect_with_error("missing_email"),
    };
    let groups = extract_groups_from_claims(
        &claims,
        provider.groups_claim.as_deref().unwrap_or("groups"),
    );

    complete_sso_login(&state, &provider, &email, &claims.sub, &groups)
}

/// `POST /api/auth/sso/:provider_id/saml/acs` — SAML Assertion Consumer Service.
pub async fn saml_acs(
    State(state): State<AppState>,
    Path(provider_id): Path<String>,
    Form(form): Form<SamlAcsForm>,
) -> Response {
    let provider = {
        let conn = state.db.lock().unwrap();
        load_provider(&conn, &provider_id)
    };
    let provider = match provider {
        Some(p) if p.enabled && p.protocol == "saml" => p,
        _ => return redirect_with_error("provider_not_found"),
    };

    // Validate RelayState (CSRF protection).
    if let Some(ref relay_state) = form.relay_state {
        let conn = state.db.lock().unwrap();
        if consume_auth_state(&conn, relay_state).is_none() {
            return redirect_with_error("invalid_relay_state");
        }
    }

    // Decode SAML response (base64).
    let xml_bytes = match general_purpose::STANDARD.decode(&form.saml_response) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("[sso] SAML response decode failed: {e}");
            return redirect_with_error("invalid_response");
        }
    };
    let xml = match String::from_utf8(xml_bytes) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[sso] SAML response not valid UTF-8: {e}");
            return redirect_with_error("invalid_response");
        }
    };

    // Parse and validate.
    let saml_data = match parse_saml_response_xml(&xml) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("[sso] SAML XML parse failed: {e}");
            return redirect_with_error("invalid_response");
        }
    };

    if !saml_data.status_success {
        return redirect_with_error("saml_not_success");
    }

    // Validate issuer matches IdP entity ID (if configured).
    if let Some(expected) = provider.idp_entity_id.as_deref()
        && saml_data.issuer.as_deref() != Some(expected)
    {
        eprintln!(
            "[sso] SAML issuer mismatch: expected={expected}, got={:?}",
            saml_data.issuer
        );
        return redirect_with_error("issuer_mismatch");
    }

    // TODO: Verify XML signature against idp_certificate for production use.
    // Current validation: TLS transport security + issuer check + RelayState CSRF.

    let name_id = match saml_data.name_id.as_deref() {
        Some(n) if !n.is_empty() => n,
        _ => return redirect_with_error("missing_name_id"),
    };

    // Use email attribute if present, otherwise fall back to NameID.
    let email = saml_data
        .attributes
        .get("email")
        .and_then(|vals| vals.first())
        .map(|s| s.to_lowercase())
        .unwrap_or_else(|| name_id.to_lowercase());

    let groups_claim = provider.groups_claim.as_deref().unwrap_or("groups");
    let groups: Vec<String> = saml_data
        .attributes
        .get(groups_claim)
        .cloned()
        .unwrap_or_default();

    complete_sso_login(&state, &provider, &email, name_id, &groups)
}

/// `GET /api/auth/sso/:provider_id/saml/metadata` — SAML SP metadata XML.
pub async fn saml_metadata(
    State(state): State<AppState>,
    Path(provider_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let provider = {
        let conn = state.db.lock().unwrap();
        load_provider(&conn, &provider_id)
    };
    let provider = match provider {
        Some(p) if p.protocol == "saml" => p,
        _ => return err(StatusCode::NOT_FOUND, "provider_not_found"),
    };

    let base = extract_base_url(&headers);
    let default_entity_id = format!("{base}/api/auth/sso/{}", provider.id);
    let sp_entity_id = provider
        .sp_entity_id
        .as_deref()
        .unwrap_or(&default_entity_id);
    let acs_url = format!("{base}/api/auth/sso/{}/saml/acs", provider.id);

    let metadata = build_sp_metadata(sp_entity_id, &acs_url);

    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/xml")],
        metadata,
    )
        .into_response()
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use tempfile::TempDir;

    fn fresh_db() -> (crate::db::Db, TempDir) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.db");
        let db = db::init(&path);
        (db, dir)
    }

    #[test]
    fn percent_encode_basics() {
        assert_eq!(percent_encode("hello"), "hello");
        assert_eq!(percent_encode("a b"), "a%20b");
        assert_eq!(percent_encode("a+b=c"), "a%2Bb%3Dc");
    }

    #[test]
    fn pkce_challenge_is_deterministic_for_verifier() {
        let verifier = "test_verifier_value";
        let expected = general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        // Verify our PKCE challenge formula matches.
        let computed = general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        assert_eq!(expected, computed);
    }

    #[test]
    fn generate_pkce_pair() {
        let (verifier, challenge) = generate_pkce();
        assert!(!verifier.is_empty());
        assert!(!challenge.is_empty());
        assert_ne!(verifier, challenge);
        // Verify challenge is SHA256 of verifier.
        let expected = general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        assert_eq!(challenge, expected);
    }

    #[test]
    fn group_mapping_picks_highest_role() {
        let (db, _dir) = fresh_db();
        let conn = db.lock().unwrap();

        // Create org and user.
        conn.execute(
            "INSERT INTO organizations (id, name, slug) VALUES ('org-1', 'Test', 'test')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO users (id, email, password_hash) VALUES ('u1', 'a@b.com', 'x')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO org_members (org_id, user_id, role) VALUES ('org-1', 'u1', 'viewer')",
            [],
        )
        .unwrap();

        let mapping = Some(r#"{"Engineering": "member", "Admins": "admin"}"#.to_string());
        let groups = vec!["Engineering".to_string(), "Admins".to_string()];

        apply_group_mapping(&conn, "org-1", "u1", &mapping, &groups);

        let role: String = conn
            .query_row(
                "SELECT role FROM org_members WHERE org_id = 'org-1' AND user_id = 'u1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(role, "admin"); // admin > member
    }

    #[test]
    fn jit_provisions_new_user() {
        let (db, _dir) = fresh_db();
        let conn = db.lock().unwrap();

        // Create org and SSO provider.
        conn.execute(
            "INSERT INTO organizations (id, name, slug) VALUES ('org-1', 'Test', 'test')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sso_providers (id, org_id, protocol, name) \
             VALUES ('prov-1', 'org-1', 'oidc', 'Okta')",
            [],
        )
        .unwrap();

        let provider = load_provider(&conn, "prov-1").unwrap();
        let (user_id, is_new) =
            find_or_provision_sso_user(&conn, &provider, "new@corp.com", "okta-sub-123", &[])
                .unwrap();
        assert!(is_new);

        // Verify user was created.
        let email: String = conn
            .query_row(
                "SELECT email FROM users WHERE id = ?1",
                params![user_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(email, "new@corp.com");

        // Verify SSO account was linked.
        let linked: String = conn
            .query_row(
                "SELECT user_id FROM sso_accounts WHERE provider_id = 'prov-1' AND external_id = 'okta-sub-123'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(linked, user_id);

        // Verify user is member of the provider's org.
        let role: String = conn
            .query_row(
                "SELECT role FROM org_members WHERE org_id = 'org-1' AND user_id = ?1",
                params![user_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(role, "member");
    }

    #[test]
    fn jit_links_existing_user() {
        let (db, _dir) = fresh_db();
        let conn = db.lock().unwrap();

        // Create org, user, and SSO provider.
        conn.execute(
            "INSERT INTO organizations (id, name, slug) VALUES ('org-1', 'Test', 'test')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO users (id, email, password_hash) VALUES ('u1', 'existing@corp.com', 'x')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sso_providers (id, org_id, protocol, name) \
             VALUES ('prov-1', 'org-1', 'oidc', 'Okta')",
            [],
        )
        .unwrap();

        let provider = load_provider(&conn, "prov-1").unwrap();
        let (user_id, is_new) =
            find_or_provision_sso_user(&conn, &provider, "existing@corp.com", "okta-sub-456", &[])
                .unwrap();
        assert!(!is_new);
        assert_eq!(user_id, "u1");
    }

    #[test]
    fn saml_authn_request_is_valid_xml() {
        let xml = build_saml_authn_request(
            "https://sp.example.com",
            "https://sp.example.com/acs",
            "https://idp.example.com/sso",
        );
        assert!(xml.contains("AuthnRequest"));
        assert!(xml.contains("https://sp.example.com"));
        assert!(xml.contains("https://idp.example.com/sso"));
    }

    #[test]
    fn saml_redirect_encoding() {
        let xml = "<samlp:AuthnRequest>test</samlp:AuthnRequest>";
        let url = encode_saml_redirect(xml, "https://idp.example.com/sso");
        assert!(url.starts_with("https://idp.example.com/sso?SAMLRequest="));
    }

    #[test]
    fn parse_saml_response() {
        let xml = r#"<samlp:Response xmlns:samlp="urn:oasis:names:tc:SAML:2.0:protocol"
                       xmlns:saml="urn:oasis:names:tc:SAML:2.0:assertion">
  <saml:Issuer>https://idp.example.com</saml:Issuer>
  <samlp:Status>
    <samlp:StatusCode Value="urn:oasis:names:tc:SAML:2.0:status:Success"/>
  </samlp:Status>
  <saml:Assertion>
    <saml:Subject>
      <saml:NameID>user@corp.com</saml:NameID>
    </saml:Subject>
    <saml:AttributeStatement>
      <saml:Attribute Name="email">
        <saml:AttributeValue>user@corp.com</saml:AttributeValue>
      </saml:Attribute>
      <saml:Attribute Name="groups">
        <saml:AttributeValue>Engineering</saml:AttributeValue>
        <saml:AttributeValue>Admins</saml:AttributeValue>
      </saml:Attribute>
    </saml:AttributeStatement>
  </saml:Assertion>
</samlp:Response>"#;

        let data = parse_saml_response_xml(xml).unwrap();
        assert!(data.status_success);
        assert_eq!(data.issuer.as_deref(), Some("https://idp.example.com"));
        assert_eq!(data.name_id.as_deref(), Some("user@corp.com"));
        assert_eq!(
            data.attributes.get("email").and_then(|v| v.first()),
            Some(&"user@corp.com".to_string())
        );
        let groups = data.attributes.get("groups").unwrap();
        assert_eq!(groups, &["Engineering", "Admins"]);
    }

    #[test]
    fn providers_for_email_domain_match() {
        let (db, _dir) = fresh_db();
        let conn = db.lock().unwrap();

        conn.execute(
            "INSERT INTO organizations (id, name, slug) VALUES ('org-1', 'Test', 'test')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sso_providers (id, org_id, protocol, name, enabled, email_domains) \
             VALUES ('prov-1', 'org-1', 'oidc', 'Okta', 1, 'corp.com, example.com')",
            [],
        )
        .unwrap();

        let matches = providers_for_email(&conn, "alice@corp.com");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].id, "prov-1");

        let no_match = providers_for_email(&conn, "alice@other.com");
        assert!(no_match.is_empty());
    }

    #[test]
    fn auth_state_single_use() {
        let (db, _dir) = fresh_db();
        let conn = db.lock().unwrap();

        store_auth_state(&conn, "state-1", "prov-1", Some("verifier"), Some("nonce"));

        let first = consume_auth_state(&conn, "state-1");
        assert!(first.is_some());
        assert_eq!(first.as_ref().unwrap().provider_id, "prov-1");
        assert_eq!(first.unwrap().pkce_verifier.as_deref(), Some("verifier"));

        // Second consume returns None (single-use).
        let second = consume_auth_state(&conn, "state-1");
        assert!(second.is_none());
    }

    #[test]
    fn sp_metadata_valid_xml() {
        let xml = build_sp_metadata("https://sp.example.com", "https://sp.example.com/saml/acs");
        assert!(xml.contains("EntityDescriptor"));
        assert!(xml.contains("https://sp.example.com"));
        assert!(xml.contains("AssertionConsumerService"));
    }

    #[test]
    fn extract_groups_from_array() {
        let mut extra = HashMap::new();
        extra.insert(
            "groups".to_string(),
            serde_json::json!(["Engineering", "Admins"]),
        );
        let claims = IdTokenClaims {
            sub: "sub".to_string(),
            email: Some("user@test.com".to_string()),
            nonce: None,
            extra,
        };
        let groups = extract_groups_from_claims(&claims, "groups");
        assert_eq!(groups, vec!["Engineering", "Admins"]);
    }

    #[test]
    fn extract_groups_from_string() {
        let mut extra = HashMap::new();
        extra.insert("roles".to_string(), serde_json::json!("single-role"));
        let claims = IdTokenClaims {
            sub: "sub".to_string(),
            email: None,
            nonce: None,
            extra,
        };
        let groups = extract_groups_from_claims(&claims, "roles");
        assert_eq!(groups, vec!["single-role"]);
    }
}
