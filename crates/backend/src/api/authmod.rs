//! not a mirror: the screepsmod-auth surface (signin, password, register/submit, HTTP Basic) (§8.5). Owner: WP04.
//!
//! Shapes follow the client and xxscreeps' password mod (spec 10 AUTHMOD-*): `email` may be a username or an
//! e-mail; passwords are PBKDF2-HMAC-SHA1 (100 000 iterations, 16-byte salt) in `UserState.auth`
//! `{password, salt}` (hex). screeps-krebs also verifies screepsmod-auth's own hashes, as an imported world or a
//! perf tool writing `users.password/salt` holds them: PBKDF2-HMAC-SHA256 of the password over the salt text, 25 000
//! iterations, 512 bytes, hex (told apart by the hash's length).

use crate::api::auth::{default_code, new_user_doc};
use crate::http::{obj, Request, Response};
use crate::{authlib, edit_user, obj_set, s, str_field, ApiCtx};
use engine::doc::Obj;
use engine::world::{UserIdx, WorldView};
use std::sync::Arc;
use util::json::Json;

pub const PBKDF2_ITERATIONS: u32 = 100_000;

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    s.len().is_multiple_of(2).then(|| (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok()).collect())?
}

fn hash(password: &str, salt: &[u8]) -> [u8; 20] {
    let mut out = [0u8; 20];
    util::hmac::pbkdf2_sha1(password.as_bytes(), salt, PBKDF2_ITERATIONS, &mut out);
    out
}

/// `UserState.auth` with a fresh salt and hash for `password`.
fn auth_for(password: &str, keep: &Obj) -> Obj {
    let mut salt = [0u8; 16];
    util::sys::os_random(&mut salt);
    let mut a = keep.clone();
    obj_set(&mut a, "password", s(&hex(&hash(password, &salt))));
    obj_set(&mut a, "salt", s(&hex(&salt)));
    a
}

fn check_password(auth: &Obj, password: &str) -> bool {
    let (Some(stored), Some(salt_text)) = (str_field(auth, "password"), str_field(auth, "salt")) else { return false };
    if stored.len() == SCREEPSMOD_AUTH_KEYLEN * 2 {
        return check_screepsmod_auth(stored, salt_text, password);
    }
    let (Some(h), Some(salt)) = (unhex(stored), unhex(salt_text)) else { return false };
    let got = hash(password, &salt);
    h.len() == 20 && h.iter().zip(got).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0
}

/// screepsmod-auth's `crypto.pbkdf2(password, salt, 25000, 512, 'sha256')` in hex: the salt string's own bytes.
const SCREEPSMOD_AUTH_ITERATIONS: u32 = 25_000;
const SCREEPSMOD_AUTH_KEYLEN: usize = 512;

fn check_screepsmod_auth(stored: &str, salt: &str, password: &str) -> bool {
    let Some(h) = unhex(stored) else { return false };
    let mut got = vec![0u8; SCREEPSMOD_AUTH_KEYLEN];
    util::sha256::pbkdf2_sha256(password.as_bytes(), salt.as_bytes(), SCREEPSMOD_AUTH_ITERATIONS, &mut got);
    h.len() == got.len() && h.iter().zip(&got).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0
}

/// A username, or an e-mail when it contains `@` (xxscreeps findUserByUsernameOrEmail).
fn find_user(view: &WorldView, name: &str) -> Option<UserIdx> {
    let lower = name.to_lowercase();
    if name.contains('@') {
        return view.users.iter().position(|u| str_field(&u.doc, "email") == Some(lower.as_str())).map(|i| i as UserIdx);
    }
    view.usernames.get(lower.as_str()).copied()
}

/// HTTP Basic (`Authorization: Basic base64(user:password)`) → the user, when the password matches.
pub(crate) fn basic_user(view: &WorldView, header: &str) -> Option<UserIdx> {
    let b64 = header.strip_prefix("Basic ")?;
    let raw = String::from_utf8(util::base64::decode(b64.trim())?).ok()?;
    let (name, password) = raw.split_once(':')?;
    let u = find_user(view, name)?;
    check_password(&view.users[u as usize].auth, password).then_some(u)
}

fn str_param<'a>(b: &'a Json, k: &str) -> Option<&'a str> {
    b.get(k).and_then(Json::as_str)
}

/// POST /api/auth/signin
pub fn signin(req: &Request, ctx: &ApiCtx) -> Response {
    let b = req.body();
    let (Some(email), Some(password)) = (str_param(&b, "email"), str_param(&b, "password")) else {
        return Response::unauthorized();
    };
    let view = ctx.published.get();
    match find_user(&view, email) {
        Some(u) if check_password(&view.users[u as usize].auth, password) => Response::ok(obj(vec![("token", Json::Str(authlib::gen_token(view.users[u as usize].id.as_str())))])),
        _ => Response::unauthorized(),
    }
}

