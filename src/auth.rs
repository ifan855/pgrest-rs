// JWT verification: HS256 (shared secret) and RS256 (JWKS), PostgREST style.
use base64::Engine;
use hmac::{Hmac, Mac};
use sha2::Sha256;

fn b64url_decode(s: &str) -> Result<Vec<u8>, String> {
    // tokens/JWKS fields are url-safe without padding; also accept standard
    // alphabet with padding (some JWKS producers)
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(s.trim_end_matches('='))
        .or_else(|_| {
            let mut t = s.trim_end_matches('=').replace('-', "+").replace('_', "/");
            while t.len() % 4 != 0 {
                t.push('=');
            }
            base64::engine::general_purpose::STANDARD.decode(&t)
        })
        .map_err(|e| format!("base64: {}", e))
}

fn b64url_decode_std(s: &str) -> Result<Vec<u8>, String> {
    // token parts are already url-safe
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(s.as_bytes())
        .or_else(|_| b64url_decode(s))
}

/// Verify an HS256 JWT. Returns claims on success.
pub fn verify_hs256(token: &str, secret: &[u8]) -> Result<serde_json::Value, String> {
    let parts: Vec<&str> = token.trim().split('.').collect();
    if parts.len() != 3 {
        return Err("token has invalid structure".into());
    }
    let header_bytes = b64url_decode_std(parts[0])?;
    let header: serde_json::Value =
        serde_json::from_slice(&header_bytes).map_err(|e| e.to_string())?;
    let alg = header.get("alg").and_then(|v| v.as_str()).unwrap_or("");
    if !alg.eq_ignore_ascii_case("HS256") {
        return Err(format!("unsupported JWT algorithm '{}' (configured keys: HS256)", alg));
    }
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).map_err(|e| e.to_string())?;
    mac.update(format!("{}.{}", parts[0], parts[1]).as_bytes());
    let expected = mac.finalize().into_bytes();
    let given = b64url_decode_std(parts[2])?;
    if given.len() != expected.len() {
        return Err("signature mismatch".into());
    }
    let mut diff = 0u8;
    for (a, b) in expected.iter().zip(given.iter()) {
        diff |= a ^ b;
    }
    if diff != 0 {
        return Err("signature mismatch".into());
    }
    let payload = b64url_decode_std(parts[1])?;
    let claims: serde_json::Value =
        serde_json::from_slice(&payload).map_err(|e| e.to_string())?;
    check_time_claims(&claims)?;
    Ok(claims)
}

/// Verify an RS256 JWT against a JWK (n, e) pair.
pub fn verify_rs256(token: &str, jwk: &Jwk) -> Result<serde_json::Value, String> {
    use rsa::pkcs1v15::{Signature, VerifyingKey};
    use rsa::sha2::Sha256;
    use rsa::signature::Verifier;
    let parts: Vec<&str> = token.trim().split('.').collect();
    if parts.len() != 3 {
        return Err("token has invalid structure".into());
    }
    let header: serde_json::Value =
        serde_json::from_slice(&b64url_decode_std(parts[0])?).map_err(|e| e.to_string())?;
    let alg = header.get("alg").and_then(|v| v.as_str()).unwrap_or("");
    if !alg.eq_ignore_ascii_case("RS256") {
        return Err(format!("unsupported JWT algorithm '{}' for this key", alg));
    }
    let n_bytes = b64url_decode(&jwk.n)?;
    let e_bytes = b64url_decode(&jwk.e)?;
    let n = rsa::BigUint::from_bytes_be(&n_bytes);
    let e = rsa::BigUint::from_bytes_be(&e_bytes);
    let pk = rsa::RsaPublicKey::new(n, e).map_err(|err| format!("jwk: {}", err))?;
    let sig_bytes = b64url_decode_std(parts[2])?;
    let sig = Signature::try_from(sig_bytes.as_slice()).map_err(|_| "signature mismatch".to_string())?;
    let vk = VerifyingKey::<Sha256>::new(pk);
    let signed = format!("{}.{}", parts[0], parts[1]);
    vk.verify(signed.as_bytes(), &sig)
        .map_err(|_| "signature mismatch".to_string())?;
    let claims: serde_json::Value =
        serde_json::from_slice(&b64url_decode_std(parts[1])?).map_err(|e| e.to_string())?;
    check_time_claims(&claims)?;
    Ok(claims)
}

fn check_time_claims(claims: &serde_json::Value) -> Result<(), String> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if let Some(exp) = claims.get("exp").and_then(|v| v.as_i64()) {
        if now > (exp.saturating_add(30)).max(0) as u64 {
            return Err("token expired".into());
        }
    }
    if let Some(nbf) = claims.get("nbf").and_then(|v| v.as_i64()) {
        if now.saturating_add(30) < nbf.max(0) as u64 {
            return Err("token not yet valid".into());
        }
    }
    Ok(())
}

// ---------- JWKS ----------

#[derive(Debug, Clone)]
pub struct Jwk {
    pub kid: Option<String>,
    pub kty: String,
    pub alg: Option<String>,
    pub n: String,
    pub e: String,
    /// kty=oct keys hold an HS256 shared secret (base64url, no padding)
    pub sym: Option<Vec<u8>>,
}

/// Cache of JWKS keys fetched from `jwks-url`. Refreshed lazily when an
/// unknown `kid` shows up (rate-limited to one fetch per few seconds).
pub struct JwksCache {
    url: Option<String>,
    keys: Vec<Jwk>,
    fetched_at: Option<std::time::Instant>,
}

impl JwksCache {
    pub fn new() -> JwksCache {
        JwksCache { url: None, keys: Vec::new(), fetched_at: None }
    }

