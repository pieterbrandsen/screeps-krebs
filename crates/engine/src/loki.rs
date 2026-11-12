//! mirror: reference/storage/lib/db.js
//! parity: FAITHFUL
//!
//! Official Loki `db.json` import and export (ARCHITECTURE.md §9, D30). Of db.js only `upgradeDb` (db.js:32-254,
//! STORE-MIGRATIONS) is mirrored here, statement by statement; the rest of db.js is the storage RPC server that the
//! world model replaces. Fixtures: crates/engine/tests/fixtures/loki (`node tools/oracle/loki.js`), which run the
//! official `loadDb` + `upgradeDb` over the same files.
//!
//! Export writes a file the official server loads (Loki collections with `$loki`/`meta`, lazy binary indices,
//! the env keys of ENV-*), plus an env key `chitin` with what only chitin needs to resume exactly (world seed,
//! clock, id counter, sequence numbers as `$loki`). Import reads either; without `chitin` the file is an official
//! DB: ids are marked imported, `$loki` order is creation order.
// ponytail: Json::parse turns a lone surrogate in Memory into U+FFFD; a Memory that holds one loses it on reload.

use crate::constants as C;
use crate::doc::{Atom, Doc, DocRef, Id, Obj, Val};
use crate::ids::IdGen;
use crate::policy::{Compat, Profile, VClock};
use crate::room_name::RoomXY;
use crate::terrain::RoomTerrain;
use crate::world::{
    ActiveSet, CodeState, EnvState, Inbox, MarketState, RoomIdx, RoomMeta, RoomSnapshot, RoomsMeta, TerrainStore, UserIdx, UserState, World,
    WorldView,
};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use util::json::Json;
use util::jsstr::JsStr;

/// A parsed `db.json`: env doc 1 and every other collection's docs in file order, `$loki` and `meta` split off.
pub struct Db {
    /// env doc 1 `data` (None: no env doc, so `upgradeDb` returns at once).
    pub env: Option<Obj>,
    /// env doc 1 `databaseVersion`.
    pub version: Option<f64>,
    pub colls: Vec<(String, Vec<(f64, Obj)>)>,
}

fn s(x: &str) -> Val {
    Val::Str(x.into())
}

fn key(name: &str) -> Atom {
    Atom::intern(name)
}

fn str_of<'a>(o: &'a Obj, k: &str) -> Option<&'a str> {
    o.get(key(k)).and_then(Val::as_str)
}

impl Db {
    pub fn parse(text: &str) -> Result<Db, String> {
        let root = Json::parse(text).map_err(|e| format!("db.json: {e}"))?;
        let colls = root.get("collections").and_then(Json::as_array).ok_or("db.json: no collections")?;
        let mut db = Db { env: None, version: None, colls: Vec::new() };
        for c in colls {
            let name = c.get("name").and_then(Json::as_str).ok_or("db.json: a collection without a name")?;
            let mut docs = Vec::new();
            for d in c.get("data").and_then(Json::as_array).map_or(&[][..], |a| a.as_slice()) {
                let Val::Obj(mut o) = Val::from_trusted_json(d) else { continue };
                let loki = o.remove(key("$loki")).and_then(|v| v.as_f64()).unwrap_or(0.0);
                o.remove(key("meta"));
                docs.push((loki, o));
            }
            if name == "env" {
                if let Some((_, doc)) = docs.iter().find(|(l, _)| *l == 1.0) {
                    db.env = Some(doc.get(key("data")).and_then(Val::as_obj).cloned().unwrap_or_default());
                    db.version = doc.get(key("databaseVersion")).and_then(Val::as_f64);
                }
                continue;
            }
            db.colls.push((name.to_string(), docs));
        }
        Ok(db)
    }

    /// `db.getCollection(name)` (None when the file has no such collection).
    fn coll(&mut self, name: &str) -> Option<&mut Vec<(f64, Obj)>> {
        self.colls.iter_mut().find(|(n, _)| n == name).map(|(_, d)| d)
    }

    fn docs(&self, name: &str) -> &[(f64, Obj)] {
        self.colls.iter().find(|(n, _)| n == name).map_or(&[], |(_, d)| d)
    }
}

// ---- upgradeDb (db.js:32-254) ----

/// `/^[WE][1-9][NS][1-9]$/`
fn inner_room(id: &str) -> bool {
    let b = id.as_bytes();
    b.len() == 4 && matches!(b[0], b'W' | b'E') && (b'1'..=b'9').contains(&b[1]) && matches!(b[2], b'N' | b'S') && (b'1'..=b'9').contains(&b[3])
}

/// `/^[WE]\d*0/` or `/0$/` (the first is not anchored at the number's end: `W105N3` matches).
fn bus_room(id: &str) -> bool {
    let b = id.as_bytes();
    let first = matches!(b.first(), Some(b'W' | b'E')) && {
        let digits = b[1..].iter().take_while(|c| c.is_ascii_digit()).count();
        b[1..1 + digits].contains(&b'0')
    };
    first || id.ends_with('0')
}

/// `/^[WE](\d+)[NS](\d+)$/.exec(id)` → (longitude, latitude) digit strings.
fn lon_lat(id: &str) -> Option<(&str, &str)> {
    let b = id.as_bytes();
    if !matches!(b.first(), Some(b'W' | b'E')) {
        return None;
    }
    let lon = b[1..].iter().take_while(|c| c.is_ascii_digit()).count();
    let rest = &id[1 + lon..];
    if lon == 0 || !matches!(rest.as_bytes().first(), Some(b'N' | b'S')) {
        return None;
    }
    let lat = &rest[1..];
    (!lat.is_empty() && lat.bytes().all(|c| c.is_ascii_digit())).then(|| (&id[1..1 + lon], lat))
}

/// `a + b` in JS for stored values (`undefined` = None, results NaN/±Inf persist as null).
fn js_add(a: Option<&Val>, b: Option<&Val>) -> Option<Val> {
    let v = |x: Option<&Val>| x.map_or(crate::live::V::Undef, crate::live::from_val);
    crate::live::to_val(&crate::processor::js_add(&v(a), &v(b)))
}

