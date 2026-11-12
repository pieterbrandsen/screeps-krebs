//! mirror: reference/backend/lib/game/socket/server.js
//! parity: DIVERGENT(SOCK-SUBSCRIBE): `user:<id>/*` and `mapVisual:<id>` need the same authenticated user (SEC fix),
//! each connection's messages run in order (SOCK-AUTH race fixed), gzip keeps message order (SOCK-GZIP),
//! listeners are routed by exact channel (no regex firehose)
//!
//! Greeting `time <ms>` + `protocol 14`; `auth <token>`; `gzip on|off`; subscribe/unsubscribe and the
//! channel table (ARCHITECTURE.md §8.4). Owner: WP04.

use super::{map, rooms, system, user};
use crate::{authlib, Published};
use engine::doc::DocRef;
use engine::world::{UserIdx, WorldView};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;
use util::json::Json;

/// A socket channel handler: called on subscribe and after every tick for each subscribed channel, with
/// the connection's user (None = unauthenticated) and the full channel name. `None` rejects the
/// subscription / sends nothing this tick; `Some(msg)` is pushed as `[channel, msg]`.
pub type ChannelFn = fn(Option<UserIdx>, &str, &WorldView) -> Option<Json>;

/// What a connection did (screeps-krebs): a vanilla socket module sees each `subscribe` in its `onSubscribe`, each
/// `unsubscribe` in its `onUnsubscribe`, and the end of the connection through `conn.on('close')`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SocketEvent<'a> {
    Subscribe(&'a str),
    Unsubscribe(&'a str),
    Close,
}

/// What a socket event handler gets besides the event: the connection's id, its user (None = unauthenticated), the
/// world, and the command channel through which a handler changes the world (`crate::call`, or a `Command` sent
/// without waiting).
pub struct SocketEventCtx<'a> {
    pub conn: u64,
    pub user: Option<UserIdx>,
    pub view: &'a WorldView,
    pub commands: Option<&'a std::sync::mpsc::Sender<engine::world::Command>>,
}

/// A mod's socket event handler (`Registry::socket_events`), called after the built-ins handled the message.
pub type SocketEventFn = fn(SocketEvent<'_>, &SocketEventCtx<'_>);

/// Every mod's socket event handler, in mod load order.
pub(crate) fn socket_event(p: &Published, conn: &Conn, event: SocketEvent<'_>) {
    let mods = p.mods();
    if mods.socket_events.is_empty() {
        return;
    }
    let view = p.get();
    let ctx = SocketEventCtx { conn: conn.id, user: conn.user(), view: &view, commands: mods.commands.as_ref() };
    for f in &mods.socket_events {
        f(event, &ctx);
    }
}

/// `conn.on('close')`: the hub forgets the connection, then the mods hear of it.
pub(crate) fn close(p: &Published, conn: &Conn) {
    p.hub().remove_conn(conn.id);
    socket_event(p, conn, SocketEvent::Close);
}

/// Built-in channels by name prefix; mods add theirs (`Registry::socket_channels`, `Published::mods`).
pub const CHANNELS: &[(&str, ChannelFn)] = &[
    ("room:", rooms::room),
    ("roomMap2:", map::room_map2),
    ("user:", user::user),
    ("server-message", system::server_message),
];

/// Per-connection send queue cap (ARCHITECTURE.md §8.1): beyond it event messages are dropped and the
/// connection's room baselines are reset, so the next push is a full `getDiff([], …)` snapshot.
pub const OUT_CAP: usize = 4 << 20;

/// What a queued message costs beyond its text, counted against `OUT_CAP`, or short replies would cost several times the cap.
/// A place in the queue is `size_of::<Out>()`, 32 bytes, and `VecDeque` doubles, so up to two of those. Its text has a heap
/// block of 32 bytes or more even when it is 11 bytes long, as `auth failed` is.
const ITEM_COST: usize = 96;

/// What the writer thread sends next.
pub(crate) enum Out {
    /// One protocol message (framed `a[…]` on SockJS, one text frame on raw websockets).
    Text(String),
    Pong(Vec<u8>),
    Close(u16, String),
}

