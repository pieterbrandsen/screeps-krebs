//! `chitin bench`: N bot players on a generated world, run headless on the real runtime and processor, with
//! the tick stages timed (§2.9 targets). Bots are placed as `bots.spawn` places them (CLI-BOTS-SPAWN): a user, a
//! code branch, Memory `{}`, a spawn near the controller, the controller claimed at level 1, the room active.
//!   chitin bench [--bots N] [--world WxH] [--ticks T] [--warmup W] [--bot main.js|dir] [--runner-threads N]
//!                [--max-memory-mb M] [--mods a,b] [--scenario file.json [--tile N] | --load dir] [--save dir]
//!                [--console] [--p3-repeat N] [--report-every N] [--save-every N]
//! `--scenario` runs a screeps-sim scenario (its rooms, players and their bots) instead of generated bot rooms,
//! `--tile N` N copies of it side by side (capacity, ARCHITECTURE.md §2.9); `--console` prints every run's console
//! output. `--save dir` writes the world as it ends to `dir/world.bin` (a checkpoint, as `serve` writes one: rooms,
//! users, code, Memory, segments); `--load dir` starts from one, so a world that ran long enough to grow up can be
//! measured again and again from the same state, the bots' globals reset as after a code push. `--p3-repeat N`
//! processes every tick's rooms N more times on the tick thread from the same input (results dropped) and prints
//! their µs per room: a P3 change measured without the bots' variance or the other threads (ms/tick then includes
//! the repeats; `CHITIN_NATIVE_PROFILE_P3` samples them too). `--report-every N` prints an `every {json}` line of what
//! the last N ticks cost and held, and `--save-every N` writes a checkpoint to `<save>/<tick>/`: a run's cost by age.
//! Every player VM holds its own V8 heap (~12 MB each with the M1 bot), so the run is capped at M MB (default
//! 4096): refused up front when the bots alone would pass it, stopped as soon as the process does.
//! Profiling: the JS tick is split by `natives.mark` calls (host/prelude.js, driver/runtime.js; `mark(4..35)` in
//! any JS file adds a numbered probe). `CHITIN_JS_PROFILE=<µs>` samples the JS stack of running VMs that often and
//! prints the hottest functions and lines (`CHITIN_JS_PROFILE_TOP=<n>` rows), GC pauses and per-space heap sizes;
//! `CHITIN_V8_FLAGS` passes V8 flags (e.g. `--log-ic --logfile=ic.log`).
//! The bench also counts what V8 allocates between marks (KB/tick beside the JS sub-stages) and each user's run time.
//! `CHITIN_NATIVE_PROFILE=<µs>` samples every running thread's instruction pointer that often (sampler.rs) and prints
//! the hottest native functions: room processing, V8's runtime and builtins, the natives; with
//! `CHITIN_NATIVE_PROFILE_P3=1` only while rooms are processed.

use crate::config::{Args, Config};
use crate::tick::{PhaseTimes, PHASES};
use engine::coll::{self, Write};
use engine::doc::{Atom, Obj, Val};
use engine::world::{RoomIdx, World};
use std::sync::Arc;
use std::time::Instant;
use util::json::Json;

const M1_BOT: &str = include_str!("../../../tools/bots/m1/main.js");

fn obj(j: Json) -> Obj {
    match Val::from_json(&j) {
        Val::Obj(o) => o,
        _ => unreachable!(),
    }
}

fn s(x: &str) -> Json {
    Json::Str(x.into())
}

fn n(x: f64) -> Json {
    Json::Num(x)
}

/// A plain tile at least 4 from the edges with a free 3×3 around it and no object, nearest the room centre.
fn spawn_spot(w: &World, r: RoomIdx) -> Option<(u8, u8)> {
    let t = w.terrain.get(r);
    let taken: Vec<(f64, f64)> = w.rooms[r as usize]
        .objects
        .iter()
        .filter_map(|d| Some((d.obj.get(Atom::intern("x"))?.as_f64()?, d.obj.get(Atom::intern("y"))?.as_f64()?)))
        .collect();
    let mut best: Option<(u32, u8, u8)> = None;
    for y in 5..45u8 {
        for x in 5..45u8 {
            let free = (x - 1..=x + 1).all(|xx| (y - 1..=y + 1).all(|yy| !t.is_wall(xx, yy) && !taken.contains(&(xx as f64, yy as f64))));
            let d = (x as i32 - 25).unsigned_abs() + (y as i32 - 25).unsigned_abs();
            if free && best.is_none_or(|b| d < b.0) {
                best = Some((d, x, y));
            }
        }
    }
    best.map(|(_, x, y)| (x, y))
}