/// `o[k] = v` where `v` may be `undefined`: the key then vanishes from the JSON, so it is not stored.
fn put(o: &mut Obj, k: &str, v: Option<Val>) {
    match v {
        Some(v) => o.set(key(k), v),
        None => {
            o.remove(key(k));
        }
    }
}

fn get(o: &Obj, k: &str) -> Option<Val> {
    o.get(key(k)).cloned()
}

/// `{k1: o.f1, k2: o.f2, …}` (undefined values left out).
fn pick(o: &Obj, pairs: &[(&str, &str)]) -> Val {
    let mut r = Obj::new();
    for &(k, f) in pairs {
        put(&mut r, k, get(o, f));
    }
    Val::Obj(r)
}

fn delete(o: &mut Obj, keys: &[&str]) {
    for k in keys {
        o.remove(key(k));
    }
}

fn energy_only(st: &mut Obj) {
    let store = pick(st, &[("energy", "energy")]);
    st.set(key("store"), store);
    let cap = pick(st, &[("energy", "energyCapacity")]);
    st.set(key("storeCapacityResource"), cap);
    delete(st, &["energy", "energyCapacity"]);
}

fn store_only(st: &mut Obj) {
    if let Some(cap) = get(st, "energyCapacity") {
        st.set(key("storeCapacity"), cap);
        delete(st, &["energyCapacity"]);
    }
    st.set(key("store"), Val::Obj(Obj::new()));
    let mut store = Obj::new();
    for r in C::RESOURCES_ALL {
        if let Some(v) = st.remove(key(r.as_str())) {
            store.set(key(r.as_str()), v);
        }
    }
    st.set(key("store"), Val::Obj(store));
}

/// `converters[type]` (db.js:73-138); false for other types.
fn convert(st: &mut Obj) -> bool {
    match str_of(st, "type").unwrap_or("") {
        "spawn" | "extension" | "tower" | "link" => energy_only(st),
        "storage" | "terminal" | "container" | "factory" | "creep" | "powerCreep" | "tombstone" => store_only(st),
        "nuker" => {
            let store = pick(st, &[("energy", "energy"), ("G", "G")]);
            st.set(key("store"), store);
            let cap = pick(st, &[("energy", "energyCapacity"), ("G", "GCapacity")]);
            st.set(key("storeCapacityResource"), cap);
            delete(st, &["energy", "energyCapacity", "G", "GCapacity"]);
        }
        "powerSpawn" => {
            let store = pick(st, &[("energy", "energy"), ("power", "power")]);
            st.set(key("store"), store);
            let cap = pick(st, &[("energy", "energyCapacity"), ("power", "powerCapacity")]);
            st.set(key("storeCapacityResource"), cap);
            delete(st, &["energy", "energyCapacity", "power", "powerCapacity"]);
        }
        "lab" => {
            let Val::Obj(mut store) = pick(st, &[("energy", "energy")]) else { unreachable!() };
            let Val::Obj(mut cap) = pick(st, &[("energy", "energyCapacity")]) else { unreachable!() };
            let truthy = |v: Option<&Val>| v.is_some_and(|v| crate::live::truthy(&crate::live::from_val(v)));
            if truthy(st.get(key("mineralType"))) && truthy(st.get(key("mineralAmount"))) {
                let m = key(&crate::live::to_js_string(&crate::live::from_val(st.get(key("mineralType")).unwrap())));
                put(&mut store, m.as_str(), get(st, "mineralAmount"));
                put(&mut cap, m.as_str(), get(st, "mineralCapacity"));
                st.set(key("store"), Val::Obj(store));
                st.set(key("storeCapacityResource"), Val::Obj(cap));
            } else {
                st.set(key("store"), Val::Obj(store));
                st.set(key("storeCapacityResource"), Val::Obj(cap));
                let sum = js_add(st.get(key("energyCapacity")), st.get(key("mineralCapacity")));
                put(st, "storeCapacity", sum);
            }
            delete(st, &["energy", "energyCapacity", "mineralType", "mineralAmount", "mineralCapacity"]);
        }
        _ => return false,
    }
    true
}

const INVADER_BADGE: &str = r##"{"type":{"path1":"m 60.493413,13.745781 -1.122536,7.527255 -23.302365,-6.118884 -24.097204,26.333431 6.412507,0.949878 -5.161481,19.706217 26.301441,24.114728 1.116562,-7.546193 23.350173,6.122868 24.097202,-26.318478 -6.462307,-0.95785 5.16845,-19.699243 z m -1.58271,10.611118 -0.270923,1.821013 C 57.330986,25.69819 55.969864,25.331543 54.570958,25.072546 Z m -8.952409,4.554029 c 11.653612,0 21.055294,9.408134 21.055294,21.069735 0,11.661603 -9.401682,21.068738 -21.055294,21.068738 -11.65361,0 -21.055297,-9.407135 -21.055297,-21.068738 0,-11.661601 9.401687,-21.069735 21.055297,-21.069735 z M 26.634018,40.123069 c -0.262324,0.618965 -0.494865,1.252967 -0.708185,1.895768 l -0.0508,-0.104656 -0.194228,-0.417627 c 0.261245,-0.385697 0.631962,-0.909531 0.953211,-1.373485 z m 47.391601,17.714764 0.115539,0.237219 0.214148,0.462479 c -0.380159,0.55986 -0.886342,1.281124 -1.3835,1.988466 0.400298,-0.870957 0.752837,-1.767746 1.053813,-2.688164 z M 41.364458,73.812322 c 0.694434,0.251619 1.40261,0.471895 2.123558,0.662817 l -2.303841,0.558165 z","path2":"m 60.857962,24.035953 -6.397566,1.055531 c 6.084137,1.084905 11.78633,4.394548 15.786244,9.746957 5.741405,7.682749 6.465607,17.544704 2.736121,25.67958 1.511089,-2.147013 2.622575,-3.851337 2.622575,-3.851337 l 1.628526,0.241209 c 0.726895,-2.869027 1.004942,-5.843252 0.811775,-8.806053 l 1.185288,-8.634615 -3.768025,-3.072898 -2.908435,-3.21842 c -0.0103,-0.01383 -0.01958,-0.02805 -0.02988,-0.04186 -3.118009,-4.172293 -7.17889,-7.228662 -11.666624,-9.098091 z M 50.001124,37.965163 A 12.020784,12.029027 0 0 0 37.979913,49.994617 12.020784,12.029027 0 0 0 50.001124,62.024074 12.020784,12.029027 0 0 0 62.022337,49.994617 12.020784,12.029027 0 0 0 50.001124,37.965163 Z M 27.019485,39.55693 c -1.481686,2.114179 -2.5658,3.779575 -2.5658,3.779575 l -1.647451,-0.244197 c -0.69707,2.775045 -0.977606,5.64628 -0.81476,8.511019 l -1.22015,8.890775 3.768021,3.072896 3.422394,3.786551 c 2.921501,3.715734 6.608397,6.499915 10.668588,8.29872 l 5.050921,-1.223973 C 38.324728,73.038607 33.383805,69.887984 29.806406,65.100956 28.655972,63.561522 27.71377,61.932905 26.961715,60.249903 L 24.8272,48.359991 c 0.194234,-3.030146 0.935183,-6.015406 2.192285,-8.803061 z"},"color1":"#735252","color2":"#390305","color3":"#ff0d39","flip":false}"##;