#[derive(Default)]
struct OutState {
    items: VecDeque<Out>,
    bytes: usize,
    /// The pong owed. One will do (RFC 6455 5.5.3 lets it answer the latest ping only), so a client that pings without
    /// reading grows nothing.
    pong: Option<Vec<u8>>,
    closed: bool,
}

/// Bounded queue drained by the connection's writer thread; producers never block.
#[derive(Default)]
pub(crate) struct Outbox {
    st: Mutex<OutState>,
    cv: Condvar,
}

impl Outbox {
    fn lock(&self) -> MutexGuard<'_, OutState> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// False when the queue is full (message dropped) or closed.
    pub(crate) fn push_text(&self, s: String) -> bool {
        let mut st = self.lock();
        let cost = s.len() + ITEM_COST;
        if st.closed || (st.bytes + cost > OUT_CAP && !st.items.is_empty()) {
            return false;
        }
        st.bytes += cost;
        st.items.push_back(Out::Text(s));
        self.cv.notify_one();
        true
    }

    pub(crate) fn push_control(&self, o: Out) {
        let mut st = self.lock();
        if !st.closed {
            st.items.push_back(o);
            self.cv.notify_one();
        }
    }

    /// Answers a ping, in place of any pong still waiting.
    pub(crate) fn push_pong(&self, payload: Vec<u8>) {
        let mut st = self.lock();
        if !st.closed {
            st.pong = Some(payload);
            self.cv.notify_one();
        }
    }

    pub(crate) fn close(&self) {
        self.lock().closed = true;
        self.cv.notify_all();
    }

    #[cfg(test)]
    pub(crate) fn is_closed(&self) -> bool {
        self.lock().closed
    }

    /// Everything queued, waiting up to `timeout` for the first item: `Some(vec![])` on timeout, `None` once
    /// closed and empty.
    pub(crate) fn drain(&self, timeout: Duration) -> Option<Vec<Out>> {
        let mut st = self.lock();
        if st.items.is_empty() && st.pong.is_none() && !st.closed {
            st = self.cv.wait_timeout(st, timeout).unwrap_or_else(|e| e.into_inner()).0;
        }
        if st.items.is_empty() && st.pong.is_none() {
            return if st.closed { None } else { Some(vec![]) };
        }
        st.bytes = 0;
        let pong = st.pong.take().map(Out::Pong);
        Some(pong.into_iter().chain(st.items.drain(..)).collect())
    }
}

/// One socket connection (SockJS session or raw websocket).
pub(crate) struct Conn {
    pub id: u64,
    user: Mutex<Option<UserIdx>>,
    gzip: AtomicBool,
    pub out: Outbox,
    /// Pongs received (heartbeat).
    pub pongs: AtomicU64,
}

