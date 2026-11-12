//! The browser client of the UCS perf lanes (pieterbrandsen/screepsmod-client-new v0.2.33-single-shard-room-channel.5,
//! its index.js) and their `/broadcast` page (perf/mods/run-broadcast.js):
//!
//! * `GET /client`, `/client/`, `/client/index.html`: the client's index.html with `window.__SCREEPS_CLIENT_EMBEDDED__`
//!   and `window.__SCREEPS_BOOTSTRAP__` (this server's `/api/version` reply) put before `</head>`, `no-cache`;
//! * `GET /client/<file>`: the file from the client's build (`dist/embedded`, copied to `<data>/client` or to
//!   `clientDir` in ucs.json), `_client/` assets cached for good and the rest revalidated; any other GET under
//!   `/client/` is the index (the single-page app's fallback);
//! * `GET /`: a 302 to `/client/`;
//! * `GET /broadcast`: the recording page, an iframe of the client on one room.

use backend::http::{Request, Response};
use backend::mods::Registry;
use backend::{ApiCtx, Route};
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;
use util::json::Json;

const PACKAGE: &str = "screepsmod-client-new";
const PACKAGE_VERSION: &str = "0.2.33-single-shard-room-channel.5";
const BROADCAST: &str = include_str!("broadcast.html");

static DIR: OnceLock<PathBuf> = OnceLock::new();

pub fn register(r: &mut Registry, ucs_json: &Json) {
    let dir = ucs_json.get("clientDir").and_then(Json::as_str).map_or_else(|| r.data_dir.join("client"), PathBuf::from);
    let _ = DIR.set(dir);
    for path in ["/client", "/client/*"] {
        r.routes.push(Route { method: "GET", path, handler: client });
    }
    r.routes.push(Route { method: "GET", path: "/", handler: root });
    r.routes.push(Route { method: "GET", path: "/broadcast", handler: broadcast });
}

fn root(_req: &Request, _ctx: &ApiCtx) -> Response {
    let mut res = Response::text(302, "text/plain; charset=utf-8", "Found. Redirecting to /client/");
    res.headers.push(("Location".into(), "/client/".into()));
    res
}

fn broadcast(_req: &Request, _ctx: &ApiCtx) -> Response {
    Response::text(200, "text/html; charset=utf-8", BROADCAST)
}

/// `JSON.stringify(value).replace(/</g, '\\u003c')`: safe inside a `<script>`.
fn json_for_script(v: &Json) -> String {
    v.stringify().replace('<', "\\u003c")
}

/// index.js renderInjectedIndex.
pub fn inject(html: &str, version: Option<&Json>) -> String {
    let metadata = Json::Obj(vec![
        ("kind".into(), Json::Str("screeps-mod".into())),
        ("packageName".into(), Json::Str(PACKAGE.into())),
        ("version".into(), Json::Str(PACKAGE_VERSION.into())),
    ]);
    let bootstrap = version.map(|v| format!(";window.__SCREEPS_BOOTSTRAP__={}", json_for_script(v))).unwrap_or_default();
    let script = format!("<script>window.__SCREEPS_CLIENT_EMBEDDED__={}{bootstrap}</script>", json_for_script(&metadata));
    match html.find("</head>") {
        Some(i) => format!("{}{script}{}", &html[..i], &html[i..]),
        None => format!("{script}{html}"),
    }
}

fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "application/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "wasm" => "application/wasm",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// The file under the client's build a request names, if it names one inside it.
pub fn file_under(dir: &Path, rel: &str) -> Option<PathBuf> {
    let rel = Path::new(rel.trim_start_matches('/'));
    if rel.as_os_str().is_empty() || rel.components().any(|c| !matches!(c, Component::Normal(_))) {
        return None;
    }
    Some(dir.join(rel))
}

fn index(dir: &Path, req: &Request, ctx: &ApiCtx) -> Response {
    let Ok(html) = std::fs::read_to_string(dir.join("index.html")) else {
        return Response::text(404, "text/plain; charset=utf-8", "The browser client is not installed on this server (<data>/client).");
    };
    let version = Json::parse(&String::from_utf8_lossy(&backend::server::version(req, ctx).body)).ok();
    let mut res = Response::text(200, "text/html; charset=utf-8", &inject(&html, version.as_ref()));
    res.headers.push(("Cache-Control".into(), "no-cache".into()));
    res
}

fn client(req: &Request, ctx: &ApiCtx) -> Response {
    let Some(dir) = DIR.get() else { return Response::not_found("GET", &req.path) };
    let rel = req.path.strip_prefix("/client").unwrap_or("");
    if matches!(rel, "" | "/" | "/index.html") {
        return index(dir, req, ctx);
    }
    match file_under(dir, rel).filter(|f| f.is_file()) {
        Some(file) => match std::fs::read(&file) {
            Ok(body) => {
                let cache = if rel.starts_with("/_client/") { "public, max-age=31536000, immutable" } else { "no-cache" };
                Response {
                    status: 200,
                    headers: vec![("Content-Type".into(), content_type(&file).into()), ("Cache-Control".into(), cache.into())],
                    body,
                }
            }
            Err(_) => index(dir, req, ctx),
        },
        None => index(dir, req, ctx),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn injects_before_head_end_as_index_js() {
        let v = Json::parse(r#"{"ok":1,"serverData":{"shards":["shard"],"welcomeText":"</script>"}}"#).unwrap();
        let out = inject("<html><head><title>x</title></head><body></body></html>", Some(&v));
        assert!(out.starts_with("<html><head><title>x</title><script>window.__SCREEPS_CLIENT_EMBEDDED__={\"kind\":\"screeps-mod\""), "{out}");
        assert!(out.contains(";window.__SCREEPS_BOOTSTRAP__={\"ok\":1,"), "{out}");
        assert!(out.contains("\\u003c/script>"), "a `<` inside the JSON cannot end the script: {out}");
        assert!(out.ends_with("</script></head><body></body></html>"));
        assert!(inject("<p>no head</p>", None).starts_with("<script>window.__SCREEPS_CLIENT_EMBEDDED__="));
    }

    #[test]
    fn serves_only_files_inside_the_build() {
        let dir = Path::new("/srv/client");
        assert_eq!(file_under(dir, "/_client/app.js"), Some(dir.join("_client/app.js")));
        assert_eq!(file_under(dir, "/../etc/passwd"), None);
        assert_eq!(file_under(dir, "/themes/../../x"), None);
        assert_eq!(file_under(dir, "/"), None);
    }
}
