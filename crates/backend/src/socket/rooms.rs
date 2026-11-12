//! mirror: reference/backend/lib/game/socket/rooms.js
//! parity: DIVERGENT(SOCK-ROOM-USERS): users are `{_id, username, badge}` (not whole docs); the diff is computed
//! once per baseline per room (§8.4, spec 10 §20)
//!
//! `room:<name>`: subscribe snapshot, per-tick diffs, 2-room limit (SOCK-ROOM-*, ARCHITECTURE.md §8.4).
//! Owner: WP04. Replayed against the official socket modules in tests/fixtures/sockets (server.rs oracle).
//! `get_diff` is a local port of common/index.js getDiff over stored docs until
//! `engine::common` exposes one (WP01, interface request).

use crate::socket::server::{Conn, HubState, RoomSub};
use crate::{room_idx, user_brief, Published};
use engine::doc::{DocRef, Obj, Val};
use engine::world::{RoomIdx, UserIdx, WorldView};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use util::json::Json;

/// rooms.js `USER_LIMIT`.
pub const USER_LIMIT: u32 = 2;

/// How many rooms one account may watch at once (screeps-krebs): `USER_LIMIT` unless a mod set another
/// (`set_user_limit`), as the UCS recording lanes lift it to watch every room of a run.
static LIMIT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(USER_LIMIT);

pub fn set_user_limit(limit: u32) {
    LIMIT.store(limit, std::sync::atomic::Ordering::Relaxed);
}

pub fn user_limit() -> u32 {
    LIMIT.load(std::sync::atomic::Ordering::Relaxed)
}

/// rooms.js `roomBuiltinUsers`.
const BUILTIN: [(&str, &str); 2] = [("2", "Invader"), ("3", "Source Keeper")];

/// The subscribe snapshot for `room:<name>` (None without a user or for an unknown room).
pub fn room(user: Option<UserIdx>, channel: &str, view: &WorldView) -> Option<Json> {
    let u = user?;
    let r = room_idx(view, channel.strip_prefix("room:")?)?;
    Json::parse(&snapshot(view, r, u).0).ok()
}

/// Inserts `k` in JS own-property order (canonical array-index keys first, ascending).
pub(crate) fn js_push(m: &mut Vec<(String, String)>, k: String, v: String) {
    if let Some(slot) = m.iter_mut().find(|(x, _)| *x == k) {
        slot.1 = v;
        return;
    }
    let at = match engine::doc::array_index(&k) {
        None => m.len(),
        Some(i) => m.iter().position(|(x, _)| engine::doc::array_index(x).is_none_or(|j| j > i)).unwrap_or(m.len()),
    };
    m.insert(at, (k, v));
}

/// `{"k":v,…}` from already-serialized values.
pub(crate) fn write_members(m: &[(String, String)]) -> String {
    let mut out = String::from("{");
    for (i, (k, v)) in m.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        util::json::write_quoted(&mut out, k);
        out.push(':');
        out.push_str(v);
    }
    out.push('}');
    out
}

fn json(v: &Val) -> String {
    let mut s = String::new();
    v.write_json(&mut s);
    s
}

fn type_of(v: &Val) -> &'static str {
    match v {
        Val::Null | Val::Arr(_) | Val::Obj(_) => "object",
        Val::Bool(_) => "boolean",
        Val::Num(_) => "number",
        Val::Str(_) => "string",
    }
}

fn truthy(v: &Val) -> bool {
    crate::truthy(Some(v))
}

/// `for (k in v)` over an object or array: (key, value) in JS order.
fn children(v: &Val) -> Vec<(String, &Val)> {
    match v {
        Val::Obj(o) => o.iter().map(|(k, x)| (k.as_str().to_string(), x)).collect(),
        Val::Arr(a) => a.iter().enumerate().map(|(i, x)| (i.to_string(), x)).collect(),
        _ => vec![],
    }
}

