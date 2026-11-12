//! mirror: reference/backend/lib/game/server.js
//! parity: DIVERGENT(HTTP-SERVER-PASSWORD): `/api/version` is exempt from the password gate (spec 10 Q5); CORS is open
//! to the stock client and the server's own pages, and any other page is refused (HTTP-CORS); `X-Token` alone authenticates and `?_token=`/HTTP Basic are accepted (AUTH-TOKEN-MW,
//! AUTHMOD-BASIC); `serverData` carries the client's default `features` plus `shards: []` (API-SERVERDATA)
//!
//! `/api/version`, `/api/servers/list` stub and the router assembly (§8.3). Owner: WP04.

use crate::http::{self, obj, Request, Response};
use crate::{api, authlib, png, sockjs, ApiCtx, Route, Shared};
use engine::world::{UserIdx, WorldView};
use util::json::Json;

/// server.js `PROTOCOL`.
pub const PROTOCOL: u32 = 14;

/// server.js `config.backend.welcomeText`.
pub const WELCOME_TEXT: &str = "<h4>Welcome to your own Screeps private server!</h4>This text can be changed by adding a mod to your server, see <code>mods.json</code> file in your server folder.";

/// Every built-in route; mods append theirs. A trailing `*` matches any rest of the path.
pub fn routes() -> Vec<Route> {
    vec![
        Route { method: "GET", path: "/api/version", handler: version },
        Route { method: "POST", path: "/api/servers/list", handler: servers_list },
        Route { method: "GET", path: "/api/auth/me", handler: api::auth::me },
        Route { method: "POST", path: "/api/auth/steam-ticket", handler: api::auth::steam_ticket },
        Route { method: "POST", path: "/api/game/map-stats", handler: api::game::map_stats },
        Route { method: "GET", path: "/api/game/time", handler: api::game::time },
        Route { method: "GET", path: "/api/game/room-terrain", handler: api::game::room_terrain },
        Route { method: "GET", path: "/api/game/room-status", handler: api::game::room_status },
        Route { method: "POST", path: "/api/game/gen-unique-object-name", handler: api::game::gen_unique_object_name },
        Route { method: "POST", path: "/api/game/check-unique-object-name", handler: api::game::check_unique_object_name },
        Route { method: "POST", path: "/api/game/place-spawn", handler: api::game::place_spawn },
        Route { method: "POST", path: "/api/game/rooms", handler: api::game::rooms },
        Route { method: "POST", path: "/api/game/create-flag", handler: api::game::create_flag },
        Route { method: "POST", path: "/api/game/gen-unique-flag-name", handler: api::game::gen_unique_flag_name },
        Route { method: "POST", path: "/api/game/check-unique-flag-name", handler: api::game::check_unique_flag_name },
        Route { method: "POST", path: "/api/game/change-flag-color", handler: api::game::change_flag_color },
        Route { method: "POST", path: "/api/game/remove-flag", handler: api::game::remove_flag },
        Route { method: "POST", path: "/api/game/add-object-intent", handler: api::game::add_object_intent },
        Route { method: "POST", path: "/api/game/create-construction", handler: api::game::create_construction },
        Route { method: "GET", path: "/api/game/room-overview", handler: api::game::room_overview },
        Route { method: "POST", path: "/api/game/set-notify-when-attacked", handler: api::game::set_notify_when_attacked },
        Route { method: "POST", path: "/api/game/create-invader", handler: api::game::create_invader },
        Route { method: "POST", path: "/api/game/remove-invader", handler: api::game::remove_invader },
        Route { method: "GET", path: "/api/game/world-size", handler: api::game::world_size },
        Route { method: "POST", path: "/api/game/add-global-intent", handler: api::game::add_global_intent },
        Route { method: "GET", path: "/api/game/tick", handler: api::game::tick },
        Route { method: "GET", path: "/api/leaderboard/list", handler: api::leaderboard::list },
        Route { method: "GET", path: "/api/leaderboard/find", handler: api::leaderboard::find },
        Route { method: "GET", path: "/api/leaderboard/seasons", handler: api::leaderboard::seasons },
        Route { method: "GET", path: "/api/game/market/orders-index", handler: api::market::orders_index },
        Route { method: "GET", path: "/api/game/market/orders", handler: api::market::orders },
        Route { method: "GET", path: "/api/game/market/my-orders", handler: api::market::my_orders },
        Route { method: "GET", path: "/api/game/market/stats", handler: api::market::stats },
        Route { method: "GET", path: "/api/game/power-creeps/list", handler: api::power_creeps::list },
        Route { method: "POST", path: "/api/game/power-creeps/create", handler: api::power_creeps::create },
        Route { method: "POST", path: "/api/game/power-creeps/delete", handler: api::power_creeps::delete },
        Route { method: "POST", path: "/api/game/power-creeps/cancel-delete", handler: api::power_creeps::cancel_delete },
        Route { method: "POST", path: "/api/game/power-creeps/upgrade", handler: api::power_creeps::upgrade },
        Route { method: "POST", path: "/api/game/power-creeps/rename", handler: api::power_creeps::rename },
        Route { method: "POST", path: "/api/game/power-creeps/experimentation", handler: api::power_creeps::experimentation },
        Route { method: "GET", path: "/api/register/check-email", handler: api::register::check_email },
        Route { method: "GET", path: "/api/register/check-username", handler: api::register::check_username },
        Route { method: "POST", path: "/api/register/set-username", handler: api::register::set_username },
        Route { method: "POST", path: "/api/user/messages/send", handler: api::user_messages::send },
        Route { method: "GET", path: "/api/user/messages/list", handler: api::user_messages::list },
        Route { method: "GET", path: "/api/user/messages/index", handler: api::user_messages::index },
        Route { method: "POST", path: "/api/user/messages/mark-read", handler: api::user_messages::mark_read },
        Route { method: "GET", path: "/api/user/messages/unread-count", handler: api::user_messages::unread_count },
        Route { method: "GET", path: "/api/user/world-start-room", handler: api::user::world_start_room },
        Route { method: "GET", path: "/api/user/world-status", handler: api::user::world_status },
        Route { method: "GET", path: "/api/user/branches", handler: api::user::branches },
        Route { method: "POST", path: "/api/user/code", handler: api::user::code_post },
        Route { method: "GET", path: "/api/user/code", handler: api::user::code_get },
        Route { method: "POST", path: "/api/user/badge", handler: api::user::badge },
        Route { method: "GET", path: "/api/user/respawn-prohibited-rooms", handler: api::user::respawn_prohibited_rooms },
        Route { method: "POST", path: "/api/user/respawn", handler: api::user::respawn },
        Route { method: "POST", path: "/api/user/set-active-branch", handler: api::user::set_active_branch },
        Route { method: "POST", path: "/api/user/clone-branch", handler: api::user::clone_branch },
        Route { method: "POST", path: "/api/user/delete-branch", handler: api::user::delete_branch },
        Route { method: "GET", path: "/api/user/memory", handler: api::user::memory_get },
        Route { method: "POST", path: "/api/user/memory", handler: api::user::memory_post },
        Route { method: "GET", path: "/api/user/memory-segment", handler: api::user::memory_segment_get },
        Route { method: "POST", path: "/api/user/memory-segment", handler: api::user::memory_segment_post },
        Route { method: "POST", path: "/api/user/console", handler: api::user::console },
        Route { method: "GET", path: "/api/user/find", handler: api::user::find },
        Route { method: "GET", path: "/api/user/stats", handler: api::user::stats },
        Route { method: "GET", path: "/api/user/rooms", handler: api::user::rooms },
        Route { method: "POST", path: "/api/user/notify-prefs", handler: api::user::notify_prefs },
        Route { method: "GET", path: "/api/user/overview", handler: api::user::overview },
        Route { method: "POST", path: "/api/user/tutorial-done", handler: api::user::tutorial_done },
        Route { method: "POST", path: "/api/user/email", handler: api::user::email },
        Route { method: "GET", path: "/api/user/money-history", handler: api::user::money_history },
        Route { method: "GET", path: "/api/user/badge-svg", handler: api::user::badge_svg },
        Route { method: "POST", path: "/api/user/set-steam-visible", handler: api::user::set_steam_visible },
        Route { method: "GET", path: "/api/auth/query-token", handler: api::auth::query_token },
        Route { method: "POST", path: "/api/auth/signin", handler: api::authmod::signin },
        Route { method: "POST", path: "/api/user/password", handler: api::authmod::password },
        Route { method: "POST", path: "/api/register/submit", handler: api::authmod::submit },
        Route { method: "POST", path: "/api/game/change-flag", handler: api::game::change_flag },
        Route { method: "GET", path: "/api/game/room-decorations", handler: api::game::room_decorations },
        Route { method: "GET", path: "/api/game/find-object", handler: api::game::find_object },
        Route { method: "GET", path: "/api/user/decorations/inventory", handler: api::user::decorations_inventory },
        Route { method: "GET", path: "/api/user/decorations/themes", handler: api::user::decorations_themes },
        Route { method: "GET", path: "/api/user/auth-token", handler: api::user::auth_tokens },
        Route { method: "GET", path: "/assets/map/*", handler: png::map_png },
    ]
}

