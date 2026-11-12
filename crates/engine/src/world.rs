//! The in-memory world: rooms, users, market, env, indexes and the published view
//! (ARCHITECTURE.md §3.4–3.8, FROZEN). Bodies: WP01.
//!
//! Beyond the FROZEN items this file holds the world services the other packages build on: `World::new` /
//! `reindex` / `set_room` (keep `WorldIndex` in step with snapshot replacements), `derive_room` (presence,
//! user refs and global-stage flags of a room's docs), `RoomsMeta::new` (grid + world size),
//! `AccessSnapshot::build` (TICK-ACCESSIBLE, driver/lib/index.js:490-524) and the room blob (§6.6).

use crate::doc::{DocRef, Id, Obj, Val};
use crate::ids;
use crate::live;
use crate::policy;
use crate::room_name::RoomXY;
use crate::terrain::RoomTerrain;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap, HashMap};
use std::sync::{Arc, OnceLock};
use util::jsstr::JsStr;

// ---- §3.4 World, rooms, terrain ----

/// Room creation order = `rooms` insertion order (ORDER-GAME-ROOMS).
pub type RoomIdx = u32;
/// `users` insertion order; NPC users "2"/"3" are ordinary entries.
pub type UserIdx = u32;

pub struct World {
    /// env gameTime
    pub time: u64,
    /// worldSeed (RNG-SEED)
    pub seed: u64,
    pub clock: VClock,
    pub compat: Arc<policy::Compat>,
    /// CFG-ENGINE-DEFAULTS (cpuMaxPerTick, cpuBucketSize, …)
    pub config: Arc<EngineConfig>,
    pub meta: Arc<RoomsMeta>,
    pub terrain: Arc<TerrainStore>,
    pub access: Arc<AccessSnapshot>,
    /// By RoomIdx.
    pub rooms: Vec<Arc<RoomSnapshot>>,
    /// By UserIdx.
    pub users: Vec<Arc<UserState>>,
    pub user_ids: Arc<UserIds>,
    /// usernameLower
    pub usernames: Arc<HashMap<Box<str>, UserIdx>>,
    pub market: Arc<MarketState>,
    pub env: Arc<EnvState>,
    pub active: ActiveSet,
    /// INT-MANUAL, for the next P2: (room, user, object id, intents).
    pub manual_intents: Vec<(RoomIdx, UserIdx, Box<str>, Obj)>,
    /// users.intents; consumed by P5.
    pub global_intents: Vec<GlobalIntentsDoc>,
    pub index: WorldIndex,
    pub ids: ids::IdGen,
    pub next_seq: u64,
    /// Resolved mod order (persisted).
    pub mods: Arc<[Box<str>]>,
    /// `view()`'s room slices (build a World with `World::new`).
    view_rooms: std::sync::Mutex<ViewRooms>,
}

/// Double buffer of published room slices: the one not published last is patched in place once no reader holds
/// it, so `view()` costs O(rooms) pointer compares plus O(changed) refcount updates, not 65 k scattered atomic
/// increments (≈ 1.5 ms on the dev box).
#[derive(Default)]
struct ViewRooms {
    spare: Option<Arc<[Arc<RoomSnapshot>]>>,
    last: Option<Arc<[Arc<RoomSnapshot>]>>,
}

/// CLOCK-VIRTUAL lives in `policy` (§2.7); re-exported here as §3.4 lists it.
pub use crate::policy::VClock;

/// Map topology; replaced as a whole on map edits (world gen, CLI generateRoom/removeRoom).
pub struct RoomsMeta {
    /// Bounding box, engine coords (room_name::RoomXY).
    pub x0: i32,
    pub y0: i32,
    pub w: u32,
    pub h: u32,
    /// w*h cells: RoomIdx + 1, 0 = no room.
    pub grid: Vec<u32>,
    /// By RoomIdx; removed rooms keep a tombstone slot.
    pub rooms: Vec<RoomMeta>,
    /// common::calc_world_size incl. BUG-WORLDSIZE.
    pub world_size: u32,
}

pub struct RoomMeta {
    pub name: Box<str>,
    pub xy: RoomXY,
    pub removed: bool,
}

impl RoomsMeta {
    /// The grid over the bounding box of the live rooms (`rooms` in RoomIdx order) and
    /// `calcWorldSize` over their names (no rooms: `-Infinity`, stored as 0).
    pub fn new(rooms: Vec<RoomMeta>) -> RoomsMeta {
        let live = || rooms.iter().enumerate().filter(|(_, r)| !r.removed);
        let (mut x0, mut y0, mut x1, mut y1) = (0, 0, -1, -1);
        for (i, (_, r)) in live().enumerate() {
            let RoomXY { x, y } = r.xy;
            (x0, y0, x1, y1) = if i == 0 { (x, y, x, y) } else { (x0.min(x), y0.min(y), x1.max(x), y1.max(y)) };
        }
        let (w, h) = ((x1 - x0 + 1) as u32, (y1 - y0 + 1) as u32);
        let mut grid = vec![0; w as usize * h as usize];
        for (i, r) in live() {
            grid[(r.xy.y - y0) as usize * w as usize + (r.xy.x - x0) as usize] = i as u32 + 1;
        }
        let world_size = crate::common::calc_world_size(live().map(|(_, r)| &*r.name)) as u32; // -Inf saturates to 0
        RoomsMeta { x0, y0, w, h, grid, rooms, world_size }
    }

    pub fn idx(&self, xy: RoomXY) -> Option<RoomIdx> {
        let (dx, dy) = (xy.x as i64 - self.x0 as i64, xy.y as i64 - self.y0 as i64);
        if dx < 0 || dy < 0 || dx >= self.w as i64 || dy >= self.h as i64 {
            return None;
        }
        self.grid[(dy * self.w as i64 + dx) as usize].checked_sub(1)
    }

    /// RoomXY::parse_engine + idx, and the stored name must equal `name`: vanilla finds rooms by exact `_id`
    /// (`rooms.find({_id})`, terrain `roomOffsets[name]`), so `w1n1` or `W1N1x` are not W1N1.
    pub fn by_name(&self, name: &str) -> Option<RoomIdx> {
        let r = self.idx(RoomXY::parse_engine(name)?)?;
        (&*self.rooms[r as usize].name == name).then_some(r)
    }
}

pub struct TerrainStore {
    /// 625 B per room (engine::terrain).
    packed: Vec<RoomTerrain>,
    /// Row-major codes, built on first use.
    raw: Vec<OnceLock<Box<[u8; 2500]>>>,
    /// mapgrid.js t/r/b/l plain-tile counts.
    exits: Vec<OnceLock<[u8; 4]>>,
    /// The terrain blob view (`set_blob`): rooms walled there, the extra all-wall ring rooms, the entry count.
    walled: Vec<bool>,
    ring: std::collections::HashSet<Box<str>>,
    blob_entries: Option<usize>,
}

/// A terrain blob entry of 2500 × '1'.
static WALLS: [u8; 2500] = [1; 2500];

impl TerrainStore {
    /// One packed terrain per RoomIdx; the unpacked and exit caches start empty.
    pub fn new(packed: Vec<RoomTerrain>) -> TerrainStore {
        let n = packed.len();
        TerrainStore {
            packed,
            raw: (0..n).map(|_| OnceLock::new()).collect(),
            exits: (0..n).map(|_| OnceLock::new()).collect(),
            walled: Vec::new(),
            ring: Default::default(),
            blob_entries: None,
        }
    }

    /// The runtime's terrain blob as `updateTerrainData` (backend/lib/cli/map.js:661-695, TERRAIN-WORLD-BLOB)
    /// builds it from the live rooms of `meta`: the rooms in `walled` (status 'out of borders' when it runs) read as
    /// all wall through `raw`, and each room adds the names one step further from the origin in x and in y
    /// (`/(W|E)(\d+)(N|S)(\d+)/`, same hemisphere) as all-wall entries when no room has them. Without this call
    /// the blob is exactly the rooms.
    pub fn set_blob(&mut self, meta: &RoomsMeta, walled: &[RoomIdx]) {
        let live: Vec<&RoomMeta> = meta.rooms.iter().filter(|m| !m.removed).collect();
        let names: std::collections::HashSet<&str> = live.iter().map(|m| &*m.name).collect();
        let mut ring = std::collections::HashSet::new();
        for m in &live {
            let Some((h, x, v, y)) = blob_name_parts(&m.name) else { continue };
            for name in [format!("{h}{}{v}{y}", plus_one(x)), format!("{h}{x}{v}{}", plus_one(y))] {
                if !names.contains(name.as_str()) {
                    ring.insert(name.into_boxed_str());
                }
            }
        }
        self.walled = vec![false; self.packed.len()];
        for &r in walled {
            self.walled[r as usize] = true;
        }
        self.raw = (0..self.packed.len()).map(|_| OnceLock::new()).collect();
        self.exits = (0..self.packed.len()).map(|_| OnceLock::new()).collect();
        self.blob_entries = Some(live.len() + ring.len());
        self.ring = ring;
    }

