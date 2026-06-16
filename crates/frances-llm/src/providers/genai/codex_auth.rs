//! Reuse the credentials the OpenAI Codex CLI stores after `codex login`.
//!
//! Reads `auth.json` (the same file `codex` writes), hands back the
//! access token plus the ChatGPT account id, and refreshes the access
//! token against `auth.openai.com` when it is at or near expiry —
//! persisting the rotated tokens back to the file so the next refresh
//! (ours or the Codex CLI's) sees the current `refresh_token`.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error as ThisError;
use tracing::debug;

/// OAuth client id the Codex CLI uses for the token-refresh grant.
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const REFRESH_URL: &str = "https://auth.openai.com/oauth/token";
/// Refresh once the access token is within this many seconds of expiry.
const REFRESH_MARGIN_SECS: u64 = 300;

#[derive(Debug, ThisError)]
pub enum Error {
    #[error("no HOME in client environment; set auth.codex_home or CODEX_HOME")]
    NoHome,
    #[error("read codex auth file {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("parse codex auth file {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("codex auth file {path} has no tokens — run `codex login`")]
    NoTokens { path: PathBuf },
    #[error("write codex auth file {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("serialize codex auth json: {0}")]
    Serialize(#[source] serde_json::Error),
    #[error("send codex token refresh: {0}")]
    RefreshSend(#[source] reqwest::Error),
    #[error("codex token refresh failed ({status}): {body}")]
    RefreshStatus {
        status: reqwest::StatusCode,
        body: String,
    },
    #[error("decode codex token refresh response: {0}")]
    RefreshDecode(#[source] reqwest::Error),
}

/// What the genai provider needs to authenticate one request: the bearer
/// token and the workspace account id (sent as `ChatGPT-Account-ID`).
pub(super) struct CodexCredentials {
    pub(super) access_token: String,
    pub(super) account_id: Option<String>,
}

/// `auth.json`. We model only the fields we touch; everything else is
/// preserved through `rest` so a refresh write-back doesn't drop the
/// Codex CLI's own keys (`OPENAI_API_KEY`, `auth_mode`, …).
#[derive(Debug, Deserialize, Serialize)]
struct AuthDotJson {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tokens: Option<Tokens>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_refresh: Option<String>,
    #[serde(flatten)]
    rest: serde_json::Map<String, Value>,
}

#[derive(Debug, Deserialize, Serialize)]
struct Tokens {
    access_token: String,
    refresh_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    id_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    account_id: Option<String>,
    #[serde(flatten)]
    rest: serde_json::Map<String, Value>,
}

#[derive(Serialize)]
struct RefreshRequest<'a> {
    client_id: &'a str,
    grant_type: &'a str,
    refresh_token: &'a str,
}

#[derive(Deserialize)]
struct RefreshResponse {
    id_token: Option<String>,
    access_token: Option<String>,
    refresh_token: Option<String>,
}

pub(super) async fn resolve(
    codex_home: Option<&Path>,
    env: &HashMap<OsString, OsString>,
    http: &reqwest::Client,
) -> Result<CodexCredentials, Error> {
    let path = auth_json_path(codex_home, env)?;
    let raw = std::fs::read_to_string(&path).map_err(|source| Error::Read {
        path: path.clone(),
        source,
    })?;
    let mut auth: AuthDotJson = serde_json::from_str(&raw).map_err(|source| Error::Parse {
        path: path.clone(),
        source,
    })?;

    let (refresh_token, needs_refresh) = {
        let tokens = auth
            .tokens
            .as_ref()
            .ok_or_else(|| Error::NoTokens { path: path.clone() })?;
        // Only refresh when we can read an `exp` and it has (nearly) passed.
        // An undecodable token is left as-is — refreshing on a hunch would
        // burn a single-use refresh token for a token that may be fine.
        let needs = token_expiry(&tokens.access_token)
            .is_some_and(|exp| exp <= now_unix() + REFRESH_MARGIN_SECS);
        (tokens.refresh_token.clone(), needs)
    };

    if needs_refresh {
        debug!(path = %path.display(), "refreshing codex access token");
        let refreshed = request_refresh(&refresh_token, http).await?;
        let tokens = auth.tokens.as_mut().expect("tokens present after check");
        if let Some(access_token) = refreshed.access_token {
            tokens.access_token = access_token;
        }
        if let Some(refresh_token) = refreshed.refresh_token {
            tokens.refresh_token = refresh_token;
        }
        if let Some(id_token) = refreshed.id_token {
            tokens.id_token = Some(id_token);
        }
        auth.last_refresh = Some(chrono::Utc::now().to_rfc3339());
        write_auth_json(&path, &auth)?;
    }

    let tokens = auth.tokens.as_ref().expect("tokens present after check");
    let account_id = tokens.account_id.clone().or_else(|| {
        tokens
            .id_token
            .as_deref()
            .and_then(account_id_from_id_token)
    });
    Ok(CodexCredentials {
        access_token: tokens.access_token.clone(),
        account_id,
    })
}

fn auth_json_path(
    codex_home: Option<&Path>,
    env: &HashMap<OsString, OsString>,
) -> Result<PathBuf, Error> {
    if let Some(home) = codex_home {
        return Ok(home.join("auth.json"));
    }
    if let Some(dir) = env.get(OsStr::new("CODEX_HOME")) {
        return Ok(PathBuf::from(dir).join("auth.json"));
    }
    let home = env.get(OsStr::new("HOME")).ok_or(Error::NoHome)?;
    Ok(PathBuf::from(home).join(".codex").join("auth.json"))
}

async fn request_refresh(
    refresh_token: &str,
    http: &reqwest::Client,
) -> Result<RefreshResponse, Error> {
    let response = http
        .post(REFRESH_URL)
        .header("Content-Type", "application/json")
        .json(&RefreshRequest {
            client_id: CLIENT_ID,
            grant_type: "refresh_token",
            refresh_token,
        })
        .send()
        .await
        .map_err(Error::RefreshSend)?;

    let status = response.status();
    if !status.is_success() {
        // Cap the (possibly large, possibly HTML) error body. Take chars,
        // not bytes — String::truncate panics off a char boundary.
        let body: String = response
            .text()
            .await
            .unwrap_or_default()
            .chars()
            .take(500)
            .collect();
        return Err(Error::RefreshStatus { status, body });
    }
    response
        .json::<RefreshResponse>()
        .await
        .map_err(Error::RefreshDecode)
}

fn write_auth_json(path: &Path, auth: &AuthDotJson) -> Result<(), Error> {
    let json = serde_json::to_string_pretty(auth).map_err(Error::Serialize)?;
    // Write to a sibling temp file, then rename, so a crash mid-write can't
    // truncate the live credential file. "auth.json" -> "auth.json.tmp".
    let tmp = path.with_extension("json.tmp");
    write_private(&tmp, json.as_bytes()).map_err(|source| Error::Write {
        path: tmp.clone(),
        source,
    })?;
    std::fs::rename(&tmp, path).map_err(|source| Error::Write {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(unix)]
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)
}

#[cfg(not(unix))]
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, bytes)
}