/// The Invader badge the v7 migration sets (db.js:186-204).
pub fn invader_badge() -> Val {
    Val::from_trusted_json(&Json::parse(INVADER_BADGE).expect("badge JSON"))
}

/// `upgradeDb()` (db.js:32-254): the v4..v9 migrations by env `databaseVersion`. `now_ms` is `new Date()` of the
/// v5 market-order step. Errors are the TypeErrors vanilla throws (which abort its startup).
pub fn upgrade(db: &mut Db, now_ms: i64) -> Result<(), String> {
    if db.env.is_none() {
        return Ok(());
    }
    let version = |db: &Db| db.version.unwrap_or(0.0);
    let missing = |name: &str| format!("TypeError: Cannot read properties of null (reading 'find') [{name}]");

    if version(db) == 0.0 || version(db) < 4.0 {
        let rooms = db.coll("rooms").ok_or_else(|| missing("rooms"))?;
        for (_, room) in rooms.iter_mut() {
            if str_of(room, "_id").is_some_and(inner_room) {
                room.set(key("bus"), Val::Bool(false));
            }
        }
        let deposit_types = [C::RESOURCE_SILICON, C::RESOURCE_METAL, C::RESOURCE_BIOMASS, C::RESOURCE_MIST];
        for (_, room) in rooms.iter_mut() {
            let Some(id) = str_of(room, "_id").map(str::to_string) else { continue };
            if !bus_room(&id) {
                continue;
            }
            let Some((lon, lat)) = lon_lat(&id) else {
                return Err("TypeError: object null is not iterable (cannot read property Symbol(Symbol.iterator))".into());
            };
            // `(longitude+latitude)%4`: two strings, concatenated
            let n: f64 = format!("{lon}{lat}").parse().unwrap_or(f64::NAN);
            let i = n % 4.0;
            if let Some(t) = (i.fract() == 0.0).then(|| deposit_types.get(i as usize)).flatten() {
                room.set(key("depositType"), s(t));
            } else {
                room.remove(key("depositType"));
            }
        }
        db.version = Some(4.0);
    }

    if version(db) < 5.0 {
        if let Some(pcs) = db.coll("users.power_creeps") {
            for (_, pc) in pcs.iter_mut() {
                store_only(pc);
            }
        }
        let objects = db.coll("rooms.objects").ok_or_else(|| missing("rooms.objects"))?;
        for (_, o) in objects.iter_mut() {
            convert(o);
        }
        let orders = db.coll("market.orders").ok_or_else(|| missing("market.orders"))?;
        for (_, order) in orders.iter_mut() {
            let ts = order.get(key("createdTimestamp")).map(crate::live::from_val);
            if !ts.as_ref().is_some_and(crate::live::truthy) {
                order.set(key("createdTimestamp"), Val::Num(now_ms as f64));
            }
        }
        db.version = Some(5.0);
    }

    if version(db) < 6.0 {
        let objects = db.coll("rooms.objects").ok_or_else(|| missing("rooms.objects"))?;
        for (_, o) in objects.iter_mut() {
            if str_of(o, "type") == Some("powerBank") {
                let store = pick(o, &[("power", "power")]);
                o.set(key("store"), store);
                delete(o, &["power"]);
            }
        }
        db.version = Some(6.0);
    }

    if version(db) < 7.0 {
        let users = db.coll("users").ok_or_else(|| missing("users"))?;
        let Some((_, user)) = users.iter_mut().find(|(_, u)| str_of(u, "_id") == Some("2")) else {
            return Err("TypeError: Cannot set properties of null (setting 'badge')".into());
        };
        user.set(key("badge"), invader_badge());
        db.version = Some(7.0);
    }

    if version(db) < 8.0 {
        // parseInt(envData.data.gameTime)
        let game_time = db.env.as_ref().and_then(|e| e.get(key("gameTime"))).map(|v| parse_int(&crate::live::to_js_string(&crate::live::from_val(v))));
        let game_time = Val::Num(game_time.unwrap_or(f64::NAN));
        let objects = db.coll("rooms.objects").ok_or_else(|| missing("rooms.objects"))?;
        for (_, o) in objects.iter_mut() {
            if !matches!(str_of(o, "type"), Some("spawn" | "invaderCore")) || matches!(o.get(key("spawning")), None | Some(Val::Null)) {
                continue;
            }
            let Some(Val::Obj(spawning)) = o.get_mut(key("spawning")) else { continue };
            let Some(remaining) = spawning.get(key("remainingTime")).cloned() else { continue };
            put(spawning, "spawnTime", js_add(Some(&game_time), Some(&remaining)));
            spawning.remove(key("remainingTime"));
        }
        db.version = Some(8.0);
    }

    if version(db) < 9.0 {
        let rooms = db.coll("rooms").ok_or_else(|| missing("rooms"))?;
        let mut active = Vec::new();
        for (_, room) in rooms.iter_mut() {
            if room.get(key("active")).is_some_and(|v| crate::live::truthy(&crate::live::from_val(v))) {
                active.push(get(room, "_id").unwrap_or(Val::Null));
            }
            room.remove(key("active"));
        }
        db.env.as_mut().unwrap().set(key("activeRooms"), Val::Arr(active));
        db.version = Some(9.0);
    }
    Ok(())
}