    /// The blob terrain of a name no room has (the walled ring), for `new Room.Terrain(name)` / the pathfinder.
    pub fn blob_only(&self, name: &str) -> Option<&'static [u8; 2500]> {
        self.ring.contains(name).then_some(&WALLS)
    }

    pub fn get(&self, r: RoomIdx) -> &RoomTerrain {
        &self.packed[r as usize]
    }

    /// Pathfinder terrain callback; source of isolate copies (the blob view: walled rooms are all wall).
    pub fn raw(&self, r: RoomIdx) -> &[u8; 2500] {
        if self.walled.get(r as usize) == Some(&true) {
            return &WALLS;
        }
        self.raw[r as usize].get_or_init(|| Box::new(self.packed[r as usize].raw_buffer()))
    }

    /// `WorldMapGrid._buildGridData` (driver/lib/runtime/mapgrid.js:68-120) for one room: `[t, r, b, l]` = tiles
    /// with raw code exactly 0 on the y=0, x=49, y=49 and x=0 edges, corners included (swamp exits do not count).
    /// Vanilla stores only the non-zero sides, for accessible rooms only; that filter is the reader's.
    pub fn exit_counts(&self, r: RoomIdx) -> [u8; 4] {
        *self.exits[r as usize].get_or_init(|| {
            let t = self.raw(r);
            let count = |f: &dyn Fn(usize) -> usize| (0..50).filter(|&i| t[f(i)] == 0).count() as u8;
            [count(&|i| i), count(&|i| i * 50 + 49), count(&|i| 49 * 50 + i), count(&|i| i * 50)]
        })
    }

    /// Vanilla staticTerrainDataSize / 2^20 (heap limit formula, user-vm.js:30): 2500 B per blob entry (rooms and
    /// ring after `set_blob`, else one per terrain).
    pub fn heap_mb(&self) -> f64 {
        (self.blob_entries.unwrap_or(self.packed.len()) * 2500) as f64 / 1024.0 / 1024.0
    }
}

/// The first `(W|E)(\d+)(N|S)(\d+)` match of a name, digits as written.
fn blob_name_parts(name: &str) -> Option<(char, &str, char, &str)> {
    let b = name.as_bytes();
    (0..b.len()).find_map(|i| {
        if b[i] != b'W' && b[i] != b'E' {
            return None;
        }
        let xs = &name[i + 1..];
        let xl = xs.bytes().take_while(u8::is_ascii_digit).count();
        let v = *xs.as_bytes().get(xl).filter(|v| **v == b'N' || **v == b'S')?; // ASCII, so `xl + 1` is a char boundary
        let ys = &xs[xl + 1..];
        let yl = ys.bytes().take_while(u8::is_ascii_digit).count();
        (xl > 0 && yl > 0).then(|| (b[i] as char, &xs[..xl], v as char, &ys[..yl]))
    })
}

/// `+digits + 1` as JS writes it.
fn plus_one(digits: &str) -> String {
    util::num::to_js_string(util::num::string_to_number(digits) + 1.0)
}

/// TICK-ACCESSIBLE output (21-tick cadence, rebuilt only when dirty).
pub struct AccessSnapshot {
    pub version: u64,
    /// Bitset by RoomIdx at vclock(T+1).
    pub accessible: Vec<u64>,
    /// Non-normal rooms: novice/respawn/closed + timestamp.
    pub status: HashMap<RoomIdx, (RoomStatus, Option<i64>)>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RoomStatus {
    Normal,
    Closed,
    Novice,
    Respawn,
}

impl AccessSnapshot {
    /// `updateAccessibleRoomsList` + `updateRoomStatusData` (driver/lib/index.js:490-524) over the live rooms'
    /// `rooms` docs at `now` (= vclock(T+1), CLK-ROOM-STATUS). Accessible: `status === 'normal'` and
    /// `(!openTime || openTime < now)`. Status: rooms the Loki `$or` query selects (`novice`/`respawnArea`/
    /// `openTime` `$gt now`, or `status === 'out of borders'`), classified by the JS `>` chain; an
    /// out-of-borders room is closed with timestamp `Infinity` (`None`, JSON `null`).
    pub fn build(meta: &RoomsMeta, rooms: &[Arc<RoomSnapshot>], now: i64, version: u64) -> AccessSnapshot {
        let now_f = now as f64;
        let mut accessible = vec![0u64; rooms.len().div_ceil(64)];
        let mut status = HashMap::new();
        for (r, snap) in rooms.iter().enumerate() {
            if meta.rooms.get(r).is_none_or(|m| m.removed) {
                continue;
            }
            let info = &snap.info;
            let field = |k| info.get(k);
            let status_is = |s: &str| matches!(field(crate::atom!("status")), Some(Val::Str(v)) if &**v == s);
            let open_time = field(crate::atom!("openTime"));
            if room_accessible(info, now) {
                accessible[r / 64] |= 1 << (r % 64);
            }
            let (novice, respawn) = (field(crate::atom!("novice")), field(crate::atom!("respawnArea")));
            if loki_gt(novice, now_f) || loki_gt(respawn, now_f) || loki_gt(open_time, now_f) || status_is("out of borders") {
                let entry = if js_number(novice) > now_f {
                    (RoomStatus::Novice, timestamp(novice))
                } else if js_number(respawn) > now_f {
                    (RoomStatus::Respawn, timestamp(respawn))
                } else if js_number(open_time) > now_f {
                    (RoomStatus::Closed, timestamp(open_time))
                } else {
                    (RoomStatus::Closed, None)
                };
                status.insert(r as RoomIdx, entry);
            }
        }
        AccessSnapshot { version, accessible, status }
    }

    /// Note: the snapshot is taken at vclock(T+1) on a 21-tick cadence; the global stage uses `room_accessible`
    /// on the live `rooms` doc instead, as vanilla's getInterRoom re-queries every tick.
    pub fn is_accessible(&self, r: RoomIdx) -> bool {
        self.accessible.get(r as usize / 64).is_some_and(|w| w >> (r % 64) & 1 == 1)
    }
}

fn truthy(v: Option<&Val>) -> bool {
    v.is_some_and(|v| live::truthy(&live::from_val(v)))
}

/// ToNumber(ToPrimitive(v)); absent = undefined = NaN.
fn js_number(v: Option<&Val>) -> f64 {
    v.map_or(f64::NAN, |v| live::to_number(&live::from_val(v)))
}

/// The stored timestamp (a number in every vanilla writer).
fn timestamp(v: Option<&Val>) -> Option<i64> {
    Some(js_number(v) as i64)
}

/// lokijs 1.5 `gtHelper(prop, now, false)` (the `$gt` of `rooms.find`): undefined/null/booleans/"" never
/// compare greater; anything whose Number() is NaN does.
fn loki_gt(v: Option<&Val>, now: f64) -> bool {
    match v {
        None | Some(Val::Null) | Some(Val::Bool(_)) => false,
        Some(Val::Str(s)) if s.is_empty() => false,
        v => {
            let n = js_number(v);
            n.is_nan() || n > now
        }
    }
}

/// One room at the end of some tick. Immutable; replaced, never mutated.
pub struct RoomSnapshot {
    /// `rooms` doc (= processor roomInfo).
    pub info: Arc<Obj>,
    /// rooms.objects of this room, ascending seq.
    pub objects: Arc<[DocRef]>,
    /// rooms.flags docs {_id,user,room,data}, creation order.
    pub flags: Arc<[Arc<Obj>]>,
    /// Last processed tick's eventLog JSON (stale while inactive).
    pub event_log: Arc<str>,
    pub map_view: Arc<str>,
    pub processed_at: u64,
    /// Derived.
    pub presence: Arc<[(UserIdx, Presence)]>,
    /// Users named by docs here (user, reservation.user, sign.user).
    pub user_refs: Arc<[UserIdx]>,
    pub has_movers: bool,
    pub has_terminal_send: bool,
    pub has_power_creeps: bool,
    /// §6.6, built by the first viewer.
    blob: OnceLock<RoomBlob>,
}

impl RoomSnapshot {
    /// P4, world generation, import and recovery all build a snapshot from a draft; the blob cache starts
    /// empty.
    pub fn new(d: crate::processor::RoomDraft, processed_at: u64) -> RoomSnapshot {
        RoomSnapshot {
            info: d.info,
            objects: d.objects.into(),
            flags: d.flags,
            event_log: d.event_log,
            map_view: d.map_view,
            processed_at,
            presence: d.presence.into(),
            user_refs: d.user_refs.into(),
            has_movers: d.has_movers,
            has_terminal_send: d.has_terminal_send,
            has_power_creeps: d.has_power_creeps,
            blob: OnceLock::new(),
        }
    }

    /// A snapshot whose derived fields (`derive_room`) are computed from `objects`.
    pub fn from_docs(
        info: Arc<Obj>,
        objects: Vec<DocRef>,
        flags: Arc<[Arc<Obj>]>,
        event_log: Arc<str>,
        map_view: Arc<str>,
        processed_at: u64,
        user_ids: &UserIds,
    ) -> RoomSnapshot {
        let d = derive_room(&objects, user_ids);
        let draft = crate::processor::RoomDraft {
            info,
            objects,
            flags,
            event_log,
            map_view,
            presence: d.presence,
            user_refs: d.user_refs,
            has_movers: d.has_movers,
            has_terminal_send: d.has_terminal_send,
            has_power_creeps: d.has_power_creeps,
        };
        RoomSnapshot::new(draft, processed_at)
    }