/// Places `count` bots running `modules` (users.code form), spread evenly over the claimable rooms (normal status, an unowned
/// controller, no keeper lair). Returns how many were placed.
pub fn place_bots(w: &mut World, count: usize, modules: &Json) -> usize {
    let kind = |d: &engine::doc::DocRef| d.obj.get(Atom::intern("type")).and_then(Val::as_str).map(str::to_string);
    let candidates: Vec<RoomIdx> = (0..w.rooms.len() as RoomIdx)
        .filter(|&r| !w.meta.rooms[r as usize].removed && w.rooms[r as usize].info.get(Atom::intern("status")).and_then(Val::as_str) == Some("normal"))
        .filter(|&r| {
            let objs = &w.rooms[r as usize].objects;
            objs.iter().any(|d| kind(d).as_deref() == Some("controller") && d.obj.get(Atom::intern("user")).is_none_or(|u| matches!(u, Val::Null)))
                && !objs.iter().any(|d| kind(d).as_deref() == Some("keeperLair"))
        })
        .collect();
    let count = count.min(candidates.len());
    let step = candidates.len() as f64 / count.max(1) as f64;
    let rooms: Vec<RoomIdx> = (0..count).map(|i| candidates[(i as f64 * step) as usize]).collect();

    let users: Vec<Write> = (0..count)
        .map(|i| {
            let name = format!("Bench{i}");
            Write::Insert(obj(Json::Obj(vec![
                ("username".into(), s(&name)),
                ("usernameLower".into(), s(&name.to_lowercase())),
                ("cpu".into(), n(100.0)),
                ("gcl".into(), n(0.0)),
                ("cpuAvailable".into(), n(10000.0)),
                ("bot".into(), s("bench")),
                ("active".into(), n(10000.0)),
            ])))
        })
        .collect();
    let inserted = coll::write(w, "users", users).expect("insert bench users");
    let ids: Vec<String> = inserted.iter().map(|u| u.get(Atom::intern("_id")).and_then(Val::as_str).unwrap().to_string()).collect();
    let code_docs = ids
        .iter()
        .map(|id| {
            Write::Insert(obj(Json::Obj(vec![
                ("user".into(), s(id)),
                ("modules".into(), modules.clone()),
                ("branch".into(), s("default")),
                ("timestamp".into(), n(1.0)),
                ("activeWorld".into(), Json::Bool(true)),
                ("activeSim".into(), Json::Bool(true)),
            ])))
        })
        .collect();
    coll::write(w, "users.code", code_docs).expect("insert bench code");
    let g = w.time as f64;
    let (mut spawns, mut controllers) = (Vec::new(), Vec::new());
    for (id, &r) in ids.iter().zip(&rooms) {
        let _ = coll::env_set(w, &format!("memory:{id}"), &Val::Str("{}".into()));
        let room = w.meta.rooms[r as usize].name.to_string();
        let Some((x, y)) = spawn_spot(w, r) else { continue };
        spawns.push(Write::Insert(obj(Json::Obj(vec![
            ("type".into(), s("spawn")),
            ("room".into(), s(&room)),
            ("x".into(), n(x as f64)),
            ("y".into(), n(y as f64)),
            ("name".into(), s("Spawn1")),
            ("user".into(), s(id)),
            ("store".into(), Json::Obj(vec![("energy".into(), n(300.0))])),
            ("storeCapacityResource".into(), Json::Obj(vec![("energy".into(), n(300.0))])),
            ("hits".into(), n(5000.0)),
            ("hitsMax".into(), n(5000.0)),
            ("spawning".into(), Json::Null),
            ("notifyWhenAttacked".into(), Json::Bool(false)),
        ]))));
        let c = w.rooms[r as usize].objects.iter().find(|d| kind(d).as_deref() == Some("controller")).unwrap();
        let mut doc = c.obj.clone();
        for (k, v) in [("user", Val::Str(id.as_str().into())), ("level", Val::Num(1.0)), ("progress", Val::Num(0.0)), ("downgradeTime", Val::Null), ("safeMode", Val::Num(g + 20000.0))] {
            doc.set(Atom::intern(k), v);
        }
        controllers.push(Write::Replace(c.id().as_str().into(), doc));
        w.active.insert(r);
    }
    coll::write(w, "rooms.objects", spawns).expect("insert spawns");
    coll::write(w, "rooms.objects", controllers).expect("claim controllers");
    count
}

