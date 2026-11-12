//! The storage collections and env over the world, for the CLI's `storage` (CLI-STORAGE) and JS mods: reads are
//! the docs `loki::collection` gives (what Loki would hold), writes are whole documents computed by the caller
//! (db.js `updateDocument` runs in JS, as in vanilla's storage process) and applied here in P0.
//! `rooms.terrain`, `rooms.intents`, `users.intents` and `leaderboard.*` are read-only; rooms are created and
//! removed through `map.*` only. A removed user keeps its UserIdx slot (`loki::is_removed_user`).

use crate::bulk::DbOp;
use crate::doc::{Atom, Id, Obj, Val};
use crate::loki::is_removed_user;
use crate::processor::global::{apply, exact_room, Ops};
use crate::world::{CodeState, EnvState, Inbox, RoomSnapshot, UserIdx, UserState, World};
use std::sync::Arc;
use util::jsstr::JsStr;

/// One write of a collection.
pub enum Write {
    /// The document with this `_id` becomes `doc` (`_id` kept).
    Replace(Box<str>, Obj),
    /// `_id` is generated unless the doc has one.
    Insert(Obj),
    Remove(Box<str>),
}

fn key(k: &str) -> Atom {
    Atom::intern(k)
}

fn str_of<'a>(o: &'a Obj, k: &str) -> Option<&'a str> {
    o.get(key(k)).and_then(Val::as_str)
}

fn id_of(o: &Obj) -> Option<Id> {
    str_of(o, "_id").filter(|s| !s.is_empty()).and_then(Id::new)
}

/// genId for an insert: the doc's own string `_id` if it has a non-empty one, else a fresh one (db.js genId: when
/// `!obj._id`). ponytail: a truthy non-string `_id` (an upsert copying `{_id: {$in: …}}`) is replaced too; vanilla
/// would store the object.
fn ensure_id(w: &mut World, o: &mut Obj) -> Id {
    match id_of(o) {
        Some(id) => id,
        None => {
            let id = w.ids.next();
            o.set(key("_id"), Val::Str(id.as_str().into()));
            id
        }
    }
}

fn user_of(w: &World, o: &Obj) -> Option<UserIdx> {
    str_of(o, "user").and_then(Id::new).and_then(|id| w.user_ids.get(&id).copied()).filter(|&u| !is_removed_user(&w.users[u as usize]))
}

fn clone_user(u: &UserState) -> UserState {
    crate::processor::clone_user(u)
}

/// Applies `writes` to collection `coll`; returns the inserted docs (with `_id`), as Loki's insert does.
pub fn write(w: &mut World, coll: &str, writes: Vec<Write>) -> Result<Vec<Obj>, String> {
    let mut inserted = Vec::new();
    let mut ops = Vec::new();
    for wr in writes {
        ops.push(match wr {
            Write::Replace(id, doc) => DbOp::Replace(Id::new(&id).ok_or("bad _id")?, doc),
            Write::Remove(id) => DbOp::Remove(Id::new(&id).ok_or("bad _id")?),
            Write::Insert(mut doc) => {
                ensure_id(w, &mut doc);
                inserted.push(doc.clone());
                DbOp::Insert(doc)
            }
        });
    }
    let read_only = |c: &str| Err(format!("collection {c} is read-only in Krebs"));
    let mut g = Ops::default();
    match coll {
        "rooms.objects" => {
            if let Some(DbOp::Insert(d)) = ops.iter().find(|op| matches!(op, DbOp::Insert(d) if str_of(d, "room").and_then(|n| exact_room(w, n)).is_none())) {
                return Err(format!("rooms.objects: no room {:?}", str_of(d, "room").unwrap_or("undefined")));
            }
            g.objects = ops;
        }
        "rooms" => {
            if ops.iter().any(|op| !matches!(op, DbOp::Replace(..))) {
                return Err("rooms are created and removed with map.generateRoom / map.removeRoom".into());
            }
            g.rooms = ops;
            // a status or openTime may have changed: TICK-ACCESSIBLE recomputes at its next run
            env_mut(w).status_dirty = true;
        }
        "market.orders" => g.market_orders = ops,
        "market.stats" => g.market_stats = ops,
        "transactions" => g.transactions = ops,
        "users.money" => g.users_money = ops,
        "users.resources" => g.users_resources = ops,
        "users.power_creeps" => g.users_power_creeps = ops,
        "users" => {
            users(w, ops);
            return Ok(inserted);
        }
        "users.code" | "users.console" | "users.notifications" | "users.messages" => {
            per_user(w, coll, ops);
            return Ok(inserted);
        }
        "rooms.flags" => {
            flags(w, ops);
            return Ok(inserted);
        }
        _ => return read_only(coll),
    }
    apply(w, g);
    Ok(inserted)
}