    /// The room blob, built by the first viewer from the cached doc JSON and shared by every later one.
    pub fn blob(&self) -> &RoomBlob {
        self.blob.get_or_init(|| {
            let docs_len: usize = self.objects.iter().map(|d| d.json().len() + 1).sum();
            let mut s = String::with_capacity(docs_len + self.objects.len() * 8 + 256);
            s.push_str("{\"room\":");
            write_obj_ascii(&mut s, &self.info);
            let info_end = s.len() as u32;
            s.push_str(",\"seq\":[");
            for (i, d) in self.objects.iter().enumerate() {
                if i > 0 {
                    s.push(',');
                }
                util::num::write_js_string(&mut s, d.seq as f64);
            }
            s.push_str("],\"objects\":[");
            let mut offsets = Vec::with_capacity(self.objects.len() + 1);
            for (i, d) in self.objects.iter().enumerate() {
                if i > 0 {
                    s.push(',');
                }
                offsets.push(s.len() as u32);
                s.push_str(d.json());
            }
            s.push(']');
            offsets.push(s.len() as u32);
            s.push('}');
            assert!(s.len() <= u32::MAX as usize, "room blob over 4 GiB");
            RoomBlob { json: Arc::from(s.into_bytes()), offsets: offsets.into(), info_end, docs: self.objects.clone() }
        })
    }
}

/// ASCII `{"room":<rooms doc>,"seq":[s0,…],"objects":[d0,d1,…]}`; `offsets` has n+1 entries: the start of doc i,
/// and `offsets[n]` = position of the closing `]`; doc i = `json[offsets[i] .. offsets[i+1]-1]`.
/// Precisely: `offsets[n]` is one past the closing `]` (the `]` stands where the next separator would), so the
/// slice formula holds for the last doc too. The rooms doc is `json[8 .. info_end]`; `docs` are the snapshot's docs
/// in blob order (empty for a hand-made blob), for the runtime's doc reuse (§6.6).
#[derive(Clone)]
pub struct RoomBlob {
    pub json: Arc<[u8]>,
    pub offsets: Arc<[u32]>,
    pub info_end: u32,
    pub docs: Arc<[DocRef]>,
}

/// One user's footprint in one room (VIS-ROOMS, ORDER-RUNTIME-ROOMIDS).
#[derive(Clone, Default, PartialEq, Debug)]
pub struct Presence {
    /// Docs with user == uid (incl. sites, tombstones).
    pub objects: u32,
    /// Of those, not flag/constructionSite.
    pub vision: u32,
    pub first_vision_seq: u64,
    /// (observer seq, observeRoom as stored, may be "null").
    pub observe: Option<(u64, Box<str>)>,
}

/// The derived fields of a room snapshot, from its docs (ascending seq).
pub struct RoomDerived {
    /// By UserIdx ascending.
    pub presence: Vec<(UserIdx, Presence)>,
    /// Ascending.
    pub user_refs: Vec<UserIdx>,
    pub has_movers: bool,
    pub has_terminal_send: bool,
    pub has_power_creeps: bool,
}

/// `users._id` → index (FxHash: user ids are made by the server).
pub type UserIds = util::hash::FxHashMap<Id, UserIdx>;

/// What data.js and the global stage read off a room's docs, for docs whose ids are known users:
/// * presence (data.js:103-127): every doc whose `user` is the user's id string counts in `objects`; those not
///   of type `flag`/`constructionSite` in `vision` (first one's seq kept); the first observer keeps its seq and
///   `""+observeRoom` (`"null"`, `"undefined"` included). One observer per room is the CONTROLLER_STRUCTURES cap;
///   a second one (only through edited data) is not tracked.
/// * user refs (data.js:180-189): `user`, and for controllers `reservation.user` and `sign.user`; a superset of
///   vanilla's set for own docs, which only an owned-and-reserved controller could tell apart (unobservable:
///   `runtimeData.users` is read by id only).
/// * movers (`type` creep/powerCreep with `interRoom` neither null nor undefined, driver index.js:432), terminals
///   with a truthy `send` (global-intents/market.js:71), spawned power creeps.
pub fn derive_room(objects: &[DocRef], user_ids: &UserIds) -> RoomDerived {
    let uid = |v: Option<&Val>| match v {
        Some(Val::Str(s)) => Id::new(s).and_then(|id| user_ids.get(&id).copied()),
        _ => None,
    };
    let mut presence: BTreeMap<UserIdx, Presence> = BTreeMap::new();
    let mut refs = BTreeSet::new();
    let (mut has_movers, mut has_terminal_send, mut has_power_creeps) = (false, false, false);
    for d in objects {
        let o = &d.obj;
        let kind = d.kind();
        if let Some(u) = uid(o.get(crate::atom!("user"))) {
            refs.insert(u);
            let p = presence.entry(u).or_default();
            p.objects += 1;
            if kind != "flag" && kind != "constructionSite" {
                if p.vision == 0 {
                    p.first_vision_seq = d.seq;
                }
                p.vision += 1;
                if kind == "observer" && p.observe.is_none() {
                    let target = match o.get(crate::atom!("observeRoom")) {
                        None => "undefined".into(),
                        Some(v) => live::to_js_string(&live::from_val(v)).as_ref().into(),
                    };
                    p.observe = Some((d.seq, target));
                }
            }
        }
        if kind == "controller" {
            for k in [crate::atom!("reservation"), crate::atom!("sign")] {
                if let Some(Val::Obj(sub)) = o.get(k) {
                    refs.extend(uid(sub.get(crate::atom!("user"))));
                }
            }
        }
        match kind {
            "creep" | "powerCreep" if !matches!(o.get(crate::atom!("interRoom")), None | Some(Val::Null)) => has_movers = true,
            "terminal" if truthy(o.get(crate::atom!("send"))) => has_terminal_send = true,
            _ => {}
        }
        has_power_creeps |= kind == "powerCreep";
    }
    RoomDerived { presence: presence.into_iter().collect(), user_refs: refs.into_iter().collect(), has_movers, has_terminal_send, has_power_creeps }
}

#[derive(Default)]
pub struct ActiveSet {
    bits: Vec<u64>,
    list: Vec<RoomIdx>,
}

impl ActiveSet {
    /// No duplicates; insertion order kept (ACT-SET).
    pub fn insert(&mut self, r: RoomIdx) {
        let w = r as usize / 64;
        if self.bits.len() <= w {
            self.bits.resize(w + 1, 0);
        }
        if self.bits[w] >> (r % 64) & 1 == 0 {
            self.bits[w] |= 1 << (r % 64);
            self.list.push(r);
        }
    }

    pub fn contains(&self, r: RoomIdx) -> bool {
        self.bits.get(r as usize / 64).is_some_and(|w| w >> (r % 64) & 1 == 1)
    }

    /// Empties the set (P3 start).
    pub fn take(&mut self) -> Vec<RoomIdx> {
        for &r in &self.list {
            self.bits[r as usize / 64] &= !(1 << (r % 64));
        }
        std::mem::take(&mut self.list)
    }

    /// Current members in insertion order.
    pub fn list(&self) -> &[RoomIdx] {
        &self.list
    }
}

/// A world mutation from the backend, CLI or server isolate; run on the tick thread in P0 (also while paused).
pub type CommandFn = Box<dyn FnOnce(&mut World) -> Result<util::json::Json, String> + Send>;

pub struct Command {
    pub apply: CommandFn,
    pub reply: Option<std::sync::mpsc::SyncSender<Result<util::json::Json, String>>>,
}

/// CFG-ENGINE-DEFAULTS as typed fields (reference/driver/lib/index.js:19-33): one pub field per vanilla
/// config.engine key chitin reads.
#[derive(Clone)]
pub struct EngineConfig {
    /// mainLoopMinDuration (1000)
    pub main_loop_min_duration_ms: u32,
    /// mainLoopResetInterval (5000)
    pub main_loop_reset_interval_ms: u32,
    /// cpuMaxPerTick (500)
    pub cpu_max_per_tick: u32,
    /// cpuBucketSize (10000)
    pub cpu_bucket_size: u32,
    /// historyChunkSize (20)
    pub history_chunk_size: u32,
    /// backend socketUpdateThrottle (200)
    pub socket_update_throttle_ms: u32,
    /// Whether `saveRoomHistory` has a listener (the history mod). Vanilla always builds `objectsToHistory`;
    /// without a listener nothing can observe it, so the processor skips it (POST-HISTORY).
    pub history: bool,
    /// Built-in cron jobs that never run (`engine.disabledCronjobs`, chitin's own, default none).
    pub disabled_cronjobs: Vec<String>,
}

impl Default for EngineConfig {
    /// The vanilla defaults listed on each field.
    fn default() -> EngineConfig {
        EngineConfig {
            main_loop_min_duration_ms: 1000,
            main_loop_reset_interval_ms: 5000,
            cpu_max_per_tick: 500,
            cpu_bucket_size: 10000,
            history_chunk_size: 20,
            socket_update_throttle_ms: 200,
            history: false,
            disabled_cronjobs: Vec::new(),
        }
    }
}

// ---- §3.5 Users and per-user state ----

/// The user fields kept in `UserState::auth`, never sent to clients: the password hash and salt, and screeps-krebs's
/// long-lived API tokens (`authTokens`, `[{token, full, created, description?}]`, screepsmod-auth's createAuthToken).
pub const AUTH_FIELDS: [&str; 3] = ["password", "salt", "authTokens"];

pub struct UserState {
    pub id: Id,
    pub seq: u64,
    /// `users` doc (11 COL-USERS): cpu, cpuAvailable, gcl, power, money, resources, rooms, active, badge,
    /// activeSegments, lastUsedCpu, …
    pub doc: Arc<Obj>,
    /// Password hash/salt, steam id: never sent to clients.
    pub auth: Arc<Obj>,
    pub code: Arc<CodeState>,
    /// env `memory:<uid>`; version bumps on every write.
    pub memory: JsStr,
    pub memory_version: u64,
    /// `memorySegments:<uid>`
    pub segments: Arc<BTreeMap<u8, JsStr>>,
    /// `publicMemorySegments:<uid>` ("1,2,3")
    pub public_segments: Arc<str>,
    /// mmo profile only.
    pub inter_shard: Option<JsStr>,
    /// users.console, ORDER-CONSOLE-COMMANDS (newest-first).
    pub console_queue: Arc<[Arc<Obj>]>,
    /// users.power_creeps, ORDER-POWER-CREEPS.
    pub power_creeps: Arc<[DocRef]>,
    /// users.notifications + users.messages.
    pub inbox: Arc<Inbox>,
}

pub struct CodeState {
    /// users.code docs.
    pub branches: Vec<Arc<Obj>>,
    pub active_world: Option<usize>,
    pub active_sim: Option<usize>,
    /// ++ on any change of the active world branch.
    pub version: u64,
}

#[derive(Default)]
pub struct Inbox {
    pub notifications: Vec<Arc<Obj>>,
    pub messages: Vec<Arc<Obj>>,
}

// ---- §3.6 Market, env, time ----

pub struct MarketState {
    /// market.orders, creation order; prices ×1000.
    pub orders: Arc<[DocRef]>,
    /// market.stats (calcMarketStats).
    pub stats: Arc<[Arc<Obj>]>,
    /// Capped by purgeTransactions semantics.
    pub transactions: Arc<[DocRef]>,
    /// users.money / users.resources history.
    pub money: Arc<[Arc<Obj>]>,
    pub resources: Arc<[Arc<Obj>]>,
    /// Per resource ordinal + "all", built lazily.
    orders_json: Vec<OnceLock<Arc<str>>>,
    history_json: Vec<OnceLock<Arc<str>>>,
    /// Per-user transaction and order lists (data.js:138-139,227) and active orders by id, built on first use.
    index: OnceLock<MarketIndex>,
}

/// Indices into `transactions` / `orders`, keyed by the string `sender`, `recipient` and `user` fields, and the
/// `orders.all` entry of each id.
#[derive(Default)]
struct MarketIndex {
    outgoing: HashMap<Arc<str>, Vec<u32>>,
    incoming: HashMap<Arc<str>, Vec<u32>>,
    orders: HashMap<Arc<str>, Vec<u32>>,
    by_id: HashMap<Arc<str>, u32>,
}

/// Transactions per direction (data.js:138-139 `limit: 100`).
const TRANSACTIONS_PER_USER: usize = 100;

impl MarketState {
    /// The lazy JSON caches (one per resource ordinal + "all") start empty.
    pub fn new(orders: Arc<[DocRef]>, stats: Arc<[Arc<Obj>]>, transactions: Arc<[DocRef]>, money: Arc<[Arc<Obj>]>, resources: Arc<[Arc<Obj>]>) -> MarketState {
        let n = crate::constants::ResourceType::COUNT + 1;
        MarketState {
            orders,
            stats,
            transactions,
            money,
            resources,
            orders_json: (0..n).map(|_| OnceLock::new()).collect(),
            history_json: (0..n).map(|_| OnceLock::new()).collect(),
            index: OnceLock::new(),
        }
    }