/// On `/api/version`'s reply, so a second start can tell a Krebs on its port from any other program.
pub const KREBS_HEADER: &str = "X-Krebs";

/// GET /api/version: protocol 14, `serverData.features`, `useNativeAuth:false`, user count.
pub fn version(_req: &Request, ctx: &ApiCtx) -> Response {
    let view = ctx.published.get();
    let mut response = Response::ok(obj(vec![
        ("protocol", Json::Num(PROTOCOL as f64)),
        ("useNativeAuth", Json::Bool(false)),
        ("users", Json::Num(active_users(&view) as f64)),
        ("serverData", ctx.published.mods().server_data.clone()),
    ]));
    response.headers.push((KREBS_HEADER.into(), env!("CARGO_PKG_VERSION").into()));
    response
}

/// `config.backend` as far as mods reach it: game/server.js:31-44 and socket/server.js:17-25 defaults, then each
/// mod's assignments in load order, and what is read from it.
pub struct ModBackend {
    /// getServerData() plus chitin's `features` and `shards`.
    pub(crate) server_data: Json,
    /// `config.backend.socketUpdateThrottle` (the broadcaster's `_.throttle` wait).
    pub(crate) throttle: std::time::Duration,
    pub(crate) channels: Vec<(&'static str, crate::socket::server::ChannelFn)>,
    /// The mods' socket event handlers (`Registry::socket_events`) and the command channel they write through.
    pub(crate) socket_events: Vec<crate::socket::server::SocketEventFn>,
    pub(crate) commands: Option<std::sync::mpsc::Sender<engine::world::Command>>,
}

impl ModBackend {
    /// `assignments`: `(path, value)` is `config.backend.<path> = value` (`"welcomeText"`,
    /// `"customObjectTypes.myobject"`, `"renderer.metadata.myobject"`, …); missing objects on the path are made.
    pub fn new(assignments: &[(&str, Json)], channels: Vec<(&'static str, crate::socket::server::ChannelFn)>) -> ModBackend {
        let empty = || Json::Obj(vec![]);
        let mut backend = obj(vec![
            ("welcomeText", Json::Str(WELCOME_TEXT.into())),
            ("customObjectTypes", empty()),
            ("historyChunkSize", Json::Num(20.0)),
            ("renderer", obj(vec![("resources", empty()), ("metadata", empty())])),
            ("socketUpdateThrottle", Json::Num(crate::broadcast::THROTTLE.as_millis() as f64)),
        ]);
        for (path, value) in assignments {
            assign(&mut backend, path, value.clone());
        }
        // lodash 3 _.throttle(fn, wait): `wait = wait < 0 ? 0 : (+wait || 0)`
        let wait = match backend.get("socketUpdateThrottle") {
            Some(Json::Num(n)) => *n,
            Some(Json::Str(s)) => util::num::string_to_number(s),
            Some(Json::Bool(b)) => *b as u8 as f64,
            _ => f64::NAN,
        };
        let wait = if wait > 0.0 { wait } else { 0.0 };
        ModBackend {
            server_data: server_data(&backend),
            throttle: std::time::Duration::from_secs_f64(wait.min(1e9) / 1000.0),
            channels,
            socket_events: Vec::new(),
            commands: None,
        }
    }

    /// The mods' socket event handlers, and the command channel they change the world through.
    pub fn with_socket_events(mut self, events: Vec<crate::socket::server::SocketEventFn>, commands: std::sync::mpsc::Sender<engine::world::Command>) -> ModBackend {
        self.socket_events = events;
        self.commands = Some(commands);
        self
    }
}

/// `object.a.b = value` for the path "a.b"; a missing or non-object step becomes `{}`.
fn assign(object: &mut Json, path: &str, value: Json) {
    let (key, rest) = match path.split_once('.') {
        Some((k, rest)) => (k, Some(rest)),
        None => (path, None),
    };
    if !matches!(object, Json::Obj(_)) {
        *object = Json::Obj(vec![]);
    }
    let Json::Obj(members) = object else { unreachable!() };
    let at = match members.iter().position(|(k, _)| k == key) {
        Some(i) => i,
        None => {
            members.push((key.to_string(), Json::Null));
            members.len() - 1
        }
    };
    match rest {
        None => members[at].1 = value,
        Some(rest) => assign(&mut members[at].1, rest, value),
    }
}

/// `db.users.count({$and: [{active: {$ne: 0}}, {cpu: {$gt: 0}}, {bot: {$aeq: null}}]})`.
fn active_users(view: &WorldView) -> usize {
    use engine::doc::Val;
    view.users
        .iter()
        .filter(|u| {
            let f = |k| crate::field(&u.doc, k);
            !matches!(f("active"), Some(Val::Num(n)) if *n == 0.0)
                && matches!(f("cpu"), Some(Val::Num(n)) if *n > 0.0)
                && matches!(f("bot"), None | Some(Val::Null))
        })
        .count()
}

/// `getServerData()` (game/server.js:46-54) of `config.backend`, plus `features` (the client's own fallback list) and
/// `shards: []`.
fn server_data(backend: &Json) -> Json {
    let get = |k: &str| backend.get(k).cloned().unwrap_or(Json::Null);
    let market = Json::parse(r#"{"name":"market","version":1,"menuData":[{"section":0,"after":"World","item":{"label":"Market","routerLink":"/market","svg":"market"}}]}"#).expect("static json");
    let auth = obj(vec![("name", Json::Str("auth".into())), ("version", Json::Num(1.0))]);
    obj(vec![
        ("welcomeText", get("welcomeText")),
        ("customObjectTypes", get("customObjectTypes")),
        ("historyChunkSize", get("historyChunkSize")),
        ("socketUpdateThrottle", get("socketUpdateThrottle")),
        ("renderer", get("renderer")),
        ("features", Json::Arr(vec![market, auth])),
        // `[]` as vanilla, unless a mod names the shard (`config.backend.shards`, as screepsmod-admin-utils does)
        ("shards", backend.get("shards").cloned().unwrap_or(Json::Arr(vec![]))),
    ])
}

/// POST /api/servers/list (stub).
pub fn servers_list(_req: &Request, _ctx: &ApiCtx) -> Response {
    Response::ok(obj(vec![("servers", Json::Arr(vec![]))]))
}

/// Runs a handler; a panic is a thrown error, which rejects the q-json promise: `{error: ""+e}` with HTTP 200
/// (HTTP-ERRORS).
pub(crate) fn invoke(handler: fn(&Request, &ApiCtx) -> Response, req: &Request, ctx: &ApiCtx) -> Response {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handler(req, ctx))) {
        Ok(res) => res,
        Err(e) => {
            let msg = e.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| e.downcast_ref::<String>().cloned()).unwrap_or_default();
            Response::error(&format!("Error: {msg}"))
        }
    }
}

/// Express Router defaults: paths are case-insensitive and a trailing slash is optional (`strict: false`).
fn route_matches(r: &Route, req: &Request) -> bool {
    let path = req.path.strip_suffix('/').filter(|p| !p.is_empty()).unwrap_or(&req.path);
    r.method == req.method && r.path.strip_suffix('*').map_or(r.path.eq_ignore_ascii_case(path), |p| req.path.get(..p.len()).is_some_and(|head| head.eq_ignore_ascii_case(p)))
}

/// The token in a request's head: `X-Token`, then `?token=` / `?_token=`.
fn head_token(req: &Request) -> Option<&str> {
    req.header("x-token").filter(|t| !t.is_empty()).or_else(|| req.param("token").or_else(|| req.param("_token")).filter(|t| !t.is_empty()))
}