/// `--scenario <file>`: a screeps-sim scenario (`{gameTime, rooms: [{name, terrain, objects}], players: [{username,
/// id, code, cpu, bucket, gcl, power, memory}]}`, e.g. screeps-sim/private/e10s30-e30s50) as an official db.json
/// for `chitin import`'s path: room docs shaped like generated ones (mapgen's highway `bus`/`depositType`, keeper
/// `sourceKeepers`), every room active, walled `out of borders` rooms around and between them, flag objects folded
/// into `rooms.flags` docs, Invader and Source Keeper
/// users, each player's code directory (relative to the file) loaded as `--bot` loads one.
fn load_scenario(cfg: &Config, path: &str, copies: usize) -> World {
    let fail = |e: &dyn std::fmt::Display| -> ! { crate::fail(&format!("{path}: {e}")) };
    let sc = Json::parse(&std::fs::read_to_string(path).unwrap_or_else(|e| fail(&e))).unwrap_or_else(|e| fail(&e));
    let sc = if copies > 1 { tile(&sc, copies).unwrap_or_else(|e| fail(&e)) } else { sc };
    let dir = std::path::Path::new(path).parent().unwrap_or(std::path::Path::new("."));
    let get = |j: &Json, k: &str| j.get(k).cloned().unwrap_or(Json::Null);
    let num = |j: &Json, k: &str| j.get(k).and_then(Json::as_f64).unwrap_or(0.0);
    let text = |j: &Json, k: &str| j.get(k).and_then(Json::as_str).unwrap_or("").to_string();

    let (mut rooms, mut terrain, mut objects, mut active) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    // (user, room) → `name~color~secondaryColor~x~y` entries (game.js:409 splits them back)
    let mut flags: Vec<((String, String), Vec<String>)> = Vec::new();
    let mut present = std::collections::HashSet::new();
    for r in sc.get("rooms").and_then(Json::as_array).unwrap_or_else(|| fail(&"no rooms")) {
        let name = text(r, "name");
        let xy = engine::room_name::RoomXY::parse_engine(&name).unwrap_or_else(|| fail(&format!("bad room name {name:?}")));
        present.insert((xy.x, xy.y));
        let mut info = vec![("_id".into(), s(&name)), ("status".into(), s("normal"))];
        match engine::mapgen::room_kind(xy) {
            engine::mapgen::RoomKind::Highway => {
                info.push(("bus".into(), Json::Bool(true)));
                info.push(("depositType".into(), s(engine::mapgen::deposit_type(xy))));
            }
            k => info.push(("sourceKeepers".into(), Json::Bool(k == engine::mapgen::RoomKind::Keeper))),
        }
        rooms.push(Json::Obj(info));
        terrain.push(Json::Obj(vec![("room".into(), s(&name)), ("terrain".into(), get(r, "terrain")), ("type".into(), s("terrain"))]));
        active.push(s(&name));
        for o in r.get("objects").and_then(Json::as_array).map_or(&[][..], |a| a.as_slice()) {
            if o.get("type").and_then(Json::as_str) == Some("flag") {
                let esc = |v: &str| v.replace('|', "$VLINE$").replace('~', "$TILDE$");
                let entry = format!("{}~{}~{}~{}~{}", esc(&text(o, "name")), num(o, "color"), num(o, "secondaryColor"), num(o, "x"), num(o, "y"));
                let at = (text(o, "user"), name.clone());
                match flags.iter_mut().find(|(k, _)| *k == at) {
                    Some((_, v)) => v.push(entry),
                    None => flags.push((at, vec![entry])),
                }
                continue;
            }
            let Json::Obj(mut doc) = o.clone() else { continue };
            doc.push(("room".into(), s(&name)));
            objects.push(Json::Obj(doc));
        }
    }
    // The real terrain has exits on every side, and a PathFinder search that reaches a room with no terrain throws
    // (pf.cc:33). Every missing room of the bounding box plus a ring around it is a walled `out of borders` room, the
    // edge of a closed generated world.
    let (xs, ys) = (present.iter().map(|p| p.0), present.iter().map(|p| p.1));
    let (x0, x1, y0, y1) = (xs.clone().min().unwrap() - 1, xs.max().unwrap() + 1, ys.clone().min().unwrap() - 1, ys.max().unwrap() + 1);
    for (x, y) in (y0..=y1).flat_map(|y| (x0..=x1).map(move |x| (x, y))).filter(|p| !present.contains(p)) {
        let name = engine::room_name::RoomXY { x, y }.to_string();
        rooms.push(Json::Obj(vec![("_id".into(), s(&name)), ("status".into(), s("out of borders"))]));
        terrain.push(Json::Obj(vec![("room".into(), s(&name)), ("terrain".into(), s(&"1".repeat(2500))), ("type".into(), s("terrain"))]));
    }
    let flags = flags.into_iter().map(|((user, room), v)| Json::Obj(vec![("user".into(), s(&user)), ("room".into(), s(&room)), ("data".into(), s(&v.join("|")))]));

    let npc = |id: &str, name: &str| {
        Json::Obj(vec![
            ("_id".into(), s(id)),
            ("username".into(), s(name)),
            ("usernameLower".into(), s(&name.to_lowercase())),
            ("gcl".into(), n(0.0)),
            ("cpu".into(), n(0.0)),
            ("active".into(), n(1.0)),
            ("cpuAvailable".into(), n(10000.0)),
        ])
    };
    let (mut users, mut code) = (vec![npc("2", "Invader"), npc("3", "Source Keeper")], Vec::new());
    let mut env = vec![("gameTime".into(), n(num(&sc, "gameTime"))), ("activeRooms".into(), Json::Arr(active))];
    for p in sc.get("players").and_then(Json::as_array).map_or(&[][..], |a| a.as_slice()) {
        let (id, name) = (text(p, "id"), text(p, "username"));
        users.push(Json::Obj(vec![
            ("_id".into(), s(&id)),
            ("username".into(), s(&name)),
            ("usernameLower".into(), s(&name.to_lowercase())),
            ("cpu".into(), n(num(p, "cpu"))),
            ("cpuAvailable".into(), n(num(p, "bucket"))),
            ("gcl".into(), n(num(p, "gcl"))),
            ("power".into(), n(num(p, "power"))),
            ("active".into(), n(10000.0)),
        ]));
        code.push(Json::Obj(vec![
            ("user".into(), s(&id)),
            ("modules".into(), load_bot(&dir.join(text(p, "code")).to_string_lossy())),
            ("branch".into(), s("default")),
            ("timestamp".into(), n(1.0)),
            ("activeWorld".into(), Json::Bool(true)),
            ("activeSim".into(), Json::Bool(true)),
        ]));
        if !text(p, "memory").is_empty() {
            env.push((format!("memory:{id}"), get(p, "memory")));
        }
    }
    let coll = |name: &str, data: Vec<Json>| Json::Obj(vec![("name".into(), s(name)), ("data".into(), Json::Arr(data))]);
    let db = Json::Obj(vec![(
        "collections".into(),
        Json::Arr(vec![
            // databaseVersion 9: the docs are already in the current format, upgradeDb has nothing to do
            coll("env", vec![Json::Obj(vec![("$loki".into(), n(1.0)), ("data".into(), Json::Obj(env)), ("databaseVersion".into(), n(9.0))])]),
            coll("users", users),
            coll("users.code", code),
            coll("rooms", rooms),
            coll("rooms.terrain", terrain),
            coll("rooms.objects", objects),
            coll("rooms.flags", flags.collect()),
        ]),
    )]);
    let spec = crate::new_spec(cfg);
    let (mut w, report) = engine::loki::import(&db.stringify(), &crate::import_opts(cfg, &spec)).unwrap_or_else(|e| fail(&e));
    crate::report_import(&report);
    w.compat = Arc::new(crate::world_compat(cfg, &spec));
    w.config = Arc::new(crate::copy_engine(&cfg.engine));
    w
}