/// `password`/`salt` (and screeps-krebs's API tokens, `authTokens`) live in `UserState::auth` (never sent to clients);
/// the rest is the doc.
fn split_auth(mut doc: Obj) -> (Obj, Obj) {
    let mut auth = Obj::new();
    for k in crate::world::AUTH_FIELDS {
        if let Some(v) = doc.remove(key(k)) {
            auth.set(key(k), v);
        }
    }
    (doc, auth)
}

fn users(w: &mut World, ops: Vec<DbOp>) {
    for op in ops {
        match op {
            DbOp::Replace(id, doc) => {
                let Some(&u) = w.user_ids.get(&id) else { continue };
                let (doc, auth) = split_auth(doc);
                let next = UserState { doc: Arc::new(doc), auth: Arc::new(auth), ..clone_user(&w.users[u as usize]) };
                w.users[u as usize] = Arc::new(next);
            }
            DbOp::Insert(doc) => {
                let id = id_of(&doc).expect("ensure_id ran");
                if w.user_ids.contains_key(&id) {
                    continue; // Loki: a duplicate unique `_id` aborts the insert
                }
                let (doc, auth) = split_auth(doc);
                let seq = w.next_seq;
                w.next_seq += 1;
                w.users.push(Arc::new(UserState {
                    id,
                    seq,
                    doc: Arc::new(doc),
                    auth: Arc::new(auth),
                    code: Arc::new(CodeState { branches: Vec::new(), active_world: None, active_sim: None, version: 1 }),
                    memory: JsStr::from_str(""),
                    memory_version: 0,
                    segments: Arc::default(),
                    public_segments: "".into(),
                    inter_shard: None,
                    console_queue: Arc::from(Vec::new()),
                    power_creeps: Arc::from(Vec::new()),
                    inbox: Arc::new(Inbox::default()),
                }));
            }
            DbOp::Remove(id) => {
                let Some(&u) = w.user_ids.get(&id) else { continue };
                let mut tomb = Obj::new();
                tomb.set(key("_id"), Val::Str(id.as_str().into()));
                tomb.set(key("$removed"), Val::Bool(true));
                let next = UserState {
                    doc: Arc::new(tomb),
                    auth: Arc::new(Obj::new()),
                    code: Arc::new(CodeState { branches: Vec::new(), active_world: None, active_sim: None, version: w.users[u as usize].code.version + 1 }),
                    memory: JsStr::from_str(""),
                    memory_version: w.users[u as usize].memory_version + 1,
                    ..clone_user(&w.users[u as usize])
                };
                w.users[u as usize] = Arc::new(next);
            }
            _ => {}
        }
    }
    // ids and usernames (a removed user leaves both maps)
    w.reindex();
    let removed: Vec<Id> = w.users.iter().filter(|u| is_removed_user(u)).map(|u| u.id).collect();
    if !removed.is_empty() {
        let ids = Arc::make_mut(&mut w.user_ids);
        for id in &removed {
            ids.remove(id);
        }
    }
}

/// users.code / users.console / users.notifications / users.messages: per-user lists, found by `_id` (replace,
/// remove) or by the doc's `user` (insert).
fn per_user(w: &mut World, coll: &str, ops: Vec<DbOp>) {
    let list = |s: &UserState| -> Vec<Arc<Obj>> {
        match coll {
            "users.code" => s.code.branches.clone(),
            "users.console" => s.console_queue.to_vec(),
            "users.notifications" => s.inbox.notifications.clone(),
            _ => s.inbox.messages.clone(),
        }
    };
    let set = |s: &mut UserState, docs: Vec<Arc<Obj>>| match coll {
        "users.code" => {
            let active = |k: &str| docs.iter().position(|b| matches!(b.get(key(k)), Some(Val::Bool(true))));
            // any change of a user's branches recreates its VM (the code version is a counter)
            s.code = Arc::new(CodeState { active_world: active("activeWorld"), active_sim: active("activeSim"), branches: docs, version: s.code.version + 1 });
        }
        "users.console" => s.console_queue = docs.into(),
        "users.notifications" => s.inbox = Arc::new(Inbox { notifications: docs, messages: s.inbox.messages.clone() }),
        _ => s.inbox = Arc::new(Inbox { notifications: s.inbox.notifications.clone(), messages: docs }),
    };
    let find = |w: &World, id: &Id| -> Option<(usize, usize)> {
        w.users.iter().enumerate().find_map(|(u, s)| list(s).iter().position(|d| id_of(d).as_ref() == Some(id)).map(|i| (u, i)))
    };
    for op in ops {
        let (u, docs) = match op {
            DbOp::Replace(id, doc) => {
                let Some((u, i)) = find(w, &id) else { continue };
                let mut docs = list(&w.users[u]);
                docs[i] = Arc::new(doc);
                (u, docs)
            }
            DbOp::Remove(id) => {
                let Some((u, i)) = find(w, &id) else { continue };
                let mut docs = list(&w.users[u]);
                docs.remove(i);
                (u, docs)
            }
            DbOp::Insert(doc) => {
                let Some(u) = user_of(w, &doc) else { continue };
                let mut docs = list(&w.users[u as usize]);
                if coll == "users.console" {
                    docs.insert(0, Arc::new(doc)); // ORDER-CONSOLE-COMMANDS: newest first
                } else {
                    docs.push(Arc::new(doc));
                }
                (u as usize, docs)
            }
            _ => continue,
        };
        let mut s = clone_user(&w.users[u]);
        set(&mut s, docs);
        w.users[u] = Arc::new(s);
    }
}

