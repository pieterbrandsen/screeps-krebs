//! HTTP/WebSocket/SockJS server, game API, socket channels, CLI transport and the mod registry types
//! (ARCHITECTURE.md §8, §10; no V8). World access types are FROZEN (§8.2). Module tree: WP00/WP11.

pub mod authlib;
pub mod broadcast;
pub mod cli;
pub mod http;
pub mod mods;
pub mod png;
pub mod server;
pub mod sockjs;
pub mod steam_web;
pub mod ws;

/// Game API endpoints (backend/lib/game/api/*.js, §8.3).
pub mod api {
    pub mod auth;
    pub mod authmod;
    pub mod badge;
    pub mod game;
    pub mod leaderboard;
    pub mod market;
    pub mod power_creeps;
    pub mod register;
    pub mod user;
    pub mod user_messages;
}

/// Socket channels (backend/lib/game/socket/*.js, §8.4).
pub mod socket {
    pub mod map;
    pub mod rooms;
    pub mod server;
    pub mod system;
    pub mod user;
}

pub use engine::world::{Command, CommandFn};

use engine::doc::{Id, Obj, Val};
use engine::world::{CodeState, Inbox, RoomIdx, UserIdx, UserState, World, WorldView};
use http::{Request, Response};
use std::collections::HashMap;
use std::net::{IpAddr, Ipv6Addr, SocketAddr, TcpListener};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;
use util::json::Json;

/// http.rs defines Request/Response; handlers are plain fns (mods add more through the registry).
pub struct Route {
    pub method: &'static str,
    pub path: &'static str,
    pub handler: fn(&Request, &ApiCtx) -> Response,
}

pub struct ApiCtx {
    pub published: Arc<Published>,
    pub commands: Sender<Command>,
    pub user: Option<UserIdx>,
}

/// The latest published `WorldView` (set in P6), plus the socket subscription hub (handlers publish
/// `user:<id>/code`-style events through it).
pub struct Published {
    /// The view and how many views were published so far.
    view: Mutex<(WorldView, u64)>,
    changed: Condvar,
    hub: socket::server::Hub,
    /// `BackendConfig::steam`, set by `start` (handlers only see `ApiCtx`).
    steam: std::sync::OnceLock<String>,
    /// `config.backend` after the mods' assignments, and the mods' socket channels (set by `start`).
    mods: std::sync::OnceLock<server::ModBackend>,
    /// game.js `lastTime`/`lastTicks`: wall-clock ms between the last 30 published ticks, newest first.
    ticks: Mutex<(Option<std::time::Instant>, std::collections::VecDeque<u64>)>,
    /// env `userOnline:<id>` (AUTH-USER-ONLINE): real ms of the user's last token check or socket auth.
    online: Mutex<std::collections::HashMap<UserIdx, i64>>,
}

impl Published {
    pub fn new(view: WorldView) -> Published {
        Published { view: Mutex::new((view, 0)), changed: Condvar::new(), hub: Default::default(), steam: Default::default(), mods: Default::default(), ticks: Default::default(), online: Default::default() }
    }
    /// `env.set('userOnline:'+id, Date.now())` (authlib.checkToken, socket auth).
    pub(crate) fn stamp_online(&self, u: UserIdx) {
        self.online.lock().unwrap_or_else(|e| e.into_inner()).insert(u, engine::policy::real_now_ms("CLK-API"));
    }
    /// The user's `userOnline` time (real ms), for message/error notifications (MSG-NOTIFY, CLK-NOTIFY).
    pub fn user_online(&self, u: UserIdx) -> Option<i64> {
        self.online.lock().unwrap_or_else(|e| e.into_inner()).get(&u).copied()
    }
    /// The steam-ticket policy (`single-user:<name>` | `trust` | `off`; krebs.example.json's default when unset).
    pub(crate) fn steam_mode(&self) -> &str {
        self.steam.get().map_or("single-user:player", String::as_str)
    }
    /// game.js `pubsub.subscribe('roomsDone')`: records the interval since the previous tick.
    pub(crate) fn tick_done(&self) {
        let mut t = self.ticks.lock().unwrap_or_else(|e| e.into_inner());
        let now = std::time::Instant::now();
        if let Some(last) = t.0 {
            t.1.push_front(now.duration_since(last).as_millis() as u64);
            t.1.truncate(30);
        }
        t.0 = Some(now);
    }
    /// `_.min(lastTicks)` (None = Infinity, sent as null).
    pub(crate) fn min_tick_ms(&self) -> Option<u64> {
        self.ticks.lock().unwrap_or_else(|e| e.into_inner()).1.iter().copied().min()
    }
    /// What the mods changed (vanilla's defaults when `start` has not run, as in handler tests).
    pub(crate) fn mods(&self) -> &server::ModBackend {
        self.mods.get_or_init(|| server::ModBackend::new(&[], Vec::new()))
    }
    pub fn get(&self) -> WorldView {
        self.view.lock().unwrap_or_else(|e| e.into_inner()).0.clone()
    }
    pub fn set(&self, v: WorldView) {
        let mut g = self.view.lock().unwrap_or_else(|e| e.into_inner());
        *g = (v, g.1 + 1);
        self.changed.notify_all();
    }
    /// Blocks until a view with `time > t` is published.
    pub fn wait_after(&self, t: u64) -> WorldView {
        let g = self.view.lock().unwrap_or_else(|e| e.into_inner());
        self.changed.wait_while(g, |v| v.0.time <= t).unwrap_or_else(|e| e.into_inner()).0.clone()
    }
    /// How many views have been published (read inside a Command: the next publish shows its effects).
    pub(crate) fn version(&self) -> u64 {
        self.view.lock().unwrap_or_else(|e| e.into_inner()).1
    }
    /// Waits (bounded) until a view newer than publish number `seen` exists.
    pub(crate) fn wait_published_after(&self, seen: u64, timeout: Duration) -> bool {
        let g = self.view.lock().unwrap_or_else(|e| e.into_inner());
        let (_g, r) = self.changed.wait_timeout_while(g, timeout, |v| v.1 <= seen).unwrap_or_else(|e| e.into_inner());
        !r.timed_out()
    }
    pub(crate) fn hub(&self) -> &socket::server::Hub {
        &self.hub
    }
}

