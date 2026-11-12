//! mirror: reference/backend/lib/authlib.js
//! parity: DIVERGENT(AUTH-TOKEN-GEN): stateless HMAC-SHA1 tokens valid 300 s (vanilla: random env keys, 60 s TTL)
//!
//! Stateless HMAC-SHA1 tokens (300 s, fresh `X-Token` per response), AUTH-TOKEN-MW (ARCHITECTURE.md §8.5).
//! Owner: WP04. Token = `<uid>.<expiryHex>.<hex(HMAC-SHA1(secret, "<uid>.<expiryHex>"))>`; not single-use
//! (AUTH-TOKEN-CHECK GOTCHA), so any number of requests may reuse one until it expires.

use engine::doc::Id;
use engine::policy::real_now_ms;
use engine::world::{UserIdx, WorldView};
use std::sync::OnceLock;

/// Token lifetime (spec 10 recommends ≥ 120 s; the client polls every 60 s).
pub const TOKEN_TTL_MS: i64 = 300_000;

// ponytail: per-process secret, so a restart logs every client out once (they re-authenticate through the
// steam ticket / signin). Persist it with the world when BackendConfig carries one (interface request).
fn secret() -> &'static [u8; 20] {
    static SECRET: OnceLock<[u8; 20]> = OnceLock::new();
    SECRET.get_or_init(|| {
        let mut s = [0; 20];
        util::sys::os_random(&mut s);
        s
    })
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn mac(payload: &str) -> String {
    hex(&util::hmac::hmac_sha1(secret(), payload.as_bytes()))
}

/// `genToken(id)`.
pub fn gen_token(uid: &str) -> String {
    gen_token_at(uid, real_now_ms("CLK-API"))
}

fn gen_token_at(uid: &str, now_ms: i64) -> String {
    let payload = format!("{uid}.{:x}", now_ms + TOKEN_TTL_MS);
    let m = mac(&payload);
    format!("{payload}.{m}")
}

/// `checkToken(token)`: the user the token names, if the MAC matches, it has not expired and the user exists; else
/// (screeps-krebs) the user holding it as a long-lived API token (`authTokens`, screepsmod-auth's createAuthToken).
pub fn check_token(token: &str, view: &WorldView) -> Option<UserIdx> {
    if let Some(uid) = verify(token, real_now_ms("CLK-API")) {
        return view.user_ids.get(&Id::new(uid)?).copied();
    }
    api_token_user(token, view)
}

/// The user whose private `authTokens` holds `token`. Tokens are random UUIDs; a short or empty one never matches.
pub fn api_token_user(token: &str, view: &WorldView) -> Option<UserIdx> {
    if token.len() < 16 {
        return None;
    }
    view.users.iter().position(|u| match u.auth.get(engine::doc::Atom::intern("authTokens")) {
        Some(engine::doc::Val::Arr(list)) => list.iter().any(|e| match e {
            engine::doc::Val::Obj(o) => o.get(engine::doc::Atom::intern("token")).and_then(engine::doc::Val::as_str) == Some(token),
            engine::doc::Val::Str(s) => &**s == token,
            _ => false,
        }),
        _ => false,
    }).map(|i| i as UserIdx)
}

fn verify(token: &str, now_ms: i64) -> Option<&str> {
    let (payload, m) = token.rsplit_once('.')?;
    let (uid, expiry) = payload.rsplit_once('.')?;
    let expected = mac(payload);
    // constant-time compare
    if m.len() != expected.len() || m.bytes().zip(expected.bytes()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) != 0 {
        return None;
    }
    (i64::from_str_radix(expiry, 16).ok()? > now_ms).then_some(uid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_expiry_and_tamper() {
        let t = gen_token_at("5a1b2c3d4e5f607", 1_000);
        assert_eq!(verify(&t, 1_000), Some("5a1b2c3d4e5f607"));
        assert_eq!(verify(&t, 1_000 + TOKEN_TTL_MS - 1), Some("5a1b2c3d4e5f607"));
        assert_eq!(verify(&t, 1_000 + TOKEN_TTL_MS), None);
        let forged = t.replacen("5a1b2c3d4e5f607", "5a1b2c3d4e5f608", 1);
        assert_eq!(verify(&forged, 1_000), None);
        let (head, _) = t.rsplit_once('.').unwrap();
        assert_eq!(verify(&format!("{head}.{}", "0".repeat(40)), 1_000), None);
        assert_eq!(verify("garbage", 0), None);
        assert_eq!(verify("", 0), None);
        // npc ids work too
        assert_eq!(verify(&gen_token_at("2", 0), 0), Some("2"));
    }
}