    /// `transactions.findEx({sender|recipient: user}, {sort: {time: -1}, limit: 100})` (data.js:138-139): the
    /// unindexed field scans in collection order, then Loki's stable `simplesort` by `time` descending.
    pub fn transactions_of(&self, user: &str, incoming: bool) -> impl Iterator<Item = &DocRef> + '_ {
        let ix = self.index();
        let map = if incoming { &ix.incoming } else { &ix.outgoing };
        map.get(user).into_iter().flatten().map(|&i| &self.transactions[i as usize])
    }

    /// `market.orders.find({user})` (data.js:227): no index, collection order.
    pub fn orders_of(&self, user: &str) -> impl Iterator<Item = &DocRef> + '_ {
        self.index().orders.get(user).into_iter().flatten().map(|&i| &self.orders[i as usize])
    }

    /// One pass per MarketState version instead of a scan per user per tick (O(users × transactions)) or per
    /// `getOrderById` call.
    fn index(&self) -> &MarketIndex {
        self.index.get_or_init(|| {
            let mut ix = MarketIndex::default();
            // Loki `$eq` against the string user id is strict: only string fields can match.
            let file = |map: &mut HashMap<Arc<str>, Vec<u32>>, v: Option<&Val>, i: usize| {
                if let Some(Val::Str(u)) = v {
                    map.entry(u.clone()).or_default().push(i as u32);
                }
            };
            for (i, t) in self.transactions.iter().enumerate() {
                file(&mut ix.outgoing, t.obj.get(crate::atom!("sender")), i);
                file(&mut ix.incoming, t.obj.get(crate::atom!("recipient")), i);
            }
            for (i, o) in self.orders.iter().enumerate() {
                file(&mut ix.orders, o.obj.get(crate::atom!("user")), i);
                if o.obj.get(crate::atom!("active")) == Some(&Val::Bool(true)) {
                    ix.by_id.insert(str_of(o.obj.get(crate::atom!("_id"))), i as u32); // the last one wins
                }
            }
            let time = |i: &u32| self.transactions[*i as usize].obj.get(crate::atom!("time"));
            for list in ix.outgoing.values_mut().chain(ix.incoming.values_mut()) {
                list.sort_by(|a, b| loki_cmp(time(b), time(a))); // stable: same-time rows keep collection order
                list.truncate(TRANSACTIONS_PER_USER);
            }
            ix
        })
    }

    /// No orders, stats, transactions or history.
    pub fn empty() -> MarketState {
        MarketState::new(Arc::new([]), Arc::new([]), Arc::new([]), Arc::new([]), Arc::new([]))
    }

    /// `runtimeData.market.orders[resource]` (data.js:31-48 getCachedMarketOrders) as JSON: the orders with
    /// `active === true`, each with `id = ""+_id` set and `_id` deleted, keyed by id under `""+resourceType` and
    /// under `all`; a repeated id keeps its first position and takes the last order. `None` = no such key
    /// (`all` always exists). Built once per MarketState version for `all` and the RESOURCES_ALL types.
    pub fn orders_json(&self, resource: &str) -> Option<Arc<str>> {
        let build = || {
            let all = resource == "all";
            let picked = self.orders.iter().filter(|o| o.obj.get(crate::atom!("active")) == Some(&Val::Bool(true)));
            let picked: Vec<&DocRef> = picked.filter(|o| all || &*str_of(o.obj.get(crate::atom!("resourceType"))) == resource).collect();
            if picked.is_empty() && !all {
                return Arc::from("");
            }
            let mut ids: Vec<(Arc<str>, &DocRef)> = Vec::with_capacity(picked.len());
            let mut pos = HashMap::new();
            for o in picked {
                let id = str_of(o.obj.get(crate::atom!("_id")));
                match pos.get(&id) {
                    Some(&i) => ids[i] = (id, o),
                    None => {
                        pos.insert(id.clone(), ids.len());
                        ids.push((id, o));
                    }
                }
            }
            ids.sort_by_key(|(id, _)| crate::doc::array_index(id).map_or((1, 0), |i| (0, i))); // JS key order
            let mut s = String::from("{");
            for (i, (id, o)) in ids.iter().enumerate() {
                if i > 0 {
                    s.push(',');
                }
                quote_ascii(&mut s, id);
                s.push(':');
                let mut order = o.obj.clone();
                order.set(crate::atom!("id"), Val::Str(id.clone()));
                order.remove(crate::atom!("_id"));
                write_obj_ascii(&mut s, &order);
            }
            s.push('}');
            Arc::from(s)
        };
        let json = match market_slot(resource) {
            Some(i) => self.orders_json[i].get_or_init(build).clone(),
            None => build(),
        };
        (!json.is_empty()).then_some(json)
    }

    /// `runtimeData.market.orders.all[id]` (Game.market.getOrderById) as JSON.
    pub fn order_json(&self, id: &str) -> Option<String> {
        let order = &self.orders[*self.index().by_id.get(id)? as usize];
        let mut o = order.obj.clone();
        o.set(crate::atom!("id"), Val::Str(id.into()));
        o.remove(crate::atom!("_id"));
        let mut s = String::new();
        write_obj_ascii(&mut s, &o);
        Some(s)
    }

    /// `runtimeData.market.history[resource]` (data.js:51-75): the market.stats docs without `_id` (and a truthy
    /// `meta`/`$loki`), in collection order, filed under `""+resourceType` and `all`. `None` = no such key.
    /// Rebuilt per MarketState version instead of vanilla's 3 h runner cache (CLK-RUNNER-CACHE).
    pub fn history_json(&self, resource: &str) -> Option<Arc<str>> {
        let build = || {
            // `result[rt].push(i); result.all.push(i)`: a doc whose resourceType is "all" lands in `all` twice.
            let all = resource == "all";
            let picked: Vec<&Arc<Obj>> = self
                .stats
                .iter()
                .flat_map(|h| {
                    let rt = str_of(h.get(crate::atom!("resourceType")));
                    let n = if all { 1 + (&*rt == "all") as usize } else { (&*rt == resource) as usize };
                    std::iter::repeat_n(h, n)
                })
                .collect();
            if picked.is_empty() && !all {
                return Arc::from("");
            }
            let mut s = String::from("[");
            for (i, h) in picked.into_iter().enumerate() {
                if i > 0 {
                    s.push(',');
                }
                let mut h = (**h).clone();
                h.remove(crate::atom!("_id"));
                for k in [crate::atom!("meta"), crate::atom!("$loki")] {
                    if truthy(h.get(k)) {
                        h.remove(k);
                    }
                }
                write_obj_ascii(&mut s, &h);
            }
            s.push(']');
            Arc::from(s)
        };
        let json = match market_slot(resource) {
            Some(i) => self.history_json[i].get_or_init(build).clone(),
            None => build(),
        };
        (!json.is_empty()).then_some(json)
    }
}

/// Cache slot of a market key: RESOURCES_ALL ordinal, `all` last; other keys are built uncached.
fn market_slot(resource: &str) -> Option<usize> {
    let n = crate::constants::ResourceType::COUNT;
    if resource == "all" {
        return Some(n);
    }
    resource.parse::<crate::constants::ResourceType>().ok().map(|r| r.index())
}