impl Conn {
    pub(crate) fn user(&self) -> Option<UserIdx> {
        *self.user.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `conn.write`: hello and auth replies, never gzipped.
    pub(crate) fn write_raw(&self, s: String) -> bool {
        self.out.push_text(s)
    }

    /// `conn._writeEventRaw`: deflated when gzip is on and that is shorter (SOCK-GZIP).
    pub(crate) fn write_event_raw(&self, message: String) -> bool {
        if self.gzip.load(Ordering::Relaxed) {
            let gz = format!("gz:{}", util::base64::encode(&util::deflate::zlib_compress(message.as_bytes())));
            if gz.len() < message.len() {
                return self.out.push_text(gz);
            }
        }
        self.out.push_text(message)
    }

    /// `conn._writeEvent(channel, data)` with `data` already JSON text.
    pub(crate) fn write_event(&self, channel: &str, data: &str) -> bool {
        let mut m = String::with_capacity(channel.len() + data.len() + 8);
        m.push('[');
        util::json::write_quoted(&mut m, channel);
        m.push(',');
        m.push_str(data);
        m.push(']');
        self.write_event_raw(m)
    }
}

/// One `room:<name>` subscriber (rooms.js `connectedToRooms[room][i]`).
pub(crate) struct RoomSub {
    pub conn: u64,
    pub user: UserIdx,
    /// The objects this subscriber last received (`i.objects`); diffs are taken against it.
    pub objects: Arc<[DocRef]>,
    /// Ids already sent in `users` (`i.users`).
    pub users: HashSet<Box<str>>,
}

/// One `user:<id>/memory/<path>` watch (user.js `connectedToMemory[user][path]`).
pub(crate) struct MemWatch {
    pub path: String,
    pub conns: Vec<u64>,
    /// Memory version last pushed (pushes happen only when Memory changed or the watch is new).
    pub sent: Option<u64>,
}

/// Subscription registry: one per backend, shared by connection threads, HTTP handlers (code/branch
/// events) and the broadcaster.
#[derive(Default)]
pub(crate) struct HubState {
    next_id: u64,
    pub conns: HashMap<u64, Arc<Conn>>,
    /// Exact channel → (connection, registerListener count) for pubsub-relayed channels.
    pub listeners: HashMap<String, Vec<(u64, u32)>>,
    pub rooms: BTreeMap<String, Vec<RoomSub>>,
    pub maps: BTreeMap<String, Vec<u64>>,
    pub memory: BTreeMap<UserIdx, Vec<MemWatch>>,
    pub resources: BTreeMap<UserIdx, Vec<u64>>,
    pub map_visual: BTreeMap<UserIdx, Vec<u64>>,
    /// rooms.js `usersLimit`, recounted on every broadcast.
    pub users_limit: HashMap<UserIdx, u32>,
    /// Visuals of the last broadcast tick: (user, room | "" | "map") → buffer.
    pub visuals: HashMap<(UserIdx, Box<str>), Arc<str>>,
}

#[derive(Default)]
pub(crate) struct Hub {
    st: Mutex<HubState>,
}

impl Hub {
    pub(crate) fn lock(&self) -> MutexGuard<'_, HubState> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn add_conn(&self) -> Arc<Conn> {
        let mut st = self.lock();
        st.next_id += 1;
        let c = Arc::new(Conn { id: st.next_id, user: Mutex::new(None), gzip: AtomicBool::new(false), out: Outbox::default(), pongs: AtomicU64::new(0) });
        st.conns.insert(c.id, c.clone());
        c
    }

    /// `conn.on('close')`: every listener and subscription of the connection goes.
    pub(crate) fn remove_conn(&self, id: u64) {
        let mut st = self.lock();
        if let Some(c) = st.conns.remove(&id) {
            c.out.close();
        }
        st.listeners.retain(|_, v| {
            v.retain(|(c, _)| *c != id);
            !v.is_empty()
        });
        st.rooms.values_mut().for_each(|v| v.retain(|s| s.conn != id));
        st.maps.values_mut().for_each(|v| v.retain(|c| *c != id));
        st.memory.retain(|_, watches| {
            watches.retain_mut(|w| {
                w.conns.retain(|c| *c != id);
                !w.conns.is_empty()
            });
            !watches.is_empty()
        });
        st.resources.values_mut().for_each(|v| v.retain(|c| *c != id));
        st.map_visual.values_mut().for_each(|v| v.retain(|c| *c != id));
    }

    /// server.js `emit(channel, data)`: to every connection that registered this exact channel.
    pub(crate) fn emit(&self, channel: &str, data: &str) {
        self.lock().emit(channel, data);
    }
}

impl HubState {
    pub(crate) fn emit(&self, channel: &str, data: &str) {
        for (id, _) in self.listeners.get(channel).into_iter().flatten() {
            if let Some(c) = self.conns.get(id) {
                c.write_event(channel, data);
            }
        }
    }

    pub(crate) fn conn(&self, id: u64) -> Option<&Arc<Conn>> {
        self.conns.get(&id)
    }

    fn register(&mut self, channel: &str, conn: u64) {
        let v = self.listeners.entry(channel.to_string()).or_default();
        match v.iter_mut().find(|(c, _)| *c == conn) {
            Some((_, n)) => *n += 1,
            None => v.push((conn, 1)),
        }
    }