/// rooms.flags: per-room lists, the room from the doc's `room` (a replace may move a doc).
fn flags(w: &mut World, ops: Vec<DbOp>) {
    let find = |w: &World, id: &Id| -> Option<(usize, usize)> {
        w.rooms.iter().enumerate().find_map(|(r, s)| s.flags.iter().position(|d| id_of(d).as_ref() == Some(id)).map(|i| (r, i)))
    };
    let mut drafts: std::collections::BTreeMap<usize, Vec<Arc<Obj>>> = std::collections::BTreeMap::new();
    for op in ops {
        let draft = |drafts: &mut std::collections::BTreeMap<usize, Vec<Arc<Obj>>>, w: &World, r: usize| -> Vec<Arc<Obj>> {
            drafts.remove(&r).unwrap_or_else(|| w.rooms[r].flags.to_vec())
        };
        match op {
            DbOp::Replace(id, doc) => {
                let Some((r, i)) = find(w, &id) else { continue };
                let mut docs = draft(&mut drafts, w, r);
                let dest = str_of(&doc, "room").and_then(|n| exact_room(w, n)).map(|d| d as usize).unwrap_or(r);
                if dest == r {
                    docs[i] = Arc::new(doc);
                    drafts.insert(r, docs);
                } else {
                    docs.remove(i);
                    drafts.insert(r, docs);
                    let mut other = draft(&mut drafts, w, dest);
                    other.push(Arc::new(doc));
                    drafts.insert(dest, other);
                }
            }
            DbOp::Remove(id) => {
                let Some((r, i)) = find(w, &id) else { continue };
                let mut docs = draft(&mut drafts, w, r);
                docs.remove(i);
                drafts.insert(r, docs);
            }
            DbOp::Insert(doc) => {
                let Some(r) = str_of(&doc, "room").and_then(|n| exact_room(w, n)).map(|r| r as usize) else { continue };
                let mut docs = draft(&mut drafts, w, r);
                docs.push(Arc::new(doc));
                drafts.insert(r, docs);
            }
            _ => {}
        }
    }
    let user_ids = w.user_ids.clone();
    for (r, docs) in drafts {
        let prev = &w.rooms[r];
        let next = RoomSnapshot::from_docs(prev.info.clone(), prev.objects.to_vec(), docs.into(), prev.event_log.clone(), prev.map_view.clone(), prev.processed_at, &user_ids);
        w.set_room(r as u32, Arc::new(next));
    }
}

// ---- env (ENV-*) ----