/// lokijs 1.5 `sortHelper` (ltHelper/gtHelper, ascending) over stored values, as a total order: undefined/null,
/// false, true, "", then values whose `Number()` is a number (by it, -0 = 0), then the rest by their string
/// (UTF-16 units). Total, so `sort_by` cannot panic on odd data.
fn loki_cmp(a: Option<&Val>, b: Option<&Val>) -> std::cmp::Ordering {
    use std::cmp::Ordering::Equal;
    if let (Some(Val::Num(x)), Some(Val::Num(y))) = (a, b) {
        return x.partial_cmp(y).unwrap_or(Equal); // every vanilla writer stores numbers
    }
    let key = |v: Option<&Val>| -> (u8, f64, Option<Arc<str>>) {
        match v {
            None | Some(Val::Null) => (1, 0.0, None),
            Some(Val::Bool(b)) => (3 + *b as u8, 0.0, None),
            Some(Val::Str(s)) if s.is_empty() => (5, 0.0, None),
            Some(v) => match js_number(Some(v)) {
                n if n.is_nan() => (10, 0.0, Some(str_of(Some(v)))),
                n => (9, n, None),
            },
        }
    };
    let ((ra, na, sa), (rb, nb, sb)) = (key(a), key(b));
    let units = |s: &Option<Arc<str>>| s.as_deref().unwrap_or("").encode_utf16().collect::<Vec<u16>>();
    ra.cmp(&rb).then(na.partial_cmp(&nb).unwrap_or(Equal)).then_with(|| units(&sa).cmp(&units(&sb))) // JS `<` on strings
}

/// `""+v` (property-key form); absent = "undefined".
fn str_of(v: Option<&Val>) -> Arc<str> {
    match v {
        Some(Val::Str(s)) => s.clone(),
        Some(v) => live::to_js_string(&live::from_val(v)),
        None => Arc::from("undefined"),
    }
}

pub struct EnvState {
    pub paused: bool,
    pub tick_rate_ms: u32,
    pub last_accessible_update: u64,
    pub status_dirty: bool,
}

impl Default for EnvState {
    /// Running, CFG-TICKRATE 1000 ms, TICK-ACCESSIBLE due on the first tick.
    fn default() -> EnvState {
        EnvState { paused: false, tick_rate_ms: 1000, last_accessible_update: 0, status_dirty: true }
    }
}

pub struct GlobalIntentsDoc {
    pub user: UserIdx,
    pub from_api: bool,
    pub arrival: u32,
    pub intents: Obj,
}

// ---- §3.8 Derived indexes and published views ----

/// Tick thread only; never persisted; rebuilt on load; updated in P4/P5/crons/P0 from changed docs only.
/// Construct with `WorldIndex::build` (or `default`); keep it current with `World::set_room` /
/// `WorldIndex::update_room`.
#[derive(Default)]
pub struct WorldIndex {
    /// By UserIdx (Arc: shared with WorldView, COW).
    pub presence: Vec<Arc<BTreeMap<RoomIdx, Presence>>>,
    /// Every rooms.objects doc (intent routing, API lookups).
    pub objects: HashMap<Id, ObjLoc>,
    /// Rooms holding a creep/PC with interRoom.
    pub movers: BTreeSet<RoomIdx>,
    pub terminal_sends: BTreeSet<RoomIdx>,
    /// Spawned power creeps.
    pub power_creep_rooms: BTreeSet<RoomIdx>,
    pub force_update: BinaryHeap<std::cmp::Reverse<(u64, RoomIdx)>>,
    /// Last P3 cost, LPT order.
    pub room_cost_ns: Vec<u32>,
    /// By UserIdx: rooms holding a rooms.flags doc of that user (data.js:140 without a world scan).
    flag_rooms: Vec<BTreeSet<RoomIdx>>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ObjLoc {
    pub room: RoomIdx,
    pub seq: u64,
    pub owner: Option<UserIdx>,
}

impl WorldIndex {
    /// From scratch: every live room's docs and flags (load, recovery, `World::reindex`). `force_update` gets
    /// every live room keyed by its `nextForceUpdateTime` (0 when unset or not a positive number: due at once,
    /// cronjobs.js:183); the cron re-checks status and the doc when it pops.
    pub fn build(meta: &RoomsMeta, rooms: &[Arc<RoomSnapshot>], users: usize, user_ids: &UserIds) -> WorldIndex {
        let mut ix = WorldIndex { room_cost_ns: vec![0; rooms.len()], ..WorldIndex::default() };
        ix.grow(users);
        for (r, snap) in rooms.iter().enumerate() {
            if meta.rooms.get(r).is_none_or(|m| m.removed) {
                continue;
            }
            ix.update_room(r as RoomIdx, &EMPTY_ROOM, snap, user_ids);
            let next = js_number(snap.info.get(crate::atom!("nextForceUpdateTime")));
            let key = if next > 0.0 { next as u64 } else { 0 };
            ix.force_update.push(std::cmp::Reverse((key, r as RoomIdx)));
        }
        ix
    }

    /// Room `r` went from `prev` to `next`: objects (docs shared by pointer are skipped; a doc that moved to
    /// another room is dropped only while its entry still points here), presence, flags and the room sets.
    pub fn update_room(&mut self, r: RoomIdx, prev: &RoomSnapshot, next: &RoomSnapshot, user_ids: &UserIds) {
        let owner = |d: &DocRef| match d.obj.get(crate::atom!("user")) {
            Some(Val::Str(s)) => Id::new(s).and_then(|id| user_ids.get(&id).copied()),
            _ => None,
        };
        if !Arc::ptr_eq(&prev.objects, &next.objects) {
            let (mut i, mut j) = (0, 0);
            let (a, b) = (&prev.objects, &next.objects);
            while i < a.len() || j < b.len() {
                let (sa, sb) = (a.get(i).map_or(u64::MAX, |d| d.seq), b.get(j).map_or(u64::MAX, |d| d.seq));
                if sa < sb {
                    let id = a[i].id();
                    if self.objects.get(&id).is_some_and(|l| l.room == r && l.seq == sa) {
                        self.objects.remove(&id);
                    }
                    i += 1;
                    continue;
                }
                // Same seq = same doc (seq is unique and ids never change); re-index it unless shared.
                if sa > sb || !Arc::ptr_eq(&a[i], &b[j]) {
                    let d = &b[j];
                    if d.id() != Id::EMPTY {
                        self.objects.insert(d.id(), ObjLoc { room: r, seq: d.seq, owner: owner(d) });
                    }
                }
                i += (sa == sb) as usize;
                j += 1;
            }
        }
        if !Arc::ptr_eq(&prev.presence, &next.presence) {
            for (u, _) in prev.presence.iter() {
                if next.presence.binary_search_by_key(u, |(v, _)| *v).is_err() {
                    if let Some(m) = self.presence.get_mut(*u as usize) {
                        Arc::make_mut(m).remove(&r);
                    }
                }
            }
            for (u, p) in next.presence.iter() {
                self.grow(*u as usize + 1);
                let m = Arc::make_mut(&mut self.presence[*u as usize]);
                if m.get(&r) != Some(p) {
                    m.insert(r, p.clone());
                }
            }
        }
        if !Arc::ptr_eq(&prev.flags, &next.flags) {
            let users = |f: &[Arc<Obj>]| -> BTreeSet<UserIdx> {
                f.iter()
                    .filter_map(|o| match o.get(crate::atom!("user")) {
                        Some(Val::Str(s)) => Id::new(s).and_then(|id| user_ids.get(&id).copied()),
                        _ => None,
                    })
                    .collect()
            };
            let (old, new) = (users(&prev.flags), users(&next.flags));
            for u in old.difference(&new) {
                if let Some(s) = self.flag_rooms.get_mut(*u as usize) {
                    s.remove(&r);
                }
            }
            for &u in &new {
                self.grow(u as usize + 1);
                self.flag_rooms[u as usize].insert(r);
            }
        }
        for (set, on) in [(&mut self.movers, next.has_movers), (&mut self.terminal_sends, next.has_terminal_send), (&mut self.power_creep_rooms, next.has_power_creeps)] {
            if on {
                set.insert(r);
            } else {
                set.remove(&r);
            }
        }
        if self.room_cost_ns.len() <= r as usize {
            self.room_cost_ns.resize(r as usize + 1, 0);
        }
    }

    /// Rooms holding a rooms.flags doc of user `u`, ascending.
    pub fn flag_rooms(&self, u: UserIdx) -> impl Iterator<Item = RoomIdx> + '_ {
        self.flag_rooms.get(u as usize).into_iter().flatten().copied()
    }

    fn grow(&mut self, users: usize) {
        if self.presence.len() < users {
            self.presence.resize_with(users, Default::default);
        }
        if self.flag_rooms.len() < users {
            self.flag_rooms.resize_with(users, Default::default);
        }
    }
}

/// The "before" of a room that had nothing.
static EMPTY_ROOM: std::sync::LazyLock<RoomSnapshot> = std::sync::LazyLock::new(|| RoomSnapshot {
    info: Arc::new(Obj::new()),
    objects: Arc::new([]),
    flags: Arc::new([]),
    event_log: "".into(),
    map_view: "".into(),
    processed_at: 0,
    presence: Arc::new([]),
    user_refs: Arc::new([]),
    has_movers: false,
    has_terminal_send: false,
    has_power_creeps: false,
    blob: OnceLock::new(),
});