pub struct TickReport {
    pub view: WorldView,
    pub processed: Arc<[RoomIdx]>,
    /// (user, log, results, error)
    pub console: Vec<(UserIdx, Vec<String>, Vec<String>, Option<String>)>,
    /// (user, used, memory bytes)
    pub cpu: Vec<(UserIdx, u32, u32)>,
    pub visuals: Vec<(UserIdx, Box<str>, Arc<str>)>,
    /// Code/branch changes, resources/money, new messages, memory changed.
    pub user_events: Vec<UserEvent>,
}

/// One push on `user:<id>/<channel>` (`code`, `set-active-branch`, `newMessage`, `resources`, `money`,
/// `memory`, …).
pub struct UserEvent {
    pub user: UserIdx,
    pub channel: Box<str>,
    pub data: util::json::Json,
}

/// Implemented by server over the server isolate.
pub trait CliEval: Send + Sync {
    fn open(&self) -> Box<dyn CliSession>;
    /// A session for one screeps-launcher HTTP CLI request (`cli.http`): `eval` takes the whole body and returns
    /// what the launcher writes. None where the evaluator has no such mode.
    fn open_launcher(&self) -> Option<Box<dyn CliSession>> {
        None
    }
}

pub trait CliSession: Send {
    /// What the connection is greeted with (js/cli/server.js).
    fn greeting(&self) -> String {
        String::new()
    }
    fn eval(&mut self, line: &str) -> String;
}

/// Listen addresses, limits and auth policy (from krebs.json `http`, `cli`, `auth`).
pub struct BackendConfig {
    pub host: String,
    /// 21025
    pub port: u16,
    pub cli_host: String,
    /// 21026
    pub cli_port: u16,
    /// The console port speaks screeps-launcher's HTTP CLI (`POST /cli`, `GET /greeting`) instead of the line
    /// protocol (`cli.http`, default off).
    pub cli_http: bool,
    /// 2048
    pub max_connections: usize,
    /// How long a connection may send nothing while a request is due, or is on its way. Also how long one request has to
    /// arrive, plus what a large body allows for, and how long a websocket has to sign in. Websockets get a ping at half of
    /// this, so that one that only receives has something to answer. 120 s.
    pub request_timeout_s: u64,
    /// Server-password gate (HTTP-MIDDLEWARE-ORDER); None = open.
    pub server_password: Option<String>,
    /// `single-user:<name>` | `trust` | `off`
    pub steam: String,
}