    fn unregister(&mut self, channel: &str, conn: u64) {
        if let Some(v) = self.listeners.get_mut(channel) {
            if let Some(i) = v.iter().position(|(c, _)| *c == conn) {
                v[i].1 -= 1;
                if v[i].1 == 0 {
                    v.remove(i);
                }
            }
            if v.is_empty() {
                self.listeners.remove(channel);
            }
        }
    }
}

/// `conn.write('time …'); conn.write('protocol 14')` (SOCK-HELLO).
pub(crate) fn hello(conn: &Conn) {
    conn.write_raw(format!("time {}", engine::policy::real_now_ms("CLK-INFRA")));
    conn.write_raw(format!("protocol {}", crate::server::PROTOCOL));
}

/// `conn.on('data')`: one client message, handled to completion before the next (SOCK-AUTH fix).
pub(crate) fn on_message(p: &Published, conn: &Arc<Conn>, message: &str) {
    if message.contains(['\n', '\r', '\u{2028}', '\u{2029}']) {
        return; // `.` in the vanilla regexes does not cross line terminators: nothing matches
    }
    if let Some(ch) = message.strip_prefix("subscribe ") {
        subscribe(p, conn, ch);
    }
    if let Some(ch) = message.strip_prefix("unsubscribe ") {
        unsubscribe(p, conn, ch);
    }
    if let Some(token) = message.strip_prefix("auth ") {
        let view = p.get();
        match authlib::check_token(token, &view) {
            Some(u) => {
                *conn.user.lock().unwrap_or_else(|e| e.into_inner()) = Some(u);
                p.stamp_online(u);
                conn.write_raw(format!("auth ok {}", authlib::gen_token(view.users[u as usize].id.as_str())));
            }
            None => {
                conn.write_raw("auth failed".into());
            }
        }
    }
    if message == "gzip on" {
        conn.gzip.store(true, Ordering::Relaxed);
    }
    if message == "gzip off" {
        conn.gzip.store(false, Ordering::Relaxed);
    }
}

/// The `[a-zA-Z0-9_-]+` room-name rule of rooms.js/map.js.
fn room_channel_name(name: &str) -> bool {
    !name.is_empty() && name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}

/// A `user:<id>/<sub>` channel something is sent on. Any other name would only be kept for nothing, as long as the
/// socket lasts (and the stock client's `steam-purchase` never gets a message here).
fn user_channel(view: &WorldView, sub: &str) -> bool {
    matches!(sub, "console" | "cpu" | "code" | "set-active-branch" | "newMessage" | "resources" | "money" | "memory")
        || sub.strip_prefix("message:").is_some_and(|id| crate::user_by_id(view, id).is_some())
}

/// `user:<id>/<rest>` or `mapVisual:<id>` for the connection's own user, else None.
fn own_user(view: &WorldView, user: Option<UserIdx>, id: &str) -> Option<UserIdx> {
    let u = user?;
    (view.users.get(u as usize)?.id.as_str() == id).then_some(u)
}

/// socket/server.js: every module's `onSubscribe` sees every channel, the built-ins first, then the mods'.
fn subscribe(p: &Published, conn: &Arc<Conn>, channel: &str) {
    builtin_subscribe(p, conn, channel);
    mod_subscribe(p, conn, channel);
    socket_event(p, conn, SocketEvent::Subscribe(channel));
}

/// A mod's channel (`ChannelFn`) whose prefix starts `channel`: `Some(data)` registers the connection (as a truthy
/// `onSubscribe` does) and sends `data` unless it is null; `None` declines. `user:<id>…` channels go to mods
/// only for that user's own connection, as to the built-ins (SOCK-SUBSCRIBE).
fn mod_subscribe(p: &Published, conn: &Arc<Conn>, channel: &str) {
    let channels = &p.mods().channels;
    if channels.is_empty() {
        return;
    }
    let view = p.get();
    let user = conn.user();
    if let Some(rest) = channel.strip_prefix("user:") {
        let id = rest.split_once('/').map_or(rest, |(id, _)| id);
        if own_user(&view, user, id).is_none() {
            return;
        }
    }
    for (_, f) in channels.iter().filter(|(prefix, _)| channel.starts_with(*prefix)) {
        let Some(data) = f(user, channel, &view) else { continue };
        p.hub().lock().register(channel, conn.id);
        if !matches!(data, Json::Null) {
            conn.write_event(channel, &data.stringify());
        }
    }
}