/// `parseInt(s)` for a decimal string.
fn parse_int(s: &str) -> f64 {
    let t = s.trim_start();
    let (neg, t) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    let digits: String = t.chars().take_while(char::is_ascii_digit).collect();
    match digits.parse::<f64>() {
        Ok(n) if neg => -n,
        Ok(n) => n,
        Err(_) => f64::NAN,
    }
}

// ---- import ----

pub struct ImportOpts {
    /// World seed when the file has no `chitin` key.
    pub seed: u64,
    pub profile: Profile,
    /// Real now: the v5 migration's timestamp and the clock anchor of an official DB.
    pub now_ms: i64,
    pub nominal_tick_ms: u32,
}

/// What an import left out (counts), for the operator.
#[derive(Default, Debug)]
pub struct ImportReport {
    pub orphan_objects: usize,
    pub pending_intents: usize,
}

/// `db.json` (official or a chitin checkpoint) → a World at its `gameTime`. Runs `upgradeDb` first.
pub fn import(text: &str, opts: &ImportOpts) -> Result<(World, ImportReport), String> {
    let mut db = Db::parse(text)?;
    upgrade(&mut db, opts.now_ms)?;
    build(db, None, opts)
}

fn num_of(o: &Obj, k: &str) -> Option<f64> {
    o.get(key(k)).and_then(Val::as_f64)
}