/// POST /api/user/password
pub fn password(req: &Request, ctx: &ApiCtx) -> Response {
    let Some(u) = ctx.user else { return Response::unauthorized() };
    let b = req.body();
    let (Some(old), Some(new)) = (str_param(&b, "oldPassword"), str_param(&b, "password")) else {
        return Response::error("Invalid password");
    };
    let auth = ctx.published.get().users[u as usize].auth.clone();
    // a user without a password (Steam sign-up) may set one without the old password
    if new.encode_utf16().count() < 8 || (str_field(&auth, "password").is_some() && !check_password(&auth, old)) {
        return Response::error("Invalid password");
    }
    let next = auth_for(new, &auth);
    match crate::write(ctx, move |w| {
        edit_user(w, u, |st| st.auth = Arc::new(next));
        Ok(())
    }) {
        Ok(()) => Response::ok(Json::Null),
        Err(e) => Response::error(&e),
    }
}

/// `^[a-zA-Z0-9_-]{3,}$` (register.js set-username), and at most 32 long: names go out to every page on every poll.
/// A new name only: worlds keep the names they hold.
pub fn valid_username(name: &str) -> bool {
    (3..=32).contains(&name.len()) && name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}

/// POST /api/register/submit
pub fn submit(req: &Request, ctx: &ApiCtx) -> Response {
    let b = req.body();
    let (Some(username), Some(email), Some(password)) = (str_param(&b, "username"), str_param(&b, "email"), str_param(&b, "password")) else {
        return Response::error("invalid params");
    };
    if !valid_username(username) {
        return Response::error("invalid");
    }
    let (username, email) = (username.to_string(), email.to_lowercase());
    let auth = auth_for(password, &Obj::new());
    // Here and not in `write`, which runs on the tick thread.
    let modules = match b.get("modules").filter(|m| matches!(m, Json::Obj(_))).map(|m| crate::api::user::checked_modules(m.clone())).transpose() {
        Ok(m) => m,
        Err(e) => return Response::error(&e),
    };
    let r = crate::write(ctx, move |w| {
        let lower = username.to_lowercase();
        if w.usernames.contains_key(lower.as_str()) {
            return Err("exists".to_string());
        }
        let doc = new_user_doc(w, vec![("email", s(&email)), ("username", s(&username)), ("usernameLower", s(&lower))]);
        crate::add_user(w, doc, auth, vec![default_code(modules)]);
        Ok(())
    });
    match r {
        Ok(()) => Response::ok(Json::Null),
        Err(e) => Response::error(&e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// screepsmod-auth's own hash, made by node: crypto.pbkdf2Sync(Buffer.from('hunter22'), Buffer.from(salt), 25000, 512, 'sha256').
    #[test]
    fn verifies_screepsmod_auth_hashes() {
        let mut auth = Obj::new();
        obj_set(&mut auth, "salt", s("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"));
        obj_set(&mut auth, "password", s("e191e780f6f1d2ae75f3cc49f63c191fde3938584a3aa469330e9c53cc3e29a1228106f1562e03195c18ec0f1b8a9b9ac804757956c9152f02f6d7dee9b6b0b91a148caaddc88c5aa63fd37ee56c5f244d48e3007d6eef4a2d85ab0ef38a898c7b7f4244bde04769aee69d463d694a50bf5808648fdab53ced6af495689bd057233801c4c9e3364a921a368eb11abc6c0a4218c8589888ee4da764ba1a2a7582ebaa1c25269cf571d31274dc9f23b2f7c6958c2019359956fed1ab89b7e5ac115712d18e8fd43d58dc8709a0296a9a7ee0545bfa055a88b872793eda7fa06496f86f1ad29c04a6db2e127a6f3ef4f95523ac9081734efe11bb77db9c46de072d2b0fed0479eafa78d9f45f8a1b5faef14cf49e1fbecfd74b64e69aac3c8eaa3b7849c10cec2274cff23074c7139e5fbde5cb8701e2dd28a8a36d0235f2106e82ed79e832149a07ed81f0710a264518d09013acaeb18e2501aa03b1e0efbc67cf5f63991b90613433af60ecdc0efb514d69397b035ca48e3d3df1e0d874fc2ff1b9e653475e1f0bb229ae395acb3e470a5b67398ac18349ca3879ffaa15c84021053dce0f5caeafe9a1404113e9eebb289499af2080f7bf27c7bf49534b3a78769b8296a83fbfa9ae1781cdfdb9c668f715b46683f59c6de94d50eb3a5cf4574d8530c5590e9775c03cb62fd2a9637ea2070e5373572bfeaedce2bed483602475"));
        assert!(check_password(&auth, "hunter22"));
        assert!(!check_password(&auth, "hunter23"));
        let own = auth_for("hunter22", &Obj::new());
        assert!(check_password(&own, "hunter22"), "Krebs's own hashes still verify");
        assert!(!check_password(&own, "hunter23"));
    }
    use crate::fake;

    #[test]
    fn register_signin_basic_and_password_change() {
        let (ctx, _h) = fake::ctx(None);
        let (_, j) = fake::run(submit, &ctx, "POST", "/api/register/submit", r#"{"username":"Carol","email":"C@x.io","password":"hunter22"}"#);
        assert_eq!(j.stringify(), r#"{"ok":1}"#);
        let (_, dup) = fake::run(submit, &ctx, "POST", "/api/register/submit", r#"{"username":"carol","email":"d@x.io","password":"x"}"#);
        assert_eq!(dup.get("error").and_then(Json::as_str), Some("exists"));
        let (_, bad) = fake::run(submit, &ctx, "POST", "/api/register/submit", r#"{"username":"c d","email":"d@x.io","password":"x"}"#);
        assert_eq!(bad.get("error").and_then(Json::as_str), Some("invalid"));

        let (st, j) = fake::run(signin, &ctx, "POST", "/api/auth/signin", r#"{"email":"CAROL","password":"hunter22"}"#);
        assert_eq!(st, 200);
        let view = ctx.published.get();
        let carol = authlib::check_token(j.get("token").and_then(Json::as_str).unwrap(), &view).unwrap();
        assert_eq!(fake::run(signin, &ctx, "POST", "/api/auth/signin", r#"{"email":"c@x.io","password":"hunter22"}"#).0, 200);
        assert_eq!(fake::run(signin, &ctx, "POST", "/api/auth/signin", r#"{"email":"carol","password":"wrong"}"#).0, 401);
        let basic = format!("Basic {}", util::base64::encode(b"carol:hunter22"));
        assert_eq!(basic_user(&view, &basic), Some(carol));
        assert_eq!(basic_user(&view, &format!("Basic {}", util::base64::encode(b"carol:nope"))), None);

        let ctx = crate::ApiCtx { published: ctx.published.clone(), commands: ctx.commands.clone(), user: Some(carol) };
        let (_, short) = fake::run(password, &ctx, "POST", "/api/user/password", r#"{"oldPassword":"hunter22","password":"short"}"#);
        assert_eq!(short.get("error").and_then(Json::as_str), Some("Invalid password"));
        let (_, ok) = fake::run(password, &ctx, "POST", "/api/user/password", r#"{"oldPassword":"hunter22","password":"longer-pass"}"#);
        assert_eq!(ok.stringify(), r#"{"ok":1}"#);
        assert_eq!(fake::run(signin, &ctx, "POST", "/api/auth/signin", r#"{"email":"carol","password":"longer-pass"}"#).0, 200);
    }

    #[test]
    fn usernames_have_a_length() {
        assert!(!valid_username("ab"));
        assert!(valid_username("abc"));
        assert!(valid_username(&"n".repeat(32)));
        assert!(!valid_username(&"n".repeat(33)));
        assert!(!valid_username(&"n".repeat(10 << 20)));
    }

    /// The modules of a sign-up are converted before `write`, with the same limits as an upload.
    #[test]
    fn submit_checks_modules_as_an_upload_does() {
        let (ctx, _h) = fake::ctx(None);
        let body = |name: &str, modules: String| format!(r#"{{"username":"{name}","email":"{name}@x.io","password":"hunter22","modules":{modules}}}"#);
        let many: Vec<String> = (0..=crate::api::user::MAX_MODULES).map(|i| format!(r#""m{i}":"""#)).collect();
        let (_, j) = fake::run(submit, &ctx, "POST", "/api/register/submit", &body("many", format!("{{{}}}", many.join(","))));
        assert_eq!(j.get("error").and_then(Json::as_str), Some("too many modules"));
        assert!(ctx.published.get().usernames.get("many").is_none());
        let (_, j) = fake::run(submit, &ctx, "POST", "/api/register/submit", &body("fine", r#"{"main":"x","a.b":"y"}"#.into()));
        assert_eq!(j.stringify(), r#"{"ok":1}"#);
    }
}
