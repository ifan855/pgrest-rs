// JWT (HS256) verification -> role claim + payload, PostgREST style.
use base64::Engine;
use hmac::{Hmac, Mac};
use sha2::Sha256;

fn b64url_decode(s: &str) -> Result<Vec<u8>, String> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(s.trim_end_matches('=').replace('-', "+").replace('_', "/").as_bytes())
        .map_err(|e| format!("base64: {}", e))
}

fn b64url_decode_std(s: &str) -> Result<Vec<u8>, String> {
    // token parts are already url-safe
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(s.as_bytes())
        .or_else(|_| b64url_decode(s))
}

pub struct JwtResult {
    pub role: Option<String>,
    pub claims: serde_json::Value,
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
        return Err(format!("unsupported JWT algorithm '{}' (pgrest-rs supports HS256)", alg));
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

    // exp / nbf checks (with small clock skew tolerance)
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
    Ok(claims)
}

pub fn extract_role(claims: &serde_json::Value, default_role: &str) -> String {
    claims
        .get("role")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| default_role.to_string())
}