/// After each broadcast: every connection listening on a mod's channel gets that channel's `ChannelFn` result for
/// the new world (nothing when it is `None` or null).
pub(crate) fn push_mod_channels(p: &Published, hub: &HubState, view: &WorldView) {
    let channels = &p.mods().channels;
    if channels.is_empty() {
        return;
    }
    for (channel, listeners) in &hub.listeners {
        for (_, f) in channels.iter().filter(|(prefix, _)| channel.starts_with(*prefix)) {
            for (id, _) in listeners {
                let Some(conn) = hub.conns.get(id) else { continue };
                if let Some(data) = f(conn.user(), channel, view).filter(|d| !matches!(d, Json::Null)) {
                    conn.write_event(channel, &data.stringify());
                }
            }
        }
    }
}

fn builtin_subscribe(p: &Published, conn: &Arc<Conn>, channel: &str) {
    let user = conn.user();
    // system.js returns true and user.js's `async onSubscribe` returns a (truthy) Promise: two registerListener
    // calls, so it takes two `unsubscribe server-message` to stop the messages
    if channel == "server-message" {
        let mut hub = p.hub().lock();
        hub.register(channel, conn.id);
        hub.register(channel, conn.id);
        return;
    }
    // rooms.js
    if let Some(name) = channel.strip_prefix("room:") {
        if let (Some(u), true) = (user, room_channel_name(name)) {
            rooms::subscribe(p, conn, u, name);
        }
        return;
    }
    // user.js
    if let Some(rest) = channel.strip_prefix("user:") {
        let view = p.get();
        let Some((id, sub)) = rest.split_once('/') else { return };
        let Some(u) = own_user(&view, user, id) else { return };
        if let Some(path) = sub.strip_prefix("memory/") {
            if !path.is_empty() {
                user::watch_memory(&mut p.hub().lock(), u, path, conn.id);
            }
            return;
        }
        if !user_channel(&view, sub) {
            return;
        }
        let mut hub = p.hub().lock();
        hub.register(channel, conn.id);
        if sub == "cpu" {
            let memory = &view.users[u as usize].memory;
            if memory.utf16_len() > 0 {
                hub.emit(channel, &format!("{{\"cpu\":0,\"memory\":{}}}", memory.utf16_len()));
            }
        }
        if sub == "resources" {
            add_once(hub.resources.entry(u).or_default(), conn.id);
        }
        return;
    }
    if let Some(id) = channel.strip_prefix("mapVisual:") {
        let view = p.get();
        let Some(u) = own_user(&view, user, id) else { return };
        let mut hub = p.hub().lock();
        hub.register(channel, conn.id);
        add_once(hub.map_visual.entry(u).or_default(), conn.id);
        let data = user::map_visual_json(&hub, u);
        hub.emit(channel, &data);
        return;
    }
    // map.js
    if let Some(name) = channel.strip_prefix("roomMap2:") {
        if user.is_some() && room_channel_name(name) {
            map::subscribe(p, conn, name);
        }
    }
}

/// A repeated subscribe must not grow a list that every tick walks.
pub(crate) fn add_once(conns: &mut Vec<u64>, conn: u64) {
    if !conns.contains(&conn) {
        conns.push(conn);
    }
}