/// `--load <dir>`: the checkpoint in `dir`, as `serve` loads one.
fn load_state(cfg: &Config, spec: &Json, dir: &str) -> World {
    let dir = std::path::Path::new(dir);
    let fail = |e: &dyn std::fmt::Display| -> ! { crate::fail(&format!("{}: {e}", engine::persist::path(dir).display())) };
    let (mut w, report) = engine::persist::load(dir, &crate::import_opts(cfg, spec)).unwrap_or_else(|| fail(&"no world")).unwrap_or_else(|e| fail(&e));
    crate::report_import(&report);
    let mut compat = cfg.compat.clone();
    compat.profile = w.compat.profile;
    w.compat = Arc::new(compat);
    w
}

/// `--tile N`: the scenario's rooms and players N times side by side, in a grid `ceil(√N)` copies wide. Each copy moves
/// by whole sectors (every room keeps its kind) and at least one walled room away from the next, and gets its own
/// ids (copy k in the first two hex digits of every 24-digit id) and usernames (`name_k` from the second copy on),
/// rewritten everywhere in its rooms and players, Memory and flag names included. The first copy starts at its
/// sector's corner (E10S30 becomes E0S0): bots that read room numbers as two digits need every copy
/// below 100, so at most a 3×3 grid of a 21-room-wide region.
fn tile(sc: &Json, copies: usize) -> Result<Json, String> {
    let rooms = sc.get("rooms").and_then(Json::as_array).ok_or("no rooms")?;
    let players = sc.get("players").and_then(Json::as_array).cloned().unwrap_or_default();
    if copies > 256 {
        return Err("--tile: at most 256 copies".into());
    }
    let mut xys = Vec::new();
    for r in rooms {
        let name = r.get("name").and_then(Json::as_str).unwrap_or("");
        xys.push(engine::room_name::RoomXY::parse_engine(name).ok_or_else(|| format!("bad room name {name:?}"))?);
    }
    let (west, north) = (xys.first().is_some_and(|p| p.x < 0), xys.first().is_some_and(|p| p.y < 0));
    if xys.iter().any(|p| (p.x < 0) != west || (p.y < 0) != north) {
        return Err("--tile: the rooms span more than one quadrant".into());
    }
    let shown = |c: i32| if c < 0 { -c - 1 } else { c };
    let (xs, ys) = (xys.iter().map(|p| shown(p.x)), xys.iter().map(|p| shown(p.y)));
    let (x0, x1, y0, y1) = (xs.clone().min().unwrap_or(0), xs.max().unwrap_or(0), ys.clone().min().unwrap_or(0), ys.max().unwrap_or(0));
    // the span plus one gap room, in whole sectors
    let (step_x, step_y) = ((x1 - x0 + 11) / 10 * 10, (y1 - y0 + 11) / 10 * 10);
    let cols = (1..).find(|c| c * c >= copies).unwrap_or(1);
    let names: Vec<String> = players.iter().filter_map(|p| p.get("username").and_then(Json::as_str)).map(String::from).collect();
    let letters = (if west { b'W' } else { b'E' }, if north { b'N' } else { b'S' });
    let text = Json::Arr(vec![Json::Arr(rooms.clone()), Json::Arr(players)]).stringify();
    let (mut all_rooms, mut all_players) = (Vec::new(), Vec::new());
    for k in 0..copies {
        let dx = step_x * (k % cols) as i32 - (x0 - x0 % 10);
        let dy = step_y * (k / cols) as i32 - (y0 - y0 % 10);
        let mut t = rename_ids(&move_rooms(&text, letters, dx, dy), k);
        if k > 0 {
            for n in &names {
                t = t.replace(&format!("\"{n}\""), &format!("\"{n}_{k}\""));
            }
        }
        let Ok(Json::Arr(mut parts)) = Json::parse(&t) else { return Err("--tile: the rewritten scenario is not JSON".into()) };
        let (Some(Json::Arr(p)), Some(Json::Arr(r))) = (parts.pop(), parts.pop()) else { return Err("--tile: no rooms".into()) };
        all_rooms.extend(r);
        all_players.extend(p);
    }
    let game_time = sc.get("gameTime").cloned().unwrap_or(Json::Null);
    Ok(Json::Obj(vec![("gameTime".into(), game_time), ("rooms".into(), Json::Arr(all_rooms)), ("players".into(), Json::Arr(all_players))]))
}