/// The token a request carries (AUTH-TOKEN-MW): the head's, then `body.token`. A large body got in on what its head said, so
/// it is not parsed for this: its handler's parse is the only one it costs.
fn token(req: &Request) -> Option<String> {
    if let Some(t) = head_token(req) {
        return Some(t.to_string());
    }
    if req.body.len() > http::ANON_BODY {
        return None;
    }
    match req.body().get("token") {
        Some(Json::Str(t)) if !t.is_empty() => Some(t.clone()),
        _ => None,
    }
}

fn resolve_user(req: &Request, view: &WorldView) -> Option<UserIdx> {
    match token(req) {
        Some(t) => authlib::check_token(&t, view),
        None => req.header("authorization").and_then(|h| api::authmod::basic_user(view, h)),
    }
}

/// Whether the password gate stops this request.
fn locked_out(sh: &Shared, req: &Request) -> bool {
    let gated = !req.path.starts_with("/assets/") && req.path != "/api/version";
    gated && sh.config.server_password.as_ref().is_some_and(|pw| req.header("x-server-password") != Some(pw.as_str()))
}

/// How much body a request may send, from its head alone, so a stranger's body is neither held nor parsed. Megabytes are
/// for someone signed in, on a route that exists. `body.token` cannot ask for them, and needs none.
pub(crate) fn body_limit(sh: &Shared, req: &Request) -> usize {
    let view = sh.published.get();
    let signed_in = match head_token(req) {
        Some(t) => authlib::check_token(t, &view).is_some(),
        None => req.header("authorization").and_then(|h| api::authmod::basic_user(&view, h)).is_some(),
    };
    let builtin = routes();
    if signed_in && !locked_out(sh, req) && sh.routes.iter().chain(builtin.iter()).any(|r| route_matches(r, req)) {
        http::MAX_BODY
    } else {
        http::ANON_BODY
    }
}

/// What a web page must not do through the browser of someone who can reach this port. Leaving CORS headers off stops it
/// from reading a reply, but a simple POST or a websocket still runs, so those are refused. A page that repoints its own
/// DNS name at 127.0.0.1 is same-origin there and only its `Host` gives it away, which is why a server on this PC alone
/// checks that. `None` for anything else, including every request with no `Origin` (curl, bots, the game's own tools).
pub(crate) fn refused(sh: &Shared, req: &Request) -> Option<Response> {
    let reads = matches!(req.method.as_str(), "GET" | "HEAD" | "OPTIONS") && req.header("upgrade").is_none();
    let rebound = http::names_this_pc(&sh.config.host) && req.header("host").is_some_and(|h| !http::names_this_pc(h));
    (rebound || (!reads && !http::origin_allowed(req))).then(|| Response::text(403, "text/plain; charset=utf-8", http::reason(403)))
}

/// The middleware chain of server.js for one request (HTTP-MIDDLEWARE-ORDER): sockjs, `/` banner, password gate,
/// body parsing, auth, routes (mods first), `/room-history`, 404. CORS headers on everything but sockjs.
pub(crate) fn handle(sh: &Shared, req: &Request) -> Response {
    if let Some(no) = refused(sh, req) {
        return no;
    }
    if sockjs::is_socket_path(&req.path) {
        return sockjs::http(req);
    }
    let mut res = chain(sh, req);
    http::add_cors(req, &mut res);
    res
}