/// What `env.set(key, value)` does to the world. Keys the world models are applied (and persisted with it);
/// the derived ones (`accessibleRooms`, `roomStatusData`, `terrainData`, `mapView:*`, `roomEventLog:`) are
/// refused; any other key is the caller's (kept in memory by the CLI host).
///
/// screeps-krebs: `gameTime` sets the world's clock, as vanilla's next tick reads it from env (the UCS stage lanes
/// move a world to its stage's tick this way, paused). The virtual wall clock is re-anchored at the new tick, so it
/// does not jump: vanilla's `Date.now()` does not move with the game time either.
pub fn env_set(w: &mut World, name: &str, value: &Val) -> Result<bool, String> {
    let user = |w: &World, prefix: &str| name.strip_prefix(prefix).and_then(Id::new).and_then(|id| w.user_ids.get(&id).copied());
    let text = |v: &Val| crate::live::to_js_string(&crate::live::from_val(v)).to_string();
    if name == "gameTime" {
        let n = crate::live::to_number(&crate::live::from_val(value));
        if !(n.is_finite() && n >= 1.0 && n.fract() == 0.0) {
            return Err(format!("env key gameTime: {n} is not a tick"));
        }
        let ms = w.clock.at(w.time);
        w.clock.base_ms = ms;
        w.clock.base_tick = n as u64;
        w.time = n as u64;
    } else if name == "mainLoopPaused" {
        // `+value`: the main loop reads it as a number
        let paused = crate::live::to_number(&crate::live::from_val(value));
        env_mut(w).paused = paused != 0.0 && !paused.is_nan();
    } else if name == "tickRate" {
        let ms = crate::live::to_number(&crate::live::from_val(value));
        env_mut(w).tick_rate_ms = if ms.is_finite() && ms > 0.0 { ms as u32 } else { 0 };
    } else if let Some(u) = user(w, "memory:") {
        let mut s = clone_user(&w.users[u as usize]);
        s.memory = JsStr::from_str(&text(value));
        s.memory_version += 1;
        w.users[u as usize] = Arc::new(s);
    } else if let Some(u) = user(w, "publicMemorySegments:") {
        let mut s = clone_user(&w.users[u as usize]);
        s.public_segments = text(value).into();
        w.users[u as usize] = Arc::new(s);
    } else if let Some(u) = user(w, "memorySegments:") {
        let mut s = clone_user(&w.users[u as usize]);
        let mut segs = std::collections::BTreeMap::new();
        if let Val::Obj(h) = value {
            for (k, v) in h.iter() {
                if let Ok(n) = k.as_str().parse::<u8>() {
                    segs.insert(n, JsStr::from_str(&text(v)));
                }
            }
        }
        s.segments = Arc::new(segs);
        w.users[u as usize] = Arc::new(s);
    } else if name == "activeRooms" {
        if let Val::Arr(names) = value {
            for n in names {
                if let Some(r) = n.as_str().and_then(|n| exact_room(w, n)) {
                    w.active.insert(r);
                }
            }
        }
    } else if matches!(name, "accessibleRooms" | "roomStatusData" | "terrainData" | "roomEventLog:") || name.starts_with("mapView:") {
        return Err(format!("env key {name} is derived from the world in Krebs"));
    } else {
        return Ok(false);
    }
    Ok(true)
}

fn env_mut(w: &mut World) -> &mut EnvState {
    if Arc::get_mut(&mut w.env).is_none() {
        let e = &*w.env;
        w.env = Arc::new(EnvState { paused: e.paused, tick_rate_ms: e.tick_rate_ms, last_accessible_update: e.last_accessible_update, status_dirty: e.status_dirty });
    }
    Arc::get_mut(&mut w.env).unwrap()
}

/// `env.del(key)` on a modelled key: Memory and segments become empty, `mainLoopPaused` unpaused; false for a
/// key the world does not model.
pub fn env_del(w: &mut World, name: &str) -> Result<bool, String> {
    let empty = if name.starts_with("memorySegments:") { Val::Obj(Obj::new()) } else { Val::Str("".into()) };
    match name {
        "mainLoopPaused" => env_set(w, name, &Val::Str("0".into())),
        "tickRate" | "activeRooms" => Ok(false),
        _ => env_set(w, name, &empty),
    }
}

/// The users a vanilla world starts with (storage/db.original.json after `upgradeDb`): Invader `'2'` (its v7
/// badge), Source Keeper `'3'` and the system user `Screeps` (USER-SPECIAL-IDS), whom respawns hand ruins to.
pub fn add_default_users(w: &mut World) {
    let doc = |text: &str| match Val::from_trusted_json(&util::json::Json::parse(text).expect("user JSON")) {
        Val::Obj(o) => o,
        _ => unreachable!(),
    };
    let mut invader = doc(r#"{"_id":"2","username":"Invader","usernameLower":"invader","gcl":0,"cpu":0,"active":1,"cpuAvailable":10000}"#);
    invader.set(key("badge"), crate::loki::invader_badge());
    let keeper = doc(r#"{"_id":"3","username":"Source Keeper","usernameLower":"source keeper","gcl":0,"cpu":0,"active":1,"cpuAvailable":10000}"#);
    let system = doc(r##"{"username":"Screeps","usernameLower":"screeps","gcl":0,"cpu":0,"active":false,"cpuAvailable":0,"badge":{"type":12,"color1":"#999999","color2":"#999999","color3":"#999999","flip":false,"param":26}}"##);
    let _ = write(w, "users", vec![Write::Insert(invader), Write::Insert(keeper), Write::Insert(system)]);
}