/// Every room name `<x letter><digits><y letter><digits>` in `t` that does not continue a word (`har_E11S31_3`
/// counts) with its numbers moved by dx, dy; a name that would leave the quadrant stays.
fn move_rooms(t: &str, (lx, ly): (u8, u8), dx: i32, dy: i32) -> String {
    let b = t.as_bytes();
    let digits = |i: usize| b[i..].iter().take_while(|c| c.is_ascii_digit()).count();
    let (mut out, mut done, mut i) = (String::with_capacity(t.len()), 0, 0);
    while i < b.len() {
        if b[i] == lx && (i == 0 || !b[i - 1].is_ascii_alphanumeric()) {
            let j = i + 1 + digits(i + 1);
            if j > i + 1 && b.get(j) == Some(&ly) && digits(j + 1) > 0 {
                let end = j + 1 + digits(j + 1);
                if let (Ok(x), Ok(y)) = (t[i + 1..j].parse::<i32>(), t[j + 1..end].parse::<i32>()) {
                    if x + dx >= 0 && y + dy >= 0 {
                        out.push_str(&t[done..i]);
                        out.push_str(&format!("{}{}{}{}", lx as char, x + dx, ly as char, y + dy));
                        done = end;
                    }
                }
                i = end;
                continue;
            }
        }
        i += 1;
    }
    out.push_str(&t[done..]);
    out
}

/// Every 24-hex-digit id in `t` (a whole word) with copy `k` as its first two digits.
fn rename_ids(t: &str, k: usize) -> String {
    let b = t.as_bytes();
    let word = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    let hex = |c: &u8| c.is_ascii_digit() || (b'a'..=b'f').contains(c);
    let (mut out, mut done, mut i) = (String::with_capacity(t.len()), 0, 0);
    while i + 24 <= b.len() {
        if (i == 0 || !word(b[i - 1])) && b[i..i + 24].iter().all(hex) && b.get(i + 24).is_none_or(|&c| !word(c)) {
            out.push_str(&t[done..i]);
            out.push_str(&format!("{k:02x}"));
            done = i + 2;
            i += 24;
        } else {
            i += 1;
        }
    }
    out.push_str(&t[done..]);
    out
}

/// A main.js, or a bot directory as `bots.spawn` loads it (utils.js:250: every `.js` file a module) plus each
/// `.wasm` file as a binary module named without the extension (`{binary: base64}`, as the upload tools store
/// it). Keys as translateModulesToDb.
fn load_bot(path: &str) -> Json {
    let ok = |r: std::io::Result<String>| r.unwrap_or_else(|e| crate::fail(&format!("{path}: {e}")));
    let fail = |e: std::io::Error| -> ! { crate::fail(&format!("{path}: {e}")) };
    let dir = std::path::Path::new(path);
    if !dir.is_dir() {
        return Json::Obj(vec![("main".into(), s(&ok(std::fs::read_to_string(dir))))]);
    }
    let mut files: Vec<_> = std::fs::read_dir(dir).unwrap_or_else(|e| fail(e)).flatten().map(|e| e.path()).collect();
    files.sort();
    let key = |n: &str| n.replace('.', "$DOT$").replace('/', "$SLASH$").replace('\\', "$BACKSLASH$");
    let mut modules = Vec::new();
    for f in files {
        let name = f.file_name().unwrap().to_string_lossy().to_string();
        if let Some(m) = name.strip_suffix(".js") {
            modules.push((key(m), s(&ok(std::fs::read_to_string(&f)))));
        } else if let Some(m) = name.strip_suffix(".wasm") {
            let bin = base64(&std::fs::read(&f).unwrap_or_else(|e| fail(e)));
            modules.push((key(m), Json::Obj(vec![("binary".into(), s(&bin))])));
        }
    }
    Json::Obj(modules)
}

fn base64(bytes: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for c in bytes.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            out.push(if i <= c.len() { A[(n >> (18 - 6 * i) & 63) as usize] as char } else { '=' });
        }
    }
    out
}