/// The only thing backend, persistence and history read. Consistent (one tick), immutable, shares data.
#[derive(Clone)]
pub struct WorldView {
    pub time: u64,
    pub vnow: i64,
    pub meta: Arc<RoomsMeta>,
    pub terrain: Arc<TerrainStore>,
    pub access: Arc<AccessSnapshot>,
    pub rooms: Arc<[Arc<RoomSnapshot>]>,
    pub users: Arc<[Arc<UserState>]>,
    pub user_ids: Arc<UserIds>,
    pub usernames: Arc<HashMap<Box<str>, UserIdx>>,
    pub presence: Arc<[Arc<BTreeMap<RoomIdx, Presence>>]>,
    pub market: Arc<MarketState>,
    pub env: Arc<EnvState>,
    /// seed, clock, next_seq, id counter, active list, mods.
    pub persist: Arc<PersistMeta>,
}

/// What a checkpoint manifest needs besides the snapshots (§9).
pub struct PersistMeta {
    pub seed: u64,
    pub clock: VClock,
    pub next_seq: u64,
    pub id_counter: u64,
    pub active: Vec<RoomIdx>,
    pub mods: Arc<[Box<str>]>,
}

impl World {
    /// A world at `time` 0 from its map, terrain, rooms and users (both in creation order): default config, env
    /// and empty market, ids for `compat.profile`, `next_seq` past every seq of the rooms' docs, the users and
    /// their power creeps (a loader that installs a market must keep it past those docs too), TICK-ACCESSIBLE at
    /// vclock(0), and the derived maps rebuilt (`reindex`). Every field stays public for loaders to adjust
    /// (then `reindex` again if users or rooms changed).
    pub fn new(seed: u64, clock: VClock, compat: policy::Compat, meta: RoomsMeta, terrain: TerrainStore, rooms: Vec<Arc<RoomSnapshot>>, users: Vec<Arc<UserState>>) -> World {
        let docs = rooms.iter().flat_map(|r| r.objects.iter()).chain(users.iter().flat_map(|u| u.power_creeps.iter()));
        let next_seq = docs.map(|d| d.seq + 1).chain(users.iter().map(|u| u.seq + 1)).max().unwrap_or(1);
        let access = AccessSnapshot::build(&meta, &rooms, clock.at(0), 0);
        let mut w = World {
            time: 0,
            seed,
            clock,
            ids: ids::IdGen::new(seed, compat.profile),
            compat: Arc::new(compat),
            config: Arc::new(EngineConfig::default()),
            meta: Arc::new(meta),
            terrain: Arc::new(terrain),
            access: Arc::new(access),
            rooms,
            users,
            user_ids: Arc::default(),
            usernames: Arc::default(),
            market: Arc::new(MarketState::empty()),
            env: Arc::new(EnvState::default()),
            active: ActiveSet::default(),
            manual_intents: Vec::new(),
            global_intents: Vec::new(),
            index: WorldIndex::default(),
            next_seq,
            mods: Arc::new([]),
            view_rooms: Default::default(),
        };
        w.reindex();
        w
    }

    /// Rebuilds `user_ids`, `usernames` (`usernameLower`) and `index` from rooms and users.
    pub fn reindex(&mut self) {
        self.user_ids = Arc::new(self.users.iter().enumerate().map(|(i, u)| (u.id, i as UserIdx)).collect());
        self.usernames = Arc::new(
            self.users
                .iter()
                .enumerate()
                .filter_map(|(i, u)| Some((u.doc.get(crate::atom!("usernameLower"))?.as_str()?.into(), i as UserIdx)))
                .collect(),
        );
        self.index = WorldIndex::build(&self.meta, &self.rooms, self.users.len(), &self.user_ids);
    }

    /// Installs `next` as room `r` and updates the index (every snapshot replacement goes through here or
    /// `WorldIndex::update_room`). Returns the previous snapshot.
    pub fn set_room(&mut self, r: RoomIdx, next: Arc<RoomSnapshot>) -> Arc<RoomSnapshot> {
        let prev = std::mem::replace(&mut self.rooms[r as usize], next);
        self.index.update_room(r, &prev, &self.rooms[r as usize], &self.user_ids);
        prev
    }

    /// O(rooms) pointer compares + O(users + changed rooms) refcount updates (`ViewRooms`); a full copy of the room
    /// pointers only while a reader still holds the view before last.
    pub fn view(&self) -> WorldView {
        WorldView {
            time: self.time,
            vnow: self.clock.at(self.time),
            meta: self.meta.clone(),
            terrain: self.terrain.clone(),
            access: self.access.clone(),
            rooms: self.view_rooms(),
            users: self.users.as_slice().into(),
            user_ids: self.user_ids.clone(),
            usernames: self.usernames.clone(),
            presence: self.index.presence.as_slice().into(),
            market: self.market.clone(),
            env: self.env.clone(),
            persist: Arc::new(PersistMeta {
                seed: self.seed,
                clock: self.clock,
                next_seq: self.next_seq,
                id_counter: self.ids.counter(),
                active: self.active.list().to_vec(),
                mods: self.mods.clone(),
            }),
        }
    }

    /// `self.rooms` as a shared slice. Compares every pointer, so rooms replaced without `set_room` are caught too.
    /// The snapshots the spare lets go of (replaced two views back, mostly their last holder) are freed on the
    /// `snapshot-drop` thread: freeing a tick's replaced docs, blobs and JSON caches took the tick thread ~8 ms.
    fn view_rooms(&self) -> Arc<[Arc<RoomSnapshot>]> {
        let mut c = self.view_rooms.lock().unwrap_or_else(|e| e.into_inner());
        let mut next = c.spare.take();
        let mut old = Vec::new();
        match next.as_mut().and_then(Arc::get_mut) {
            Some(buf) if buf.len() == self.rooms.len() => {
                for (b, r) in buf.iter_mut().zip(&self.rooms) {
                    if !Arc::ptr_eq(b, r) {
                        old.push(std::mem::replace(b, r.clone()));
                    }
                }
            }
            _ => next = Some(self.rooms.as_slice().into()), // a reader still holds it, or the map changed
        }
        let next = next.expect("set above");
        c.spare = c.last.replace(next.clone());
        drop_later(old);
        next
    }
}

/// Drops `old` on the `snapshot-drop` thread (started on first use).
fn drop_later(old: Vec<Arc<RoomSnapshot>>) {
    static TX: OnceLock<std::sync::mpsc::Sender<Vec<Arc<RoomSnapshot>>>> = OnceLock::new();
    if old.is_empty() {
        return;
    }
    let tx = TX.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel::<Vec<Arc<RoomSnapshot>>>();
        std::thread::Builder::new().name("snapshot-drop".into()).spawn(move || rx.into_iter().for_each(drop)).expect("snapshot-drop thread");
        tx
    });
    let _ = tx.send(old);
}

/// `Val::write_json` of an object without cloning it into a `Val` (ASCII, non-ASCII as `\uXXXX`).
pub(crate) fn write_obj_ascii(out: &mut String, o: &Obj) {
    out.push('{');
    for (i, (k, v)) in o.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        quote_ascii(out, k.as_str());
        out.push(':');
        crate::doc::write_val(out, v, true);
    }
    out.push('}');
}