pub struct BackendDeps {
    pub published: Arc<Published>,
    pub commands: Sender<Command>,
    pub cli: Arc<dyn CliEval>,
    pub config: BackendConfig,
    pub routes: Vec<Route>,
    /// Mods' socket channels (`Registry::socket_channels`), in mod load order.
    pub socket_channels: Vec<(&'static str, socket::server::ChannelFn)>,
    /// Mods' `config.backend` assignments (`Registry::server_data`), in mod load order.
    pub server_data: Vec<(&'static str, Json)>,
}

/// Running listeners, broadcaster and CLI server.
pub struct Backend {
    addr: SocketAddr,
    ticks: Arc<broadcast::TickSlot>,
    published: Arc<Published>,
}

/// An http thread's stack. It is only reserved, so it costs address space until used. JSON nests to `MAX_DEPTH` on it, which
/// takes up to about 768 KB in a dev build and 192 KB in release, and the handlers run on top of that.
pub(crate) const HTTP_STACK: usize = 1 << 20;

/// One caller's share of `max_connections`. A browser opens up to six connections to a server and the client one socket,
/// so eight players behind one router fit, and one address cannot take every slot.
const MAX_PER_ADDRESS: usize = 64;

/// What every connection thread shares.
pub(crate) struct Shared {
    pub published: Arc<Published>,
    pub commands: Sender<Command>,
    pub config: BackendConfig,
    /// Extra routes (mods) matched before the built-ins, so they shadow them (HTTP-MOD-ROUTER GOTCHA).
    pub routes: Vec<Route>,
    pub conns: AtomicUsize,
    /// Open connections by `caller`.
    pub callers: Mutex<HashMap<IpAddr, usize>>,
    /// Held while a body over `http::ANON_BODY` is handled (`server::chain`).
    pub large_body: Mutex<()>,
}

impl Shared {
    /// Whether more than three quarters of `max_connections` are in use. Keep-alive connections then end with their response,
    /// or whole small requests now and then would hold every slot for good. A client just opens a new connection.
    fn crowded(&self) -> bool {
        self.conns.load(Ordering::Relaxed).saturating_mul(4) > self.config.max_connections.saturating_mul(3)
    }
}

/// A connection's place in `max_connections` and in its caller's share. The connection thread and its websocket writer each
/// hold it, so a writer stuck on a client that stopped reading counts until it gives up.
pub(crate) struct Slot {
    shared: Arc<Shared>,
    caller: Option<IpAddr>,
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.shared.conns.fetch_sub(1, Ordering::Relaxed);
        if let Some(ip) = self.caller {
            let mut callers = self.shared.callers.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(n) = callers.get_mut(&ip) {
                *n -= 1;
                if *n == 0 {
                    callers.remove(&ip);
                }
            }
        }
    }
}

/// Who a connection counts against: its address, or the /64 as one IPv6 home gets that many. Loopback counts against no one,
/// as it is a program on this PC, which can be a proxy for a server that also listens beyond loopback. A server bound to
/// loopback alone needs no Steam key, so `server::refused` answers 403 to any `Host` that does not name this PC, and a tunnel
/// reaches it only by rewriting `Host`. That is meant: such a server must not quietly become reachable from outside.
fn caller(ip: IpAddr) -> Option<IpAddr> {
    match ip.to_canonical() {
        ip if ip.is_loopback() => None,
        IpAddr::V6(ip) => Some(IpAddr::V6(Ipv6Addr::from(ip.to_bits() & (!0u128 << 64)))),
        ip => Some(ip),
    }
}

/// A place for a connection from `peer`, if there is one. Only the accept thread takes them.
fn take_slot(shared: &Arc<Shared>, peer: Option<IpAddr>) -> Option<Slot> {
    if shared.conns.load(Ordering::Relaxed) >= shared.config.max_connections {
        return None;
    }
    let caller = peer.and_then(caller);
    if let Some(ip) = caller {
        let mut callers = shared.callers.lock().unwrap_or_else(|e| e.into_inner());
        let n = callers.entry(ip).or_insert(0);
        if *n >= MAX_PER_ADDRESS {
            return None;
        }
        *n += 1;
    }
    shared.conns.fetch_add(1, Ordering::Relaxed);
    Some(Slot { shared: shared.clone(), caller })
}

/// Binds the HTTP port and starts the accept loop and the broadcaster. Panics if the port cannot be bound.
pub fn start(deps: BackendDeps) -> Backend {
    if deps.config.cli_port != 0 {
        if deps.config.cli_http {
            cli::start_http(&deps.config.cli_host, deps.config.cli_port, deps.cli.clone());
        } else {
            cli::start(&deps.config.cli_host, deps.config.cli_port, deps.cli.clone());
        }
    }
    let listener = TcpListener::bind((deps.config.host.as_str(), deps.config.port))
        .unwrap_or_else(|e| panic!("cannot listen on {}:{}: {e}", deps.config.host, deps.config.port));
    let addr = listener.local_addr().expect("bound socket has an address");
    let _ = deps.published.steam.set(deps.config.steam.clone());
    let _ = deps.published.mods.set(server::ModBackend::new(&deps.server_data, deps.socket_channels));
    let shared = Arc::new(Shared { published: deps.published, commands: deps.commands, config: deps.config, routes: deps.routes, conns: AtomicUsize::new(0), callers: Mutex::default(), large_body: Mutex::default() });
    let ticks = Arc::new(broadcast::TickSlot::default());
    let published = shared.published.clone();
    {
        let (p, t) = (shared.published.clone(), ticks.clone());
        std::thread::Builder::new().name("broadcaster".into()).spawn(move || broadcast::run(&p, &t)).expect("spawn broadcaster");
    }
    std::thread::Builder::new()
        .name("http-accept".into())
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                let Some(slot) = take_slot(&shared, stream.peer_addr().ok().map(|a| a.ip())) else {
                    let mut s = stream;
                    let _ = http::write_response(&mut s, &Response::text(503, "text/plain; charset=utf-8", "Service Unavailable"), false);
                    continue;
                };
                let s = shared.clone();
                // ponytail: thread per connection (§8.1); IOCP/epoll only if the sockets bench demands it
                // the slot goes with the closure, so a failed spawn or a panic cannot leak it
                let _ = std::thread::Builder::new().name("http".into()).stack_size(HTTP_STACK).spawn(move || {
                    let slot = Arc::new(slot);
                    let timeout = Duration::from_secs(s.config.request_timeout_s.max(1));
                    let body_limit = |req: &Request| server::body_limit(&s, req);
                    let handle = |req: &Request| server::handle(&s, req);
                    let upgrade = |req: &Request, w: http::Socket, r: std::io::BufReader<http::Socket>| {
                        // handed back, `handle` answers it
                        if !sockjs::is_socket_path(&req.path) || server::refused(&s, req).is_some() {
                            return Some(w);
                        }
                        sockjs::upgrade(&s.published, req, w, r, slot.clone(), timeout).err()
                    };
                    http::serve_connection(stream, timeout, &body_limit, &handle, &upgrade, &|| s.crowded());
                });
            }
        })
        .expect("spawn http-accept");
    Backend { addr, ticks, published }
}

impl Backend {
    /// Never blocks; coalesces when the broadcaster lags.
    pub fn on_tick(&self, report: TickReport) {
        self.published.tick_done();
        self.ticks.put(report);
    }

    /// The bound HTTP address (useful with port 0).
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }
}

/// Runs `f` as a Command at the next P0 and waits for its result (10 s timeout ⇒ `Err("timeout")`).
pub fn call<R: Send + 'static>(tx: &Sender<Command>, f: impl FnOnce(&mut World) -> Result<R, String> + Send + 'static) -> Result<R, String> {
    let (rtx, rrx) = std::sync::mpsc::sync_channel(1);
    let apply: CommandFn = Box::new(move |w| {
        let r = f(w);
        let status = match &r {
            Ok(_) => Ok(Json::Null),
            Err(e) => Err(e.clone()),
        };
        let _ = rtx.send(r);
        status
    });
    tx.send(Command { apply, reply: None }).map_err(|_| "server is shutting down".to_string())?;
    match rrx.recv_timeout(Duration::from_secs(10)) {
        Ok(r) => r,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err("timeout".into()),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err("internal error".into()),
    }
}

// ---- helpers shared by the handlers ----

/// A write as vanilla does it (read-your-writes): run `f` at the next P0, then wait until the view of that tick
/// is published so the client's next request sees the change.
// ponytail: a paused world publishes nothing (ask WP11 to republish after paused P0s); we wait at most 5 s.
pub fn write<R: Send + 'static>(ctx: &ApiCtx, f: impl FnOnce(&mut World) -> Result<R, String> + Send + 'static) -> Result<R, String> {
    let p = ctx.published.clone();
    // the publish count as the command runs (P0): the next publish (P6 of this tick) carries the change
    let (r, seen) = call(&ctx.commands, move |w| {
        let seen = p.version();
        f(w).map(|r| (r, seen))
    })?;
    if !ctx.published.get().env.paused {
        ctx.published.wait_published_after(seen, Duration::from_secs(5));
    }
    Ok(r)
}