/// `--report-every n`: what a stretch of n ticks cost and what the world held at its end, as one line of JSON, so a long
/// run shows which stage grows and what grew with it.
fn stretch(t: &PhaseTimes, wall: f64, rt: &runtime::Runtime, w: &World) {
    let run = runtime::timing::take();
    let bytes = runtime::timing::take_bytes();
    let ticks = t.ticks.max(1) as f64;
    let per_tick = |ns: u64| ns as f64 / 1e6 / ticks;
    let mut line = format!("{{\"tick\":{},\"ms\":{:.3},\"rooms\":{:.1},\"roomUs\":{:.1},\"botCpu\":{:.2}", w.time, wall * 1000.0 / ticks, t.rooms as f64 / ticks, t.room_ns as f64 / 1e3 / t.rooms.max(1) as f64, t.bot_cpu_ms / ticks);
    for (name, ns) in PHASES.iter().zip(t.ns) {
        line.push_str(&format!(",\"{name}\":{:.3}", per_tick(ns)));
    }
    for (i, (name, ns)) in runtime::timing::STAGES.iter().zip(run).enumerate() {
        line.push_str(&format!(",\"{name}\":{:.3},\"{name} KB\":{:.0}", per_tick(ns), bytes[i] as f64 / 1024.0 / ticks));
    }
    let s = rt.stats();
    let kind = |d: &&engine::doc::DocRef| d.obj.get(Atom::intern("type")).and_then(Val::as_str) == Some("creep");
    let objects = w.rooms.iter().map(|r| r.objects.iter().count()).sum::<usize>();
    let creeps = w.rooms.iter().flat_map(|r| r.objects.iter()).filter(kind).count();
    line.push_str(&format!(",\"heapMb\":{:.1},\"heapPhysicalMb\":{:.1},\"processMb\":{:.0},\"objects\":{objects},\"creeps\":{creeps},\"errors\":{}}}", s.heap_used_mb, s.heap_physical_mb, util::sys::process_memory() as f64 / 1048576.0, t.errors));
    println!("every {line}");
}

fn report(label: &str, t: &PhaseTimes, wall: f64, rt: &runtime::Runtime, w: &World) {
    let run = runtime::timing::take();
    let bytes = runtime::timing::take_bytes();
    let ticks = t.ticks.max(1) as f64;
    println!("{label}: {} ticks in {wall:.2} s = {:.1} ticks/s ({:.1} ms/tick)", t.ticks, t.ticks as f64 / wall, wall * 1000.0 / ticks);
    println!("  per tick: {:.0} users run, {:.0} rooms processed, bot CPU {:.1} ms (sum)", t.users as f64 / ticks, t.rooms as f64 / ticks, t.bot_cpu_ms / ticks);
    println!("  room processing: {:.1} µs CPU per processed room", t.room_ns as f64 / 1e3 / t.rooms.max(1) as f64);
    if t.repeat_rooms > 0 {
        println!("  room processing repeated on one thread: {:.1} µs per room ({} rooms)", t.repeat_ns as f64 / 1e3 / t.repeat_rooms as f64, t.repeat_rooms);
    }
    if t.errors > 0 {
        println!("  {} runs ended in an error; the first: {}", t.errors, t.first_error.as_deref().unwrap_or("").lines().next().unwrap_or(""));
    }
    let s = rt.stats();
    println!("  process memory: {:.0} MB; VM heaps: {:.0} MB used, {:.0} MB physical", util::sys::process_memory() as f64 / 1048576.0, s.heap_used_mb, s.heap_physical_mb);
    let total: u64 = t.ns.iter().sum();
    for (name, ns) in PHASES.iter().zip(t.ns) {
        println!("  {name:<20} {:>9.2} ms/tick {:>5.1} %", ns as f64 / 1e6 / ticks, ns as f64 * 100.0 / total.max(1) as f64);
    }
    println!("  run phase, CPU summed over runner threads:");
    // the JS sub-stages and probes also show what V8 allocated in them
    let kb = |i: usize| if bytes[i] > 0 { format!(" {:>8.0} KB/tick", bytes[i] as f64 / 1024.0 / ticks) } else { String::new() };
    for (i, (name, ns)) in runtime::timing::STAGES.iter().zip(run).enumerate() {
        println!("    {name:<24} {:>9.2} ms/tick{}", ns as f64 / 1e6 / ticks, kb(i));
    }
    let probes = runtime::timing::STAGES.len();
    for (i, ns) in run[probes..].iter().enumerate().filter(|(_, ns)| **ns > 0) {
        println!("      probe mark({:<2})           {:>9.3} ms/tick{}", i + 4, *ns as f64 / 1e6 / ticks, kb(probes + i));
    }
    let mut runs: Vec<_> = runtime::timing::take_runs().into_iter().map(|(u, (n, ns))| (ns as f64 / 1e6 / n.max(1) as f64, u)).collect();
    runs.sort_by(|a, b| b.0.total_cmp(&a.0));
    println!("  run phase by user, slowest first (build input through output):");
    for (ms, u) in runs {
        let name = w.users.get(u as usize).and_then(|s| s.doc.get(Atom::intern("username")).and_then(Val::as_str).map(str::to_string)).unwrap_or_else(|| u.to_string());
        println!("    {name:<24} {ms:>9.2} ms/run");
    }
    let fallbacks = runtime::timing::STORE_FALLBACKS.swap(0, std::sync::atomic::Ordering::Relaxed);
    println!("  runs stored on the system isolate (not the native path): {fallbacks}");
    rt.collect_cpu_profiles();
    if let Some(p) = runtime::jsprof::report(std::env::var("CHITIN_JS_PROFILE_TOP").ok().and_then(|v| v.parse().ok()).unwrap_or(40)) {
        print!("{p}");
    }
}