/// `v[k]` on an object or array (undefined = None).
fn child<'a>(v: &'a Val, k: &str) -> Option<&'a Val> {
    match v {
        Val::Obj(o) => o.iter().find(|(a, _)| a.as_str() == k).map(|(_, x)| x),
        Val::Arr(a) => engine::doc::array_index(k).and_then(|i| a.get(i as usize)),
        _ => None,
    }
}

/// lodash 3 `_.isEqual` on JSON values (key order irrelevant; undefined only equals undefined).
pub(crate) fn is_equal(a: Option<&Val>, b: Option<&Val>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(Val::Num(x)), Some(Val::Num(y))) => x == y || (x.is_nan() && y.is_nan()),
        (Some(Val::Arr(x)), Some(Val::Arr(y))) => x.len() == y.len() && x.iter().zip(y).all(|(p, q)| is_equal(Some(p), Some(q))),
        (Some(Val::Obj(x)), Some(Val::Obj(y))) => x.len() == y.len() && x.iter().all(|(k, v)| is_equal(Some(v), y.get(k))),
        (Some(x), Some(y)) => !matches!(x, Val::Num(_) | Val::Arr(_) | Val::Obj(_)) && x == y,
        _ => false,
    }
}

/// One object's diff (`objDiff`), None when empty.
fn obj_diff(o: &Obj, n: &Obj) -> Option<String> {
    let mut d: Vec<(String, String)> = Vec::new();
    for (k, ov) in o.iter() {
        let key = k.as_str().to_string();
        let Some(nv) = n.get(k) else {
            js_push(&mut d, key, "null".into());
            continue;
        };
        if type_of(ov) != type_of(nv) || (truthy(ov) && !truthy(nv)) {
            js_push(&mut d, key, json(nv));
        } else if matches!(ov, Val::Obj(_) | Val::Arr(_)) {
            // one nested level; a removed nested key is `undefined`: counted by _.size, dropped by JSON.stringify
            let mut sub: Vec<(String, String)> = Vec::new();
            let mut size = 0;
            for (sk, sv) in children(ov) {
                let nsv = child(nv, &sk);
                if !is_equal(Some(sv), nsv) {
                    size += 1;
                    if let Some(x) = nsv {
                        js_push(&mut sub, sk, json(x));
                    }
                }
            }
            for (sk, nsv) in children(nv) {
                if child(ov, &sk).is_none() {
                    size += 1;
                    js_push(&mut sub, sk, json(nsv));
                }
            }
            if size > 0 {
                js_push(&mut d, key, write_members(&sub));
            }
        } else if !is_equal(Some(ov), Some(nv)) {
            js_push(&mut d, key, json(nv));
        }
    }
    for (k, nv) in n.iter() {
        if o.get(k).is_none() {
            js_push(&mut d, k.as_str().to_string(), json(nv));
        }
    }
    (!d.is_empty()).then(|| write_members(&d))
}

fn id_of(d: &DocRef) -> &str {
    d.obj.get(engine::atom!("_id")).and_then(Val::as_str).unwrap_or("undefined")
}

/// common/index.js `getDiff(oldData, newData)` (FMT-ROOM-DIFF) as JSON text. Unchanged docs (same `Arc` or
/// same cached JSON) are skipped without diffing.
pub(crate) fn get_diff(old: &[DocRef], new: &[DocRef]) -> String {
    let new_index: HashMap<&str, &DocRef> = new.iter().map(|d| (id_of(d), d)).collect();
    let old_ids: HashSet<&str> = old.iter().map(id_of).collect();
    let mut m: Vec<(String, String)> = Vec::new();
    for o in old {
        let id = id_of(o);
        match new_index.get(id) {
            Some(n) if Arc::ptr_eq(o, n) || o.json() == n.json() => {}
            Some(n) => {
                if let Some(d) = obj_diff(&o.obj, &n.obj) {
                    js_push(&mut m, id.to_string(), d);
                }
            }
            None => js_push(&mut m, id.to_string(), "null".into()),
        }
    }
    for n in new {
        let id = id_of(n);
        if !old_ids.contains(id) {
            js_push(&mut m, id.to_string(), n.json().to_string());
        }
    }
    write_members(&m)
}