/// An upgraded `db` → a World. `terrain`: one per `rooms` doc in order (the native store), else from `rooms.terrain`.
pub(crate) fn build(db: Db, terrain: Option<Vec<RoomTerrain>>, opts: &ImportOpts) -> Result<(World, ImportReport), String> {
    let env = db.env.clone().unwrap_or_default();
    let chitin = env.get(key("chitin")).and_then(Val::as_obj).cloned();
    let resume = chitin.is_some();
    let game_time = env.get(key("gameTime")).map_or(1.0, |v| parse_int(&crate::live::to_js_string(&crate::live::from_val(v))));
    let game_time = if game_time.is_finite() && game_time >= 0.0 { game_time as u64 } else { 1 };
    let mut report = ImportReport::default();
    let mut next_seq = 0u64;
    // creation order: `$loki - 1` of a chitin checkpoint, else file order
    let mut seq_of = |loki: f64| {
        let s = if resume && loki >= 1.0 { loki as u64 - 1 } else { next_seq };
        next_seq = next_seq.max(s + 1);
        s
    };

    // users first: room docs derive presence from user ids
    let mut users_docs: Vec<(u64, Obj)> = Vec::new();
    for (loki, doc) in db.docs("users") {
        users_docs.push((seq_of(*loki), doc.clone()));
    }
    let mut user_ids: crate::world::UserIds = Default::default();
    for (i, (_, d)) in users_docs.iter().enumerate() {
        let id = str_of(d, "_id").and_then(Id::new).ok_or_else(|| format!("users: bad _id in {}", Val::Obj(d.clone()).to_json().stringify()))?;
        if user_ids.insert(id, i as UserIdx).is_some() {
            return Err(format!("users: duplicate _id {id}"));
        }
    }
    let by_user = |coll: &str| -> HashMap<UserIdx, Vec<(f64, Obj)>> {
        let mut m: HashMap<UserIdx, Vec<(f64, Obj)>> = HashMap::new();
        for (loki, d) in db.docs(coll) {
            if let Some(&u) = str_of(d, "user").and_then(Id::new).and_then(|id| user_ids.get(&id)) {
                m.entry(u).or_default().push((*loki, d.clone()));
            }
        }
        m
    };
    let mut code = by_user("users.code");
    let mut console = by_user("users.console");
    let mut pcs = by_user("users.power_creeps");
    let mut notes = by_user("users.notifications");
    let mut messages = by_user("users.messages");
    let env_str = |k: &str| env.get(key(k)).map(|v| crate::live::to_js_string(&crate::live::from_val(v)).to_string());

    let mut users = Vec::with_capacity(users_docs.len());
    for (u, (seq, mut doc)) in users_docs.into_iter().enumerate() {
        let u = u as UserIdx;
        let id = str_of(&doc, "_id").and_then(Id::new).unwrap();
        let mut auth = Obj::new();
        for k in crate::world::AUTH_FIELDS {
            if let Some(v) = doc.remove(key(k)) {
                auth.set(key(k), v);
            }
        }
        let branches: Vec<Arc<Obj>> = code.remove(&u).unwrap_or_default().into_iter().map(|(_, d)| Arc::new(d)).collect();
        let active = |k: &str| branches.iter().position(|b| matches!(b.get(key(k)), Some(Val::Bool(true))));
        let code_state = CodeState { active_world: active("activeWorld"), active_sim: active("activeSim"), branches, version: 1 };
        let mut queue: Vec<Arc<Obj>> = console.remove(&u).unwrap_or_default().into_iter().map(|(_, d)| Arc::new(d)).collect();
        queue.reverse(); // ORDER-CONSOLE-COMMANDS: newest first
        let power_creeps: Vec<DocRef> = pcs.remove(&u).unwrap_or_default().into_iter().map(|(l, d)| Doc::new(seq_of(l), d)).collect();
        let mut segments = BTreeMap::new();
        if let Some(Val::Obj(h)) = env.get(key(&format!("memorySegments:{id}"))) {
            for (k, v) in h.iter() {
                if let (Ok(n), Some(text)) = (k.as_str().parse::<u8>(), v.as_str()) {
                    segments.insert(n, JsStr::from_str(text));
                }
            }
        }
        users.push(Arc::new(UserState {
            id,
            seq,
            doc: Arc::new(doc),
            auth: Arc::new(auth),
            code: Arc::new(code_state),
            memory: env_str(&format!("memory:{id}")).map(|m| JsStr::from_str(&m)).unwrap_or_default(),
            memory_version: 0,
            segments: Arc::new(segments),
            public_segments: env_str(&format!("publicMemorySegments:{id}")).unwrap_or_default().into(),
            inter_shard: None,
            console_queue: queue.into(),
            power_creeps: power_creeps.into(),
            inbox: Arc::new(Inbox {
                notifications: notes.remove(&u).unwrap_or_default().into_iter().map(|(_, d)| Arc::new(d)).collect(),
                messages: messages.remove(&u).unwrap_or_default().into_iter().map(|(_, d)| Arc::new(d)).collect(),
            }),
        }));
    }

    // rooms in file order = RoomIdx
    let mut metas = Vec::new();
    let mut index: HashMap<String, RoomIdx> = HashMap::new();
    for (_, d) in db.docs("rooms") {
        let name = str_of(d, "_id").ok_or("rooms: a room without _id")?;
        let xy = RoomXY::parse_engine(name).ok_or_else(|| format!("rooms: bad room name {name:?}"))?;
        if index.insert(name.to_string(), metas.len() as RoomIdx).is_some() {
            return Err(format!("rooms: duplicate room {name}"));
        }
        metas.push(RoomMeta { name: name.into(), xy, removed: false });
    }
    let mut terrains: Vec<Option<RoomTerrain>> = match terrain {
        Some(t) if t.len() == metas.len() => t.into_iter().map(Some).collect(),
        Some(t) => return Err(format!("terrain for {} rooms, {} rooms", t.len(), metas.len())),
        None => vec![None; metas.len()],
    };
    for (_, d) in db.docs("rooms.terrain") {
        if let (Some(&r), Some(t)) = (str_of(d, "room").and_then(|n| index.get(n)), str_of(d, "terrain")) {
            terrains[r as usize] = Some(RoomTerrain::from_db_str(t).ok_or_else(|| format!("rooms.terrain: bad terrain of {}", metas[r as usize].name))?);
        }
    }
    let mut objects: Vec<Vec<DocRef>> = vec![Vec::new(); metas.len()];
    let mut obj_docs: Vec<(u64, RoomIdx, Obj)> = Vec::new();
    for (loki, d) in db.docs("rooms.objects") {
        match str_of(d, "room").and_then(|n| index.get(n)) {
            Some(&r) => obj_docs.push((seq_of(*loki), r, d.clone())),
            None => report.orphan_objects += 1,
        }
    }
    obj_docs.sort_by_key(|(seq, _, _)| *seq);
    for (seq, r, d) in obj_docs {
        objects[r as usize].push(Doc::new(seq, d));
    }
    let mut flags: Vec<Vec<Arc<Obj>>> = vec![Vec::new(); metas.len()];
    for (_, d) in db.docs("rooms.flags") {
        if let Some(&r) = str_of(d, "room").and_then(|n| index.get(n)) {
            flags[r as usize].push(Arc::new(d.clone()));
        }
    }
    report.pending_intents = db.docs("rooms.intents").len() + db.docs("users.intents").len();
    let event_logs = env.get(key("roomEventLog:")).and_then(Val::as_obj).cloned().unwrap_or_default();

    let mut snapshots = Vec::with_capacity(metas.len());
    let mut packed = Vec::with_capacity(metas.len());
    let mut walled = Vec::new();
    for (i, ((_, info), docs)) in db.docs("rooms").iter().zip(objects).enumerate() {
        let name = &metas[i].name;
        let t = terrains[i].take().ok_or_else(|| format!("rooms.terrain: no terrain for room {name}"))?;
        packed.push(t);
        if str_of(info, "status") == Some("out of borders") {
            walled.push(i as RoomIdx);
        }
        let event_log = event_logs.get(key(name)).and_then(Val::as_str).unwrap_or("");
        let map_view = env_str(&format!("mapView:{name}")).unwrap_or_default();
        snapshots.push(Arc::new(RoomSnapshot::from_docs(
            Arc::new(info.clone()),
            docs,
            std::mem::take(&mut flags[i]).into(),
            Arc::from(event_log),
            Arc::from(map_view.as_str()),
            0,
            &user_ids,
        )));
    }

    let meta_val = |k: &str| chitin.as_ref().and_then(|c| c.get(key(k)).cloned());
    let seed = meta_val("worldSeed").and_then(|v| v.as_str().and_then(|s| s.parse().ok())).unwrap_or(opts.seed);
    let clock = match meta_val("clock") {
        Some(Val::Obj(c)) => VClock {
            base_ms: num_of(&c, "baseMs").unwrap_or(opts.now_ms as f64) as i64,
            base_tick: num_of(&c, "baseTick").unwrap_or(game_time as f64) as u64,
            nominal_tick_ms: num_of(&c, "nominalTickMs").unwrap_or(opts.nominal_tick_ms as f64) as u32,
        },
        _ => VClock { base_ms: opts.now_ms, base_tick: game_time, nominal_tick_ms: opts.nominal_tick_ms },
    };
    let profile = meta_val("profile").and_then(|v| v.as_str().and_then(Profile::parse)).unwrap_or(opts.profile);
    let rooms_meta = RoomsMeta::new(metas);
    let mut terrain = TerrainStore::new(packed);
    terrain.set_blob(&rooms_meta, &walled);

    // market (creation order)
    let doc_list = |coll: &str, seq_of: &mut dyn FnMut(f64) -> u64| -> Arc<[DocRef]> { db.docs(coll).iter().map(|(l, d)| Doc::new(seq_of(*l), d.clone())).collect() };
    let orders = doc_list("market.orders", &mut seq_of);
    let transactions = doc_list("transactions", &mut seq_of);
    let plain = |coll: &str| -> Arc<[Arc<Obj>]> { db.docs(coll).iter().map(|(_, d)| Arc::new(d.clone())).collect() };
    let market = MarketState::new(orders, plain("market.stats"), transactions, plain("users.money"), plain("users.resources"));

    let mut w = World::new(seed, clock, Compat::new(profile), rooms_meta, terrain, snapshots, users);
    w.time = game_time;
    w.market = Arc::new(market);
    let tick_rate = env.get(key("tickRate")).and_then(Val::as_f64).map_or(1000, |n| n as u32);
    let paused = env_str("mainLoopPaused").is_some_and(|p| p.trim().parse::<f64>().is_ok_and(|n| n != 0.0));
    w.env = Arc::new(EnvState { paused, tick_rate_ms: tick_rate, last_accessible_update: 0, status_dirty: true });
    let mut active = ActiveSet::default();
    if let Some(Val::Arr(names)) = env.get(key("activeRooms")) {
        for n in names {
            if let Some(&r) = n.as_str().and_then(|n| index.get(n)) {
                active.insert(r);
            }
        }
    }
    w.active = active;
    let mut ids = IdGen::new(seed, profile);
    match meta_val("idCounter").and_then(|v| v.as_f64()) {
        Some(c) => ids.set_counter(c as u64),
        None => {
            for (_, docs) in &db.colls {
                for (_, d) in docs {
                    if let Some(id) = str_of(d, "_id").and_then(Id::new) {
                        ids.mark_imported(id);
                    }
                }
            }
        }
    }
    w.ids = ids;
    w.next_seq = meta_val("nextSeq").and_then(|v| v.as_f64()).map_or(next_seq, |n| (n as u64).max(next_seq));
    if let Some(Val::Arr(m)) = meta_val("mods") {
        w.mods = m.iter().filter_map(|v| v.as_str().map(Box::from)).collect();
    }
    Ok((w, report))
}