pub fn run(cfg: Config, args: &Args) {
    runtime::init_v8();
    runtime::timing::BENCH.store(true, std::sync::atomic::Ordering::Relaxed);
    let num = |k: &str, d: u64| args.flag(k).and_then(|v| v.parse::<u64>().ok()).unwrap_or(d);
    let (bots, ticks, warmup) = (num("bots", 100) as usize, num("ticks", 200), num("warmup", 20));
    let max_mb = num("max-memory-mb", 4096);
    const MB_PER_BOT: u64 = 20;
    if bots as u64 * MB_PER_BOT > max_mb {
        crate::fail(&format!("{bots} bots need about {} MB (~{MB_PER_BOT} MB per VM), over --max-memory-mb {max_mb}", bots as u64 * MB_PER_BOT));
    }
    let modules = args.flag("bot").map(load_bot).unwrap_or_else(|| Json::Obj(vec![("main".into(), s(M1_BOT))]));
    let data = std::env::temp_dir().join(format!("chitin-bench-{}", std::process::id()));
    std::fs::create_dir_all(&data).unwrap();
    let spec = crate::new_spec(&cfg);

    let started = Instant::now();
    let pool = util::pool::Pool::new(crate::logical_cores());
    let mut w = match (args.flag("load"), args.flag("scenario")) {
        (Some(dir), _) => {
            let w = load_state(&cfg, &spec, dir);
            println!("world: {} rooms, {} users loaded at tick {} in {:.1} s", w.rooms.len(), w.users.len(), w.time, started.elapsed().as_secs_f64());
            w
        }
        (None, Some(path)) => {
            let copies = num("tile", 1).max(1) as usize;
            let w = load_scenario(&cfg, path, copies);
            let from = if copies > 1 { format!("{copies} copies of {path}") } else { path.to_string() };
            println!("world: {} rooms, {} users from {from} at tick {} in {:.1} s", w.rooms.len(), w.users.len(), w.time, started.elapsed().as_secs_f64());
            w
        }
        (None, None) => {
            let mut w = crate::generate(&cfg, &spec, &pool);
            println!("world: {} rooms generated in {:.1} s", w.rooms.len(), started.elapsed().as_secs_f64());
            let placed = place_bots(&mut w, bots, &modules);
            println!("bots: {placed} placed (requested {bots})");
            w
        }
    };
    println!("memory before the runtime: {:.0} MB", util::sys::process_memory() as f64 / 1048576.0);
    let env = &w.env;
    w.env = Arc::new(engine::world::EnvState { tick_rate_ms: 0, paused: false, last_accessible_update: env.last_accessible_update, status_dirty: env.status_dirty });

    // the mods enabled as for serve (`mods` in the config, `--mods`)
    let (mod_names, registry, game_js) = crate::mods::load(&cfg.mods, &data, None).unwrap_or_else(|e| crate::fail(&e));
    if !mod_names.is_empty() {
        println!("mods: {}", mod_names.join(", "));
    }
    w.mods = mod_names.iter().map(|&n| n.into()).collect();

    let rcfg = crate::runtime_config(&cfg, w.compat.profile);
    println!("runner threads: {}", rcfg.runner_threads);
    let rt = Arc::new(runtime::Runtime::new(rcfg, runtime::Bundle { files: runtime::bundle::FILES, mod_game_js: game_js }));
    let (tx, rx) = std::sync::mpsc::channel();
    let published = Arc::new(backend::Published::new(w.view()));
    let backend = backend::start(backend::BackendDeps {
        published: published.clone(),
        commands: tx,
        cli: Arc::new(crate::cli::Cli(rt.clone())),
        config: backend::BackendConfig {
            host: "127.0.0.1".into(),
            port: 0,
            cli_host: "127.0.0.1".into(),
            cli_port: 0,
            cli_http: false,
            max_connections: 64,
            request_timeout_s: 120,
            server_password: None,
            steam: "off".into(),
        },
        routes: registry.routes,
        socket_channels: registry.socket_channels,
        server_data: registry.server_data,
    });
    let hooks: &'static engine::hooks::EngineHooks = Box::leak(Box::new(registry.engine));
    let mut lp = crate::tick::TickLoop::new(crate::tick::Live { runtime: rt.clone(), pool, published, backend }, hooks, rx);
    lp.print_console = args.flag("console").is_some();
    lp.p3_repeat = num("p3-repeat", 0) as u32;

    // every 10 ticks: wall time and memory, so a long run shows where it goes
    let progress = |w: &World, since: &mut Instant| {
        let mb = util::sys::process_memory() >> 20;
        if mb > max_mb {
            eprintln!("bench stopped at tick {}: {mb} MB is over --max-memory-mb {max_mb}", w.time);
            std::process::exit(1);
        }
        if w.time % 10 == 0 {
            eprintln!("  tick {}: {:.0} ms/tick, {:.0} MB", w.time, since.elapsed().as_secs_f64() * 100.0, util::sys::process_memory() as f64 / 1048576.0);
            *since = Instant::now();
        }
    };
    let (t0, mut since) = (Instant::now(), Instant::now());
    for _ in 0..warmup {
        lp.tick(&mut w);
        progress(&w, &mut since);
    }
    report("warmup", &lp.times, t0.elapsed().as_secs_f64(), &rt, &w);
    lp.times = PhaseTimes::default();
    #[cfg(windows)]
    let sampler = crate::sampler::start_from_env();
    let t1 = Instant::now();
    let (every, save_every) = (num("report-every", 0), num("save-every", 0));
    let mut stretch_from = Instant::now();
    for i in 1..=ticks {
        lp.tick(&mut w);
        progress(&w, &mut since);
        if every > 0 && i % every == 0 {
            stretch(&lp.times, stretch_from.elapsed().as_secs_f64(), &rt, &w);
            lp.times = PhaseTimes::default();
            stretch_from = Instant::now();
        }
        if save_every > 0 && i % save_every == 0 {
            let dir = std::path::Path::new(args.flag("save").unwrap_or("saves")).join(w.time.to_string());
            std::fs::create_dir_all(&dir).and_then(|_| engine::persist::save(&dir, &w.view(), w.compat.profile)).unwrap_or_else(|e| crate::fail(&format!("{}: {e}", dir.display())));
        }
    }
    #[cfg(windows)]
    let native = sampler.map(|s| s.report(std::env::var("CHITIN_JS_PROFILE_TOP").ok().and_then(|v| v.parse().ok()).unwrap_or(40)));
    #[cfg(not(windows))]
    let native: Option<String> = None;
    report("measured", &lp.times, t1.elapsed().as_secs_f64(), &rt, &w);
    print!("{}", native.unwrap_or_default());
    let creeps = w.rooms.iter().flat_map(|r| r.objects.iter()).filter(|d| d.obj.get(Atom::intern("type")).and_then(Val::as_str) == Some("creep")).count();
    println!("end: tick {}, {creeps} creeps in the world", w.time);
    if let Some(dir) = args.flag("save") {
        let dir = std::path::Path::new(dir);
        std::fs::create_dir_all(dir).and_then(|_| engine::persist::save(dir, &w.view(), w.compat.profile)).unwrap_or_else(|e| crate::fail(&format!("{}: {e}", dir.display())));
        println!("saved {}", engine::persist::path(dir).display());
    }
    // V8 writes some recordings as an isolate is disposed (--turbo-profiling-output: builtins PGO, ARCHITECTURE.md §2.9)
    rt.reset_all("bench end");
    let _ = std::fs::remove_dir_all(&data);
}

