//! The admin CLI's server side (ARCHITECTURE.md §8.6): `CliEval` for the backend's listener over the server
//! isolate, and the `CliHost` its natives reach. Reads come from the published view, writes run as Commands at
//! the next P0 (also while paused) through `engine::coll` and the map, cron and backend ports; env keys the world
//! does not model live here in memory with their expirations (ENV-OPS, ENV-EXPIRY).

use backend::{CliEval, CliSession, Published};
use engine::doc::{Id, Val};
use engine::policy::Profile;
use engine::world::{Command, World};
use runtime::server_isolate::{CliHost, ServerIsolate};
use runtime::Runtime;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use util::json::Json;

pub struct Host {
    pub published: Arc<Published>,
    pub commands: Mutex<Sender<Command>>,
    /// None in tests without player VMs.
    pub runtime: Option<Arc<Runtime>>,
    pub data: PathBuf,
    pub profile: Profile,
    pub run_ticks: Arc<AtomicU64>,
    pub mod_crons: Vec<&'static str>,
    pub hooks: &'static engine::hooks::EngineHooks,
    /// `system.resetAllData()`: the world `chitin new` would make.
    pub reset: Box<dyn Fn() -> Result<World, String> + Send + Sync>,
    /// Env keys outside the world: value and expiry (ms).
    env: Mutex<HashMap<String, (Json, Option<i64>)>>,
    /// Mods' console natives (`Registry::cli_calls`), reached when `call` names none of the host's own commands.
    pub mod_calls: Vec<(&'static str, backend::mods::CliCallFn)>,
}

impl Host {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        published: Arc<Published>,
        commands: Sender<Command>,
        runtime: Option<Arc<Runtime>>,
        data: PathBuf,
        profile: Profile,
        run_ticks: Arc<AtomicU64>,
        hooks: &'static engine::hooks::EngineHooks,
        reset: Box<dyn Fn() -> Result<World, String> + Send + Sync>,
    ) -> Host {
        let mod_crons = hooks.crons.iter().map(|c| c.0).collect();
        Host { published, commands: Mutex::new(commands), runtime, data, profile, run_ticks, mod_crons, hooks, reset, env: Mutex::default(), mod_calls: Vec::new() }
    }