/// Decode a JWT's payload (the middle segment) as JSON. Returns `None`
/// for anything that isn't a well-formed three-part JWT.
fn jwt_payload(jwt: &str) -> Option<Value> {
    let payload = jwt.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn token_expiry(jwt: &str) -> Option<u64> {
    jwt_payload(jwt)?.get("exp")?.as_u64()
}

fn account_id_from_id_token(jwt: &str) -> Option<String> {
    jwt_payload(jwt)?
        .get("https://api.openai.com/auth")?
        .get("chatgpt_account_id")?
        .as_str()
        .map(str::to_owned)
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a fake JWT (`header.payload.sig`) whose payload is `claims`.
    fn jwt_with(claims: Value) -> String {
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&claims).unwrap());
        format!("eyJhbGciOiJub25lIn0.{payload}.sig")
    }

    #[test]
    fn token_expiry_reads_exp() {
        let jwt = jwt_with(serde_json::json!({ "exp": 1_700_000_000_u64 }));
        assert_eq!(token_expiry(&jwt), Some(1_700_000_000));
    }

    #[test]
    fn token_expiry_none_for_garbage() {
        assert_eq!(token_expiry("not-a-jwt"), None);
        assert_eq!(token_expiry(&jwt_with(serde_json::json!({}))), None);
    }

    #[test]
    fn account_id_from_nested_auth_claim() {
        let jwt = jwt_with(serde_json::json!({
            "https://api.openai.com/auth": { "chatgpt_account_id": "acct-123" }
        }));
        assert_eq!(account_id_from_id_token(&jwt), Some("acct-123".to_string()));
    }

    #[test]
    fn auth_path_prefers_explicit_home_then_env() {
        let mut env = HashMap::new();
        let explicit = Path::new("/custom/codex");
        assert_eq!(
            auth_json_path(Some(explicit), &env).unwrap(),
            PathBuf::from("/custom/codex/auth.json")
        );

        env.insert(OsString::from("CODEX_HOME"), OsString::from("/env/codex"));
        assert_eq!(
            auth_json_path(None, &env).unwrap(),
            PathBuf::from("/env/codex/auth.json")
        );

        env.remove(OsStr::new("CODEX_HOME"));
        env.insert(OsString::from("HOME"), OsString::from("/home/user"));
        assert_eq!(
            auth_json_path(None, &env).unwrap(),
            PathBuf::from("/home/user/.codex/auth.json")
        );
    }

    /// Unknown keys (e.g. `OPENAI_API_KEY`, `auth_mode`) survive a
    /// parse → serialize round-trip via the flattened `rest` catch-all.
    #[test]
    fn round_trip_preserves_unknown_fields() {
        let raw = serde_json::json!({
            "OPENAI_API_KEY": "sk-test",
            "auth_mode": "chatgpt",
            "tokens": {
                "access_token": "at",
                "refresh_token": "rt",
                "id_token": "it",
                "account_id": "acct",
                "future_field": 7
            },
            "last_refresh": "2026-01-01T00:00:00Z"
        });
        let auth: AuthDotJson = serde_json::from_value(raw.clone()).unwrap();
        let out = serde_json::to_value(&auth).unwrap();
        assert_eq!(out, raw);
    }
}