/// The rooms.js snapshot payload and the user ids it introduced (`roomUsers`).
pub(crate) fn snapshot(view: &WorldView, r: RoomIdx, u: UserIdx) -> (String, HashSet<Box<str>>) {
    let snap = &view.rooms[r as usize];
    // users of objects (not "2"/"3"), in users-collection order (db.users.find)
    let mut found: Vec<UserIdx> = Vec::new();
    for d in snap.objects.iter() {
        if let Some(id) = crate::str_field(&d.obj, "user") {
            if id != "2" && id != "3" {
                if let Some(x) = crate::user_by_id(view, id) {
                    if !found.contains(&x) {
                        found.push(x);
                    }
                }
            }
        }
    }
    found.sort_unstable();
    let mut users: Vec<(String, String)> = Vec::new();
    let mut known: HashSet<Box<str>> = HashSet::new();
    for x in found {
        let id = view.users[x as usize].id.as_str();
        known.insert(id.into());
        js_push(&mut users, id.to_string(), user_brief(view, x).stringify());
    }
    for (id, name) in BUILTIN {
        known.insert(id.into());
        js_push(&mut users, id.to_string(), format!(r#"{{"_id":"{id}","username":"{name}"}}"#));
    }
    let mut m = vec![("objects".to_string(), get_diff(&[], &snap.objects)), ("users".to_string(), write_members(&users))];
    // `flags: roomFlags && roomFlags.data`: null without a flags doc (the tick message omits it instead)
    let id = view.users[u as usize].id.as_str();
    match snap.flags.iter().find(|f| crate::str_field(f, "user") == Some(id)) {
        None => m.push(("flags".into(), "null".into())),
        Some(f) => {
            if let Some(v) = crate::field(f, "data") {
                let mut s = String::new();
                v.write_json(&mut s);
                m.push(("flags".into(), s));
            }
        }
    }
    m.push(("info".into(), r#"{"mode":"world"}"#.into()));
    (write_members(&m), known)
}

/// `userFlagsData && userFlagsData.data` as JSON text (None = undefined: no flags doc or no `data`).
fn own_flags(view: &WorldView, r: RoomIdx, u: UserIdx) -> Option<String> {
    let id = view.users[u as usize].id.as_str();
    let data = view.rooms[r as usize].flags.iter().find(|f| crate::str_field(f, "user") == Some(id)).and_then(|f| crate::field(f, "data"))?;
    Some(json(data))
}

/// `onSubscribe('room:<name>')` for an authenticated connection.
pub(crate) fn subscribe(p: &Published, conn: &Arc<Conn>, u: UserIdx, name: &str) {
    let view = p.get();
    let Some(r) = room_idx(&view, name) else { return };
    let mut hub = p.hub().lock();
    // a repeat starts over from a new snapshot; a second entry would only send every tick twice
    hub.rooms.entry(name.to_string()).or_default().retain(|s| s.conn != conn.id);
    if hub_limit(&hub, u) > user_limit() {
        let subs = hub.rooms.entry(name.to_string()).or_default();
        subs.push(RoomSub { conn: conn.id, user: u, objects: Arc::from(Vec::new()), users: BUILTIN.iter().map(|(i, _)| (*i).into()).collect() });
        conn.write_event(&format!("err@room:{name}"), "\"subscribe limit reached\"");
        return;
    }
    let (text, known) = snapshot(&view, r, u);
    hub.rooms.entry(name.to_string()).or_default().push(RoomSub { conn: conn.id, user: u, objects: view.rooms[r as usize].objects.clone(), users: known });
    conn.write_event(&format!("room:{name}"), &text);
}

fn hub_limit(hub: &HubState, u: UserIdx) -> u32 {
    hub.users_limit.get(&u).copied().unwrap_or(0)
}

/// `^(W|E)\d+(N|S)\d+$`
fn world_room_name(name: &str) -> bool {
    let b = name.as_bytes();
    let digits = |s: &[u8]| !s.is_empty() && s.iter().all(u8::is_ascii_digit);
    if b.len() < 4 || !matches!(b[0], b'W' | b'E') {
        return false;
    }
    match b[1..].iter().position(|c| matches!(c, b'N' | b'S')) {
        Some(i) => digits(&b[1..1 + i]) && digits(&b[2 + i..]),
        None => false,
    }
}

/// The throttled `roomsDone` handler: per room in shuffled order, the 2-room limit, then one diff per distinct
/// baseline and one message per subscriber.
pub(crate) fn broadcast(hub: &mut HubState, view: &WorldView, rng: &mut util::rng::Rng) {
    hub.users_limit.clear();
    let mut names: Vec<String> = hub.rooms.keys().cloned().collect();
    // _.shuffle (Math.random): which rooms lose the limit is random
    for i in (1..names.len()).rev() {
        names.swap(i, (rng.next_u64() % (i as u64 + 1)) as usize);
    }
    for name in names {
        let mut subs = hub.rooms.remove(&name).unwrap_or_default();
        let mut include = Vec::with_capacity(subs.len());
        for s in &subs {
            let n = hub.users_limit.entry(s.user).or_insert(0);
            *n += 1;
            let ok = *n <= user_limit();
            if !ok {
                if let Some(c) = hub.conns.get(&s.conn) {
                    c.write_event(&format!("err@room:{name}"), "\"subscribe limit reached\"");
                }
            }
            include.push(ok);
        }
        if let (true, Some(r)) = (include.contains(&true), room_idx(view, &name)) {
            let snap = &view.rooms[r as usize];
            let mut diffs: Vec<(Arc<[DocRef]>, String)> = Vec::new();
            let refs = referenced_users(&snap.objects);
            for (s, _) in subs.iter_mut().zip(&include).filter(|(_, ok)| **ok) {
                let diff = match diffs.iter().find(|(b, _)| Arc::ptr_eq(b, &s.objects)) {
                    Some((_, d)) => d.clone(),
                    None => {
                        let d = get_diff(&s.objects, &snap.objects);
                        diffs.push((s.objects.clone(), d.clone()));
                        d
                    }
                };
                let msg = tick_message(hub, view, r, &name, s, &diff, &refs);
                let sent = hub.conns.get(&s.conn).is_some_and(|c| c.write_event(&format!("room:{name}"), &msg));
                // a full queue drops the baseline: the next push is a complete snapshot (§8.1)
                s.objects = if sent { snap.objects.clone() } else { Arc::from(Vec::new()) };
            }
        }
        hub.rooms.insert(name, subs);
    }
}

fn tick_message(hub: &HubState, view: &WorldView, r: RoomIdx, name: &str, s: &mut RoomSub, diff: &str, refs: &[String]) -> String {
    let mut m = vec![("objects".to_string(), diff.to_string())];
    if let Some(data) = own_flags(view, r, s.user) {
        m.push(("flags".into(), data));
    }
    m.push(("gameTime".into(), view.time.to_string()));
    if world_room_name(name) {
        m.push(("info".into(), r#"{"mode":"world"}"#.into()));
    }
    let mut visual = String::new();
    for key in ["", name] {
        if let Some(v) = hub.visuals.get(&(s.user, key.into())) {
            visual.push_str(v);
        }
    }
    m.push(("visual".into(), Json::Str(visual).stringify()));
    // ids not sent to this subscriber yet → db.users.find({_id: {$in}}) (users order); found ones are remembered,
    // missing ones are asked for again next time (not negatively cached), `users` is present if any was unknown
    let unknown: Vec<&str> = refs.iter().map(String::as_str).filter(|id| !s.users.contains(*id)).collect();
    if !unknown.is_empty() {
        let mut found: Vec<UserIdx> = unknown.iter().filter_map(|id| crate::user_by_id(view, id)).collect();
        found.sort_unstable();
        found.dedup();
        let mut users: Vec<(String, String)> = Vec::new();
        for x in found {
            let id = view.users[x as usize].id.as_str();
            s.users.insert(id.into());
            js_push(&mut users, id.to_string(), user_brief(view, x).stringify());
        }
        m.push(("users".into(), write_members(&users)));
    }
    let _ = r;
    write_members(&m)
}

/// The user ids a room's objects name (`user`, `reservation.user`, `sign.user`), unique, in scan order.
fn referenced_users(objects: &[DocRef]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut add = |v: Option<&Val>| {
        if let Some(id) = v.and_then(Val::as_str).filter(|s| !s.is_empty()) {
            if !out.iter().any(|x| x == id) {
                out.push(id.to_string());
            }
        }
    };
    for d in objects {
        add(crate::field(&d.obj, "user"));
        for k in ["reservation", "sign"] {
            if crate::truthy(crate::field(&d.obj, k)) {
                add(crate::field(&d.obj, k).and_then(Val::as_obj).and_then(|o| crate::field(o, "user")));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake;
    use engine::doc::Doc;

    fn doc(seq: u64, j: &str) -> DocRef {
        Doc::new(seq, fake::obj(j))
    }

    /// Cases from FMT-ROOM-DIFF; expected texts are what common/index.js getDiff + JSON.stringify produce
    /// (checked against node with the cases in tests/fixtures/rooms/diff.json).
    #[test]
    fn get_diff_cases() {
        let fx = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/rooms/diff.json")).unwrap();
        let cases = Json::parse(&fx).unwrap();
        let mut n = 0;
        for c in cases.as_array().unwrap() {
            let list = |k: &str| -> Vec<DocRef> { c.get(k).unwrap().as_array().unwrap().iter().enumerate().map(|(i, d)| doc(i as u64, &d.stringify())).collect() };
            let got = get_diff(&list("old"), &list("new"));
            // same members in the same order (our text escapes non-ASCII, JSON.stringify does not)
            let want = Json::parse(c.get("diff").unwrap().as_str().unwrap()).unwrap();
            assert_eq!(Json::parse(&got).unwrap(), want, "case {}: {got}", c.get("name").unwrap());
            n += 1;
        }
        assert!(n >= 10);
    }

    #[test]
    fn snapshot_and_limit() {
        let w = fake::world();
        let v = fake::view(&w);
        let (text, known) = snapshot(&v, 1, fake::ALICE);
        let j = Json::parse(&text).unwrap();
        let keys: Vec<&str> = j.as_object().unwrap().iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["objects", "users", "flags", "info"]);
        let users: Vec<&str> = j.get("users").unwrap().as_object().unwrap().iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(users, ["2", "3", "a11ce0000000001"]);
        assert_eq!(j.get("users").unwrap().get("a11ce0000000001").unwrap().get("username").unwrap().as_str(), Some("alice"));
        assert!(j.get("users").unwrap().get("a11ce0000000001").unwrap().get("money").is_none());
        assert_eq!(j.get("objects").unwrap().as_object().unwrap().len(), 4);
        assert_eq!(j.get("flags").unwrap().as_str(), Some("Flag1~1~2~10~10|Home~5~5~25~26"));
        assert!(known.contains("2") && known.contains("a11ce0000000001"));
        // bob sees no flags; reservation/sign users are not in the snapshot (SOCK-ROOM-SUBSCRIBE GOTCHA)
        let (t3, _) = snapshot(&v, 2, fake::BOB);
        let j3 = Json::parse(&t3).unwrap();
        assert_eq!(j3.get("flags"), Some(&Json::Null), "no flags doc: null");
        assert_eq!(j3.get("users").unwrap().as_object().unwrap().len(), 2);
        assert!(world_room_name("W12N3") && !world_room_name("W1X2") && !world_room_name("sim") && !world_room_name("WN1"));
    }
}