/// JSON.stringify of a string with non-ASCII as `\uXXXX` (UTF-16 units, lowercase hex), like `Doc::json`.
pub(crate) fn quote_ascii(out: &mut String, s: &str) {
    if s.is_ascii() {
        return util::json::write_quoted(out, s);
    }
    let mut q = String::new();
    util::json::write_quoted(&mut q, s);
    for c in q.chars() {
        if c.is_ascii() {
            out.push(c);
        } else {
            for u in c.encode_utf16(&mut [0; 2]) {
                out.push_str(&format!("\\u{u:04x}"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::Doc;
    use util::json::Json;
    use util::rng::Rng;

    fn fixture(name: &str) -> Vec<Json> {
        let path = format!("{}/tests/fixtures/world/{name}.ndjson", env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(path).unwrap().lines().map(|l| Json::parse(l).unwrap()).collect()
    }

    fn obj(j: &Json) -> Obj {
        match Val::from_json(j) {
            Val::Obj(o) => o,
            _ => panic!("not an object: {}", j.stringify()),
        }
    }

    fn room(info: Obj, docs: Vec<DocRef>, flags: Vec<Obj>, user_ids: &UserIds) -> Arc<RoomSnapshot> {
        let flags: Vec<Arc<Obj>> = flags.into_iter().map(Arc::new).collect();
        Arc::new(RoomSnapshot::from_docs(Arc::new(info), docs, flags.into(), "".into(), "".into(), 0, user_ids))
    }

    fn meta_of(names: &[&str]) -> RoomsMeta {
        RoomsMeta::new(names.iter().map(|n| RoomMeta { name: (*n).into(), xy: n.parse().unwrap(), removed: false }).collect())
    }

    fn clock() -> VClock {
        VClock { base_ms: 0, base_tick: 0, nominal_tick_ms: 1000 }
    }

    /// The blob is node's JSON.stringify of {room, seq, objects} with non-ASCII escaped, and every offsets slice
    /// parses back to its doc.
    #[test]
    fn blob_fixture() {
        for c in fixture("blob") {
            let objects = c.get("objects").and_then(Json::as_array).unwrap();
            let seqs = c.get("seq").and_then(Json::as_array).unwrap();
            let docs: Vec<DocRef> = seqs.iter().zip(objects).map(|(s, o)| Doc::new(s.as_f64().unwrap() as u64, obj(o))).collect();
            let snap = room(obj(c.get("room").unwrap()), docs, vec![], &UserIds::default());
            let b = snap.blob();
            let text = std::str::from_utf8(&b.json).unwrap();
            assert_eq!(text, c.get("out").and_then(Json::as_str).unwrap());
            assert_eq!(b.offsets.len(), objects.len() + 1);
            let off = |i: usize| b.offsets[i] as usize;
            for (i, o) in objects.iter().enumerate() {
                assert_eq!(&Json::parse(&text[off(i)..off(i + 1) - 1]).unwrap(), o, "doc {i} of {text}");
            }
            assert_eq!(&text[off(objects.len()) - 1..], "]}");
            assert!(std::ptr::eq(snap.blob(), b), "built once");
        }
    }

    /// `exit_counts` = mapgrid.js `_buildGridData` (zero counts omitted there).
    #[test]
    fn exit_counts_fixture() {
        for c in fixture("mapgrid") {
            let t = RoomTerrain::from_db_str(c.get("terrain").and_then(Json::as_str).unwrap()).unwrap();
            let store = TerrainStore::new(vec![t]);
            let out = c.get("out").unwrap();
            let want: Vec<u8> = ["t", "r", "b", "l"].iter().map(|k| out.get(k).and_then(Json::as_f64).unwrap_or(0.0) as u8).collect();
            assert_eq!(store.exit_counts(0).to_vec(), want);
            assert_eq!(store.raw(0), &store.get(0).raw_buffer());
        }
        assert_eq!(TerrainStore::new(vec![RoomTerrain::default(); 3]).heap_mb(), 7500.0 / 1048576.0);
    }

    /// The blob view = cli/map.js updateTerrainData: out-of-borders rooms walled, the outer ring added, and the
    /// heap limit counts every entry.
    #[test]
    fn terrain_blob_fixture() {
        for c in fixture("terrain-blob") {
            let rooms = c.get("rooms").and_then(Json::as_array).unwrap();
            let names: Vec<&str> = rooms.iter().map(|r| r.get("_id").and_then(Json::as_str).unwrap()).collect();
            let meta = meta_of(&names);
            let terrain_of = |n: &str| {
                let t = c.get("terrain").and_then(Json::as_array).unwrap().iter().find(|t| t.get("room").and_then(Json::as_str) == Some(n)).unwrap();
                RoomTerrain::from_db_str(t.get("terrain").and_then(Json::as_str).unwrap()).unwrap()
            };
            let mut store = TerrainStore::new(names.iter().map(|n| terrain_of(n)).collect());
            let walled: Vec<RoomIdx> = (0..rooms.len()).filter(|&i| rooms[i].get("status").and_then(Json::as_str) == Some("out of borders")).map(|i| i as RoomIdx).collect();
            store.set_blob(&meta, &walled);
            let entries = c.get("entries").and_then(Json::as_array).unwrap();
            for e in entries {
                let (name, kind) = (e.at(0).unwrap().as_str().unwrap(), e.at(1).unwrap().as_str().unwrap());
                let got = match meta.by_name(name) {
                    Some(r) if store.raw(r) == &WALLS => "walls",
                    Some(r) if store.raw(r) == &terrain_of(name).raw_buffer() => "same",
                    Some(_) => "other",
                    None => store.blob_only(name).map_or("missing", |t| if t == &WALLS { "walls" } else { "other" }),
                };
                assert_eq!(got, kind, "{name} in {}", c.stringify());
            }
            assert_eq!(store.heap_mb(), (entries.len() * 2500) as f64 / 1048576.0, "{}", c.stringify());
            assert_eq!(store.blob_only(names[0]), None);
        }
        // The unanchored /(W|E)(\d+)(N|S)(\d+)/ moves on past a non-ASCII byte after the digits (no slicing panic).
        assert_eq!(blob_name_parts("xW1é1E2S03"), Some(('E', "2", 'S', "03")));
        assert_eq!(blob_name_parts("W1N"), None);
    }

    /// TICK-ACCESSIBLE = driver updateAccessibleRoomsList + updateRoomStatusData over Loki.
    #[test]
    fn access_fixture() {
        for c in fixture("access") {
            let docs: Vec<Obj> = c.get("rooms").and_then(Json::as_array).unwrap().iter().map(obj).collect();
            let names: Vec<String> = docs.iter().map(|d| d.get(crate::atom!("_id")).unwrap().as_str().unwrap().to_string()).collect();
            let meta = meta_of(&names.iter().map(|s| s.as_str()).collect::<Vec<_>>());
            let rooms: Vec<_> = docs.into_iter().map(|d| room(d, vec![], vec![], &UserIds::default())).collect();
            let now = c.get("now").and_then(Json::as_f64).unwrap() as i64;
            let a = AccessSnapshot::build(&meta, &rooms, now, 7);
            let accessible: Vec<&str> = (0..rooms.len()).filter(|&r| a.is_accessible(r as RoomIdx)).map(|r| names[r].as_str()).collect();
            let want: Vec<&str> = c.get("accessible").and_then(Json::as_array).unwrap().iter().map(|j| j.as_str().unwrap()).collect();
            assert_eq!(accessible, want, "{}", c.stringify());
            let status = c.get("status").unwrap();
            let mut n = 0;
            for (kind, rs) in [("novice", RoomStatus::Novice), ("respawn", RoomStatus::Respawn), ("closed", RoomStatus::Closed)] {
                for (name, ts) in status.get(kind).and_then(Json::as_object).unwrap() {
                    n += 1;
                    let r = names.iter().position(|x| x == name).unwrap() as RoomIdx;
                    // The raw stored value (a string or [n] when the doc held one); null = Infinity.
                    let want_ts = (!ts.is_null()).then(|| js_number(Some(&Val::from_json(ts))) as i64);
                    assert_eq!(a.status.get(&r), Some(&(rs, want_ts)), "{name} in {}", c.stringify());
                }
            }
            assert_eq!(a.status.len(), n, "{}", c.stringify());
            assert_eq!(a.version, 7);
        }
    }

    #[test]
    fn rooms_meta() {
        let mut rooms: Vec<RoomMeta> = ["W2N2", "E0S0", "W1N1", "E3N1"].iter().map(|n| RoomMeta { name: (*n).into(), xy: n.parse().unwrap(), removed: false }).collect();
        rooms[2].removed = true;
        let m = RoomsMeta::new(rooms);
        assert_eq!((m.x0, m.y0, m.w, m.h), (-3, -3, 7, 4));
        assert_eq!(m.by_name("W2N2"), Some(0));
        assert_eq!(m.by_name("E3N1"), Some(3));
        assert_eq!(m.by_name("W1N1"), None, "removed");
        assert_eq!(m.by_name("w2n2"), None, "exact _id only");
        assert_eq!(m.by_name("W2N2x"), None);
        assert_eq!(m.idx(RoomXY::new(100, 100)), None);
        assert_eq!(m.world_size, crate::common::calc_world_size(["W2N2", "E0S0", "E3N1"]) as u32);
        assert_eq!(RoomsMeta::new(vec![]).world_size, 0);
    }

    #[test]
    fn active_set() {
        let mut a = ActiveSet::default();
        for r in [5, 200, 5, 1, 200] {
            a.insert(r);
        }
        assert!(a.contains(200) && !a.contains(2) && !a.contains(100_000));
        assert_eq!(a.take(), vec![5, 200, 1]);
        assert!(!a.contains(5) && a.list().is_empty());
        a.insert(5);
        assert_eq!(a.take(), vec![5]);
    }

    /// Random worlds and random snapshot replacements (docs removed, re-owned, added, moved between rooms with
    /// their seq; flags changed): the incrementally kept index equals a rebuild by scan after every step.
    #[test]
    fn index_matches_scan() {
        let mut rng = Rng::new(0x51de);
        let names = ["W1N1", "W2N1", "W1N2", "W2N2", "E0S0", "E1S1"];
        let user_names = ["aaa", "bbb", "2", "ccc"];
        let pick = |rng: &mut Rng, n: usize| rng.below(n as u64) as usize;
        for _case in 0..30 {
            let users: Vec<Arc<UserState>> = user_names.iter().enumerate().map(|(i, n)| test_user(n, i as u64)).collect();
            let user_ids: UserIds = users.iter().enumerate().map(|(i, u)| (u.id, i as UserIdx)).collect();
            let mut seq = 1;
            let mut rooms = Vec::new();
            for _ in &names {
                let docs: Vec<DocRef> = (0..rng.below(6)).map(|_| random_doc(&mut rng, &mut seq, &user_names, &names)).collect();
                rooms.push(room(Obj::new(), docs, random_flags(&mut rng, &user_names), &user_ids));
            }
            let terrain = TerrainStore::new(vec![RoomTerrain::default(); names.len()]);
            let mut w = World::new(1, clock(), policy::Compat::new(policy::Profile::Private), meta_of(&names), terrain, rooms, users);
            check(&w);
            for _step in 0..40 {
                let r = pick(&mut rng, names.len());
                let prev = w.rooms[r].clone();
                let mut docs: Vec<DocRef> = prev.objects.iter().filter(|_| rng.below(4) != 0).cloned().collect();
                for d in docs.iter_mut() {
                    if rng.below(4) == 0 {
                        let mut nd = (**d).clone();
                        nd.obj_mut().set(crate::atom!("user"), Val::Str(user_names[pick(&mut rng, 4)].into()));
                        nd.obj_mut().set(crate::atom!("observeRoom"), Val::Str(names[pick(&mut rng, 6)].into()));
                        *d = Arc::new(nd);
                    }
                }
                for _ in 0..rng.below(3) {
                    docs.push(random_doc(&mut rng, &mut seq, &user_names, &names));
                }
                // A doc moving in from another room keeps its seq; the source room is installed before or after.
                let from = pick(&mut rng, names.len());
                let mover = (from != r && !w.rooms[from].objects.is_empty()).then(|| {
                    let src = &w.rooms[from];
                    docs.push(src.objects[0].clone());
                    room((*src.info).clone(), src.objects[1..].to_vec(), src.flags.iter().map(|f| (**f).clone()).collect(), &w.user_ids)
                });
                docs.sort_by_key(|d| d.seq);
                let flags = if rng.below(2) == 0 { prev.flags.iter().map(|f| (**f).clone()).collect() } else { random_flags(&mut rng, &user_names) };
                let next = room(Obj::new(), docs, flags, &w.user_ids);
                let source_first = rng.below(2) == 0;
                if let (Some(src), true) = (&mover, source_first) {
                    w.set_room(from as RoomIdx, src.clone());
                }
                w.set_room(r as RoomIdx, next);
                if let (Some(src), false) = (mover, source_first) {
                    w.set_room(from as RoomIdx, src);
                }
                check(&w);
            }
        }
    }

    fn check(w: &World) {
        let scan = WorldIndex::build(&w.meta, &w.rooms, w.users.len(), &w.user_ids);
        let ix = &w.index;
        assert_eq!(ix.presence.len(), scan.presence.len());
        for (a, b) in ix.presence.iter().zip(&scan.presence) {
            assert_eq!(**a, **b);
        }
        assert_eq!(ix.objects, scan.objects);
        assert_eq!((&ix.movers, &ix.terminal_sends, &ix.power_creep_rooms), (&scan.movers, &scan.terminal_sends, &scan.power_creep_rooms));
        for u in 0..w.users.len() as UserIdx {
            assert_eq!(ix.flag_rooms(u).collect::<Vec<_>>(), scan.flag_rooms(u).collect::<Vec<_>>());
        }
        // The scan itself against the definitions, straight off the docs.
        for (r, snap) in w.rooms.iter().enumerate() {
            for d in snap.objects.iter() {
                let loc = scan.objects[&d.id()];
                assert_eq!((loc.room, loc.seq), (r as RoomIdx, d.seq));
            }
            let movers = snap.objects.iter().any(|d| matches!(d.kind(), "creep" | "powerCreep") && !matches!(d.obj.get(crate::atom!("interRoom")), None | Some(Val::Null)));
            assert_eq!(scan.movers.contains(&(r as RoomIdx)), movers);
            // The kept index against data.js's definitions (presence, flags) and the global stage's room sets.
            let r = r as RoomIdx;
            let uid = |v: Option<&Val>| v.and_then(Val::as_str).and_then(Id::new).and_then(|id| w.user_ids.get(&id).copied());
            let mut want: BTreeMap<UserIdx, Presence> = BTreeMap::new();
            for d in snap.objects.iter() {
                let Some(u) = uid(d.obj.get(crate::atom!("user"))) else { continue };
                let p = want.entry(u).or_default();
                p.objects += 1;
                if !matches!(d.kind(), "flag" | "constructionSite") {
                    if p.vision == 0 {
                        p.first_vision_seq = d.seq;
                    }
                    p.vision += 1;
                    if d.kind() == "observer" && p.observe.is_none() {
                        let target = match d.obj.get(crate::atom!("observeRoom")) {
                            None => "undefined",
                            Some(Val::Null) => "null",
                            Some(v) => v.as_str().unwrap(),
                        };
                        p.observe = Some((d.seq, target.into()));
                    }
                }
            }
            for u in 0..w.users.len() as UserIdx {
                assert_eq!(ix.presence[u as usize].get(&r), want.get(&u), "presence of {u} in {r}");
                let flags = snap.flags.iter().any(|f| uid(f.get(crate::atom!("user"))) == Some(u));
                assert_eq!(ix.flag_rooms(u).any(|x| x == r), flags, "flags of {u} in {r}");
            }
            let has = |kind: &str, f: &dyn Fn(&Obj) -> bool| snap.objects.iter().any(|d| d.kind() == kind && f(&d.obj));
            assert_eq!(ix.terminal_sends.contains(&r), has("terminal", &|o| o.get(crate::atom!("send")) == Some(&Val::Bool(true))));
            assert_eq!(ix.power_creep_rooms.contains(&r), has("powerCreep", &|_| true));
        }
    }

    fn random_doc(rng: &mut Rng, seq: &mut u64, users: &[&str], rooms: &[&str]) -> DocRef {
        *seq += 1 + rng.below(3);
        let kinds = ["creep", "observer", "constructionSite", "flag", "terminal", "powerCreep", "source", "controller", "tombstone"];
        let mut pick = |n: usize| rng.below(n as u64) as usize;
        let mut o = Obj::new();
        o.set(crate::atom!("_id"), Val::Str(format!("id{seq}").into()));
        o.set(crate::atom!("type"), Val::Str(kinds[pick(kinds.len())].into()));
        if pick(4) != 0 {
            o.set(crate::atom!("user"), Val::Str(users[pick(users.len())].into()));
        }
        match pick(4) {
            0 => o.set(crate::atom!("observeRoom"), Val::Null),
            1 => o.set(crate::atom!("observeRoom"), Val::Str(rooms[pick(rooms.len())].into())),
            _ => {}
        }
        if pick(3) == 0 {
            o.set(crate::atom!("interRoom"), if pick(2) == 0 { Val::Null } else { Val::Obj(Obj::new()) });
        }
        if pick(3) == 0 {
            o.set(crate::atom!("send"), Val::Bool(pick(2) == 0));
        }
        if pick(3) == 0 {
            let mut sub = Obj::new();
            sub.set(crate::atom!("user"), Val::Str(users[pick(users.len())].into()));
            o.set(crate::atom!("reservation"), Val::Obj(sub));
        }
        Doc::new(*seq, o)
    }

    fn random_flags(rng: &mut Rng, users: &[&str]) -> Vec<Obj> {
        (0..rng.below(3))
            .map(|i| {
                let mut f = Obj::new();
                f.set(crate::atom!("_id"), Val::Str(format!("f{i}").into()));
                f.set(crate::atom!("user"), Val::Str(users[rng.below(users.len() as u64) as usize].into()));
                f
            })
            .collect()
    }

    fn test_user(name: &str, seq: u64) -> Arc<UserState> {
        let mut doc = Obj::new();
        doc.set(crate::atom!("_id"), Val::Str(name.into()));
        doc.set(crate::atom!("usernameLower"), Val::Str(name.into()));
        Arc::new(UserState {
            id: Id::new(name).unwrap(),
            seq,
            doc: Arc::new(doc),
            auth: Arc::new(Obj::new()),
            code: Arc::new(CodeState { branches: vec![], active_world: None, active_sim: None, version: 0 }),
            memory: JsStr::from_str(""),
            memory_version: 0,
            segments: Arc::default(),
            public_segments: "".into(),
            inter_shard: None,
            console_queue: Arc::new([]),
            power_creeps: Arc::new([]),
            inbox: Arc::default(),
        })
    }

    /// view() of a 256×256 world with 1000 users, ticks the way P6 publishes them (a few rooms replaced, one of them
    /// behind `set_room`'s back; the backend drops the view before last): < 1 ms in release builds, and every view
    /// holds exactly the world's room pointers, also while an old view is still held.
    #[test]
    fn view_65k_rooms() {
        let xys: Vec<RoomXY> = (-128..128).flat_map(|x| (-128..128).map(move |y| RoomXY::new(x, y))).collect();
        let meta = RoomsMeta::new(xys.iter().map(|xy| RoomMeta { name: xy.to_string().into(), xy: *xy, removed: false }).collect());
        let rooms: Vec<_> = xys.iter().map(|_| room(Obj::new(), vec![], vec![], &UserIds::default())).collect();
        let users = (0..1000).map(|i| test_user(&format!("u{i}"), i)).collect();
        let terrain = TerrainStore::new(vec![RoomTerrain::default(); xys.len()]);
        let mut w = World::new(1, clock(), policy::Compat::new(policy::Profile::Private), meta, terrain, rooms, users);
        assert_eq!((w.meta.world_size, w.meta.by_name("W127N127"), w.meta.by_name("E127S127")), (256, Some(0), Some(65535)));
        let same = |v: &WorldView, w: &World| v.rooms.len() == w.rooms.len() && v.rooms.iter().zip(&w.rooms).all(|(a, b)| Arc::ptr_eq(a, b));
        let mut published = w.view();
        let kept = w.view(); // a checkpoint thread holding a view: the next one cannot reuse its slice
        let mut best = std::time::Duration::MAX;
        for tick in 0..8u32 {
            for k in 0..20 {
                let r = (tick * 7919 + k * 104729) % 65536;
                w.set_room(r, room(Obj::new(), vec![], vec![], &UserIds::default()));
            }
            w.rooms[(tick as usize * 31) % 65536] = room(Obj::new(), vec![], vec![], &UserIds::default());
            let t = std::time::Instant::now();
            let v = w.view();
            best = best.min(t.elapsed());
            assert!(same(&v, &w) && v.users.len() == 1000, "tick {tick}");
            published = v;
        }
        assert!(same(&published, &w) && !same(&kept, &w));
        let limit = if cfg!(debug_assertions) { 50 } else { 1 };
        assert!(best < std::time::Duration::from_millis(limit), "view() took {best:?}");
    }
}

/// `{status: 'normal'}` and `!openTime || openTime < now` on one `rooms` doc (driver index.js:433-434,
/// updateAccessibleRoomsList :490-496).
pub fn room_accessible(info: &Obj, now: i64) -> bool {
    let open_time = info.get(crate::atom!("openTime"));
    matches!(info.get(crate::atom!("status")), Some(Val::Str(v)) if &**v == "normal") && (!truthy(open_time) || js_number(open_time) < now as f64)
}