/// `db.rooms.findOne({_id: name})`: exact name match.
// ponytail: grid lookup assuming row-major cells, verified by name, with a scan as fallback; switch to a
// strict `RoomsMeta::by_name` once WP01 provides one (theirs uses the lenient engine parser).
pub(crate) fn room_idx(view: &WorldView, name: &str) -> Option<RoomIdx> {
    room_idx_meta(&view.meta, name)
}

pub(crate) fn room_idx_meta(m: &engine::world::RoomsMeta, name: &str) -> Option<RoomIdx> {
    let ok = |r: u32| m.rooms.get(r as usize).is_some_and(|rm| &*rm.name == name && !rm.removed);
    // a stored `_id` is canonical and inside the bounding box: anything else is absent without a scan (the client's
    // map-stats asks for every room in view, also past the world edge)
    let xy = name.parse::<engine::room_name::RoomXY>().ok().filter(|xy| xy.to_string() == name)?;
    let (dx, dy) = (xy.x as i64 - m.x0 as i64, xy.y as i64 - m.y0 as i64);
    if dx < 0 || dy < 0 || dx >= m.w as i64 || dy >= m.h as i64 {
        return None;
    }
    let cell = m.grid.get((dy * m.w as i64 + dx) as usize).copied().unwrap_or(0);
    if cell > 0 && ok(cell - 1) {
        return Some(cell - 1);
    }
    // ponytail: O(rooms) only for empty/mismatched in-box cells (cell order unconfirmed until WP01's lookup lands)
    (0..m.rooms.len() as u32).find(|&r| ok(r))
}

/// `view.terrain.get(r)`. Tests substitute terrain through `fake::TERRAIN` while WP01's TerrainStore accessors
/// are stubs (no effect outside `cfg(test)`).
pub(crate) fn terrain_of(view: &WorldView, r: RoomIdx) -> engine::terrain::RoomTerrain {
    #[cfg(test)]
    if let Some(t) = fake::TERRAIN.with(|m| m.borrow().get(&r).cloned()) {
        return t;
    }
    view.terrain.get(r).clone()
}

/// A users doc field.
pub fn field<'a>(o: &'a Obj, k: &str) -> Option<&'a Val> {
    o.get(engine::doc::Atom::intern(k))
}

pub fn str_field<'a>(o: &'a Obj, k: &str) -> Option<&'a str> {
    field(o, k).and_then(Val::as_str)
}