fn unsubscribe(p: &Published, conn: &Arc<Conn>, channel: &str) {
    let mut hub = p.hub().lock();
    if let Some(name) = channel.strip_prefix("room:") {
        if let Some(v) = hub.rooms.get_mut(name) {
            v.retain(|s| s.conn != conn.id);
        }
    }
    if let Some(name) = channel.strip_prefix("roomMap2:") {
        if let Some(v) = hub.maps.get_mut(name) {
            v.retain(|c| *c != conn.id);
        }
    }
    if let Some(rest) = channel.strip_prefix("user:") {
        let view = p.get();
        if let Some((id, sub)) = rest.split_once('/') {
            if let Some(&u) = engine::doc::Id::new(id).and_then(|i| view.user_ids.get(&i)) {
                if let Some(path) = sub.strip_prefix("memory/") {
                    // a watch nobody holds goes, or it would use up the user's limit for good
                    if let Some(watches) = hub.memory.get_mut(&u) {
                        watches.retain_mut(|w| {
                            if w.path == path {
                                w.conns.retain(|c| *c != conn.id);
                            }
                            !w.conns.is_empty()
                        });
                    }
                }
                if sub == "resources" {
                    if let Some(v) = hub.resources.get_mut(&u) {
                        v.retain(|c| *c != conn.id);
                    }
                }
            }
        }
    }
    if let Some(id) = channel.strip_prefix("mapVisual:") {
        let view = p.get();
        if let Some(&u) = engine::doc::Id::new(id).and_then(|i| view.user_ids.get(&i)) {
            if let Some(v) = hub.map_visual.get_mut(&u) {
                v.retain(|c| *c != conn.id);
            }
        }
    }
    hub.unregister(channel, conn.id);
    drop(hub);
    socket_event(p, conn, SocketEvent::Unsubscribe(channel));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outbox_cap_and_close() {
        let o = Outbox::default();
        assert!(o.push_text("x".repeat(OUT_CAP))); // one oversized message still goes out alone
        assert!(!o.push_text("y".into()));
        let items = o.drain(Duration::from_millis(1)).unwrap();
        assert_eq!(items.len(), 1);
        assert!(o.push_text("z".into()));
        assert!(o.drain(Duration::from_millis(1)).unwrap().len() == 1);
        assert!(o.drain(Duration::from_millis(1)).unwrap().is_empty());
        o.close();
        assert!(o.drain(Duration::from_millis(1)).is_none());
        assert!(!o.push_text("w".into()));
    }

    /// Counting text alone let 11-byte replies queue up to about 25 MB with nothing read.
    #[test]
    fn short_messages_cost_what_they_really_take() {
        assert!(ITEM_COST >= 2 * std::mem::size_of::<Out>());
        let o = Outbox::default();
        let mut queued = 0;
        while o.push_text("auth failed".into()) {
            queued += 1;
        }
        assert_eq!(queued, OUT_CAP / (ITEM_COST + 11));
        assert_eq!(o.drain(Duration::ZERO).unwrap().len(), queued);
        assert!(o.push_text("the queue has room again".into()));
    }

    #[test]
    fn a_normal_burst_still_fits() {
        let o = Outbox::default();
        let message = "x".repeat(1 << 10);
        assert!((0..3000).all(|_| o.push_text(message.clone())), "3 MB of room updates");
    }

    /// Each ping took a place in the queue, so 48 MB of them cost 250 MB, with the socket's buffers full and the writer stuck.
    #[test]
    fn pings_queue_one_pong() {
        let o = Outbox::default();
        assert!(o.push_text("first".into()));
        for i in 0..200_000u32 {
            o.push_pong(i.to_be_bytes().to_vec());
        }
        let items = o.drain(Duration::ZERO).unwrap();
        assert_eq!(items.len(), 2, "one pong and the message");
        assert!(matches!(&items[0], Out::Pong(d) if d == &199_999u32.to_be_bytes()), "for the latest ping, ahead of what waits");
        assert!(matches!(&items[1], Out::Text(t) if t == "first"));
        assert!(o.drain(Duration::ZERO).unwrap().is_empty());
        o.push_pong(vec![]);
        assert!(matches!(&o.drain(Duration::from_secs(5)).unwrap()[..], [Out::Pong(_)]), "the writer does not wait it out");
        o.close();
        o.push_pong(vec![]);
        assert!(o.drain(Duration::ZERO).is_none(), "nothing more once closed");
    }

    #[test]
    fn gzip_only_when_shorter() {
        let hub = Hub::default();
        let c = hub.add_conn();
        c.gzip.store(true, Ordering::Relaxed);
        c.write_event("a", "1");
        let long = format!("\"{}\"", "abc".repeat(500));
        c.write_event("user:x/console", &long);
        let items = c.out.drain(Duration::from_millis(1)).unwrap();
        let texts: Vec<&str> = items.iter().map(|o| if let Out::Text(t) = o { t.as_str() } else { "" }).collect();
        assert_eq!(texts[0], "[\"a\",1]");
        let gz = texts[1].strip_prefix("gz:").unwrap();
        let raw = util::deflate::zlib_decompress(&util::base64::decode(gz).unwrap(), 1 << 20).unwrap();
        assert_eq!(String::from_utf8(raw).unwrap(), format!("[\"user:x/console\",{long}]"));
    }

    /// system.js + user.js's async onSubscribe both accept `server-message`: one unsubscribe is not enough.
    #[test]
    fn server_message_needs_two_unsubscribes() {
        let p = crate::Published::new(crate::fake::view(&crate::fake::world()));
        let c = p.hub().add_conn();
        on_message(&p, &c, "subscribe server-message");
        on_message(&p, &c, "unsubscribe server-message");
        system::send_server_message(&p, "maintenance");
        let got = c.out.drain(Duration::ZERO).unwrap();
        assert!(matches!(&got[..], [Out::Text(t)] if t == r#"["server-message","maintenance"]"#));
        on_message(&p, &c, "unsubscribe server-message");
        system::send_server_message(&p, "again");
        assert!(c.out.drain(Duration::ZERO).unwrap().is_empty());
    }

    const ALICE: &str = "a11ce0000000001";

    fn signed_in(p: &crate::Published) -> Arc<Conn> {
        let c = p.hub().add_conn();
        on_message(p, &c, &format!("auth {}", crate::authlib::gen_token(ALICE)));
        assert!(c.user().is_some());
        c
    }

    fn watch(verb: &str, path: &str) -> String {
        format!("{verb} user:{ALICE}/memory/{path}")
    }

    fn watches(p: &crate::Published) -> usize {
        p.hub().lock().memory.values().map(Vec::len).sum()
    }

    static EVENTS: Mutex<Vec<String>> = Mutex::new(Vec::new());

    fn record(event: SocketEvent<'_>, ctx: &SocketEventCtx<'_>) {
        let user = ctx.user.map(|u| ctx.view.users[u as usize].id.as_str().to_string());
        EVENTS.lock().unwrap().push(format!("{} {event:?} {user:?} {}", ctx.conn, ctx.commands.is_some()));
    }

    #[test]
    fn mods_hear_subscribe_unsubscribe_and_close() {
        let p = crate::Published::new(crate::fake::view(&crate::fake::world()));
        let (tx, _rx) = std::sync::mpsc::channel();
        let _ = p.mods.set(crate::server::ModBackend::new(&[], Vec::new()).with_socket_events(vec![record], tx));
        let c = signed_in(&p);
        on_message(&p, &c, "subscribe room:W7N3");
        on_message(&p, &c, "unsubscribe room:W7N3");
        close(&p, &c);
        let id = c.id;
        assert_eq!(*EVENTS.lock().unwrap(), vec![
            format!("{id} Subscribe(\"room:W7N3\") Some(\"{ALICE}\") true"),
            format!("{id} Unsubscribe(\"room:W7N3\") Some(\"{ALICE}\") true"),
            format!("{id} Close Some(\"{ALICE}\") true"),
        ]);
        assert!(c.out.is_closed(), "the hub let the connection go first");
    }

    #[test]
    fn memory_watches_are_capped_and_go_with_their_sockets() {
        let p = crate::Published::new(crate::fake::view(&crate::fake::world()));
        let c = signed_in(&p);
        for i in 0..user::MAX_WATCHES + 20 {
            on_message(&p, &c, &watch("subscribe", &format!("w{i}")));
        }
        assert_eq!(watches(&p), user::MAX_WATCHES);
        on_message(&p, &c, &watch("unsubscribe", "w0"));
        assert_eq!(watches(&p), user::MAX_WATCHES - 1, "a dropped watch makes room");
        on_message(&p, &c, &watch("subscribe", "again"));
        assert_eq!(watches(&p), user::MAX_WATCHES);
        p.hub().remove_conn(c.id);
        assert!(p.hub().lock().memory.is_empty(), "the path strings must not outlive the socket");
    }

    #[test]
    fn a_long_memory_path_is_not_kept() {
        let p = crate::Published::new(crate::fake::view(&crate::fake::world()));
        let c = signed_in(&p);
        on_message(&p, &c, &watch("subscribe", &"x".repeat(user::MAX_WATCH_PATH)));
        assert_eq!(watches(&p), 1);
        on_message(&p, &c, &watch("subscribe", &"x".repeat(user::MAX_WATCH_PATH + 1)));
        assert_eq!(watches(&p), 1);
    }

    #[test]
    fn repeating_a_subscribe_grows_nothing() {
        let p = crate::Published::new(crate::fake::view(&crate::fake::world()));
        let c = signed_in(&p);
        for _ in 0..50 {
            on_message(&p, &c, &format!("subscribe user:{ALICE}/resources"));
            on_message(&p, &c, &format!("subscribe mapVisual:{ALICE}"));
            on_message(&p, &c, &watch("subscribe", "a.s"));
            on_message(&p, &c, "subscribe room:W2N1");
            on_message(&p, &c, "subscribe roomMap2:W2N1");
        }
        let hub = p.hub().lock();
        assert_eq!(hub.resources[&crate::fake::ALICE].len(), 1);
        assert_eq!(hub.map_visual[&crate::fake::ALICE].len(), 1);
        assert_eq!(hub.memory[&crate::fake::ALICE][0].conns.len(), 1);
        assert_eq!(hub.rooms["W2N1"].len(), 1);
        assert_eq!(hub.maps["W2N1"].len(), 1);
    }

    #[test]
    fn only_real_channels_are_registered() {
        let p = crate::Published::new(crate::fake::view(&crate::fake::world()));
        let c = signed_in(&p);
        for i in 0..100 {
            on_message(&p, &c, &format!("subscribe user:{ALICE}/junk{i}"));
            on_message(&p, &c, &format!("subscribe user:{ALICE}/message:nobody{i}"));
            on_message(&p, &c, &format!("subscribe roomMap2:nosuchroom{i}"));
        }
        for sub in ["console", "cpu", "code", "set-active-branch", "newMessage", "resources", "money", "memory", "message:b0b000000000002"] {
            on_message(&p, &c, &format!("subscribe user:{ALICE}/{sub}"));
            let channel = format!("user:{ALICE}/{sub}");
            assert!(p.hub().lock().listeners.contains_key(&channel), "{channel}");
        }
        let hub = p.hub().lock();
        assert_eq!(hub.listeners.len(), 9);
        assert!(hub.maps.is_empty(), "a room that is not there has nothing to push");
    }

    #[test]
    fn a_room_past_the_edge_of_the_map_still_gets_its_answer() {
        let p = crate::Published::new(crate::fake::view(&crate::fake::world()));
        let c = signed_in(&p);
        c.out.drain(Duration::ZERO);
        on_message(&p, &c, "subscribe roomMap2:E40S40");
        let got = c.out.drain(Duration::ZERO).unwrap();
        assert!(matches!(&got[..], [Out::Text(t)] if t == r#"["roomMap2:E40S40",{}]"#));
    }

    #[test]
    fn listener_refcount_and_emit() {
        let hub = Hub::default();
        let a = hub.add_conn();
        let b = hub.add_conn();
        {
            let mut st = hub.lock();
            st.register("server-message", a.id);
            st.register("server-message", a.id);
            st.register("server-message", b.id);
            st.unregister("server-message", a.id);
        }
        hub.emit("server-message", "\"hi\"");
        assert_eq!(a.out.drain(Duration::ZERO).unwrap().len(), 1);
        hub.lock().unregister("server-message", a.id);
        hub.emit("server-message", "\"hi\"");
        assert!(a.out.drain(Duration::ZERO).unwrap().is_empty());
        assert_eq!(b.out.drain(Duration::ZERO).unwrap().len(), 2);
        hub.remove_conn(b.id);
        assert!(hub.lock().listeners.is_empty());
        assert!(b.out.is_closed());
    }
}