    /// Runs `f` on the tick thread. Reads come from the published view, and the tick's own publish is up to a tick away,
    /// or never while paused, so `f`'s result is published before the caller gets it: a later read sees the write.
    fn run<R: Send + 'static>(&self, f: impl FnOnce(&mut World) -> Result<R, String> + Send + 'static) -> Result<R, String> {
        let tx = self.commands.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let published = self.published.clone();
        backend::call(&tx, move |w| {
            let r = f(w);
            published.set(w.view());
            r
        })
    }

    /// The bot AIs: `<data>/bots/<name>/` (mods.json's `bots` map in vanilla).
    fn bots(&self) -> Vec<(String, PathBuf)> {
        let mut out: Vec<(String, PathBuf)> = std::fs::read_dir(self.data.join("bots"))
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.path().is_dir())
            .filter_map(|e| Some((e.file_name().to_str()?.to_string(), e.path())))
            .collect();
        out.sort();
        out
    }

    /// A modelled env key's value in the published world (None: not one of them, or empty).
    fn world_env(&self, key: &str) -> Option<Option<Json>> {
        let v = self.published.get();
        let user = |prefix: &str| key.strip_prefix(prefix).and_then(Id::new).and_then(|id| v.user_ids.get(&id).copied()).map(|u| v.users[u as usize].clone());
        let name = |r: usize| v.meta.rooms[r].name.to_string();
        Some(match key {
            "gameTime" => Some(Json::Num(v.time as f64)),
            "mainLoopPaused" => Some(Json::Str(if v.env.paused { "1" } else { "0" }.into())),
            "tickRate" => Some(Json::Num(v.env.tick_rate_ms as f64)),
            "activeRooms" => Some(Json::Arr(v.persist.active.iter().map(|&r| Json::Str(name(r as usize))).collect())),
            "accessibleRooms" | "roomStatusData" | "terrainData" => Json::parse(&engine::loki::env_data(&v, self.profile)).ok()?.get(key).cloned(),
            "roomEventLog:" => Some(Json::Obj(
                (0..v.rooms.len()).filter(|&r| !v.meta.rooms[r].removed && !v.rooms[r].event_log.is_empty()).map(|r| (name(r), Json::Str(v.rooms[r].event_log.to_string()))).collect(),
            )),
            _ if key.starts_with("memory:") => user("memory:").map(|u| Json::Str(u.memory.to_string_lossy())).filter(|m| m.as_str() != Some("")),
            _ if key.starts_with("memorySegments:") => user("memorySegments:").map(|u| Json::Obj(u.segments.iter().map(|(n, s)| (n.to_string(), Json::Str(s.to_string_lossy()))).collect())),
            _ if key.starts_with("publicMemorySegments:") => user("publicMemorySegments:").map(|u| Json::Str(u.public_segments.to_string())).filter(|s| s.as_str() != Some("")),
            _ if key.starts_with("mapView:") => {
                let r = v.meta.by_name(&key["mapView:".len()..])?;
                Some(Json::Str(v.rooms[r as usize].map_view.to_string())).filter(|s| s.as_str() != Some(""))
            }
            _ => return None,
        })
    }

    /// `env.set` of a key the world models (Some: applied), else None.
    fn world_set(&self, key: &str, value: Json) -> Result<bool, String> {
        let key = key.to_string();
        self.run(move |w| engine::coll::env_set(w, &key, &Val::from_trusted_json(&value)))
    }

    fn get(&self, key: &str) -> Option<Json> {
        if let Some(v) = self.world_env(key) {
            return v;
        }
        let now = engine::policy::real_now_ms("ENV-EXPIRY");
        let mut env = self.env.lock().unwrap_or_else(|e| e.into_inner());
        if env.get(key).is_some_and(|(_, exp)| exp.is_some_and(|t| now > t)) {
            env.remove(key);
        }
        env.get(key).map(|(v, _)| v.clone())
    }

    fn set(&self, key: &str, value: Json) -> Result<(), String> {
        if self.world_set(key, value.clone())? {
            return Ok(());
        }
        let mut env = self.env.lock().unwrap_or_else(|e| e.into_inner());
        let exp = env.get(key).and_then(|(_, e)| *e);
        env.insert(key.to_string(), (value, exp));
        Ok(())
    }
}

fn arg(args: &Json, i: usize) -> Json {
    args.at(i).cloned().unwrap_or(Json::Null)
}

fn text(v: &Json) -> String {
    match v {
        Json::Str(s) => s.clone(),
        Json::Null => "null".into(),
        other => other.stringify(),
    }
}