/// JS truthiness of a stored value (absent = undefined = false).
pub(crate) fn truthy(v: Option<&Val>) -> bool {
    match v {
        None | Some(Val::Null) => false,
        Some(Val::Bool(b)) => *b,
        Some(Val::Num(n)) => *n != 0.0 && !n.is_nan(),
        Some(Val::Str(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

/// What clients may see of a user: `{_id, username, badge}` (never the whole doc, SOCK-ROOM-USERS / FMT-LOKI-ARTIFACTS).
pub(crate) fn user_brief(view: &WorldView, u: UserIdx) -> Json {
    let s = &view.users[u as usize];
    let mut out = vec![("_id".to_string(), Json::Str(s.id.as_str().into()))];
    for k in ["username", "badge"] {
        if let Some(v) = field(&s.doc, k) {
            out.push((k.into(), v.to_json()));
        }
    }
    Json::Obj(out)
}

/// The user a doc's `user`-like field names.
pub(crate) fn user_by_id(view: &WorldView, id: &str) -> Option<UserIdx> {
    view.user_ids.get(&Id::new(id)?).copied()
}

pub fn obj_set(o: &mut Obj, k: &str, v: Val) {
    o.set(engine::doc::Atom::intern(k), v);
}

pub fn s(v: &str) -> Val {
    Val::Str(v.into())
}

pub(crate) use util::num::iso_date;

/// Field-by-field copy (UserState is not Clone); every field is an Arc or small.
pub(crate) fn clone_user(s: &UserState) -> UserState {
    UserState {
        id: s.id,
        seq: s.seq,
        doc: s.doc.clone(),
        auth: s.auth.clone(),
        code: s.code.clone(),
        memory: s.memory.clone(),
        memory_version: s.memory_version,
        segments: s.segments.clone(),
        public_segments: s.public_segments.clone(),
        inter_shard: s.inter_shard.clone(),
        console_queue: s.console_queue.clone(),
        power_creeps: s.power_creeps.clone(),
        inbox: s.inbox.clone(),
    }
}

/// Replaces user `u` with an edited copy (copy-on-write; the published view keeps the old one).
pub fn edit_user(w: &mut World, u: UserIdx, f: impl FnOnce(&mut UserState)) {
    let mut s = clone_user(&w.users[u as usize]);
    f(&mut s);
    w.users[u as usize] = Arc::new(s);
}

/// `db.users.insert(doc)` (+ the user's code branches): `_id` appended last as storage's genId does.
// ponytail: local stand-in for a WP01 world helper (interface request); not journaled until WP16 lands.
pub fn add_user(w: &mut World, mut doc: Obj, auth: Obj, code: Vec<Obj>) -> UserIdx {
    let id = w.ids.next();
    let seq = w.next_seq;
    w.next_seq += 1;
    obj_set(&mut doc, "_id", s(id.as_str()));
    let branches: Vec<Arc<Obj>> = code
        .into_iter()
        .map(|mut c| {
            let cid = w.ids.next();
            obj_set(&mut c, "user", s(id.as_str()));
            obj_set(&mut c, "_id", s(cid.as_str()));
            Arc::new(c)
        })
        .collect();
    let active = |k: &str| branches.iter().position(|b| matches!(field(b, k), Some(Val::Bool(true))));
    let code = CodeState { active_world: active("activeWorld"), active_sim: active("activeSim"), branches, version: 1 };
    let u = w.users.len() as UserIdx;
    if let Some(name) = str_field(&doc, "usernameLower") {
        Arc::make_mut(&mut w.usernames).insert(name.into(), u);
    }
    w.users.push(Arc::new(UserState {
        id,
        seq,
        doc: Arc::new(doc),
        auth: Arc::new(auth),
        code: Arc::new(code),
        memory: Default::default(),
        memory_version: 0,
        segments: Default::default(),
        public_segments: Arc::from(""),
        inter_shard: None,
        console_queue: Arc::from(Vec::new()),
        power_creeps: Arc::from(Vec::new()),
        inbox: Arc::new(Inbox { notifications: vec![], messages: vec![] }),
    }));
    Arc::make_mut(&mut w.user_ids).insert(id, u);
    w.index.presence.push(Arc::default());
    u
}

/// A clock for API handlers: `vclock(G)` (CLK-API, CLOCK-VIRTUAL).
pub(crate) fn vnow(w: &World) -> i64 {
    w.clock.at(w.time)
}


// ---- room edits from Commands ----
// ponytail: local stand-ins for WP01's presence/index maintenance (no FROZEN helper exists for P0 edits;
// interface request). They derive exactly the §3.4 fields the backend and runners read.

/// A fresh snapshot of room `r`'s docs (seq order kept by the caller), recomputing the derived fields.
pub(crate) fn room_snapshot(prev: &engine::world::RoomSnapshot, objects: Vec<engine::doc::DocRef>, flags: Arc<[Arc<Obj>]>, user_ids: &engine::world::UserIds, time: u64) -> engine::world::RoomSnapshot {
    use engine::world::Presence;
    let mut presence: Vec<(UserIdx, Presence)> = Vec::new();
    let mut refs = std::collections::BTreeSet::new();
    let (mut movers, mut sends, mut pcs) = (false, false, false);
    let uid = |v: Option<&Val>| v.and_then(Val::as_str).and_then(Id::new).and_then(|i| user_ids.get(&i).copied());
    for d in &objects {
        let o = &d.obj;
        let kind = d.kind();
        movers |= matches!(kind, "creep" | "powerCreep") && truthy(field(o, "interRoom"));
        sends |= kind == "terminal" && truthy(field(o, "send"));
        pcs |= kind == "powerCreep";
        for k in ["reservation", "sign"] {
            if let Some(u) = uid(field(o, k).and_then(Val::as_obj).and_then(|x| field(x, "user"))) {
                refs.insert(u);
            }
        }
        let Some(u) = uid(field(o, "user")) else { continue };
        refs.insert(u);
        let i = match presence.iter().position(|(x, _)| *x == u) {
            Some(i) => i,
            None => {
                presence.push((u, Presence::default()));
                presence.len() - 1
            }
        };
        let p = &mut presence[i].1;
        p.objects += 1;
        if kind != "flag" && kind != "constructionSite" {
            if p.vision == 0 {
                p.first_vision_seq = d.seq;
            }
            p.vision += 1;
        }
        if kind == "observer" {
            let target = match field(o, "observeRoom") {
                Some(Val::Str(s)) => s.to_string(),
                Some(v) => {
                    let mut t = String::new();
                    v.write_json(&mut t);
                    t
                }
                None => "undefined".into(),
            };
            p.observe = Some((d.seq, target.into()));
        }
    }
    let draft = engine::processor::RoomDraft {
        info: prev.info.clone(),
        objects,
        flags,
        event_log: prev.event_log.clone(),
        map_view: prev.map_view.clone(),
        presence,
        user_refs: refs.into_iter().collect(),
        has_movers: movers,
        has_terminal_send: sends,
        has_power_creeps: pcs,
    };
    engine::world::RoomSnapshot::new(draft, time)
}

/// Installs new docs/flags/`rooms` doc for room `r` and updates `index.objects` / `index.presence` for it.
pub(crate) fn install_room(w: &mut World, r: RoomIdx, objects: Vec<engine::doc::DocRef>, flags: Option<Arc<[Arc<Obj>]>>, info: Option<Arc<Obj>>) {
    let prev = w.rooms[r as usize].clone();
    let flags = flags.unwrap_or_else(|| prev.flags.clone());
    let mut snap = room_snapshot(&prev, objects, flags, &w.user_ids, prev.processed_at);
    if let Some(info) = info {
        snap.info = info;
    }
    for d in prev.objects.iter() {
        w.index.objects.remove(&d.id());
    }
    for d in snap.objects.iter() {
        let owner = str_field(&d.obj, "user").and_then(Id::new).and_then(|i| w.user_ids.get(&i).copied());
        w.index.objects.insert(d.id(), engine::world::ObjLoc { room: r, seq: d.seq, owner });
    }
    for (u, _) in prev.presence.iter() {
        if let Some(m) = w.index.presence.get_mut(*u as usize) {
            Arc::make_mut(m).remove(&r);
        }
    }
    for (u, p) in snap.presence.iter() {
        if let Some(m) = w.index.presence.get_mut(*u as usize) {
            Arc::make_mut(m).insert(r, p.clone());
        }
    }
    // the room-set indexes P5 scans (§3.8): kept equal to the snapshot flags
    for (set, on) in [(&mut w.index.movers, snap.has_movers), (&mut w.index.terminal_sends, snap.has_terminal_send), (&mut w.index.power_creep_rooms, snap.has_power_creeps)] {
        if on {
            set.insert(r);
        } else {
            set.remove(&r);
        }
    }
    w.rooms[r as usize] = Arc::new(snap);
}

/// `utils.activateRoom(room)`: processed this tick (ACT-SOURCES, P0 row).
// ponytail: tolerates WP01's `todo!()` ActiveSet::insert until it lands; drop the catch_unwind then.
pub(crate) fn activate(w: &mut World, r: RoomIdx) {
    let a = &mut w.active;
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| a.insert(r)));
}

/// `db['rooms.objects'].insert(doc)` from a P0 command: `_id` (appended last, as storage's genId does) and
/// `seq` assigned now (§3.7).
pub(crate) fn new_doc(w: &mut World, mut o: Obj) -> engine::doc::DocRef {
    let id = w.ids.next();
    obj_set(&mut o, "_id", s(id.as_str()));
    let seq = w.next_seq;
    w.next_seq += 1;
    engine::doc::Doc::new(seq, o)
}

/// Test world: rooms W1N1 (free controller, 2 sources, mineral), W2N1 (alice: RCL 2, spawn, creep, flags),
/// W1N2 (reserved and signed by bob); users "2", "3", alice, bob.
#[cfg(test)]
pub(crate) mod fake {
    use super::*;
    use engine::doc::{Doc, DocRef};
    use engine::policy::{Compat, Profile, VClock};
    use engine::room_name::RoomXY;
    use engine::terrain::RoomTerrain;
    use engine::world::*;
    use std::collections::HashMap;

    pub(crate) fn obj(json: &str) -> Obj {
        match Val::from_json(&Json::parse(json).unwrap()) {
            Val::Obj(o) => o,
            _ => panic!("not an object: {json}"),
        }
    }

    pub(crate) const ALICE: UserIdx = 2;
    pub(crate) const BOB: UserIdx = 3;

    thread_local! {
        /// Terrain by room for `terrain_of` on this test thread.
        pub(crate) static TERRAIN: std::cell::RefCell<HashMap<RoomIdx, RoomTerrain>> = Default::default();
    }

    pub(crate) fn world() -> World {
        let names = ["W1N1", "W2N1", "W1N2"];
        let rooms_meta: Vec<RoomMeta> = names.iter().map(|n| RoomMeta { name: (*n).into(), xy: n.parse::<RoomXY>().unwrap(), removed: false }).collect();
        let meta = RoomsMeta { x0: -3, y0: -3, w: 2, h: 2, grid: vec![0, 3, 2, 1], rooms: rooms_meta, world_size: 12 };
        let mut user_ids = engine::world::UserIds::default();
        let mut usernames = HashMap::new();
        let mut users = Vec::new();
        let udocs = [
            r#"{"_id":"2","username":"Invader","cpu":0}"#,
            r#"{"_id":"3","username":"Source Keeper","cpu":0}"#,
            r##"{"_id":"a11ce0000000001","username":"alice","usernameLower":"alice","cpu":100,"active":10000,"gcl":1000,"money":12345,"resources":{"token":2},"badge":{"type":1,"color1":"#ff0000","color2":"#00ff00","color3":"#0000ff","param":0,"flip":false},"steam":{"id":"7656","displayName":"A"}}"##,
            r#"{"_id":"b0b000000000002","username":"bob","usernameLower":"bob","cpu":100,"bot":"simplebot"}"#,
        ];
        for (i, d) in udocs.iter().enumerate() {
            let doc = obj(d);
            let id = Id::new(str_field(&doc, "_id").unwrap()).unwrap();
            user_ids.insert(id, i as UserIdx);
            usernames.insert(str_field(&doc, "username").unwrap().to_lowercase().into(), i as UserIdx);
            let code = obj(&format!(r#"{{"_id":"c{i}","user":"{id}","branch":"default","modules":{{"main":"module.exports.loop=function(){{}}","lib$DOT$util":"x"}},"activeWorld":true,"activeSim":true,"timestamp":5}}"#));
            users.push(Arc::new(UserState {
                id,
                seq: i as u64,
                doc: Arc::new(doc),
                auth: Arc::new(Obj::new()),
                code: Arc::new(CodeState { branches: vec![Arc::new(code)], active_world: Some(0), active_sim: Some(0), version: 1 }),
                memory: util::jsstr::JsStr::from_str(if i == ALICE as usize { r#"{"a":{"b":[1,{"c":null}],"s":"str"},"n":5}"# } else { "" }),
                memory_version: 3,
                segments: Default::default(),
                public_segments: Arc::from(""),
                inter_shard: None,
                console_queue: Arc::from(Vec::new()),
                power_creeps: Arc::from(Vec::new()),
                inbox: Arc::new(Inbox { notifications: vec![], messages: vec![] }),
            }));
        }
        let mut seq = 100;
        let mut docs = |list: &[&str]| -> Vec<DocRef> {
            list.iter()
                .map(|d| {
                    seq += 1;
                    Doc::new(seq, obj(d))
                })
                .collect()
        };
        let room_docs = [
            docs(&[
                r#"{"_id":"ctrl1","type":"controller","room":"W1N1","x":20,"y":20,"level":0}"#,
                r#"{"_id":"src1","type":"source","room":"W1N1","x":10,"y":10,"energy":3000,"energyCapacity":3000}"#,
                r#"{"_id":"src2","type":"source","room":"W1N1","x":30,"y":30,"energy":3000,"energyCapacity":3000,"invaderHarvested":50}"#,
                r#"{"_id":"min1","type":"mineral","room":"W1N1","x":40,"y":40,"mineralType":"H","density":3}"#,
            ]),
            docs(&[
                r#"{"_id":"ctrl2","type":"controller","room":"W2N1","x":20,"y":20,"level":2,"user":"a11ce0000000001","safeMode":99999}"#,
                r#"{"_id":"spawn2","type":"spawn","room":"W2N1","x":25,"y":25,"name":"Spawn1","user":"a11ce0000000001","store":{"energy":300}}"#,
                r#"{"_id":"creep2","type":"creep","room":"W2N1","x":26,"y":25,"name":"c1","user":"a11ce0000000001","store":{"energy":10}}"#,
                r#"{"_id":"site2","type":"constructionSite","room":"W2N1","x":27,"y":25,"structureType":"extension","name":"","user":"a11ce0000000001","progress":0,"progressTotal":3000}"#,
            ]),
            docs(&[
                r#"{"_id":"ctrl3","type":"controller","room":"W1N2","x":20,"y":20,"level":0,"reservation":{"user":"b0b000000000002","endTime":500},"sign":{"user":"b0b000000000002","text":"hi","time":5,"datetime":1}}"#,
                r#"{"_id":"core3","type":"invaderCore","room":"W1N2","x":5,"y":5,"level":2,"user":"2"}"#,
            ]),
        ];
        let flags: Arc<[Arc<Obj>]> = Arc::from(vec![Arc::new(obj(r#"{"_id":"f1","user":"a11ce0000000001","room":"W2N1","data":"Flag1~1~2~10~10|Home~5~5~25~26"}"#))]);
        let mut rooms = Vec::new();
        let mut index = WorldIndex::default();
        index.presence = vec![Arc::default(); users.len()];
        index.room_cost_ns = vec![0; 3];
        for (r, objs) in room_docs.into_iter().enumerate() {
            let name = names[r];
            let info = Arc::new(obj(&format!(r#"{{"_id":"{name}","status":"normal","active":true}}"#)));
            let blank = RoomSnapshot::new(
                engine::processor::RoomDraft {
                    info,
                    objects: vec![],
                    flags: Arc::from(Vec::new()),
                    event_log: Arc::from("[]"),
                    map_view: Arc::from(format!(r#"{{"w":[],"r":[],"s":[[10,10]],"room":"{name}"}}"#)),
                    presence: vec![],
                    user_refs: vec![],
                    has_movers: false,
                    has_terminal_send: false,
                    has_power_creeps: false,
                },
                0,
            );
            let f = if r == 1 { flags.clone() } else { Arc::from(Vec::new()) };
            let snap = room_snapshot(&blank, objs, f, &user_ids, 0);
            for d in snap.objects.iter() {
                let owner = str_field(&d.obj, "user").and_then(Id::new).and_then(|i| user_ids.get(&i).copied());
                index.objects.insert(d.id(), ObjLoc { room: r as RoomIdx, seq: d.seq, owner });
            }
            for (u, p) in snap.presence.iter() {
                Arc::make_mut(&mut index.presence[*u as usize]).insert(r as RoomIdx, p.clone());
            }
            rooms.push(Arc::new(snap));
        }
        let mut terrain = vec![RoomTerrain::default(); 3];
        terrain[0].set_raw(1, 1, 1);
        let clock = VClock { base_ms: 1_600_000_000_000, base_tick: 0, nominal_tick_ms: 1000 };
        let mut w = World::new(1, clock, Compat::new(Profile::Private), meta, TerrainStore::new(terrain), rooms, users);
        w.time = 1000;
        w.access = Arc::new(AccessSnapshot { version: 0, accessible: vec![0b111], status: HashMap::new() });
        w.user_ids = Arc::new(user_ids);
        w.usernames = Arc::new(usernames);
        w.index = index;
        w.next_seq = 1000;
        w
    }

    /// `World::view` (WP01 stub) for tests.
    pub(crate) fn view(w: &World) -> WorldView {
        WorldView {
            time: w.time,
            vnow: w.clock.at(w.time),
            meta: w.meta.clone(),
            terrain: w.terrain.clone(),
            access: w.access.clone(),
            rooms: Arc::from(w.rooms.clone()),
            users: Arc::from(w.users.clone()),
            user_ids: w.user_ids.clone(),
            usernames: w.usernames.clone(),
            presence: Arc::from(w.index.presence.clone()),
            market: w.market.clone(),
            env: w.env.clone(),
            persist: Arc::new(PersistMeta { seed: w.seed, clock: w.clock, next_seq: w.next_seq, id_counter: w.ids.counter(), active: vec![], mods: w.mods.clone() }),
        }
    }

    /// A fake tick thread: applies each Command, then publishes the next view (time + 1).
    pub(crate) fn tick_thread(p: Arc<Published>) -> (Sender<Command>, std::thread::JoinHandle<World>) {
        let (tx, rx) = std::sync::mpsc::channel::<Command>();
        let h = std::thread::spawn(move || {
            let mut w = world();
            for c in rx {
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (c.apply)(&mut w)));
                if let (Some(reply), Ok(r)) = (c.reply, &r) {
                    let _ = reply.send(r.clone());
                }
                w.time += 1;
                p.set(view(&w));
            }
            w
        });
        (tx, h)
    }

    /// Context for calling a handler directly.
    pub(crate) fn ctx(user: Option<UserIdx>) -> (ApiCtx, std::thread::JoinHandle<World>) {
        let p = Arc::new(Published::new(view(&world())));
        let (tx, h) = tick_thread(p.clone());
        (ApiCtx { published: p, commands: tx, user }, h)
    }

    /// Runs a handler and returns (status, parsed body).
    pub(crate) fn run(handler: fn(&Request, &ApiCtx) -> Response, ctx: &ApiCtx, method: &str, path_and_query: &str, body: &str) -> (u16, Json) {
        let (path, qs) = path_and_query.split_once('?').unwrap_or((path_and_query, ""));
        let query = qs.split('&').filter(|p| !p.is_empty()).map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (http::url_decode(k), http::url_decode(v))
        }).collect();
        let req = Request {
            method: method.into(),
            path: path.into(),
            query,
            headers: vec![("content-type".into(), "application/json".into())],
            body: body.as_bytes().to_vec(),
        };
        let res = server::invoke(handler, &req, ctx);
        let text = String::from_utf8(res.body).unwrap();
        (res.status, Json::parse(&text).unwrap_or(Json::Str(text)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shared(max_connections: usize) -> Arc<Shared> {
        let (ctx, _tick) = fake::ctx(None);
        let config = BackendConfig { host: "0.0.0.0".into(), port: 0, cli_host: "127.0.0.1".into(), cli_port: 0, cli_http: false, max_connections, request_timeout_s: 120, server_password: None, steam: "off".into() };
        Arc::new(Shared { published: ctx.published, commands: ctx.commands, config, routes: vec![], conns: AtomicUsize::new(0), callers: Mutex::default(), large_body: Mutex::default() })
    }

    fn ip(s: &str) -> Option<IpAddr> {
        Some(s.parse().unwrap())
    }

    #[test]
    fn one_address_cannot_take_every_slot() {
        let sh = shared(MAX_PER_ADDRESS * 2);
        let held: Vec<Slot> = (0..MAX_PER_ADDRESS).map(|_| take_slot(&sh, ip("203.0.113.5")).unwrap()).collect();
        assert!(take_slot(&sh, ip("203.0.113.5")).is_none());
        assert!(take_slot(&sh, ip("::ffff:203.0.113.5")).is_none(), "the same address over IPv6");
        let other = take_slot(&sh, ip("203.0.113.6")).expect("someone else still gets in");
        drop(other);
        drop(held);
        assert_eq!(sh.conns.load(Ordering::Relaxed), 0);
        assert!(sh.callers.lock().unwrap().is_empty(), "nothing stays behind for an address that left");
        assert!(take_slot(&sh, ip("203.0.113.5")).is_some());
    }

    #[test]
    fn an_ipv6_home_counts_as_one_caller() {
        let sh = shared(MAX_PER_ADDRESS * 2);
        let held: Vec<Slot> = (0..MAX_PER_ADDRESS as u16).map(|i| take_slot(&sh, ip(&format!("2001:db8:1:2::{:x}", i + 1))).unwrap()).collect();
        assert!(take_slot(&sh, ip("2001:db8:1:2:ffff::1")).is_none());
        assert!(take_slot(&sh, ip("2001:db8:1:3::1")).is_some());
        drop(held);
    }

    #[test]
    fn loopback_has_no_share_of_its_own() {
        let sh = shared(MAX_PER_ADDRESS + 2);
        // a proxy on this PC brings every player in from here
        let held: Vec<Slot> = (0..MAX_PER_ADDRESS + 2).map(|_| take_slot(&sh, ip("127.0.0.1")).unwrap()).collect();
        assert!(take_slot(&sh, ip("::1")).is_none(), "max_connections still counts it");
        drop(held);
    }

    #[test]
    fn a_slot_lasts_until_its_last_holder_ends() {
        let sh = shared(1);
        let reader = Arc::new(take_slot(&sh, ip("203.0.113.5")).unwrap());
        let writer = reader.clone();
        assert!(take_slot(&sh, ip("203.0.113.6")).is_none());
        drop(reader);
        assert!(take_slot(&sh, ip("203.0.113.6")).is_none(), "the websocket writer has not given up yet");
        drop(writer);
        assert!(take_slot(&sh, ip("203.0.113.6")).is_some());
    }

    #[test]
    fn crowded_means_past_three_quarters() {
        let sh = shared(8);
        let mut held: Vec<Slot> = (0..6).map(|_| take_slot(&sh, ip("127.0.0.1")).unwrap()).collect();
        assert!(!sh.crowded(), "six of eight is three quarters");
        held.push(take_slot(&sh, ip("127.0.0.1")).unwrap());
        assert!(sh.crowded());
        let sh = shared(1);
        assert!(!sh.crowded());
        let _one = take_slot(&sh, ip("127.0.0.1")).unwrap();
        assert!(sh.crowded(), "a single slot has to turn over too");
        let sh = shared(usize::MAX);
        let _one = take_slot(&sh, ip("127.0.0.1")).unwrap();
        assert!(!sh.crowded());
    }

    #[test]
    fn published_wait() {
        let w = fake::world();
        let p = Arc::new(Published::new(fake::view(&w)));
        assert!(!p.wait_published_after(0, Duration::from_millis(10)));
        let p2 = p.clone();
        let mut v = fake::view(&w);
        v.time += 1;
        let h = std::thread::spawn(move || p2.wait_after(w.time).time);
        std::thread::sleep(Duration::from_millis(20));
        p.set(v);
        assert_eq!(h.join().unwrap(), fake::world().time + 1);
    }

    #[test]
    fn call_runs_on_the_tick_thread() {
        let (tx, rx) = std::sync::mpsc::channel::<Command>();
        let t = std::thread::spawn(move || {
            let mut w = fake::world();
            for c in rx.iter().take(2) {
                let r = (c.apply)(&mut w);
                if let Some(reply) = c.reply {
                    let _ = reply.send(r);
                }
            }
        });
        assert_eq!(call(&tx, |w| Ok(w.time)), Ok(fake::world().time));
        assert_eq!(call::<()>(&tx, |_| Err("nope".into())), Err("nope".into()));
        t.join().unwrap();
        assert_eq!(call(&tx, |_| Ok(1)), Err("server is shutting down".into()));
    }

    #[test]
    fn install_room_keeps_room_set_indexes() {
        let mut w = fake::world();
        let mut objs = w.rooms[0].objects.to_vec();
        objs.push(engine::doc::Doc::new(900, fake::obj(r#"{"_id":"pcx","type":"powerCreep","room":"W1N1","x":1,"y":1,"user":"a11ce0000000001","interRoom":{"room":"W2N1"}}"#)));
        install_room(&mut w, 0, objs, None, None);
        assert!(w.index.power_creep_rooms.contains(&0) && w.index.movers.contains(&0));
        assert!(w.index.objects.contains_key(&Id::new("pcx").unwrap()));
        let objs = w.rooms[0].objects.iter().filter(|d| d.kind() != "powerCreep").cloned().collect();
        install_room(&mut w, 0, objs, None, None);
        assert!(!w.index.power_creep_rooms.contains(&0) && !w.index.movers.contains(&0));
        assert!(!w.index.objects.contains_key(&Id::new("pcx").unwrap()));
    }

    #[test]
    fn room_lookup_is_exact() {
        let w = fake::world();
        let v = fake::view(&w);
        assert_eq!(room_idx(&v, "W1N1"), Some(0));
        assert_eq!(room_idx(&v, "W2N1"), Some(1));
        assert_eq!(room_idx(&v, "W01N1"), None);
        assert_eq!(room_idx(&v, "w1n1"), None);
        assert_eq!(room_idx(&v, "W9N9"), None);
        assert_eq!(room_idx(&v, "E40S40"), None, "past the world edge: no scan");
        assert_eq!(room_idx(&v, "W1N01"), None);
        assert_eq!(room_idx(&v, "sim"), None);
        assert_eq!(room_idx(&v, "W1N2"), Some(2));
    }
}
