//! OpenID Connect sign-in: authorization-code flow with PKCE,
//! state and nonce.
//!
//! The ID token is taken directly from the token endpoint over a verified TLS connection, which
//! OpenID Connect Core 1.0 §3.1.3.7 (6) allows in place of checking its signature. Issuer,
//! audience, authorized party, expiry and nonce are still validated.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail, ensure};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use parking_lot::Mutex;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::access::OidcSettings;
use crate::auth::query_escape;
use crate::fsutil::random_hex;
use crate::github::USER_AGENT;
use crate::timeutil::now_epoch;

const PENDING_TTL: i64 = 600;
const DISCOVERY_TTL: i64 = 3600;
const CLOCK_SKEW: i64 = 120;

#[derive(Debug, Clone, Deserialize)]
struct Discovery {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
}

struct Pending {
    nonce: String,
    verifier: String,
    /// Site-relative page to return to after signing in.
    next: String,
    expires: i64,
}

pub struct Oidc {
    settings: OidcSettings,
    agent: ureq::Agent,
    discovery: Mutex<Option<(Discovery, i64)>>,
    pending: Mutex<HashMap<String, Pending>>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Identity {
    pub user: String,
    pub groups: Vec<String>,
}

impl Oidc {
    pub fn new(settings: OidcSettings, timeout_secs: u64) -> Self {
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(timeout_secs)))
            .user_agent(USER_AGENT)
            .build()
            .new_agent();
        Self {
            settings,
            agent,
            discovery: Mutex::new(None),
            pending: Mutex::new(HashMap::new()),
        }
    }

    fn discovery(&self) -> Result<Discovery> {
        if let Some((d, at)) = self.discovery.lock().clone()
            && now_epoch() - at < DISCOVERY_TTL
        {
            return Ok(d);
        }
        let url = format!("{}/.well-known/openid-configuration", self.settings.issuer);
        let mut resp = self
            .agent
            .get(&url)
            .call()
            .with_context(|| format!("fetch {url}"))?;
        ensure!(
            resp.status().as_u16() == 200,
            "{url}: HTTP {}",
            resp.status().as_u16()
        );
        let d: Discovery = serde_json::from_str(&resp.body_mut().read_to_string()?)
            .context("parse provider metadata")?;
        ensure!(
            d.issuer.trim_end_matches('/') == self.settings.issuer,
            "provider reports issuer {:?}, configured {:?}",
            d.issuer,
            self.settings.issuer
        );
        if self.settings.issuer.starts_with("https://") {
            ensure!(
                d.token_endpoint.starts_with("https://"),
                "token endpoint must use https"
            );
        }
        *self.discovery.lock() = Some((d.clone(), now_epoch()));
        Ok(d)
    }

    /// Start a login that returns to `next`: the provider URL to redirect the browser to and the
    /// `state`, which the caller binds to the browser (cookie) so a callback cannot be replayed in
    /// another browser.
    pub fn start(&self, next: String) -> Result<(String, String)> {
        let d = self.discovery()?;
        let state = random_hex(24);
        let nonce = random_hex(24);
        let verifier = URL_SAFE_NO_PAD.encode(random_hex(32));
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        {
            let now = now_epoch();
            let mut p = self.pending.lock();
            p.retain(|_, v| v.expires > now);
            p.insert(
                state.clone(),
                Pending {
                    nonce: nonce.clone(),
                    verifier,
                    next,
                    expires: now + PENDING_TTL,
                },
            );
        }
        let sep = if d.authorization_endpoint.contains('?') {
            '&'
        } else {
            '?'
        };
        let url = format!(
            "{}{sep}response_type=code&client_id={}&redirect_uri={}&scope={}&state={state}&nonce={nonce}&code_challenge={challenge}&code_challenge_method=S256",
            d.authorization_endpoint,
            query_escape(&self.settings.client_id),
            query_escape(&self.settings.redirect_url),
            query_escape(&self.settings.scopes.join(" ")),
        );
        Ok((url, state))
    }

    /// Finish a login from the callback's `code` and `state`: the identity and the page to
    /// return to.
    pub fn finish(&self, code: &str, state: &str) -> Result<(Identity, String)> {
        let pending = self
            .pending
            .lock()
            .remove(state)
            .ok_or_else(|| anyhow!("unknown or reused login state"))?;
        ensure!(
            pending.expires > now_epoch(),
            "login took too long; start again"
        );
        let d = self.discovery()?;
        let secret = std::fs::read_to_string(&self.settings.client_secret_file)
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        let mut req = self
            .agent
            .post(&d.token_endpoint)
            .header("Accept", "application/json");
        if !secret.is_empty() {
            let creds = format!(
                "{}:{}",
                query_escape(&self.settings.client_id),
                query_escape(&secret)
            );
            req = req.header("Authorization", format!("Basic {}", STANDARD.encode(creds)));
        }
        let form = [
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", self.settings.redirect_url.as_str()),
            ("client_id", self.settings.client_id.as_str()),
            ("code_verifier", pending.verifier.as_str()),
        ];
        let mut resp = req.send_form(form).context("token request")?;
        let status = resp.status().as_u16();
        let body = resp.body_mut().read_to_string().unwrap_or_default();
        ensure!(status == 200, "token endpoint answered HTTP {status}");
        let tokens: serde_json::Value =
            serde_json::from_str(&body).context("parse token response")?;
        let id_token = tokens["id_token"]
            .as_str()
            .ok_or_else(|| anyhow!("token response has no id_token"))?;
        let identity = self.identity_from(id_token, &d.issuer, &pending.nonce)?;
        Ok((identity, pending.next))
    }

    fn identity_from(&self, id_token: &str, issuer: &str, nonce: &str) -> Result<Identity> {
        let payload = id_token
            .split('.')
            .nth(1)
            .ok_or_else(|| anyhow!("malformed id_token"))?;
        let claims: serde_json::Value = serde_json::from_slice(
            &URL_SAFE_NO_PAD
                .decode(payload.trim_end_matches('='))
                .context("decode id_token")?,
        )?;
        let client = &self.settings.client_id;
        ensure!(
            claims["iss"].as_str() == Some(issuer),
            "id_token issuer mismatch"
        );
        let aud: Vec<&str> = match &claims["aud"] {
            serde_json::Value::String(s) => vec![s.as_str()],
            serde_json::Value::Array(a) => a.iter().filter_map(|v| v.as_str()).collect(),
            _ => vec![],
        };
        ensure!(
            aud.contains(&client.as_str()),
            "id_token audience does not include this client"
        );
        if aud.len() > 1 || claims.get("azp").is_some() {
            ensure!(
                claims["azp"].as_str() == Some(client.as_str()),
                "id_token authorized party mismatch"
            );
        }
        let exp = claims["exp"]
            .as_i64()
            .ok_or_else(|| anyhow!("id_token has no exp"))?;
        ensure!(exp + CLOCK_SKEW > now_epoch(), "id_token expired");
        ensure!(
            claims["nonce"].as_str() == Some(nonce),
            "id_token nonce mismatch"
        );
        let user = claims[self.settings.username_claim.as_str()]
            .as_str()
            .or_else(|| claims["sub"].as_str())
            .filter(|u| !u.is_empty())
            .ok_or_else(|| anyhow!("id_token has no user name"))?
            .to_string();
        let groups = match &claims[self.settings.groups_claim.as_str()] {
            serde_json::Value::Array(a) => a
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect(),
            serde_json::Value::String(s) => vec![s.clone()],
            _ => vec![],
        };
        if user.contains(['\r', '\n']) {
            bail!("invalid user name");
        }
        Ok(Identity { user, groups })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn oidc() -> Oidc {
        Oidc::new(
            OidcSettings {
                issuer: "https://id.example.net".into(),
                client_id: "craft".into(),
                client_secret_file: PathBuf::from("/nonexistent"),
                redirect_url: "https://apps.example.net/auth/oidc/callback".into(),
                scopes: vec!["openid".into()],
                username_claim: "preferred_username".into(),
                groups_claim: "groups".into(),
            },
            5,
        )
    }

    fn token(claims: serde_json::Value) -> String {
        format!(
            "eyJhbGciOiJSUzI1NiJ9.{}.sig",
            URL_SAFE_NO_PAD.encode(claims.to_string())
        )
    }

    #[test]
    fn id_token_claims_are_validated() {
        let o = oidc();
        let exp = now_epoch() + 300;
        let good = serde_json::json!({"iss": "https://id.example.net", "aud": "craft", "exp": exp, "nonce": "n1", "preferred_username": "ana", "groups": ["admins"]});
        assert_eq!(
            o.identity_from(&token(good.clone()), "https://id.example.net", "n1")
                .unwrap(),
            Identity {
                user: "ana".into(),
                groups: vec!["admins".into()]
            }
        );
        for (field, value, expect) in [
            ("iss", serde_json::json!("https://evil"), "issuer"),
            ("aud", serde_json::json!("other"), "audience"),
            ("exp", serde_json::json!(now_epoch() - 600), "expired"),
            ("nonce", serde_json::json!("n2"), "nonce"),
        ] {
            let mut c = good.clone();
            c[field] = value;
            let err = o
                .identity_from(&token(c), "https://id.example.net", "n1")
                .unwrap_err()
                .to_string();
            assert!(err.contains(expect), "{field}: {err}");
        }
        let mut multi = good.clone();
        multi["aud"] = serde_json::json!(["craft", "other"]);
        assert!(
            o.identity_from(&token(multi.clone()), "https://id.example.net", "n1")
                .unwrap_err()
                .to_string()
                .contains("authorized party")
        );
        multi["azp"] = serde_json::json!("craft");
        assert!(
            o.identity_from(&token(multi), "https://id.example.net", "n1")
                .is_ok()
        );
    }

    #[test]
    fn unknown_state_is_rejected_without_network() {
        assert!(
            oidc()
                .finish("code", "never-issued")
                .unwrap_err()
                .to_string()
                .contains("unknown or reused")
        );
    }
}