fn chain(sh: &Shared, req: &Request) -> Response {
    if req.method == "OPTIONS" {
        return Response { status: 204, headers: vec![], body: vec![] };
    }
    if req.method == "GET" && req.path == "/" && !sh.routes.iter().any(|r| route_matches(r, req)) {
        let host = format!("{}:{}", sh.config.host, sh.config.port);
        return Response::text(200, "text/html; charset=utf-8", &format!("<html><body>\n                            Screeps server  is running on {host}.\n                            Use your <a href=\"http://store.steampowered.com/app/464350\">Steam game client</a> to connect.\n                            </body></html>"));
    }
    if locked_out(sh, req) {
        return Response::error("incorrect server password");
    }
    // A parsed body takes about 40 times its text, so a large one waits for its turn here, once it has all arrived: a slow
    // sender holds nothing, and however many a player sends, they cost one tree. A panic must not end the turns.
    let _turn = (req.body.len() > http::ANON_BODY).then(|| sh.large_body.lock().unwrap_or_else(|e| e.into_inner()));
    if let Some(bad) = req.body_error() {
        return bad;
    }
    let user = resolve_user(req, &sh.published.get());
    if let Some(u) = user {
        sh.published.stamp_online(u);
    }
    let ctx = ApiCtx { published: sh.published.clone(), commands: sh.commands.clone(), user };
    let builtin = routes();
    if let Some(r) = sh.routes.iter().chain(builtin.iter()).find(|r| route_matches(r, req)) {
        let mut res = invoke(r.handler, req, &ctx);
        if let (Some(u), true) = (ctx.user, res.status != 401) {
            // a fresh token on every authenticated response (AUTH-TOKEN-MW, AUTH-CLIENT-TOKEN)
            if let Some(st) = sh.published.get().users.get(u as usize) {
                res.headers.push(("X-Token".into(), authlib::gen_token(st.id.as_str())));
            }
        }
        return res;
    }
    if req.path == "/room-history" || req.path.starts_with("/room-history/") {
        // default config.backend.onGetRoomHistory (the history mod registers a route)
        return Response::text(500, "text/html; charset=utf-8", "not implemented");
    }
    Response::not_found(&req.method, &req.path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn shared(password: Option<&str>) -> Shared {
        let (ctx, _h) = fake::ctx(None);
        Shared {
            published: ctx.published,
            commands: ctx.commands,
            config: crate::BackendConfig {
                host: "127.0.0.1".into(),
                port: 21025,
                cli_host: "127.0.0.1".into(),
                cli_port: 21026,
                cli_http: false,
                max_connections: 10,
                request_timeout_s: 120,
                server_password: password.map(Into::into),
                steam: "single-user:alice".into(),
            },
            routes: vec![],
            conns: AtomicUsize::new(0),
            callers: Default::default(),
            large_body: Default::default(),
        }
    }

    fn req(method: &str, path: &str, headers: &[(&str, &str)]) -> Request {
        Request { method: method.into(), path: path.into(), query: vec![], headers: headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(), body: vec![] }
    }

    fn body(r: &Response) -> Json {
        Json::parse(std::str::from_utf8(&r.body).unwrap()).unwrap()
    }

    #[test]
    fn a_wildcard_route_takes_any_case_like_the_others() {
        let wild = Route { method: "POST", path: "/api/user/messages/*", handler: api::auth::steam_ticket };
        assert!(route_matches(&wild, &req("POST", "/api/user/messages/send", &[])));
        assert!(route_matches(&wild, &req("POST", "/API/USER/MESSAGES/SEND", &[])));
        assert!(!route_matches(&wild, &req("POST", "/api/user/message", &[])));
        assert!(!route_matches(&wild, &req("POST", "/\u{e9}", &[])), "a short path that is not ASCII");
        let exact = Route { method: "POST", path: "/api/user/messages/send", handler: api::auth::steam_ticket };
        assert!(route_matches(&exact, &req("POST", "/API/user/messages/SEND/", &[])));
    }

    #[test]
    fn version_shape() {
        let sh = shared(None);
        let r = handle(&sh, &req("GET", "/api/version", &[]));
        let j = body(&r);
        let keys: Vec<&str> = j.as_object().unwrap().iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["ok", "protocol", "useNativeAuth", "users", "serverData"]);
        assert_eq!(j.get("protocol").and_then(Json::as_f64), Some(14.0));
        // alice counts; bob is a bot; "2"/"3" have cpu 0
        assert_eq!(j.get("users").and_then(Json::as_f64), Some(1.0));
        let sd = j.get("serverData").unwrap();
        assert_eq!(sd.get("historyChunkSize").and_then(Json::as_f64), Some(20.0));
        assert_eq!(sd.get("socketUpdateThrottle").and_then(Json::as_f64), Some(200.0));
        assert_eq!(sd.get("features").unwrap().at(1).unwrap().get("name").and_then(Json::as_str), Some("auth"));
        assert_eq!(r.header("access-control-allow-origin"), Some("*"));
        assert!(r.header("x-token").is_none());
    }

    #[test]
    fn password_gate_auth_and_404() {
        let sh = shared(Some("pw"));
        // exempt: version, sockjs, assets
        assert_eq!(handle(&sh, &req("GET", "/api/version", &[])).status, 200);
        assert_eq!(handle(&sh, &req("GET", "/socket/info", &[])).status, 200);
        let gated = handle(&sh, &req("GET", "/api/game/time", &[]));
        assert_eq!((gated.status, body(&gated).get("error").and_then(Json::as_str)), (200, Some("incorrect server password")));
        let ok = handle(&sh, &req("GET", "/api/game/time", &[("x-server-password", "pw")]));
        assert_eq!(body(&ok).get("time").and_then(Json::as_f64), Some(1000.0));

        let sh = shared(None);
        let me = handle(&sh, &req("GET", "/api/auth/me", &[]));
        assert_eq!((me.status, body(&me).get("error").and_then(Json::as_str)), (401, Some("unauthorized")));
        let tok = authlib::gen_token("a11ce0000000001");
        let me = handle(&sh, &req("GET", "/api/auth/me", &[("x-token", &tok)]));
        assert_eq!(me.status, 200);
        assert_eq!(body(&me).get("username").and_then(Json::as_str), Some("alice"));
        let fresh = me.header("x-token").unwrap();
        assert_eq!(authlib::check_token(fresh, &sh.published.get()), Some(fake::ALICE));
        let mut q = req("GET", "/api/auth/me", &[]);
        q.query = vec![("_token".into(), tok)];
        assert_eq!(handle(&sh, &q).status, 200);
        assert_eq!(handle(&sh, &req("GET", "/api/auth/me", &[("x-token", "bogus")])).status, 401);

        assert_eq!(body(&handle(&sh, &req("GET", "/API/Game/Time/", &[]))).get("time").and_then(Json::as_f64), Some(1000.0), "Express: case-insensitive, trailing slash");
        let nf = handle(&sh, &req("GET", "/nope", &[]));
        assert_eq!(nf.status, 404);
        assert!(String::from_utf8_lossy(&nf.body).contains("Cannot GET /nope"));
        assert_eq!(handle(&sh, &req("OPTIONS", "/api/user/code", &[])).status, 204);
        assert_eq!(handle(&sh, &req("GET", "/room-history", &[])).status, 500);
        let mut bad = req("POST", "/api/user/console", &[("content-type", "application/json")]);
        bad.body = b"{oops".to_vec();
        assert_eq!(handle(&sh, &bad).status, 400);
    }

    #[test]
    fn a_page_from_elsewhere_gets_no_cors_and_cannot_post() {
        let sh = shared(None);
        let evil = ("origin", "https://evil.example");
        let host = ("host", "127.0.0.1:21025");
        // a simple POST needs no preflight, so the missing headers alone would let it run
        for path in ["/api/auth/steam-ticket", "/api/user/code", "/api/register/submit", "/socket/info"] {
            let r = handle(&sh, &req("POST", path, &[evil, host]));
            assert_eq!(r.status, 403, "{path}");
            assert!(r.header("access-control-allow-origin").is_none(), "{path}");
        }
        assert_eq!(handle(&sh, &req("POST", "/api/auth/steam-ticket", &[("origin", "null"), host])).status, 403, "a sandboxed frame");
        let read = handle(&sh, &req("GET", "/api/game/time", &[evil, host]));
        assert_eq!(read.status, 200, "it may go, but the browser hides the reply");
        assert!(read.header("access-control-allow-origin").is_none() && read.header("access-control-allow-credentials").is_none());
        let preflight = handle(&sh, &req("OPTIONS", "/api/user/code", &[evil, host, ("access-control-request-headers", "x-token")]));
        assert!(preflight.header("access-control-allow-origin").is_none() && preflight.header("access-control-allow-headers").is_none());
        let info = handle(&sh, &req("GET", "/socket/info", &[evil, host]));
        assert!(info.header("access-control-allow-origin").is_none(), "sockjs's own route too");
    }

    #[test]
    fn the_client_and_the_servers_own_pages_keep_working() {
        let sh = shared(None);
        let host = ("host", "127.0.0.1:21025");
        let client = ("origin", "chrome-extension://abcdefghijklmnop");
        let page = ("origin", "http://127.0.0.1:21025");
        for origin in [client, page] {
            let mut signin = req("POST", "/api/auth/steam-ticket", &[origin, host, ("content-type", "application/json")]);
            signin.body = br#"{"ticket":"ab"}"#.to_vec();
            let r = handle(&sh, &signin);
            assert_eq!((r.status, r.header("access-control-allow-origin")), (200, Some(origin.1)));
            assert!(body(&r).get("token").is_some(), "{}", origin.1);
            let preflight = handle(&sh, &req("OPTIONS", "/api/user/code", &[origin, host, ("access-control-request-headers", "x-token")]));
            assert_eq!((preflight.status, preflight.header("access-control-allow-headers")), (204, Some("x-token")));
            assert_eq!(handle(&sh, &req("GET", "/socket/info", &[origin, host])).header("access-control-allow-origin"), Some(origin.1));
        }
        let curl = handle(&sh, &req("POST", "/api/auth/steam-ticket", &[("user-agent", "curl/8")]));
        assert!(curl.status == 200 && body(&curl).get("token").is_some(), "no Origin, as from curl");
    }

    #[test]
    fn a_server_on_this_pc_only_answers_to_its_own_names() {
        let sh = shared(None);
        for host in ["127.0.0.1", "127.0.0.1:21025", "localhost:21025", "[::1]:21025"] {
            assert_eq!(handle(&sh, &req("GET", "/api/game/time", &[("host", host)])).status, 200, "{host}");
        }
        // DNS rebinding: same origin as far as the browser knows, and only Host shows it
        let rebound = [("host", "evil.example:21025"), ("origin", "http://evil.example:21025")];
        assert_eq!(handle(&sh, &req("GET", "/api/game/time", &rebound[..1])).status, 403);
        assert_eq!(handle(&sh, &req("POST", "/api/auth/steam-ticket", &rebound)).status, 403);
        assert_eq!(handle(&sh, &req("GET", "/socket/info", &rebound[..1])).status, 403);
        let mut open = shared(None);
        open.config.host = "0.0.0.0".into();
        assert_eq!(handle(&open, &req("GET", "/api/game/time", &[("host", "game.example:21025")])).status, 200, "a public server has names it cannot know");
    }

    #[test]
    fn a_nested_form_cannot_end_the_server() {
        let sh = Arc::new(shared(None));
        for depth in [1000, 100_000] {
            let sh = sh.clone();
            let run = move || {
                let mut form = req("POST", "/api/version", &[("content-type", "application/x-www-form-urlencoded")]);
                form.body = format!("a{}=1", "[b]".repeat(depth)).into_bytes();
                // the path a crash took: chain, resolve_user, token, the body
                assert_eq!(handle(&sh, &form).status, 404, "POST /api/version is no route");
            };
            std::thread::Builder::new().stack_size(256 << 10).spawn(run).unwrap().join().unwrap();
        }
    }

    #[test]
    fn megabytes_are_for_someone_signed_in_on_a_real_route() {
        let sh = shared(Some("pw"));
        let token = authlib::gen_token("a11ce0000000001");
        let pw = ("x-server-password", "pw");
        let limit = |r: &Request| body_limit(&sh, r);
        assert_eq!(limit(&req("POST", "/api/user/code", &[pw])), http::ANON_BODY, "no token");
        assert_eq!(limit(&req("POST", "/api/user/code", &[pw, ("x-token", "bogus")])), http::ANON_BODY, "a bad token");
        assert_eq!(limit(&req("POST", "/api/user/code", &[pw, ("x-token", &token)])), http::MAX_BODY);
        assert_eq!(limit(&req("POST", "/nope", &[pw, ("x-token", &token)])), http::ANON_BODY, "no such route");
        assert_eq!(limit(&req("POST", "/api/user/code", &[("x-token", &token)])), http::ANON_BODY, "the password gate comes first");
        let mut q = req("POST", "/api/user/code", &[pw]);
        q.query = vec![("_token".into(), token)];
        assert_eq!(limit(&q), http::MAX_BODY, "?_token= is a token as well");
    }

    fn large(path: &str) -> Request {
        let mut r = req("POST", path, &[]);
        r.body = vec![b' '; http::ANON_BODY + 1];
        r
    }

    /// Eight held 10 MiB bodies from one player came to 3475 MB, as each cost about 425 MB of tree at once.
    #[test]
    fn large_bodies_take_turns() {
        static RUNNING: AtomicUsize = AtomicUsize::new(0);
        static MOST: AtomicUsize = AtomicUsize::new(0);
        fn parse(_: &Request, _: &ApiCtx) -> Response {
            MOST.fetch_max(RUNNING.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(20));
            RUNNING.fetch_sub(1, Ordering::SeqCst);
            Response::ok(Json::Null)
        }
        let mut sh = shared(None);
        sh.routes = vec![Route { method: "POST", path: "/parse", handler: parse }];
        let sh = Arc::new(sh);
        let senders: Vec<_> = (0..8)
            .map(|_| {
                let sh = sh.clone();
                std::thread::spawn(move || handle(&sh, &large("/parse")).status)
            })
            .collect();
        assert!(senders.into_iter().all(|s| s.join().unwrap() == 200));
        assert_eq!(MOST.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn only_a_large_body_waits_for_a_turn() {
        let sh = shared(None);
        let _taken = sh.large_body.lock().unwrap();
        let mut at_the_limit = req("POST", "/nope", &[]);
        at_the_limit.body = vec![b' '; http::ANON_BODY];
        assert_eq!(handle(&sh, &at_the_limit).status, 404);
        assert_eq!(handle(&sh, &req("GET", "/api/version", &[])).status, 200);
        let mut stranger = large("/nope");
        stranger.headers.push(("host".into(), "evil.example".into()));
        assert_eq!(handle(&sh, &stranger).status, 403, "refused before it comes to a turn");
    }

    #[test]
    fn a_panic_does_not_end_the_turns() {
        let sh = Arc::new(shared(None));
        let holder = sh.clone();
        let _ = std::thread::spawn(move || {
            let _turn = holder.large_body.lock().unwrap();
            panic!("in a handler");
        })
        .join();
        assert!(sh.large_body.is_poisoned());
        assert_eq!(handle(&sh, &large("/nope")).status, 404);
    }

    #[test]
    fn a_token_in_the_body_still_counts() {
        let sh = shared(None);
        let mut r = req("GET", "/api/auth/me", &[("content-type", "application/json")]);
        r.body = format!(r#"{{"token":"{}"}}"#, authlib::gen_token("a11ce0000000001")).into_bytes();
        assert_eq!(handle(&sh, &r).status, 200);
        let mut r = req("GET", "/api/auth/me", &[("content-type", "application/x-www-form-urlencoded")]);
        r.body = format!("token={}", authlib::gen_token("a11ce0000000001")).into_bytes();
        assert_eq!(handle(&sh, &r).status, 200);
    }

    #[test]
    fn a_large_body_is_not_parsed_for_a_token() {
        let mut r = req("POST", "/api/user/code", &[("content-type", "application/json")]);
        r.body = format!(r#"{{"token":"t","x":"{}"}}"#, "x".repeat(http::ANON_BODY)).into_bytes();
        assert_eq!(token(&r), None);
        r.body.truncate(http::ANON_BODY);
        r.body.splice(r.body.len() - 2.., *b"\"}");
        assert_eq!(token(&r).as_deref(), Some("t"), "one that small is");
    }

    #[test]
    fn mod_routes_shadow_builtins() {
        fn mine(_: &Request, _: &ApiCtx) -> Response {
            Response::ok(obj(vec![("mine", Json::Bool(true))]))
        }
        let mut sh = shared(None);
        sh.routes.push(Route { method: "GET", path: "/api/game/time", handler: mine });
        assert_eq!(body(&handle(&sh, &req("GET", "/api/game/time", &[]))).get("mine"), Some(&Json::Bool(true)));
    }
}

/// Replays tests/fixtures/api/api.json (official routes on the official storage over Loki, gen-api.js) and
/// compares every response and every collection a step changed. Divergences are normalised by name below.
#[cfg(test)]
mod oracle {
    use super::*;
    use crate::room_snapshot;
    use engine::doc::{Doc, DocRef, Id, Obj, Val};
    use engine::policy::{Compat, Profile, VClock};
    use engine::room_name::RoomXY;
    use engine::terrain::RoomTerrain;
    use engine::world::*;
    use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
    use std::sync::{Arc, Mutex};

    fn obj_of(j: &Json) -> Obj {
        match Val::from_json(j) {
            Val::Obj(o) => o,
            _ => panic!("not an object"),
        }
    }

    fn arr<'a>(j: &'a Json, k: &str) -> &'a Vec<Json> {
        j.get(k).and_then(Json::as_array).unwrap_or_else(|| panic!("{k}"))
    }

    /// The fixture world as a chitin World (collection order defines RoomIdx/UserIdx/seq, as Loki import does).
    fn world_from(fx: &Json) -> World {
        let wj = fx.get("world").unwrap();
        let fixed = fx.get("fixed").and_then(Json::as_f64).unwrap() as i64;
        let env = wj.get("env").unwrap();
        let time = env.get("gameTime").and_then(Json::as_f64).unwrap() as u64;
        let by_user = |coll: &str, id: &str| -> Vec<Obj> { arr(wj, coll).iter().filter(|d| d.get("user").and_then(Json::as_str) == Some(id)).map(obj_of).collect() };
        let mut users = Vec::new();
        let (mut user_ids, mut usernames) = (engine::world::UserIds::default(), HashMap::new());
        for (i, d) in arr(wj, "users").iter().enumerate() {
            let id = d.get("_id").and_then(Json::as_str).unwrap();
            user_ids.insert(Id::new(id).unwrap(), i as UserIdx);
            if let Some(n) = d.get("usernameLower").and_then(Json::as_str) {
                usernames.insert(n.into(), i as UserIdx);
            }
            let branches: Vec<Arc<Obj>> = by_user("users.code", id).into_iter().map(Arc::new).collect();
            let active = |k: &str| branches.iter().position(|b| crate::field(b, k) == Some(&Val::Bool(true)));
            let segments = env.get("memorySegments").and_then(|s| s.get(id)).and_then(Json::as_object).map(|m| m.iter().map(|(k, v)| (k.parse::<u8>().unwrap(), util::jsstr::JsStr::from_str(v.as_str().unwrap()))).collect()).unwrap_or_default();
            let mut console: Vec<Arc<Obj>> = by_user("users.console", id).into_iter().map(Arc::new).collect();
            console.reverse();
            users.push(Arc::new(UserState {
                id: Id::new(id).unwrap(),
                seq: i as u64,
                doc: Arc::new(obj_of(d)),
                auth: Arc::new(Obj::new()),
                code: Arc::new(CodeState { active_world: active("activeWorld"), active_sim: active("activeSim"), branches, version: 1 }),
                memory: util::jsstr::JsStr::from_str(env.get(&format!("memory:{id}")).and_then(Json::as_str).unwrap_or("")),
                memory_version: 1,
                segments: Arc::new(segments),
                public_segments: Arc::from(""),
                inter_shard: None,
                console_queue: console.into(),
                power_creeps: by_user("users.power_creeps", id).into_iter().enumerate().map(|(k, o)| Doc::new(100_000 + k as u64, o)).collect(),
                inbox: Arc::new(Inbox { notifications: vec![], messages: by_user("users.messages", id).into_iter().map(Arc::new).collect() }),
            }));
        }
        let orders: Vec<DocRef> = arr(wj, "market.orders").iter().enumerate().map(|(k, o)| Doc::new(200_000 + k as u64, obj_of(o))).collect();
        let stats: Vec<Arc<Obj>> = arr(wj, "market.stats").iter().map(|o| Arc::new(obj_of(o))).collect();
        let rooms_j = arr(wj, "rooms");
        let names: Vec<String> = rooms_j.iter().map(|r| r.get("_id").and_then(Json::as_str).unwrap().to_string()).collect();
        let xys: Vec<RoomXY> = names.iter().map(|n| n.parse().unwrap()).collect();
        let (x0, y0) = (xys.iter().map(|p| p.x).min().unwrap(), xys.iter().map(|p| p.y).min().unwrap());
        let (w, h) = ((xys.iter().map(|p| p.x).max().unwrap() - x0 + 1) as u32, (xys.iter().map(|p| p.y).max().unwrap() - y0 + 1) as u32);
        let mut grid = vec![0; (w * h) as usize];
        for (i, p) in xys.iter().enumerate() {
            grid[((p.y - y0) as u32 * w + (p.x - x0) as u32) as usize] = i as u32 + 1;
        }
        // common.calcWorldSize (maxX/maxY start at 0: BUG-WORLDSIZE)
        let (mx, my) = (xys.iter().map(|p| p.x).max().unwrap().max(0), xys.iter().map(|p| p.y).max().unwrap().max(0));
        let world_size = ((mx - x0 + 1).max(my - y0 + 1)) as u32;
        let meta = RoomsMeta { x0, y0, w, h, grid, rooms: names.iter().zip(&xys).map(|(n, p)| RoomMeta { name: n.as_str().into(), xy: *p, removed: false }).collect(), world_size };
        let terrain: Vec<RoomTerrain> = names
            .iter()
            .map(|n| {
                let t = arr(wj, "rooms.terrain").iter().find(|t| t.get("room").and_then(Json::as_str) == Some(n)).unwrap();
                RoomTerrain::from_db_str(t.get("terrain").and_then(Json::as_str).unwrap()).unwrap()
            })
            .collect();
        crate::fake::TERRAIN.with(|m| *m.borrow_mut() = terrain.iter().cloned().enumerate().map(|(i, t)| (i as RoomIdx, t)).collect());
        let mut per_room: Vec<Vec<DocRef>> = vec![vec![]; names.len()];
        for (i, d) in arr(wj, "rooms.objects").iter().enumerate() {
            let r = names.iter().position(|n| Some(n.as_str()) == d.get("room").and_then(Json::as_str)).unwrap();
            per_room[r].push(Doc::new(i as u64 + 1, obj_of(d)));
        }
        let mut index = WorldIndex::default();
        index.presence = vec![Arc::default(); users.len()];
        index.room_cost_ns = vec![0; names.len()];
        let mut rooms = Vec::new();
        for (r, objs) in per_room.into_iter().enumerate() {
            let flags: Vec<Arc<Obj>> = arr(wj, "rooms.flags").iter().filter(|f| f.get("room").and_then(Json::as_str) == Some(&names[r])).map(|f| Arc::new(obj_of(f))).collect();
            let map_view: Arc<str> = Arc::from(env.get(&format!("mapView:{}", names[r])).and_then(Json::as_str).unwrap_or(""));
            let draft = engine::processor::RoomDraft { info: Arc::new(obj_of(&rooms_j[r])), objects: vec![], flags: Arc::from(Vec::new()), event_log: Arc::from("[]"), map_view, presence: vec![], user_refs: vec![], has_movers: false, has_terminal_send: false, has_power_creeps: false };
            let blank = RoomSnapshot::new(draft, 0);
            let snap = room_snapshot(&blank, objs, flags.into(), &user_ids, 0);
            for d in snap.objects.iter() {
                index.objects.insert(d.id(), ObjLoc { room: r as RoomIdx, seq: d.seq, owner: None });
            }
            for (u, p) in snap.presence.iter() {
                Arc::make_mut(&mut index.presence[*u as usize]).insert(r as RoomIdx, p.clone());
            }
            rooms.push(Arc::new(snap));
        }
        let n_objects = arr(wj, "rooms.objects").len() as u64;
        let clock = VClock { base_ms: fixed, base_tick: time, nominal_tick_ms: 1000 };
        let mut w = World::new(1, clock, Compat::new(Profile::Private), meta, TerrainStore::new(terrain), rooms, users);
        w.time = time;
        w.access = Arc::new(AccessSnapshot { version: 0, accessible: vec![u64::MAX], status: HashMap::new() });
        w.user_ids = Arc::new(user_ids);
        w.usernames = Arc::new(usernames);
        w.market = Arc::new(MarketState::new(orders.into(), stats.into(), Arc::from(Vec::new()), Arc::from(Vec::new()), Arc::from(Vec::new())));
        w.index = index;
        w.next_seq = 300_000 + n_objects + 1;
        w
    }

    /// The collections a step can change, as Loki would hold them, grouped by owner/room (chitin keeps them per
    /// user/room; vanilla's global insertion order across groups is not modelled).
    fn dump(w: &World, coll: &str) -> BTreeMap<String, Vec<Json>> {
        let o = |d: &Obj| Val::Obj(d.clone()).to_json();
        let mut out: BTreeMap<String, Vec<Json>> = BTreeMap::new();
        match coll {
            "users" => {
                out.insert(String::new(), w.users.iter().map(|u| o(&u.doc)).collect());
            }
            "rooms" => {
                out.insert(String::new(), w.rooms.iter().map(|r| o(&r.info)).collect());
            }
            "market.orders" => {
                out.insert(String::new(), w.market.orders.iter().map(|d| o(&d.obj)).collect());
            }
            "users.code" | "users.console" | "users.messages" | "users.power_creeps" | "users.notifications" => {
                for u in w.users.iter() {
                    let list: Vec<Json> = match coll {
                        "users.code" => u.code.branches.iter().map(|b| o(b)).collect(),
                        // adaptive index order, as the generator dumps adaptive collections
                        "users.console" => u.console_queue.iter().map(|b| o(b)).collect(),
                        "users.power_creeps" => u.power_creeps.iter().map(|d| o(&d.obj)).collect(),
                        "users.notifications" => u.inbox.notifications.iter().map(|b| o(b)).collect(),
                        _ => u.inbox.messages.iter().map(|b| o(b)).collect(),
                    };
                    if !list.is_empty() {
                        out.insert(u.id.as_str().to_string(), list);
                    }
                }
            }
            "rooms.objects" | "rooms.flags" => {
                for (r, s) in w.rooms.iter().enumerate() {
                    let list: Vec<Json> = if coll == "rooms.objects" { s.objects.iter().map(|d| o(&d.obj)).collect() } else { s.flags.iter().map(|f| o(f)).collect() };
                    if !list.is_empty() {
                        out.insert(w.meta.rooms[r].name.to_string(), list);
                    }
                }
            }
            "env" => {
                for u in w.users.iter() {
                    if u.memory.utf16_len() > 0 {
                        out.insert(format!("memory:{}", u.id), vec![Json::Str(u.memory.to_string_lossy())]);
                    }
                    if !u.segments.is_empty() {
                        out.insert(format!("memorySegments:{}", u.id), vec![Json::Obj(u.segments.iter().map(|(k, v)| (k.to_string(), Json::Str(v.to_string_lossy()))).collect())]);
                    }
                }
            }
            c => panic!("collection {c}"),
        }
        out
    }

    /// The vanilla side of `dump`.
    fn group(coll: &str, docs: &[Json]) -> BTreeMap<String, Vec<Json>> {
        let mut out: BTreeMap<String, Vec<Json>> = BTreeMap::new();
        for d in docs {
            let key = match coll {
                "users" | "rooms" | "market.orders" => String::new(),
                "rooms.objects" | "rooms.flags" => d.get("room").and_then(Json::as_str).unwrap_or("").to_string(),
                _ => d.get("user").and_then(Json::as_str).unwrap_or("").to_string(),
            };
            out.entry(key).or_default().push(d.clone());
        }
        out
    }

    /// Ids created during the run → placeholders in order of first appearance (each side keeps its own map).
    #[derive(Default)]
    struct Ids {
        known: HashSet<String>,
        map: HashMap<String, String>,
    }

    impl Ids {
        fn collect(&mut self, j: &Json) {
            match j {
                Json::Obj(m) => {
                    for (k, v) in m {
                        if let (true, Json::Str(s)) = (k == "_id", v) {
                            if !self.known.contains(s) && !self.map.contains_key(s) {
                                let n = format!("<id{}>", self.map.len());
                                self.map.insert(s.clone(), n);
                            }
                        }
                        self.collect(v);
                    }
                }
                Json::Arr(a) => a.iter().for_each(|x| self.collect(x)),
                _ => {}
            }
        }
        fn apply(&self, j: Json) -> Json {
            match j {
                Json::Str(s) => Json::Str(self.map.get(&s).cloned().unwrap_or(s)),
                Json::Arr(a) => Json::Arr(a.into_iter().map(|x| self.apply(x)).collect()),
                Json::Obj(m) => Json::Obj(m.into_iter().map(|(k, v)| (k, self.apply(v))).collect()),
                x => x,
            }
        }
    }

    /// Shared normalisation: Loki artefacts (FMT-LOKI-ARTIFACTS: chitin never sends `$loki`/`meta`), real-time
    /// message dates, the random invader-name suffix.
    fn clean(j: Json) -> Json {
        match j {
            Json::Obj(m) => Json::Obj(
                m.into_iter()
                    .filter(|(k, _)| k != "$loki" && k != "meta")
                    .map(|(k, v)| match (k.as_str(), v) {
                        ("date", Json::Str(_) | Json::Num(_)) => (k, Json::Str("<date>".into())), // real-time stamps (CLK-API, CLK-NOTIFY)
                        ("name", Json::Str(n)) if n.starts_with("invader_") => {
                            let n = n.rsplit_once('_').map_or(n.clone(), |(a, _)| format!("{a}_N"));
                            (k, Json::Str(n))
                        }
                        (_, v) => (k, clean(v)),
                    })
                    .collect(),
            ),
            Json::Arr(a) => Json::Arr(a.into_iter().map(clean).collect()),
            // FMT-MEMORY-GZ: the gzip header's OS byte depends on the platform node runs on; compare content
            Json::Str(s) if s.starts_with("gz:") => {
                let raw = util::base64::decode(&s[3..]).and_then(|b| util::deflate::gzip_decompress(&b, 1 << 24).ok());
                Json::Str(format!("gz:{}", raw.map_or(s.clone(), |r| String::from_utf8_lossy(&r).into_owned())))
            }
            x => x,
        }
    }

    /// Route-specific divergences of the vanilla answer, each named after its spec row.
    fn vanilla_only(url: &str, j: Json) -> Json {
        let path = url.split('?').next().unwrap();
        match (path, j) {
            // GAME-MAP-STATS SEC: the Loki projection is ignored; chitin sends {_id, username, badge}
            ("/api/game/map-stats", Json::Obj(mut m)) => {
                if let Some((_, Json::Obj(users))) = m.iter_mut().find(|(k, _)| k == "users") {
                    for (_, u) in users.iter_mut() {
                        if let Json::Obj(f) = u {
                            f.retain(|(k, _)| matches!(k.as_str(), "_id" | "username" | "badge"));
                        }
                    }
                }
                Json::Obj(m)
            }
            (_, j) => j,
        }
    }

    /// Terrain docs compare by room and terrain only (chitin's `rooms.terrain` docs carry no `_id`).
    fn terrain_docs(url: &str, j: Json) -> Json {
        fn strip(j: Json) -> Json {
            match j {
                Json::Obj(m) if m.iter().any(|(k, v)| k == "terrain" && v.as_str().is_some()) => {
                    let mut f: Vec<(String, Json)> = m.into_iter().filter(|(k, _)| k == "room" || k == "terrain").collect();
                    f.sort_by(|a, b| a.0.cmp(&b.0));
                    Json::Obj(f)
                }
                Json::Obj(m) => Json::Obj(m.into_iter().map(|(k, v)| (k, strip(v))).collect()),
                Json::Arr(a) => Json::Arr(a.into_iter().map(strip).collect()),
                x => x,
            }
        }
        if url.starts_with("/api/game/room-terrain") || url.starts_with("/api/game/rooms") { strip(j) } else { j }
    }

    fn all_ids(j: &Json, out: &mut HashSet<String>) {
        match j {
            Json::Obj(m) => m.iter().for_each(|(k, v)| {
                if let (true, Json::Str(s)) = (k == "_id", v) {
                    out.insert(s.clone());
                }
                all_ids(v, out)
            }),
            Json::Arr(a) => a.iter().for_each(|x| all_ids(x, out)),
            _ => {}
        }
    }

    #[test]
    fn official_routes() {
        let text = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/api/api.json")).unwrap();
        let fx = Json::parse(&text).unwrap();
        let world = world_from(&fx);
        let mut known = HashSet::new();
        all_ids(fx.get("world").unwrap(), &mut known);
        let (mut ours_ids, mut theirs_ids) = (Ids { known: known.clone(), ..Default::default() }, Ids { known, ..Default::default() });

        let published = Arc::new(crate::Published::new(crate::fake::view(&world)));
        let shared = Arc::new(Mutex::new(world));
        let (tx, rx) = std::sync::mpsc::channel::<crate::Command>();
        {
            let (p, w) = (published.clone(), shared.clone());
            // the tick thread: apply, then publish the same tick (no time passes in the oracle either)
            std::thread::spawn(move || {
                for c in rx {
                    let mut w = w.lock().unwrap();
                    let _ = (c.apply)(&mut w);
                    p.set(crate::fake::view(&w));
                }
            });
        }
        let routes = routes();
        let mut failures = Vec::new();
        for (i, step) in fx.get("steps").unwrap().as_array().unwrap().iter().enumerate() {
            let method = step.get("method").and_then(Json::as_str).unwrap();
            let url = step.get("url").and_then(Json::as_str).unwrap();
            let (path, qs) = url.split_once('?').unwrap_or((url, ""));
            let user = step.get("as").and_then(Json::as_str).and_then(|id| published.get().user_ids.get(&Id::new(id).unwrap()).copied());
            let body = match step.get("body") {
                Some(Json::Null) | None => Vec::new(),
                Some(b) => b.stringify().into_bytes(),
            };
            let query = qs.split('&').filter(|p| !p.is_empty()).map(|p| p.split_once('=').map_or((p.to_string(), String::new()), |(k, v)| (http::url_decode(k), http::url_decode(v)))).collect();
            let req = Request { method: method.into(), path: path.into(), query, headers: vec![("content-type".into(), "application/json".into())], body };
            let route = routes.iter().find(|r| route_matches(r, &req)).unwrap_or_else(|| panic!("no route for {url}"));
            if let Some(u) = user {
                published.stamp_online(u); // authlib.checkToken's userOnline stamp, as `handle` does
            }
            let ctx = ApiCtx { published: published.clone(), commands: tx.clone(), user };
            let res = invoke(route.handler, &req, &ctx);
            let ours_text = String::from_utf8(res.body).unwrap();
            let theirs_text = step.get("response").and_then(Json::as_str).unwrap();
            let ours = terrain_docs(url, clean(Json::parse(&ours_text).unwrap_or(Json::Str(ours_text.clone()))));
            let theirs = terrain_docs(url, clean(vanilla_only(url, Json::parse(theirs_text).unwrap_or(Json::Str(theirs_text.into())))));
            ours_ids.collect(&ours);
            theirs_ids.collect(&theirs);
            let (ours, theirs) = (ours_ids.apply(ours), theirs_ids.apply(theirs));
            let status = step.get("status").and_then(Json::as_f64).unwrap() as u16;
            // HTTP-ERRORS: Express' 500 page is an HTML stack trace; only the status is compared
            if res.status != status || (status != 500 && ours != theirs) {
                failures.push(format!("step {i} {method} {url}\n  ours   {} {}\n  theirs {status} {}", res.status, ours.stringify(), theirs.stringify()));
            }
            let w = shared.lock().unwrap();
            for (coll, docs) in step.get("db").and_then(Json::as_object).unwrap() {
                let theirs_g: BTreeMap<String, Vec<Json>> = if coll == "env" { docs.as_object().unwrap().iter().map(|(k, v)| (k.clone(), vec![v.clone()])).collect() } else { group(coll, docs.as_array().unwrap()) };
                let ours_g = dump(&w, coll);
                let keys: BTreeSet<&String> = theirs_g.keys().chain(ours_g.keys()).collect();
                for k in keys {
                    let t = clean(Json::Arr(theirs_g.get(k).cloned().unwrap_or_default()));
                    let o = clean(Json::Arr(ours_g.get(k).cloned().unwrap_or_default()));
                    ours_ids.collect(&o);
                    theirs_ids.collect(&t);
                    let (o, t) = (ours_ids.apply(o), theirs_ids.apply(t));
                    if o != t {
                        failures.push(format!("step {i} {method} {url}: {coll}[{k}]\n  ours   {}\n  theirs {}", o.stringify(), t.stringify()));
                    }
                }
            }
        }
        assert!(failures.is_empty(), "{} divergences:\n{}", failures.len(), failures.join("\n"));
        let steps = fx.get("steps").unwrap().as_array().unwrap();
        let writes = steps.iter().filter(|s| s.get("db").and_then(Json::as_object).is_some_and(|m| !m.is_empty())).count();
        assert!(steps.len() >= 120 && writes >= 30, "{} steps, {writes} with DB changes", steps.len());
    }

    fn drain(c: &crate::socket::server::Conn) -> Vec<String> {
        c.out.drain(std::time::Duration::ZERO).unwrap_or_default().into_iter().filter_map(|o| if let crate::socket::server::Out::Text(t) = o { Some(t) } else { None }).collect()
    }

    /// One socket message, comparable: Loki artefacts stripped, room `users` projected (SOCK-ROOM-USERS).
    fn norm_message(m: &str) -> String {
        let mut j = clean(Json::parse(m).unwrap_or_else(|_| Json::Str(m.into())));
        if let Json::Arr(a) = &mut j {
            if let (Some(Json::Str(ch)), Some(Json::Obj(data))) = (a.first().cloned(), a.get_mut(1)) {
                if ch.starts_with("room:") {
                    if let Some((_, Json::Obj(users))) = data.iter_mut().find(|(k, _)| k == "users") {
                        for (_, u) in users.iter_mut() {
                            if let Json::Obj(f) = u {
                                f.retain(|(k, _)| matches!(k.as_str(), "_id" | "username" | "badge"));
                            }
                        }
                    }
                }
            }
        }
        j.stringify()
    }

    fn sorted(v: Vec<String>) -> Vec<String> {
        let mut v: Vec<String> = v.iter().map(|m| norm_message(m)).collect();
        v.sort();
        v
    }

    /// Room, map and user channels vs the official socket modules (tests/fixtures/sockets, gen-sockets.js).
    #[test]
    fn official_sockets() {
        let text = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/sockets/sockets.json")).unwrap();
        let fx = Json::parse(&text).unwrap();
        let mut w = world_from(&fx);
        let p = Arc::new(crate::Published::new(crate::fake::view(&w)));
        let mut conns = Vec::new();
        for c in fx.get("conns").unwrap().as_array().unwrap() {
            let (name, uid) = (c.at(0).unwrap().as_str().unwrap(), c.at(1).unwrap().as_str().unwrap());
            let conn = p.hub().add_conn();
            crate::socket::server::on_message(&p, &conn, &format!("auth {}", authlib::gen_token(uid)));
            assert!(drain(&conn)[0].starts_with("auth ok "));
            for ch in c.at(2).unwrap().as_array().unwrap() {
                crate::socket::server::on_message(&p, &conn, &format!("subscribe {}", ch.as_str().unwrap()));
            }
            conns.push((name.to_string(), conn));
        }
        let (mut failures, mut compared) = (Vec::new(), 0);
        for (name, conn) in &conns {
            let theirs: Vec<String> = fx.get("subscribed").unwrap().get(name).unwrap().as_array().unwrap().iter().map(|m| m.as_str().unwrap().to_string()).collect();
            let (o, t) = (sorted(drain(conn)), sorted(theirs));
            compared += t.len();
            if o != t {
                failures.push(format!("subscribe {name}\n  ours   {o:?}\n  theirs {t:?}"));
            }
        }

        // the tick
        let prev = p.get();
        let t = fx.get("tick").unwrap();
        w.time = t.get("gameTime").and_then(Json::as_f64).unwrap() as u64;
        let room_of = |w: &World, id: &str| w.rooms.iter().position(|s| s.objects.iter().any(|d| d.id().as_str() == id)).unwrap() as RoomIdx;
        let mut edits: BTreeMap<RoomIdx, Vec<DocRef>> = BTreeMap::new();
        for s in t.get("set").unwrap().as_array().unwrap() {
            let (id, set) = (s.at(0).unwrap().as_str().unwrap(), s.at(1).unwrap());
            let r = room_of(&w, id);
            let objs = edits.entry(r).or_insert_with(|| w.rooms[r as usize].objects.to_vec());
            for d in objs.iter_mut().filter(|d| d.id().as_str() == id) {
                let mut o = d.obj.clone();
                for (k, v) in set.as_object().unwrap() {
                    crate::obj_set(&mut o, k, Val::from_json(v));
                }
                *d = Doc::new(d.seq, o);
            }
        }
        for doc in t.get("insert").unwrap().as_array().unwrap() {
            let room = doc.get("room").and_then(Json::as_str).unwrap();
            let r = w.meta.rooms.iter().position(|m| &*m.name == room).unwrap() as RoomIdx;
            let seq = w.next_seq;
            w.next_seq += 1;
            edits.entry(r).or_insert_with(|| w.rooms[r as usize].objects.to_vec()).push(Doc::new(seq, obj_of(doc)));
        }
        for id in t.get("remove").unwrap().as_array().unwrap() {
            let id = id.as_str().unwrap();
            let r = room_of(&w, id);
            edits.entry(r).or_insert_with(|| w.rooms[r as usize].objects.to_vec()).retain(|d| d.id().as_str() != id);
        }
        for (r, objs) in edits {
            crate::install_room(&mut w, r, objs, None, None);
        }
        for (uid, mem) in t.get("memory").unwrap().as_object().unwrap() {
            let u = w.user_ids[&Id::new(uid).unwrap()];
            crate::edit_user(&mut w, u, |st| {
                st.memory = util::jsstr::JsStr::from_str(mem.as_str().unwrap());
                st.memory_version += 1;
            });
        }
        for (room, mv) in t.get("mapView").unwrap().as_object().unwrap() {
            let r = w.meta.rooms.iter().position(|m| &*m.name == room).unwrap();
            let prev = w.rooms[r].clone();
            let draft = engine::processor::RoomDraft {
                info: prev.info.clone(),
                objects: prev.objects.to_vec(),
                flags: prev.flags.clone(),
                event_log: prev.event_log.clone(),
                map_view: Arc::from(mv.as_str().unwrap()),
                presence: prev.presence.to_vec(),
                user_refs: prev.user_refs.to_vec(),
                has_movers: prev.has_movers,
                has_terminal_send: prev.has_terminal_send,
                has_power_creeps: prev.has_power_creeps,
            };
            w.rooms[r] = Arc::new(RoomSnapshot::new(draft, prev.processed_at));
        }
        let visuals = t.get("visuals").unwrap().as_array().unwrap().iter().map(|v| (w.user_ids[&Id::new(v.at(0).unwrap().as_str().unwrap()).unwrap()], v.at(1).unwrap().as_str().unwrap().into(), Arc::from(v.at(2).unwrap().as_str().unwrap()))).collect();
        let view = crate::fake::view(&w);
        p.set(view.clone());
        let report = crate::TickReport { view, processed: Arc::from(Vec::new()), console: vec![], cpu: vec![], visuals, user_events: vec![] };
        crate::broadcast::broadcast(&p, &report, Some(&prev), &mut util::rng::Rng::new(1));
        let unchanged_map = |m: &str| {
            // SOCK-ROOMMAP2: chitin pushes map views only for rooms whose snapshot changed
            m.strip_prefix("[\"roomMap2:").and_then(|r| r.split('"').next()).is_some_and(|room| {
                crate::room_idx(&prev, room).is_none_or(|r| Arc::ptr_eq(&prev.rooms[r as usize], &p.get().rooms[r as usize]))
            })
        };
        for (name, conn) in &conns {
            let theirs: Vec<String> = fx.get("broadcast").unwrap().get(name).unwrap().as_array().unwrap().iter().map(|m| m.as_str().unwrap().to_string()).filter(|m| !unchanged_map(m)).collect();
            let (o, t) = (sorted(drain(conn)), sorted(theirs));
            compared += t.len();
            if o != t {
                failures.push(format!("tick {name}\n  ours   {o:?}\n  theirs {t:?}"));
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
        assert!(compared >= 12, "only {compared} messages compared");
    }
}

#[cfg(test)]
mod mod_backend {
    use super::ModBackend;
    use util::json::Json;

    #[test]
    fn assignments_follow_vanilla_property_sets() {
        let j = |s: &str| Json::parse(s).unwrap();
        let m = ModBackend::new(
            &[
                ("welcomeText", Json::Str("hi".into())),
                ("customObjectTypes.a", j(r#"{"sidepanel":"x"}"#)),
                ("renderer.metadata.a", j("{}")),
                ("customObjectTypes.a", j(r#"{"sidepanel":"y"}"#)),
                ("socketUpdateThrottle", Json::Num(100.0)),
            ],
            Vec::new(),
        );
        assert_eq!(
            m.server_data.get("welcomeText").unwrap().stringify(),
            r#""hi""#
        );
        assert_eq!(m.server_data.get("customObjectTypes").unwrap().stringify(), r#"{"a":{"sidepanel":"y"}}"#);
        assert_eq!(m.server_data.get("renderer").unwrap().stringify(), r#"{"resources":{},"metadata":{"a":{}}}"#);
        assert_eq!(m.throttle, std::time::Duration::from_millis(100));
        // _.throttle: `+wait`, negative or NaN is 0
        let throttle = |v: Json| ModBackend::new(&[("socketUpdateThrottle", v)], Vec::new()).throttle;
        assert_eq!(throttle(Json::Str("x".into())), std::time::Duration::ZERO);
        assert_eq!(throttle(Json::Str(" 50".into())), std::time::Duration::from_millis(50));
        assert_eq!(throttle(Json::Num(-5.0)), std::time::Duration::ZERO);
        assert_eq!(ModBackend::new(&[], Vec::new()).throttle, crate::broadcast::THROTTLE);
    }
}