impl CliHost for Host {
    fn docs(&self, coll: &str) -> Option<String> {
        let v = self.published.get();
        let docs = engine::loki::collection(&v, coll)?;
        let mut s = String::from("[");
        for (i, (_, d)) in docs.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            s.push_str(d);
        }
        s.push(']');
        Some(s)
    }

    fn write(&self, coll: &str, writes: &str) -> Result<String, String> {
        let parsed = Json::parse(writes).map_err(|e| e.to_string())?;
        let mut list = Vec::new();
        for w in parsed.as_array().map_or(&[][..], |a| a.as_slice()) {
            // The host's own input: a new field must not be lost because players spent the names `from_json` allows.
            let doc = || match w.get("doc").map(Val::from_trusted_json) {
                Some(Val::Obj(o)) => Ok(o),
                _ => Err("Document needs to be an object".to_string()),
            };
            let id = || w.get("id").map(text).unwrap_or_default().into_boxed_str();
            list.push(match w.get("op").and_then(Json::as_str) {
                Some("insert") => engine::coll::Write::Insert(doc()?),
                Some("replace") => engine::coll::Write::Replace(id(), doc()?),
                _ => engine::coll::Write::Remove(id()),
            });
        }
        let coll = coll.to_string();
        let inserted = self.run(move |w| engine::coll::write(w, &coll, list))?;
        Ok(Json::Arr(inserted.into_iter().map(|o| Val::Obj(o).to_json()).collect()).stringify())
    }

    fn env(&self, op: &str, args: &str) -> Result<Option<String>, String> {
        let args = Json::parse(args).map_err(|e| e.to_string())?;
        let key = text(&arg(&args, 0));
        let now = engine::policy::real_now_ms("ENV-EXPIRY");
        let hash = |h: Option<Json>| match h {
            Some(Json::Obj(o)) => o,
            _ => Vec::new(),
        };
        let out = match op {
            "get" => self.get(&key),
            "mget" => Some(Json::Arr(arg(&args, 0).as_array().map_or(&[][..], |a| a.as_slice()).iter().map(|k| self.get(&text(k)).unwrap_or(Json::Null)).collect())),
            "set" => {
                self.set(&key, arg(&args, 1))?;
                Some(arg(&args, 1))
            }
            "setex" => {
                self.set(&key, arg(&args, 2))?;
                let secs = arg(&args, 1).as_f64().unwrap_or(f64::NAN);
                if let Some(e) = self.env.lock().unwrap_or_else(|e| e.into_inner()).get_mut(&key) {
                    e.1 = Some(now + (secs * 1000.0) as i64);
                }
                None
            }
            "expire" => {
                let secs = arg(&args, 1).as_f64().unwrap_or(f64::NAN);
                if let Some(e) = self.env.lock().unwrap_or_else(|e| e.into_inner()).get_mut(&key) {
                    e.1 = Some(now + (secs * 1000.0) as i64);
                }
                None
            }
            "ttl" => {
                let env = self.env.lock().unwrap_or_else(|e| e.into_inner());
                Some(Json::Num(match env.get(&key).and_then(|(_, e)| *e) {
                    Some(t) if t >= now => (t - now) as f64 / 1000.0,
                    _ => -1.0,
                }))
            }
            "del" => {
                let k = key.clone();
                if self.world_env(&key).is_some() {
                    let applied = self.run(move |w| engine::coll::env_del(w, &k))?;
                    Some(Json::Num(if applied { 1.0 } else { 0.0 }))
                } else {
                    let mut env = self.env.lock().unwrap_or_else(|e| e.into_inner());
                    let had = env.get(&key).is_some_and(|(v, _)| !matches!(v, Json::Null | Json::Bool(false)) && v.as_str() != Some("") && v.as_f64() != Some(0.0));
                    if had {
                        env.remove(&key);
                    }
                    Some(Json::Num(if had { 1.0 } else { 0.0 }))
                }
            }
            "hget" => self.get(&key).and_then(|h| h.get(&text(&arg(&args, 1))).cloned()),
            "hmget" => {
                let h = self.get(&key);
                Some(Json::Arr(arg(&args, 1).as_array().map_or(&[][..], |a| a.as_slice()).iter().map(|f| h.as_ref().and_then(|h| h.get(&text(f))).cloned().unwrap_or(Json::Null)).collect()))
            }
            "hset" | "hmset" => {
                let mut h = hash(self.get(&key));
                let add: Vec<(String, Json)> = if op == "hset" { vec![(text(&arg(&args, 1)), arg(&args, 2))] } else { hash(Some(arg(&args, 1))) };
                for (f, v) in add {
                    match h.iter_mut().find(|(k, _)| *k == f) {
                        Some(slot) => slot.1 = v,
                        None => h.push((f, v)),
                    }
                }
                self.set(&key, Json::Obj(h.clone()))?;
                Some(if op == "hset" { h.iter().find(|(k, _)| *k == text(&arg(&args, 1))).map(|(_, v)| v.clone()).unwrap_or(Json::Null) } else { Json::Obj(h) })
            }
            "sadd" => {
                let mut set = match self.get(&key) {
                    Some(Json::Arr(a)) => a,
                    _ => Vec::new(),
                };
                let add = match arg(&args, 1) {
                    Json::Arr(a) => a,
                    v => vec![v],
                };
                for v in add {
                    if !set.contains(&v) {
                        set.push(v);
                    }
                }
                self.set(&key, Json::Arr(set))?;
                self.get(&key)
            }
            "smembers" => self.get(&key),
            _ => return Err(format!("env.{op} is not a function")),
        };
        Ok(out.map(|j| j.stringify()))
    }

    fn call(&self, name: &str, args: &str) -> Result<String, String> {
        let args = Json::parse(args).map_err(|e| e.to_string())?;
        let room = text(&arg(&args, 0));
        let ok = |j: Json| Ok(j.stringify());
        match name {
            "bots" => ok(Json::Obj(self.bots().into_iter().map(|(n, p)| (n, Json::Str(p.display().to_string()))).collect())),
            "loadBot" => {
                // utils.js:250 loadBot
                let Some((_, dir)) = self.bots().into_iter().find(|(n, _)| *n == room) else {
                    return Err(format!("Bot AI with the name \"{room}\" doesn't exist"));
                };
                let main = dir.join("main.js");
                if !main.exists() {
                    return Err(format!("ENOENT: no such file or directory, stat '{}'", main.display()));
                }
                let mut modules = Vec::new();
                let mut files: Vec<_> = std::fs::read_dir(&dir).map_err(|e| e.to_string())?.flatten().collect();
                files.sort_by_key(|e| e.file_name());
                for f in files {
                    let file = f.file_name().to_string_lossy().to_string();
                    if let Some(m) = file.strip_suffix(".js") {
                        modules.push((m.to_string(), Json::Str(std::fs::read_to_string(f.path()).map_err(|e| e.to_string())?)));
                    }
                }
                ok(Json::Obj(modules))
            }
            "strongholdTemplates" => ok(engine::strongholds::data().as_obj().and_then(|d| d.get(engine::doc::Atom::intern("templates"))).map_or(Json::Obj(vec![]), Val::to_json)),
            "spawnStronghold" => {
                if matches!(arg(&args, 0), Json::Null | Json::Bool(false)) || room.is_empty() {
                    return Err("invalid room".into());
                }
                let opts = arg(&args, 1);
                let num = |k: &str| opts.get(k).and_then(Json::as_f64);
                let o = engine::cron::strongholds::SpawnOpts {
                    template_name: opts.get("templateName").map(text),
                    x: opts.get("x").map(|v| v.as_f64().unwrap_or(f64::NAN)),
                    y: opts.get("y").map(|v| v.as_f64().unwrap_or(f64::NAN)),
                    user: opts.get("user").filter(|u| !matches!(u, Json::Null)).map(text),
                    deploy_time: num("deployTime"),
                };
                let mut returned = opts.clone();
                if num("deployTime").filter(|d| *d != 0.0).is_none() {
                    if let Json::Obj(o) = &mut returned {
                        o.retain(|(k, _)| k != "deployTime");
                        o.push(("deployTime".into(), Json::Num(1.0)));
                    }
                }
                let spawned = self.run(move |w| {
                    let r = w.meta.by_name(&room).ok_or("TypeError: Cannot destructure property 'terrain' of '(intermediate value)[0]' as it is null.")?;
                    let mut cx = engine::cron::CronCtx::new(w.seed, w.time, w.clock.at(w.time), "cli");
                    cx.unit(&room);
                    let s = engine::cron::strongholds::spawn_stronghold(w, &mut cx, r, &o)?;
                    let active: Vec<Json> = w.active.list().iter().map(|&r| Json::Str(w.meta.rooms[r as usize].name.to_string())).collect();
                    Ok((matches!(s, engine::cron::strongholds::Spawned::OptsReturned), active))
                })?;
                // spawnStronghold resolves `opts` (template and spot given) or utils.activateRoom's sadd result
                ok(if spawned.0 { returned } else { Json::Arr(spawned.1) })
            }
            "expandStronghold" => self.run(move |w| {
                let id = Id::new(&room).ok_or("no core")?;
                let core = w.index.objects.get(&id).and_then(|loc| w.rooms[loc.room as usize].objects.iter().find(|d| d.id() == id).cloned()).ok_or("no core")?;
                let mut cx = engine::cron::CronCtx::new(w.seed, w.time, w.clock.at(w.time), "cli");
                cx.unit(&room);
                Ok(Json::Bool(engine::cron::strongholds::expand_stronghold(w, &mut cx, &core)?).stringify())
            }),
            "cronjobs" => ok(Json::Arr(engine::cron::JOBS.iter().map(|j| j.0).chain(self.mod_crons.iter().copied()).map(|n| Json::Str(n.into())).collect())),
            "runCronjob" => {
                let hooks = self.hooks;
                self.run(move |w| {
                    let (g, now) = (w.time, w.clock.at(w.time));
                    if let Some(job) = engine::cron::JOBS.iter().find(|j| j.0 == room) {
                        (job.2)(w, &mut engine::cron::CronCtx::new(w.seed, g, now, job.0))?;
                    } else if let Some(job) = hooks.crons.iter().find(|j| j.0 == room) {
                        (job.2)(w, &mut engine::cron::CronCtx::new(w.seed, g, now, job.0));
                    }
                    Ok("null".to_string())
                })
            }
            "respawnUser" => self.run(move |w| {
                if let Some(u) = Id::new(&room).and_then(|id| w.user_ids.get(&id).copied()) {
                    backend::api::user::respawn_user(w, u)?;
                } else if !w.users.iter().any(|s| s.doc.get(engine::doc::Atom::intern("username")).and_then(Val::as_str) == Some(engine::constants::SYSTEM_USERNAME)) {
                    return Err("no system user".into());
                }
                Ok("null".to_string())
            }),
            "generateRoom" => {
                let opts = engine::mapgen::GenRoomOpts::from_json(&arg(&args, 1));
                self.run(move |w| {
                    let mut rng = util::rng::Rng::new(engine::policy::seed(w.seed, "cli-generateRoom", &[w.time, engine::policy::real_now_ms("CLI-GEN") as u64]));
                    engine::mapgen::generate_room_cmd(w, &room, &opts, &mut rng)?;
                    Ok("null".to_string())
                })
            }
            "openRoom" => {
                let ts = arg(&args, 1).as_f64();
                self.run(move |w| Ok(if engine::mapgen::open_room(w, &room, ts) { r#"{"modified":1}"# } else { "{}" }.to_string()))
            }
            "closeRoom" => self.run(move |w| Ok(if engine::mapgen::close_room(w, &room) { r#"{"modified":1}"# } else { "{}" }.to_string())),
            "removeRoom" => self.run(move |w| engine::mapgen::remove_room(w, &room).map(|_| "null".to_string())),
            // the backend renders map PNGs from terrain on request
            "updateRoomImageAssets" | "removeRoomImage" => ok(Json::Null),
            "updateTerrainData" => self.run(|w| {
                engine::mapgen::update_terrain_data_cmd(w);
                Ok("null".to_string())
            }),
            "resetAllData" => {
                let fresh = (self.reset)()?;
                let slot = Arc::new(Mutex::new(Some(fresh)));
                self.run(move |w| {
                    if let Some(f) = slot.lock().unwrap_or_else(|e| e.into_inner()).take() {
                        *w = f;
                    }
                    Ok(())
                })?;
                if let Some(rt) = &self.runtime {
                    rt.reset_all("resetAllData");
                }
                ok(Json::Null)
            }
            "publish" => {
                if room == "serverMessage" {
                    backend::socket::system::send_server_message(&self.published, &text(&arg(&args, 1)));
                }
                ok(Json::Null)
            }
            "runTicks" => {
                let n = arg(&args, 0).as_f64().filter(|n| n.is_finite() && *n > 0.0).unwrap_or(0.0) as u64;
                self.run_ticks.fetch_add(n, Ordering::Relaxed);
                ok(Json::Null)
            }
            "save" => {
                engine::persist::save(&self.data, &self.published.get(), self.profile).map_err(|e| e.to_string())?;
                ok(Json::Null)
            }
            "resetUserRuntime" => {
                let v = self.published.get();
                let u = *v.usernames.get(room.to_lowercase().as_str()).ok_or_else(|| format!("User {room} not found"))?;
                if let Some(rt) = &self.runtime {
                    rt.reset_user(u, "CLI resetUserRuntime");
                }
                ok(Json::Null)
            }
            _ => match self.mod_calls.iter().find(|(n, _)| *n == name) {
                Some((_, f)) => {
                    let commands = self.commands.lock().unwrap_or_else(|e| e.into_inner()).clone();
                    let ctx = backend::mods::CliCallCtx { published: &self.published, commands: &commands, data_dir: &self.data };
                    f(&args, &ctx).map(|j| j.stringify())
                }
                None => Err(format!("unknown CLI command {name}")),
            },
        }
    }
}

/// The backend's CLI evaluator: one server-isolate session per connection.
pub struct Cli(pub Arc<Runtime>);

impl CliEval for Cli {
    fn open(&self) -> Box<dyn CliSession> {
        let iso = self.0.clone();
        match iso.server().open_session() {
            Ok(id) => Box::new(Session { rt: iso, id: Some(id), error: None }),
            Err(e) => Box::new(Session { rt: iso, id: None, error: Some(e) }),
        }
    }

    /// One session per HTTP CLI request (`cli.http`), closed when the request is answered.
    fn open_launcher(&self) -> Option<Box<dyn CliSession>> {
        let iso = self.0.clone();
        Some(match iso.server().open_launcher_session() {
            Ok(id) => Box::new(Session { rt: iso, id: Some(id), error: None }),
            Err(e) => Box::new(Session { rt: iso, id: None, error: Some(e) }),
        })
    }
}

/// What stands behind the CLI port with `cli.off`. The console has no password and runs anything, so this runs nothing.
pub struct Closed;

impl CliEval for Closed {
    fn open(&self) -> Box<dyn CliSession> {
        Box::new(Closed)
    }
}

impl CliSession for Closed {
    fn greeting(&self) -> String {
        "The admin console is off on this server.\n".to_string()
    }

    fn eval(&mut self, _line: &str) -> String {
        String::new()
    }
}

struct Session {
    rt: Arc<Runtime>,
    id: Option<u64>,
    error: Option<String>,
}

impl CliSession for Session {
    fn greeting(&self) -> String {
        let server: &ServerIsolate = self.rt.server();
        match &self.error {
            Some(e) => format!("CLI unavailable: {e}\r\n"),
            None => server.greeting(),
        }
    }
    fn eval(&mut self, line: &str) -> String {
        match self.id {
            Some(id) => self.rt.server().eval(id, line),
            None => String::new(),
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Some(id) = self.id {
            self.rt.server().close_session(id);
        }
    }
}

/// tools/oracle/cli.js: the official backend/lib/cli/*.js behind the official server.js listener, over the real
/// storage db.js. The same db.json is imported here, the same lines run through the server isolate and this
/// host, and every output, the collections afterwards and the modelled env keys must be equal, up to what
/// cannot be shared: generated ids (named by first appearance), bot badges, registration dates, the
/// `t<timestamp>` branch of bots.reload, node's stack frames and the greeting's version.
#[cfg(test)]
mod oracle {
    use super::*;
    use engine::world::Command;

    /// Stack frames dropped; a bot AI's directory (` - name [dir]`, temp dirs on both sides) as `<dir>`.
    fn frames(s: &str) -> String {
        s.lines()
            .filter(|l| !l.trim_start().starts_with("at "))
            .map(|l| match (l.starts_with(" - "), l.find(" ["), l.ends_with(']') && (l.contains('/') || l.contains('\\'))) {
                (true, Some(i), true) => format!("{} [<dir>]", &l[..i]),
                _ => l.to_string(),
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Generated ids (not in `known`) → `#n` wherever they occur, plus the per-field masks.
    fn canon(v: &Json, known: &std::collections::HashSet<String>, names: &mut Vec<String>) -> Json {
        match v {
            Json::Arr(a) => Json::Arr(a.iter().map(|x| canon(x, known, names)).collect()),
            Json::Obj(o) => {
                let bot = o.iter().any(|(k, _)| k == "bot");
                let reload = o.iter().any(|(k, x)| k == "branch" && x.as_str().is_some_and(|b| b.starts_with('t') && b[1..].bytes().all(|c| c.is_ascii_digit())));
                Json::Obj(
                    o.iter()
                        .map(|(k, x)| {
                            let masked = (k == "registeredDate") || (bot && k == "badge") || (reload && (k == "branch" || k == "timestamp"));
                            (k.clone(), if masked { Json::Str("<masked>".into()) } else { canon(x, known, names) })
                        })
                        .collect(),
                )
            }
            Json::Str(s) if s.len() >= 10 && !known.contains(s) && s.bytes().all(|c| c.is_ascii_hexdigit()) => {
                let i = names.iter().position(|n| n == s).unwrap_or_else(|| {
                    names.push(s.clone());
                    names.len() - 1
                });
                Json::Str(format!("#{}", i + 1))
            }
            _ => v.clone(),
        }
    }

    #[test]
    fn cli_session_oracle() {
        runtime::init_v8();
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/oracle/cli");
        let text = std::fs::read_to_string(dir.join("session.ndjson")).unwrap();
        let case = Json::parse(text.lines().next().unwrap()).unwrap();
        let sc = case.get("scenario").unwrap();
        let expected = case.get("result").unwrap();
        let now = sc.get("nowMs").and_then(Json::as_f64).unwrap() as i64;
        let db = sc.get("db").unwrap().stringify();
        let opts = engine::loki::ImportOpts { seed: 1, profile: Profile::Private, now_ms: now, nominal_tick_ms: 1000 };
        let (w, _) = engine::loki::import(&db, &opts).unwrap();
        let mut known: std::collections::HashSet<String> = std::collections::HashSet::new();
        fn ids(v: &Json, out: &mut std::collections::HashSet<String>) {
            match v {
                Json::Arr(a) => a.iter().for_each(|x| ids(x, out)),
                Json::Obj(o) => o.iter().for_each(|(k, x)| {
                    if k == "_id" {
                        if let Some(s) = x.as_str() {
                            out.insert(s.to_string());
                        }
                    }
                    ids(x, out)
                }),
                _ => {}
            }
        }
        ids(sc.get("db").unwrap(), &mut known);

        // P0 stand-in: Commands applied in arrival order, and no tick to publish, as in a paused world
        let world = Arc::new(Mutex::new(w));
        let published = Arc::new(Published::new(world.lock().unwrap().view()));
        let (tx, rx) = std::sync::mpsc::channel::<Command>();
        {
            let world = world.clone();
            std::thread::spawn(move || {
                for c in rx {
                    let mut w = world.lock().unwrap();
                    let r = (c.apply)(&mut w);
                    if let Some(t) = c.reply {
                        let _ = t.try_send(r);
                    }
                }
            });
        }
        let data = std::env::temp_dir().join(format!("chitin-cli-oracle-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data);
        if let Some(Json::Obj(bots)) = sc.get("bots") {
            for (name, files) in bots {
                let d = data.join("bots").join(name);
                std::fs::create_dir_all(&d).unwrap();
                for (f, src) in files.as_object().unwrap() {
                    std::fs::write(d.join(f), src.as_str().unwrap()).unwrap();
                }
            }
        }
        let hooks: &'static engine::hooks::EngineHooks = Box::leak(Box::default());
        let host = Host::new(published.clone(), tx, None, data.clone(), Profile::Private, Arc::new(AtomicU64::new(0)), hooks, Box::new(|| Err("no reset here".into())));
        let iso = ServerIsolate::default();
        iso.set_host(Arc::new(host));
        let id = iso.open_session().unwrap();

        let greeting = iso.greeting().replace(&format!("v{} ", env!("CARGO_PKG_VERSION")), "");
        assert_eq!(greeting, expected.get("greeting").and_then(Json::as_str).unwrap());
        let lines = sc.get("lines").and_then(Json::as_array).unwrap();
        let outs = expected.get("outputs").and_then(Json::as_array).unwrap();
        for (line, want) in lines.iter().zip(outs) {
            let line = line.as_str().unwrap();
            let got = iso.eval(id, line);
            assert_eq!(frames(&got), frames(want.as_str().unwrap()), "line {line:?}");
        }

        let view = published.get();
        let (mut ours_names, mut theirs_names) = (Vec::new(), Vec::new());
        let want = expected.get("collections").and_then(|c| c.as_object().cloned()).unwrap();
        for (coll, docs) in want.iter() {
            let ours: Vec<Json> = engine::loki::collection(&view, coll).unwrap().iter().map(|(_, d)| Json::parse(d).unwrap()).collect();
            let (a, b) = (canon(&Json::Arr(ours), &known, &mut ours_names), canon(docs, &known, &mut theirs_names));
            assert_eq!(a.stringify(), b.stringify(), "collection {coll}");
        }
        let env_want = expected.get("env").and_then(|e| e.as_object().cloned()).unwrap();
        let host_env = Json::parse(&engine::loki::env_data(&view, Profile::Private)).unwrap();
        for (k, v) in env_want.iter() {
            assert_eq!(host_env.get(k).map(Json::stringify), Some(v.stringify()), "env {k}");
        }
        let _ = std::fs::remove_dir_all(&data);
    }
}