// ---- export ----

/// The binary indices each collection has (STORE-INDEXES), saved dirty so Loki rebuilds them lazily.
pub(crate) const COLLECTIONS: &[(&str, &[&str])] = &[
    ("users", &["username"]),
    ("rooms", &[]),
    ("rooms.objects", &["room", "user"]),
    ("rooms.terrain", &["room"]),
    ("rooms.flags", &["room", "user"]),
    ("rooms.intents", &["room"]),
    ("users.code", &["user"]),
    ("users.console", &["user"]),
    ("users.messages", &[]),
    ("users.notifications", &["user"]),
    ("users.money", &["user"]),
    ("users.resources", &["user"]),
    ("users.power_creeps", &["user"]),
    ("users.intents", &[]),
    ("market.orders", &[]),
    ("market.stats", &[]),
    ("transactions", &["user"]),
];

/// One Loki collection: docs are `(loki, doc JSON)` with ascending `loki`.
fn write_collection(out: &mut String, name: &str, indices: &[&str], unique_id: bool, docs: &[(u64, &str)]) {
    use std::fmt::Write;
    out.push_str("{\"name\":");
    out.push_str(&Json::Str(name.into()).stringify());
    out.push_str(",\"data\":[");
    for (i, (loki, json)) in docs.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let body = &json[..json.len() - 1];
        out.push_str(body);
        if body.len() > 1 {
            out.push(',');
        }
        let _ = write!(out, "\"meta\":{{\"revision\":0,\"created\":0,\"version\":0}},\"$loki\":{loki}}}");
    }
    out.push_str("],\"idIndex\":[");
    for (i, (loki, _)) in docs.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(out, "{loki}");
    }
    out.push_str("],\"binaryIndices\":{");
    for (i, ix) in indices.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(out, "\"{ix}\":{{\"name\":\"{ix}\",\"dirty\":true,\"values\":[]}}");
    }
    let max = docs.iter().map(|d| d.0).max().unwrap_or(0);
    let _ = write!(
        out,
        "}},\"constraints\":null,\"uniqueNames\":[{}],\"transforms\":{{}},\"objType\":\"{name}\",\"dirty\":true,\"cachedIndex\":null,\
\"cachedBinaryIndex\":null,\"cachedData\":null,\"adaptiveBinaryIndices\":false,\"transactional\":false,\"cloneObjects\":false,\
\"cloneMethod\":\"parse-stringify\",\"asyncListeners\":false,\"disableChangesApi\":true,\"autoupdate\":false,\"ttl\":null,\
\"maxId\":{max},\"DynamicViews\":[],\"events\":{{\"insert\":[null],\"update\":[null],\"pre-insert\":[],\"pre-update\":[],\"close\":[],\
\"flushbuffer\":[],\"error\":[],\"delete\":[null],\"warning\":[null]}},\"changes\":[]}}",
        if unique_id { "\"_id\"" } else { "" }
    );
}

fn obj_json(o: &Obj) -> String {
    let mut s = String::new();
    Val::Obj(o.clone()).write_json(&mut s);
    s
}

/// `updateTerrainData` (TERRAIN-WORLD-BLOB): base64(zlib(JSON of `{room, terrain}` in blob order)).
fn terrain_data(v: &WorldView) -> String {
    let mut text = String::from("[");
    for (i, (xy, src)) in crate::mapgen::terrain_blob(&v.meta, &v.rooms).into_iter().enumerate() {
        if i > 0 {
            text.push(',');
        }
        let t = src.map_or_else(|| "1".repeat(2500), |r| v.terrain.get(r).to_db_string());
        text.push_str(&format!("{{\"room\":\"{xy}\",\"terrain\":\"{t}\"}}"));
    }
    text.push(']');
    util::base64::encode(&util::deflate::zlib_compress(text.as_bytes()))
}

fn live_rooms(v: &WorldView) -> Vec<RoomIdx> {
    (0..v.rooms.len() as RoomIdx).filter(|&r| !v.meta.rooms[r as usize].removed).collect()
}

/// A user removed through storage (`removeWhere`): its slot stays (UserIdx is stable), nothing shows it.
pub fn is_removed_user(u: &UserState) -> bool {
    u.doc.get(key("$removed")).is_some()
}