#[cfg(test)]
mod tests {
    #[test]
    fn base64() {
        let back = |s: &str| engine::runner::data::buffer_from_base64(&super::base64(s.as_bytes()));
        for s in ["", "f", "fo", "foo", "foob", "fooba", "foobar"] {
            assert_eq!(back(s), s.as_bytes());
        }
        assert_eq!(super::base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(super::base64(b"fo"), "Zm8=");
    }

    #[test]
    fn tile_moves_rooms_and_renames_ids_and_users() {
        use util::json::Json;
        let sc = Json::parse(
            r#"{"gameTime":5,"rooms":[{"name":"E11S31","terrain":"0123456789abcdef0123456789","objects":[
                {"_id":"0123456789abcdef01234567","type":"source"},{"type":"flag","name":"har_E11S31_1"}]}],
               "players":[{"username":"Bob","id":"aaaaaaaaaaaaaaaaaaaaaaaa",
                "memory":"{\"r\":\"E11S31\",\"o\":\"0123456789abcdef01234567\",\"x\":\"xE11S31 E1S1\"}"}]}"#,
        )
        .unwrap();
        // a one-room region: copies one sector apart, the first at its sector's corner
        let room = |name: &str, k: &str| {
            format!(r#"{{"name":"{name}","terrain":"0123456789abcdef0123456789","objects":[{{"_id":"{k}23456789abcdef01234567","type":"source"}},{{"type":"flag","name":"har_{name}_1"}}]}}"#)
        };
        let player = |name: &str, k: &str, room: &str, near: &str| {
            format!(r#"{{"username":"{name}","id":"{k}aaaaaaaaaaaaaaaaaaaaaa","memory":"{{\"r\":\"{room}\",\"o\":\"{k}23456789abcdef01234567\",\"x\":\"xE11S31 {near}\"}}"}}"#)
        };
        assert_eq!(
            super::tile(&sc, 2).unwrap().stringify(),
            format!(
                r#"{{"gameTime":5,"rooms":[{},{}],"players":[{},{}]}}"#,
                room("E1S1", "00"),
                room("E11S1", "01"),
                player("Bob", "00", "E1S1", "E1S1"),
                // E1S1 would leave the quadrant in both copies
                player("Bob_1", "01", "E11S1", "E1S1")
            )
        );
        assert!(super::tile(&Json::parse(r#"{"rooms":[{"name":"E1S1"},{"name":"W1S1"}]}"#).unwrap(), 2).is_err());
    }
}