    pub fn set_url(&mut self, url: String) {
        if self.url.as_deref() != Some(url.as_str()) {
            self.url = Some(url);
            self.keys.clear();
        }
    }

    /// Whether a (re)fetch is needed: never fetched, cache stale, or an
    /// unknown kid was presented.
    fn needs_fetch(&self, kid: Option<&str>) -> bool {
        match &self.fetched_at {
            None => true,
            Some(t) if t.elapsed() >= std::time::Duration::from_secs(300) => true,
            Some(_) => match kid {
                Some(k) => !self.keys.iter().any(|j| j.kid.as_deref() == Some(k)),
                None => self.keys.is_empty(),
            },
        }
    }

    fn store(&mut self, keys: Vec<Jwk>) {
        self.keys = keys;
        self.fetched_at = Some(std::time::Instant::now());
    }
}

pub fn fetch_jwks(url: &str) -> Result<Vec<Jwk>, String> {
    let resp = ureq::get(url)
        .timeout(std::time::Duration::from_secs(5))
        .call()
        .map_err(|e| format!("jwks fetch: {}", e))?;
    let body = resp.into_string().map_err(|e| format!("jwks read: {}", e))?;
    parse_jwks(&body)
}

pub fn parse_jwks(body: &str) -> Result<Vec<Jwk>, String> {
    let v: serde_json::Value = serde_json::from_str(body).map_err(|e| format!("jwks json: {}", e))?;
    let keys = v.get("keys").and_then(|k| k.as_array()).ok_or("jwks: missing 'keys'")?;
    let mut out = Vec::new();
    for k in keys {
        let kty = k.get("kty").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let kid = k.get("kid").and_then(|v| v.as_str()).map(|s| s.to_string());
        let alg = k.get("alg").and_then(|v| v.as_str()).map(|s| s.to_string());
        match kty.as_str() {
            "RSA" => {
                let (Some(n), Some(e)) = (
                    k.get("n").and_then(|v| v.as_str()),
                    k.get("e").and_then(|v| v.as_str()),
                ) else { continue };
                out.push(Jwk { kid, kty, alg, n: n.to_string(), e: e.to_string(), sym: None });
            }
            "oct" => {
                let Some(kk) = k.get("k").and_then(|v| v.as_str()) else { continue };
                let sym = b64url_decode(kk).ok();
                if let Some(sym) = sym {
                    out.push(Jwk { kid, kty, alg, n: String::new(), e: String::new(), sym: Some(sym) });
                }
            }
            _ => {}
        }
    }
    Ok(out)
}

/// Entry point: verify a token with the configured secret and/or JWKS.
/// Tokens are ignored when neither is configured (documented PostgREST-like).
pub async fn verify_token(
    token: &str,
    cfg: &crate::config::Config,
    jwks: &tokio::sync::RwLock<JwksCache>,
) -> Result<serde_json::Value, String> {
    let header = token_header(token)?;
    let alg = header.get("alg").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let kid = header.get("kid").and_then(|v| v.as_str()).map(|s| s.to_string());
    let alg_is_rs = alg.eq_ignore_ascii_case("RS256");

    // HS256 with the configured secret, or an oct key from the JWKS
    if alg.eq_ignore_ascii_case("HS256") {
        if let Some(secret) = &cfg.jwt_secret {
            return verify_hs256(token, secret);
        }
        if cfg.jwks_url.is_some() {
            let keys = jwks.read().await.keys.clone();
            let octs: Vec<&Jwk> = keys
                .iter()
                .filter(|j| j.kty == "oct")
                .filter(|j| match &kid {
                    Some(k) => j.kid.as_deref() == Some(k.as_str()),
                    None => true,
                })
                .collect();
            let mut last_err = "no matching oct key in JWKS".to_string();
            for j in octs {
                if let Some(sym) = &j.sym {
                    match verify_hs256(token, sym) {
                        Ok(c) => return Ok(c),
                        Err(e) => last_err = e,
                    }
                }
            }
            return Err(last_err);
        }
    }

    // RS256 requires a JWKS
    if alg_is_rs {
        let Some(url) = cfg.jwks_url.clone() else {
            return Err(format!("unsupported JWT algorithm '{}' (no jwks-url configured)", alg));
        };
        let need = {
            let r = jwks.read().await;
            r.needs_fetch(kid.as_deref())
        };
        if need {
            let fetched = tokio::task::spawn_blocking(move || fetch_jwks(&url))
                .await
                .map_err(|e| format!("jwks task: {}", e))??;
            jwks.write().await.store(fetched);
        }
        let keys: Vec<Jwk> = jwks.read().await.keys.clone();
        let candidates: Vec<&Jwk> = keys
            .iter()
            .filter(|j| match &kid {
                Some(k) => j.kid.as_deref() == Some(k.as_str()),
                None => true,
            })
            .filter(|j| j.kty == "RSA")
            .collect();
        let mut last_err = "no matching RSA key in JWKS".to_string();
        for j in candidates {
            match verify_rs256(token, j) {
                Ok(c) => return Ok(c),
                Err(e) => last_err = e,
            }
        }
        return Err(last_err);
    }

    // unknown algorithm / no secret configured: reject (PostgREST rejects
    // tokens when it cannot validate them)
    Err(format!("unsupported JWT algorithm '{}'", alg))
}

fn token_header(token: &str) -> Result<serde_json::Value, String> {
    let parts: Vec<&str> = token.trim().split('.').collect();
    if parts.len() != 3 {
        return Err("token has invalid structure".into());
    }
    let hdr = b64url_decode_std(parts[0])?;
    serde_json::from_slice(&hdr).map_err(|e| e.to_string())
}

pub fn extract_role(claims: &serde_json::Value, default_role: &str) -> String {
    claims
        .get("role")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| default_role.to_string())
}