/// The env doc 1 `data` keys but the users' (Memory) and `chitin`. `derived`: also the keys the official server
/// reads and `import` rebuilds (accessibleRooms, roomStatusData, terrainData).
fn env_head(v: &WorldView, derived: bool) -> Obj {
    let p = &v.persist;
    let live = live_rooms(v);
    let name = |r: RoomIdx| &*v.meta.rooms[r as usize].name;
    let mut env = Obj::new();
    env.set(key("gameTime"), Val::Num(v.time as f64));
    if derived {
        let accessible: Vec<Json> = live.iter().filter(|&&r| v.access.is_accessible(r)).map(|&r| Json::Str(name(r).into())).collect();
        env.set(key("accessibleRooms"), s(&Json::Arr(accessible).stringify()));
        let mut status = [Vec::new(), Vec::new(), Vec::new()];
        for &r in &live {
            if let Some((st, ts)) = v.access.status.get(&r) {
                let i = match st {
                    crate::world::RoomStatus::Novice => 0,
                    crate::world::RoomStatus::Respawn => 1,
                    _ => 2,
                };
                status[i].push((name(r).to_string(), ts.map_or(Json::Null, |t| Json::Num(t as f64))));
            }
        }
        let [novice, respawn, closed] = status;
        let status = Json::Obj(vec![("novice".into(), Json::Obj(novice)), ("respawn".into(), Json::Obj(respawn)), ("closed".into(), Json::Obj(closed))]);
        env.set(key("roomStatusData"), s(&status.stringify()));
        env.set(key("terrainData"), s(&terrain_data(v)));
    }
    env.set(key("mainLoopPaused"), s(if v.env.paused { "1" } else { "0" }));
    env.set(key("tickRate"), Val::Num(v.env.tick_rate_ms as f64));
    env.set(key("activeRooms"), Val::Arr(p.active.iter().map(|&r| s(name(r))).collect()));
    let mut event_logs = Obj::new();
    for &r in &live {
        let snap = &v.rooms[r as usize];
        if !snap.map_view.is_empty() {
            env.set(key(&format!("mapView:{}", name(r))), s(&snap.map_view));
        }
        if !snap.event_log.is_empty() {
            event_logs.set(key(name(r)), s(&snap.event_log));
        }
    }
    env.set(key("roomEventLog:"), Val::Obj(event_logs));
    env
}

/// What only chitin needs to resume exactly (env key `chitin`).
fn chitin_meta(v: &WorldView, profile: Profile) -> Json {
    let p = &v.persist;
    Json::Obj(vec![
        ("format".into(), Json::Num(1.0)),
        ("worldSeed".into(), Json::Str(p.seed.to_string())),
        ("profile".into(), Json::Str(profile.name().into())),
        (
            "clock".into(),
            Json::Obj(vec![
                ("baseMs".into(), Json::Num(p.clock.base_ms as f64)),
                ("baseTick".into(), Json::Num(p.clock.base_tick as f64)),
                ("nominalTickMs".into(), Json::Num(p.clock.nominal_tick_ms as f64)),
            ]),
        ),
        ("idCounter".into(), Json::Num(p.id_counter as f64)),
        ("nextSeq".into(), Json::Num(p.next_seq as f64)),
        ("mods".into(), Json::Arr(p.mods.iter().map(|m| Json::Str(m.to_string())).collect())),
    ])
}

/// The env keys `import` reads but the users' (native store): `env_data` without the derived keys.
pub(crate) fn env_obj(v: &WorldView, profile: Profile) -> Obj {
    let mut env = env_head(v, false);
    env.set(key("chitin"), Val::from_trusted_json(&chitin_meta(v, profile)));
    env
}

/// A user's env keys (`memory:`, `memorySegments:`, `publicMemorySegments:`) as JSON carries them (a lone
/// surrogate becomes U+FFFD, as `import` reads `env_data`).
pub(crate) fn user_env(u: &UserState) -> Vec<(String, Val)> {
    let mut out = Vec::new();
    if u.memory.utf16_len() > 0 {
        out.push((format!("memory:{}", u.id), Val::Str(u.memory.to_string_lossy().into())));
    }
    if !u.segments.is_empty() {
        let mut segs = Obj::new();
        for (n, seg) in u.segments.iter() {
            segs.set(key(&n.to_string()), Val::Str(seg.to_string_lossy().into()));
        }
        out.push((format!("memorySegments:{}", u.id), Val::Obj(segs)));
    }
    if !u.public_segments.is_empty() {
        out.push((format!("publicMemorySegments:{}", u.id), s(&u.public_segments)));
    }
    out
}

/// The env doc 1 `data` object as JSON text: every modelled ENV-* key, then `chitin` (resume data).
pub fn env_data(v: &WorldView, profile: Profile) -> String {
    let mut env_text = obj_json(&env_head(v, true));
    env_text.pop();
    for u in v.users.iter().filter(|u| !is_removed_user(u)) {
        if u.memory.utf16_len() > 0 {
            env_text.push_str(&format!(",\"memory:{}\":", u.id));
            u.memory.write_json_quoted(&mut env_text);
        }
        if !u.segments.is_empty() {
            env_text.push_str(&format!(",\"memorySegments:{}\":{{", u.id));
            for (i, (n, seg)) in u.segments.iter().enumerate() {
                env_text.push_str(&format!("{}\"{n}\":", if i > 0 { "," } else { "" }));
                seg.write_json_quoted(&mut env_text);
            }
            env_text.push('}');
        }
        if !u.public_segments.is_empty() {
            env_text.push_str(&format!(",\"publicMemorySegments:{}\":", u.id));
            env_text.push_str(&Json::Str(u.public_segments.to_string()).stringify());
        }
    }
    env_text.push_str(",\"chitin\":");
    env_text.push_str(&chitin_meta(v, profile).stringify());
    env_text.push('}');
    env_text
}

/// A doc of `docs`: the world's own, or one built for the collection (users with their auth, terrain).
pub enum DocSrc<'a> {
    Doc(&'a Doc),
    Obj(&'a Obj),
    Own(Obj),
}

impl DocSrc<'_> {
    pub fn obj(&self) -> &Obj {
        match self {
            DocSrc::Doc(d) => &d.obj,
            DocSrc::Obj(o) => o,
            DocSrc::Own(o) => o,
        }
    }

    fn json(&self) -> String {
        match self {
            DocSrc::Doc(d) => d.json().to_string(),
            _ => obj_json(self.obj()),
        }
    }
}

/// One collection's docs as `(loki, doc)` in creation order, as Loki would hold them (None: not a collection).
pub fn docs<'a>(v: &'a WorldView, coll: &str) -> Option<Vec<(u64, DocSrc<'a>)>> {
    let live = live_rooms(v);
    let name = |r: RoomIdx| &*v.meta.rooms[r as usize].name;
    let users = || v.users.iter().filter(|u| !is_removed_user(u));
    let numbered = |docs: Vec<&'a Obj>| docs.into_iter().enumerate().map(|(i, d)| (i as u64 + 1, DocSrc::Obj(d))).collect::<Vec<_>>();
    let seq_docs = |docs: Vec<&'a DocRef>| docs.into_iter().map(|d| (d.seq + 1, DocSrc::Doc(d))).collect::<Vec<_>>();
    let docs = match coll {
        // auth fields back on the doc
        "users" => users()
            .map(|u| {
                let mut d = (*u.doc).clone();
                for (k, val) in u.auth.iter() {
                    d.set(k, val.clone());
                }
                (u.seq + 1, DocSrc::Own(d))
            })
            .collect(),
        "rooms" => live.iter().enumerate().map(|(i, &r)| (i as u64 + 1, DocSrc::Obj(&v.rooms[r as usize].info))).collect(),
        "rooms.objects" => {
            let mut objs: Vec<&DocRef> = live.iter().flat_map(|&r| v.rooms[r as usize].objects.iter()).collect();
            objs.sort_by_key(|d| d.seq);
            seq_docs(objs)
        }
        "rooms.terrain" => live
            .iter()
            .enumerate()
            .map(|(i, &r)| {
                let mut d = Obj::new();
                d.set(key("room"), s(name(r)));
                d.set(key("terrain"), s(&v.terrain.get(r).to_db_string()));
                d.set(key("type"), s("terrain"));
                (i as u64 + 1, DocSrc::Own(d))
            })
            .collect(),
        "rooms.flags" => numbered(live.iter().flat_map(|&r| v.rooms[r as usize].flags.iter().map(|f| &**f)).collect()),
        "users.code" => numbered(users().flat_map(|u| u.code.branches.iter().map(|b| &**b)).collect()),
        "users.console" => numbered(users().flat_map(|u| u.console_queue.iter().rev().map(|c| &**c)).collect()),
        "users.messages" => numbered(users().flat_map(|u| u.inbox.messages.iter().map(|m| &**m)).collect()),
        "users.notifications" => numbered(users().flat_map(|u| u.inbox.notifications.iter().map(|m| &**m)).collect()),
        "users.money" => numbered(v.market.money.iter().map(|m| &**m).collect()),
        "users.resources" => numbered(v.market.resources.iter().map(|m| &**m).collect()),
        "users.power_creeps" => {
            let mut pcs: Vec<&DocRef> = users().flat_map(|u| u.power_creeps.iter()).collect();
            pcs.sort_by_key(|d| d.seq);
            seq_docs(pcs)
        }
        "market.orders" => seq_docs(v.market.orders.iter().collect()),
        "market.stats" => numbered(v.market.stats.iter().map(|m| &**m).collect()),
        "transactions" => seq_docs(v.market.transactions.iter().collect()),
        // transient (pending intents are not kept between ticks) or never written (COL-LEADERBOARD)
        "rooms.intents" | "users.intents" | "leaderboard.power" | "leaderboard.seasons" | "leaderboard.world" => Vec::new(),
        _ => return None,
    };
    Some(docs)
}

/// One collection's docs as `(loki, JSON)` in creation order, as Loki would hold them (None: not a collection).
pub fn collection(v: &WorldView, coll: &str) -> Option<Vec<(u64, String)>> {
    Some(docs(v, coll)?.into_iter().map(|(l, d)| (l, d.json())).collect())
}

/// A world view as a `db.json` the official server loads (and `import` resumes exactly).
pub fn export(v: &WorldView, profile: Profile) -> String {
    let env_doc = format!("{{\"data\":{},\"databaseVersion\":9}}", env_data(v, profile));
    let mut out = String::from("{\"filename\":\"db.json\",\"collections\":[");
    write_collection(&mut out, "env", &[], false, &[(1, &env_doc)]);
    for &(coll, indices) in COLLECTIONS {
        let docs = collection(v, coll).unwrap_or_default();
        let mut docs: Vec<(u64, &str)> = docs.iter().map(|(l, d)| (*l, d.as_str())).collect();
        docs.sort_by_key(|d| d.0);
        out.push(',');
        write_collection(&mut out, coll, indices, true, &docs);
    }
    out.push_str(
        "],\"databaseVersion\":1.5,\"engineVersion\":1.5,\"autosave\":true,\"autosaveInterval\":10000,\"autosaveHandle\":null,\
\"options\":{\"autosave\":true,\"autosaveInterval\":10000,\"serializationMethod\":\"normal\",\"destructureDelimiter\":\"$<\\n\"},\
\"persistenceMethod\":\"fs\",\"persistenceAdapter\":null,\"verbose\":false,\"events\":{\"init\":[null],\"loaded\":[],\"flushChanges\":[],\
\"close\":[],\"changes\":[],\"warning\":[]},\"ENV\":\"NODEJS\"}",
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regexes() {
        assert!(inner_room("W1N1") && !inner_room("W10N1") && !inner_room("W0N1"));
        assert!(bus_room("W105N3") && bus_room("E3N10") && bus_room("W0N5") && !bus_room("W5N5"));
        assert_eq!(lon_lat("W10N5"), Some(("10", "5")));
        assert_eq!(lon_lat("W10N5x"), None);
        assert_eq!(parse_int(" 42abc"), 42.0);
    }
}
