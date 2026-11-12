//! mirror: reference/engine/src/main.js, reference/engine/src/runner.js
//! parity: DIVERGENT(TICK-WATCHDOG, TICK-ERROR, TICK-EMPTY-ROOMS, CRON-STAGE): no queue watchdog (the per-VM hard
//! timeout bounds runs), no global-stage abort (COMPAT-CRASH-GLOBAL), a missing active set is empty, and crons run
//! between ticks (spec 01 §2.2 "Allowed differences", §16 CRON-STAGE). No oracle can replay a main loop, so no
//! fixtures; the stage order is tested in this file.
//!
//! The tick pipeline P0–P7 (ARCHITECTURE.md §2.2). Owner: WP11.

use backend::{Backend, Published, TickReport, UserEvent};
use engine::hooks::{EngineHooks, Stage};
use engine::intents::RoomIntents;
use engine::live::Crash;
use engine::processor::{self, global::GlobalCtx, RoomResult, TickCtx};
use engine::runner::{make, RunJob, RunOutput};
use engine::world::{AccessSnapshot, Command, EnvState, RoomIdx, World};
use runtime::Runtime;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use util::pool::Pool;

/// Everything the pipeline calls outside its own code. `Live` is the server (runtime, processor, backend);
/// tests drive the same pipeline with fakes.
pub trait Stages {
    /// P1 (§2.3).
    fn run_phase(&mut self, w: &World, jobs: &[RunJob]) -> Vec<RunOutput>;
    /// After P1: memory governor and idle eviction (§6.11).
    fn after_phase(&mut self, w: &World);
    /// ACT-SET `sadd`.
    fn activate(&mut self, w: &mut World, r: RoomIdx);
    /// TICK-GET-ROOMS: `smembers` + `del`.
    fn take_active(&mut self, w: &mut World) -> Vec<RoomIdx>;
    /// P3 (§2.4); `rooms` in LPT order, results in any order.
    fn process_rooms(&mut self, w: &World, ctx: &TickCtx, rooms: Vec<(RoomIdx, RoomIntents)>) -> Vec<RoomResult>;
    /// P4 (§5.5); `results` in RoomIdx order.
    fn install(&mut self, w: &mut World, results: Vec<RoomResult>);
    /// P5 (§5.6).
    fn global(&mut self, w: &mut World, ctx: &GlobalCtx);
    /// P6 publish: `WorldView(T+1)` + `TickReport` (notifyRoomsDone).
    fn publish(&mut self, w: &World, out: TickOut);
}

/// What a tick produced besides world state.
pub struct TickOut {
    /// Rooms processed this tick, ascending.
    pub processed: Vec<RoomIdx>,
    pub saved: make::Saved,
}

/// Wall time of each tick stage, summed since the last reset (`chitin bench` prints it).
#[derive(Default, Clone, Debug)]
pub struct PhaseTimes {
    pub ticks: u64,
    /// Users run, rooms processed, bot CPU reported (ms), summed over the ticks.
    pub users: u64,
    pub rooms: u64,
    pub bot_cpu_ms: f64,
    /// Each processed room's own time (`RoomResult::cost_ns`), summed: P3 CPU, apart from how rooms share threads.
    pub room_ns: u64,
    /// `p3_repeat`: rooms processed again and the time it took (not in `ns`).
    pub repeat_rooms: u64,
    pub repeat_ns: u64,
    /// Runs that ended in an error, and the first such error.
    pub errors: u64,
    pub first_error: Option<String>,
    pub ns: [u64; PHASES.len()],
}

pub const PHASES: [&str; 8] = ["P0 commands", "P1 users to run", "P1 run phase", "P2 save", "P3 rooms", "P4 install", "P5 global", "P6 crons, publish"];

/// `system.runTicks` / devtools: the running loop's counter of ticks to run while paused.
pub static RUN_TICKS: std::sync::OnceLock<Arc<AtomicU64>> = std::sync::OnceLock::new();

/// The tick thread's loop state (main.js module state + chitin pacing).
pub struct TickLoop<S: Stages> {
    pub stages: S,
    hooks: &'static EngineHooks,
    commands: Receiver<Command>,
    /// `system.runTicks(n)`: ticks still to run while paused (PROTO-DEVTOOLS).
    pub run_ticks: Arc<AtomicU64>,
    pub stop: Arc<AtomicBool>,
    /// CRON-STAGE `last[job]` in vclock ms, in memory only (starts at 0, as at a backend start).
    cron_last: Vec<i64>,
    /// The history writer thread (POST-HISTORY runs off the tick, as vanilla's chained promise does).
    history: Option<std::sync::mpsc::Sender<crate::history::Tick>>,
    /// Periodic checkpoints (§9): every interval of real time, between ticks.
    pub checkpoint: Option<(Duration, Box<dyn FnMut(&World)>)>,
    last_checkpoint: Instant,
    pub times: PhaseTimes,
    /// `bench --console`: every run's console output and error on stdout.
    pub print_console: bool,
    /// `bench --p3-repeat n`: every tick's rooms n more times on the tick thread, from the same snapshots and
    /// intents, results dropped: P3's cost on identical input, without the other threads or the bots' variance.
    pub p3_repeat: u32,
}

impl<S: Stages> TickLoop<S> {
    pub fn new(stages: S, hooks: &'static EngineHooks, commands: Receiver<Command>) -> TickLoop<S> {
        TickLoop { stages, hooks, commands, run_ticks: Arc::default(), stop: Arc::default(), cron_last: Vec::new(), history: None, checkpoint: None, last_checkpoint: Instant::now(), times: PhaseTimes::default(), print_console: false, p3_repeat: 0 }
    }

    /// `p3_repeat` passes over `rooms` on this thread, timed without copying their input or dropping their results.
    fn repeat_rooms(&mut self, w: &World, ctx: &TickCtx, rooms: &[(RoomIdx, RoomIntents)]) {
        for _ in 0..self.p3_repeat {
            let input = rooms.to_vec();
            let t = Instant::now();
            let out: Vec<_> = input.into_iter().map(|(r, i)| catch_unwind(AssertUnwindSafe(|| processor::process_room(ctx, r, &w.rooms[r as usize], i)))).collect();
            self.times.repeat_ns += t.elapsed().as_nanos() as u64;
            drop(out);
        }
        self.times.repeat_rooms += rooms.len() as u64 * self.p3_repeat as u64;
    }

    /// The main loop until `stop`. BUG-STATUSDATA-COLD (F): the access snapshot exists before the first tick;
    /// `lastAccessibleRoomsUpdate` starts at 0 as at an engine_main start (main.js:8).
    pub fn run(&mut self, w: &mut World) {
        refresh_access(w, w.clock.at(w.time));
        env_mut(w).last_accessible_update = 0;
        while !self.stop.load(Ordering::Relaxed) {
            if let Some((every, save)) = &mut self.checkpoint {
                if self.last_checkpoint.elapsed() >= *every {
                    save(w);
                    self.last_checkpoint = Instant::now();
                }
            }
            let start = Instant::now();
            if !self.tick(w) {
                self.wait(w, Duration::from_millis(100), true); // paused: P0 only (§2.8)
                continue;
            }
            // P7 (TICK-DELAY): getNextTickDelay(used) or max(tickRate - used, 0); real time (CLK-INFRA)
            let used = start.elapsed().as_millis() as u64;
            let delay = match self.hooks.next_tick_delay {
                Some(f) => f(used),
                None => (w.env.tick_rate_ms as u64).saturating_sub(used),
            };
            self.wait(w, Duration::from_millis(delay), false);
        }
    }

    /// One main.js `loop()`: P0–P6. False when paused (TICK-PAUSE-CHECK: no stage ran).
    pub fn tick(&mut self, w: &mut World) -> bool {
        let mut clock = Instant::now();
        let mut lap = |times: &mut PhaseTimes, i: usize| {
            let now = Instant::now();
            times.ns[i] += (now - clock).as_nanos() as u64;
            clock = now;
        };
        // P0: Commands that arrived since the last P7 (backend writes, CLI, API intents and activations)
        while let Ok(c) = self.commands.try_recv() {
            apply(w, c);
        }
        self.stage(w, Stage::Start);
        if w.env.paused && self.run_ticks.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1)).is_err() {
            self.stage(w, Stage::Finish);
            return false;
        }
        let t = w.time;
        lap(&mut self.times, 0);

        // P1 (TICK-GET-USERS, RUN-*)
        self.stage(w, Stage::GetUsers);
        let (jobs, skipped) = make::users_to_run(w, t);
        lap(&mut self.times, 1);
        self.stage(w, Stage::AddUsersToQueue);
        self.stage(w, Stage::WaitForUsers);
        self.times.users += jobs.len() as u64;
        let mut outs = self.stages.run_phase(w, &jobs);
        outs.extend(skipped);
        self.stages.after_phase(w);
        lap(&mut self.times, 2);

        // P2: saveResult per user; every room with intents joins the active set (ACT-SOURCES row 1)
        let mut saved = make::save_run(w, outs);
        self.times.bot_cpu_ms += saved.cpu.iter().filter(|c| c.1 != make::CPU_ERROR).map(|c| c.1 as f64).sum::<f64>();
        if self.print_console {
            for (u, log, _, err) in &saved.console {
                let name = w.users[*u as usize].doc.get(engine::doc::Atom::intern("username")).and_then(engine::doc::Val::as_str).unwrap_or("?");
                for line in log.iter().chain(err) {
                    println!("[{name} @{}] {line}", w.time);
                }
            }
        }
        for e in saved.console.iter().filter_map(|c| c.3.as_ref()) {
            self.times.errors += 1;
            self.times.first_error.get_or_insert_with(|| e.clone());
        }
        let mut buckets = std::mem::take(&mut saved.buckets);
        for &r in buckets.keys() {
            self.stages.activate(w, r);
        }

        // P3 (TICK-GET-ROOMS: the set is consumed here, so later activations land in T+1)
        self.stage(w, Stage::GetRooms);
        let mut active = self.stages.take_active(w);
        let cost = |r: RoomIdx| w.index.room_cost_ns.get(r as usize).copied().unwrap_or(0);
        active.sort_by_key(|&r| (std::cmp::Reverse(cost(r)), r));
        self.stage(w, Stage::AddRoomsToQueue);
        self.stage(w, Stage::WaitForRooms);
        let rooms: Vec<_> = active.iter().map(|&r| (r, buckets.remove(&r).unwrap_or_else(|| RoomIntents { users: Vec::new() }))).collect();
        self.times.rooms += active.len() as u64;
        lap(&mut self.times, 3);
        debug_assert!(buckets.is_empty(), "rooms with intents that were not activated");
        let hooks = self.hooks;
        let ctx = TickCtx {
            time: t,
            vnow: w.clock.at(t),
            seed: w.seed,
            compat: &w.compat,
            config: &w.config,
            meta: &w.meta,
            terrain: &w.terrain,
            access: &w.access,
            users: &w.users,
            user_ids: &w.user_ids,
            hooks: &hooks.processor,
            order_override: None,
        };
        let aside = Instant::now();
        let again = (self.p3_repeat > 0).then(|| rooms.clone());
        let mut aside = aside.elapsed();
        #[cfg(windows)]
        crate::sampler::rooms_phase(true);
        let mut results = self.stages.process_rooms(w, &ctx, rooms);
        if let Some(again) = again {
            let t = Instant::now();
            self.repeat_rooms(w, &ctx, &again);
            aside += t.elapsed();
        }
        #[cfg(windows)]
        crate::sampler::rooms_phase(false);
        self.times.room_ns += results.iter().map(|r| r.cost_ns as u64).sum::<u64>();
        lap(&mut self.times, 4);
        self.times.ns[4] -= aside.as_nanos() as u64;

        // P4
        results.sort_by_key(|r| r.room);
        let history: Vec<(RoomIdx, Box<str>)> = results.iter_mut().filter_map(|r| r.history.take().map(|h| (r.room, h))).collect();
        self.stages.install(w, results);
        for (r, data) in history {
            // processor.js:504-507 saveRoomHistory, after the room's writes
            let sinks = &self.hooks.save_room_history;
            let tx = self.history.get_or_insert_with(|| crate::history::spawn(sinks.clone()));
            let tick = crate::history::Tick { room: w.meta.rooms[r as usize].name.clone(), game_time: t, data, chunk: w.config.history_chunk_size as u64, now_ms: w.clock.at(t) };
            let _ = tx.send(tick);
        }
        self.stage(w, Stage::Commit1);
        lap(&mut self.times, 5);

        // P5
        self.stage(w, Stage::Global);
        let gctx = GlobalCtx { time: t, vnow: w.clock.at(t), hooks: &self.hooks.processor, order_override: None };
        self.stages.global(w, &gctx);
        self.stage(w, Stage::Commit2);
        lap(&mut self.times, 6);

        // P6: TICK-INC-TIME, TICK-ACCESSIBLE, CRON-STAGE, notifyRoomsDone
        self.stage(w, Stage::IncrementGameTime);
        let g = t + 1;
        w.time = g;
        if g > w.env.last_accessible_update + 20 {
            // main.js:79-83, rebuilt only when something can have changed (§2.5)
            if w.env.status_dirty || w.access.status.values().any(|(_, ts)| ts.is_some()) {
                refresh_access(w, w.clock.at(g));
            }
            let env = env_mut(w);
            env.last_accessible_update = g;
            env.status_dirty = false;
        }
        self.crons(w, g);
        self.stage(w, Stage::NotifyRoomsDone);
        active.sort_unstable();
        self.stages.publish(w, TickOut { processed: active, saved });
        self.stage(w, Stage::Custom);
        self.stage(w, Stage::Finish);
        lap(&mut self.times, 7);
        self.times.ticks += 1;
        true
    }

    /// CRON-STAGE (spec 01 §16): jobs in registration order (the built-in `config.cronjobs`, then mod crons), each
    /// when `vclock(G) − last > period × 1000`. A failing built-in job is logged, like an unhandled rejection.
    /// `engine.disabledCronjobs` skips a built-in job; the others keep their RNG streams, which are per job.
    fn crons(&mut self, w: &mut World, g: u64) {
        let now = w.clock.at(g);
        let builtin = engine::cron::JOBS;
        self.cron_last.resize(builtin.len() + self.hooks.crons.len(), 0);
        for (i, &(name, period, job)) in builtin.iter().enumerate() {
            if now - self.cron_last[i] > period as i64 * 1000 {
                self.cron_last[i] = now;
                if w.config.disabled_cronjobs.iter().any(|d| d == name) {
                    continue;
                }
                if let Err(e) = job(w, &mut engine::cron::CronCtx::new(w.seed, g, now, name)) {
                    eprintln!("cronjob '{name}' error: {e}");
                }
            }
        }
        for (i, &(name, period, job)) in self.hooks.crons.iter().enumerate() {
            let last = &mut self.cron_last[builtin.len() + i];
            if now - *last > period as i64 * 1000 {
                *last = now;
                job(w, &mut engine::cron::CronCtx::new(w.seed, g, now, name));
            }
        }
    }

    /// `mainLoopStage` events (TICK-STAGE-EVENTS); free when no mod listens.
    fn stage(&self, w: &World, s: Stage) {
        if self.hooks.main_loop_stage.is_empty() {
            return;
        }
        let view = w.view();
        for f in &self.hooks.main_loop_stage {
            f(s, &view);
        }
    }

    /// P7: sleep up to `d`, applying Commands as they arrive. `one` returns after the first Command (paused).
    fn wait(&mut self, w: &mut World, d: Duration, one: bool) {
        let end = Instant::now() + d;
        while !self.stop.load(Ordering::Relaxed) {
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return;
            }
            match self.commands.recv_timeout(left.min(Duration::from_millis(100))) {
                Ok(c) => {
                    apply(w, c);
                    if one {
                        return;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => std::thread::sleep(left.min(Duration::from_millis(100))),
            }
        }
    }
}

/// Runs one Command on the tick thread; a panicking Command is an error reply, not a dead server.
fn apply(w: &mut World, c: Command) {
    let r = catch_unwind(AssertUnwindSafe(|| (c.apply)(w))).unwrap_or_else(|p| {
        eprintln!("command panicked: {}", panic_text(&p));
        Err("internal error".into())
    });
    if let Some(tx) = c.reply {
        // never block the tick thread on a requester that stopped listening (D27); a waiting one gets it
        let _ = tx.try_send(r);
    }
}

fn panic_text(p: &Box<dyn std::any::Any + Send>) -> String {
    p.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| p.downcast_ref::<String>().cloned()).unwrap_or_default()
}

fn env_mut(w: &mut World) -> &mut EnvState {
    if Arc::get_mut(&mut w.env).is_none() {
        let e = &*w.env;
        w.env = Arc::new(EnvState { paused: e.paused, tick_rate_ms: e.tick_rate_ms, last_accessible_update: e.last_accessible_update, status_dirty: e.status_dirty });
    }
    Arc::get_mut(&mut w.env).unwrap()
}

/// TICK-ACCESSIBLE at `now` (vclock, CLK-ROOM-STATUS): driver/lib/index.js:490-496 updateAccessibleRoomsList and
/// :498-524 updateRoomStatusData over the `rooms` docs.
fn refresh_access(w: &mut World, now: i64) {
    let version = w.access.version + 1;
    w.access = Arc::new(AccessSnapshot::build(&w.meta, &w.rooms, now, version));
}

/// The server: runtime, pool, processor, backend.
pub struct Live {
    pub runtime: Arc<Runtime>,
    pub pool: Pool,
    pub published: Arc<Published>,
    pub backend: Backend,
}

impl Stages for Live {
    fn run_phase(&mut self, w: &World, jobs: &[RunJob]) -> Vec<RunOutput> {
        self.runtime.run_phase(&self.pool, w, jobs)
    }
    fn after_phase(&mut self, w: &World) {
        self.runtime.after_phase(w)
    }
    fn activate(&mut self, w: &mut World, r: RoomIdx) {
        w.active.insert(r)
    }
    fn take_active(&mut self, w: &mut World) -> Vec<RoomIdx> {
        w.active.take()
    }
    fn process_rooms(&mut self, w: &World, ctx: &TickCtx, rooms: Vec<(RoomIdx, RoomIntents)>) -> Vec<RoomResult> {
        let slots: Vec<(RoomIdx, Mutex<Option<RoomIntents>>)> = rooms.into_iter().map(|(r, i)| (r, Mutex::new(Some(i)))).collect();
        self.pool.map_dyn(slots.len(), |i| {
            let (r, ref slot) = slots[i];
            let intents = slot.lock().unwrap().take().unwrap();
            // D37: a panic is a COMPAT-CRASH-ROOM outcome, logged, and the room runs again next tick
            catch_unwind(AssertUnwindSafe(|| processor::process_room(ctx, r, &w.rooms[r as usize], intents))).unwrap_or_else(|p| {
                let text = panic_text(&p);
                eprintln!("Error processing room {}: panic: {text}", w.meta.rooms.get(r as usize).map_or("?", |m| &m.name));
                crashed(r, text)
            })
        })
    }
    fn install(&mut self, w: &mut World, results: Vec<RoomResult>) {
        processor::install(w, results)
    }
    fn global(&mut self, w: &mut World, ctx: &GlobalCtx) {
        processor::global::run(w, ctx)
    }
    fn publish(&mut self, w: &World, out: TickOut) {
        let view = w.view();
        self.published.set(view.clone());
        let s = out.saved;
        // `memory` = Memory written this tick (the backend re-reads watched `memory/<path>` channels)
        let user_events = s.memory_changed.iter().map(|&user| UserEvent { user, channel: "memory".into(), data: util::json::Json::Null }).collect();
        self.backend.on_tick(TickReport { view, processed: out.processed.into(), console: s.console, cpu: s.cpu, visuals: s.visuals, user_events });
    }
}

fn crashed(room: RoomIdx, text: String) -> RoomResult {
    RoomResult {
        room,
        next: None,
        inserts: Vec::new(),
        crashed: Some(Crash(format!("panic: {text}").into())),
        activate_next: true,
        user_ops: Vec::new(),
        foreign_inserts: Vec::new(),
        notifications: Vec::new(),
        history: None,
        stats: Vec::new(),
        cost_ns: 0,
        index: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine::doc::Val;
    use std::collections::HashMap;
    use engine::doc::{Atom, Id, Obj};
    use engine::intents::UserIntents;
    use engine::policy::{Compat, Profile, VClock};
    use engine::processor::RoomDraft;
    use engine::room_name::RoomXY;
    use engine::terrain::RoomTerrain;
    use engine::runner::RunStatus;
    use engine::world::*;
    use std::cell::RefCell;
    use std::collections::{BTreeMap, BTreeSet};
    use util::jsstr::JsStr;

    thread_local! {
        /// The fake ACT-SET plus an event log; Commands and crons run on the tick (= test) thread.
        static ACTIVE: RefCell<BTreeSet<RoomIdx>> = RefCell::default();
        static LOG: RefCell<Vec<String>> = RefCell::default();
    }

    fn log(s: String) {
        LOG.with(|l| l.borrow_mut().push(s));
    }

    fn take_log() -> Vec<String> {
        LOG.with(|l| std::mem::take(&mut *l.borrow_mut()))
    }

    fn activate(r: RoomIdx) {
        ACTIVE.with(|a| a.borrow_mut().insert(r));
    }

    fn obj(json: &str) -> Obj {
        match Val::from_json(&util::json::Json::parse(json).unwrap()) {
            Val::Obj(o) => o,
            _ => panic!("not an object"),
        }
    }

    fn room(name: &str, info: &str) -> Arc<RoomSnapshot> {
        let draft = RoomDraft {
            info: Arc::new(obj(&format!(r#"{{"_id":"{name}",{info}}}"#))),
            objects: Vec::new(),
            flags: Arc::from(Vec::new()),
            event_log: "[]".into(),
            map_view: "{}".into(),
            presence: Vec::new(),
            user_refs: Vec::new(),
            has_movers: false,
            has_terminal_send: false,
            has_power_creeps: false,
        };
        Arc::new(RoomSnapshot::new(draft, 0))
    }

    /// `rooms` named W1N1.. with `status: normal`; `users` players each owning one object in room 0.
    fn world(rooms: usize, users: usize) -> World {
        let names: Vec<String> = (0..rooms).map(|i| format!("W{}N1", i + 1)).collect();
        let user = |i: usize| UserState {
            id: Id::new(&format!("u{i}")).unwrap(),
            seq: i as u64,
            doc: Arc::new(obj(&format!(r#"{{"_id":"u{i}","username":"p{i}","cpu":100,"cpuAvailable":10000,"active":10000}}"#))),
            auth: Arc::new(Obj::new()),
            code: Arc::new(CodeState { branches: Vec::new(), active_world: None, active_sim: None, version: 0 }),
            memory: JsStr::default(),
            memory_version: 0,
            segments: Arc::default(),
            public_segments: "".into(),
            inter_shard: None,
            console_queue: Arc::from(Vec::new()),
            power_creeps: Arc::from(Vec::new()),
            inbox: Arc::new(Inbox { notifications: Vec::new(), messages: Vec::new() }),
        };
        let presence = Presence { objects: 1, vision: 1, first_vision_seq: 0, observe: None };
        let meta = RoomsMeta {
            x0: -(rooms as i32) - 1,
            y0: -2,
            w: rooms as u32,
            h: 1,
            grid: (0..rooms as u32).rev().map(|i| i + 1).collect(), // room i sits at x0 + rooms - 1 - i
            rooms: names.iter().enumerate().map(|(i, n)| RoomMeta { name: n.as_str().into(), xy: RoomXY::new(-(i as i32) - 2, -2), removed: false }).collect(),
            world_size: 12,
        };
        let clock = VClock { base_ms: 1_000_000, base_tick: 0, nominal_tick_ms: 1000 };
        let snaps = names.iter().map(|n| room(n, r#""status":"normal""#)).collect();
        let mut w = World::new(1, clock, Compat::new(Profile::Private), meta, TerrainStore::new(vec![RoomTerrain::default(); rooms]), snaps, (0..users).map(|i| Arc::new(user(i))).collect());
        w.access = Arc::new(AccessSnapshot { version: 0, accessible: Vec::new(), status: HashMap::new() });
        w.usernames = Arc::new((0..users).map(|i| (format!("p{i}").into(), i as UserIdx)).collect());
        w.env = Arc::new(EnvState { paused: false, tick_rate_ms: 0, last_accessible_update: 0, status_dirty: false });
        w.index = WorldIndex::default();
        w.index.presence = (0..users).map(|_| Arc::new(BTreeMap::from([(0, presence.clone())]))).collect();
        w.next_seq = 0;
        w
    }

    fn done(user: UserIdx, used: u32, rooms: Vec<(RoomIdx, Vec<(Box<str>, Obj)>)>) -> RunOutput {
        let count = rooms.iter().map(|(_, o)| o.len() as u32).sum();
        RunOutput {
            user,
            status: RunStatus::Done,
            error: None,
            intents: Some(UserIntents { rooms, global: None, notify: Vec::new(), count }),
            memory: None,
            segments: Vec::new(),
            seg_settings: None,
            inter_shard: None,
            console_log: Vec::new(),
            console_results: Vec::new(),
            visuals: Vec::new(),
            used_time: used,
            used_clean_ms: used as f64,
            used_dirty_ms: used,
        }
    }

    type Plan = Box<dyn FnMut(&World, &[RunJob]) -> Vec<RunOutput>>;

    /// Fake callees: the run plan, rooms that self-activate (P4), global activations (P5) by tick.
    struct Fake {
        plan: Plan,
        self_activate: BTreeSet<RoomIdx>,
        global_activate: Vec<(u64, RoomIdx)>,
        processed: Vec<(u64, RoomIdx)>,
        /// Sets the loop's `stop` once a tick publishes this game time.
        stop_at: Option<(u64, Arc<AtomicBool>)>,
    }

    impl Fake {
        fn new(plan: Plan) -> Fake {
            ACTIVE.with(|a| a.borrow_mut().clear());
            take_log();
            Fake { plan, self_activate: BTreeSet::new(), global_activate: Vec::new(), processed: Vec::new(), stop_at: None }
        }
    }

    impl Stages for Fake {
        fn run_phase(&mut self, w: &World, jobs: &[RunJob]) -> Vec<RunOutput> {
            log(format!("run@{}", w.time));
            (self.plan)(w, jobs)
        }
        fn after_phase(&mut self, _: &World) {}
        fn activate(&mut self, w: &mut World, r: RoomIdx) {
            log(format!("activate {r}@{}", w.time));
            activate(r);
        }
        fn take_active(&mut self, _: &mut World) -> Vec<RoomIdx> {
            ACTIVE.with(|a| std::mem::take(&mut *a.borrow_mut())).into_iter().collect()
        }
        fn process_rooms(&mut self, w: &World, ctx: &TickCtx, rooms: Vec<(RoomIdx, RoomIntents)>) -> Vec<RoomResult> {
            assert_eq!(ctx.time, w.time);
            assert_eq!(ctx.vnow, w.clock.at(w.time));
            rooms
                .into_iter()
                .map(|(r, intents)| {
                    let users: Vec<_> = intents.users.iter().map(|u| u.user).collect();
                    log(format!("room {r}@{} users {users:?}", w.time));
                    self.processed.push((w.time, r));
                    let mut res = crashed(r, String::new());
                    res.crashed = None;
                    res.activate_next = self.self_activate.remove(&r);
                    res
                })
                .collect()
        }
        fn install(&mut self, w: &mut World, results: Vec<RoomResult>) {
            assert!(results.windows(2).all(|p| p[0].room < p[1].room), "P4 gets RoomIdx order");
            log(format!("install@{}", w.time));
            for r in results.iter().filter(|r| r.activate_next) {
                activate(r.room);
            }
        }
        fn global(&mut self, w: &mut World, ctx: &GlobalCtx) {
            assert_eq!((ctx.time, ctx.vnow), (w.time, w.clock.at(w.time)));
            log(format!("global@{}", w.time));
            for &(_, r) in self.global_activate.iter().filter(|(t, _)| *t == w.time) {
                activate(r);
            }
        }
        fn publish(&mut self, w: &World, out: TickOut) {
            log(format!("publish@{} {:?}", w.time, out.processed));
            if let Some((g, stop)) = &self.stop_at {
                if w.time >= *g {
                    stop.store(true, Ordering::Relaxed);
                }
            }
        }
    }

    type CronFn = fn(&mut World, &mut engine::cron::CronCtx);

    fn hooks(crons: Vec<(&'static str, u32, CronFn)>) -> &'static EngineHooks {
        Box::leak(Box::new(EngineHooks { crons, ..Default::default() }))
    }

    fn cron_wakes_room5(w: &mut World, _: &mut engine::cron::CronCtx) {
        log(format!("cron@{} now {}", w.time, w.clock.at(w.time)));
        if w.time == 8 {
            activate(5);
        }
    }

    fn command(f: impl FnOnce(&mut World) + Send + 'static) -> Command {
        Command {
            apply: Box::new(move |w| {
                f(w);
                Ok(util::json::Json::Null)
            }),
            reply: None,
        }
    }

    fn set_marker(w: &mut World, m: String) {
        let u = Arc::get_mut(&mut w.users[0]).unwrap();
        Arc::make_mut(&mut u.doc).set(Atom::intern("marker"), Val::Str(m.into()));
    }

    /// ACT-SOURCES (§2.6): runner intents and API activations → T; processor, global and cron activations → T+1.
    #[test]
    fn act_sources_timing() {
        let mut fake = Fake::new(Box::new(|w, jobs| {
            // user 0 sends an intent to room 1 at T=0 only
            jobs.iter().map(|j| done(j.user, 1, if w.time == 0 && j.user == 0 { vec![(1, vec![("o".into(), Obj::new())])] } else { vec![] })).collect()
        }));
        fake.self_activate.insert(3);
        fake.global_activate.push((5, 4));
        let (tx, rx) = std::sync::mpsc::channel();
        let mut lp = TickLoop::new(fake, hooks(vec![("wake", 0, cron_wakes_room5)]), rx);
        let mut w = world(8, 2);
        for t in 0..10 {
            match t {
                1 => tx.send(command(|_| activate(2))).unwrap(), // API place-spawn & co. in P0 of T
                2 => tx.send(command(|_| activate(3))).unwrap(),
                _ => {}
            }
            assert!(lp.tick(&mut w));
        }
        assert_eq!(w.time, 10);
        assert_eq!(lp.stages.processed, [(0, 1), (1, 2), (2, 3), (3, 3), (6, 4), (8, 5)]);
    }

    struct Check(Fake);

    impl Stages for Check {
        fn run_phase(&mut self, w: &World, jobs: &[RunJob]) -> Vec<RunOutput> {
            self.0.run_phase(w, jobs)
        }
        fn after_phase(&mut self, w: &World) {
            self.0.after_phase(w)
        }
        fn activate(&mut self, w: &mut World, r: RoomIdx) {
            self.0.activate(w, r)
        }
        fn take_active(&mut self, w: &mut World) -> Vec<RoomIdx> {
            self.0.take_active(w)
        }
        fn process_rooms(&mut self, w: &World, ctx: &TickCtx, rooms: Vec<(RoomIdx, RoomIntents)>) -> Vec<RoomResult> {
            // rooms of T see P2's billing of T (user docs frozen at P3 start) and T's intents of both users
            let used = ctx.users[0].doc.get(Atom::intern("lastUsedCpu")).and_then(Val::as_f64);
            assert_eq!(used, Some(10.0 + w.time as f64));
            assert_eq!(rooms.iter().map(|(_, i)| i.users.len()).sum::<usize>(), 2);
            self.0.process_rooms(w, ctx, rooms)
        }
        fn install(&mut self, w: &mut World, results: Vec<RoomResult>) {
            self.0.install(w, results);
            set_marker(w, format!("install@{}", w.time));
        }
        fn global(&mut self, w: &mut World, ctx: &GlobalCtx) {
            // the global stage of T sees every room commit of T
            let m = w.users[0].doc.get(Atom::intern("marker")).and_then(Val::as_str).map(str::to_string);
            assert_eq!(m, Some(format!("install@{}", w.time)));
            self.0.global(w, ctx);
            set_marker(w, format!("global@{}", w.time));
        }
        fn publish(&mut self, w: &World, out: TickOut) {
            assert_eq!(out.saved.cpu.len(), 2);
            self.0.publish(w, out)
        }
    }

    fn cron_marks(w: &mut World, _: &mut engine::cron::CronCtx) {
        // crons see the post-global state at G = T+1
        let m = w.users[0].doc.get(Atom::intern("marker")).and_then(Val::as_str).map(str::to_string);
        log(format!("cron@{} sees {m:?}", w.time));
        set_marker(w, format!("cron@{}", w.time));
    }

    /// Phase visibility (§2.2 table): who sees what, in which order.
    #[test]
    fn barrier_visibility() {
        let fake = Fake::new(Box::new(|w, jobs| {
            // runners of T see the Command applied in P0 of T, else the cron of G = T
            let m = w.users[0].doc.get(Atom::intern("marker")).and_then(Val::as_str).map(str::to_string);
            log(format!("runner sees {m:?}"));
            jobs.iter().map(|j| done(j.user, 10 + w.time as u32, vec![(0, vec![("o".into(), Obj::new())])])).collect()
        }));
        let (tx, rx) = std::sync::mpsc::channel();
        let mut lp = TickLoop::new(Check(fake), hooks(vec![("mark", 0, cron_marks)]), rx);
        let mut w = world(2, 2);
        tx.send(command(|w| {
            log(format!("command@{}", w.time));
            set_marker(w, "command".into());
        }))
        .unwrap();
        lp.tick(&mut w);
        lp.tick(&mut w);
        assert_eq!(
            take_log(),
            [
                "command@0",
                "run@0",
                "runner sees Some(\"command\")",
                "activate 0@0",
                "room 0@0 users [0, 1]",
                "install@0",
                "global@0",
                "cron@1 sees Some(\"global@0\")",
                "publish@1 [0]",
                "run@1",
                "runner sees Some(\"cron@1\")",
                "activate 0@1",
                "room 0@1 users [0, 1]",
                "install@1",
                "global@1",
                "cron@2 sees Some(\"global@1\")",
                "publish@2 [0]",
            ]
        );
    }

    /// A Command whose requester holds the reply channel but no longer reads it cannot stall the tick thread;
    /// a waiting requester gets its reply; a panicking Command is an error reply.
    #[test]
    fn command_replies_never_block() {
        let mut w = world(1, 1);
        let (stale, _held) = std::sync::mpsc::sync_channel(0);
        apply(&mut w, Command { apply: Box::new(|_| Ok(util::json::Json::Null)), reply: Some(stale) });
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        apply(&mut w, Command { apply: Box::new(|_| panic!("boom")), reply: Some(tx) });
        assert_eq!(rx.recv().unwrap(), Err("internal error".to_string()));
    }

    /// TICK-PAUSE-CHECK / CFG-PAUSE / runTicks: paused ticks run no stage but still apply Commands.
    #[test]
    fn pause_and_run_ticks() {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut lp = TickLoop::new(Fake::new(Box::new(|_, _| Vec::new())), hooks(vec![]), rx);
        let mut w = world(1, 1);
        tx.send(command(|w| env_mut(w).paused = true)).unwrap();
        assert!(!lp.tick(&mut w));
        assert_eq!(w.time, 0);
        lp.run_ticks.store(2, Ordering::Relaxed);
        assert!(lp.tick(&mut w) && lp.tick(&mut w) && !lp.tick(&mut w));
        assert_eq!(w.time, 2);
        tx.send(command(|w| env_mut(w).paused = false)).unwrap();
        assert!(lp.tick(&mut w));
        assert_eq!(w.time, 3);
    }

    /// P7 / `run()`: TICK-DELAY paces ticks at `tickRate`; a paused loop applies Commands as they arrive and
    /// resumes on one; `stop` ends the loop.
    #[test]
    fn run_paces_pauses_and_stops() {
        // pacing: 3 ticks at 40 ms wait twice
        let (_tx, rx) = std::sync::mpsc::channel();
        let mut lp = TickLoop::new(Fake::new(Box::new(|_, _| Vec::new())), hooks(vec![]), rx);
        lp.stages.stop_at = Some((3, lp.stop.clone()));
        let mut w = world(1, 1);
        env_mut(&mut w).tick_rate_ms = 40;
        let t0 = Instant::now();
        lp.run(&mut w);
        assert_eq!(w.time, 3);
        assert!(t0.elapsed() >= Duration::from_millis(80), "{:?}", t0.elapsed());

        // paused: runTicks(2) from a Command, then nothing until another thread's Command resumes
        let (tx, rx) = std::sync::mpsc::channel();
        let mut lp = TickLoop::new(Fake::new(Box::new(|_, _| Vec::new())), hooks(vec![]), rx);
        lp.stages.stop_at = Some((3, lp.stop.clone()));
        let mut w = world(1, 1);
        env_mut(&mut w).tick_rate_ms = 0;
        env_mut(&mut w).paused = true;
        let n = lp.run_ticks.clone();
        tx.send(command(move |_| n.store(2, Ordering::Relaxed))).unwrap();
        let resume = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(250));
            tx.send(command(|w| env_mut(w).paused = false)).unwrap();
        });
        let t0 = Instant::now();
        lp.run(&mut w);
        resume.join().unwrap();
        assert_eq!(w.time, 3);
        assert!(t0.elapsed() >= Duration::from_millis(250), "{:?}", t0.elapsed());
        assert!(!w.env.paused);
    }

    /// TICK-ACCESSIBLE: refresh when G > last + 20 (21-tick period), statuses at vclock(G) (CLK-ROOM-STATUS).
    #[test]
    fn accessible_cadence_and_status() {
        let (_tx, rx) = std::sync::mpsc::channel();
        let mut lp = TickLoop::new(Fake::new(Box::new(|_, _| Vec::new())), hooks(vec![]), rx);
        let mut w = world(4, 0);
        let novice_until = w.clock.at(30);
        w.rooms[1] = room("W2N1", &format!(r#""status":"normal","novice":{novice_until}"#));
        w.rooms[2] = room("W3N1", r#""status":"out of borders""#);
        w.rooms[3] = room("W4N1", &format!(r#""status":"normal","openTime":{}"#, w.clock.at(50)));
        env_mut(&mut w).status_dirty = true;
        let mut versions = vec![w.access.version];
        for _ in 0..70 {
            lp.tick(&mut w);
            versions.push(w.access.version);
        }
        // refreshes at G = 21, 42, 63
        let at: Vec<usize> = (1..versions.len()).filter(|&g| versions[g] != versions[g - 1]).collect();
        assert_eq!(at, [21, 42, 63]);
        let a = &w.access; // as of G = 63
        assert_eq!(a.accessible[0] & 0b1111, 0b1011);
        assert!(matches!(a.status.get(&2), Some((RoomStatus::Closed, None))));
        assert_eq!(a.status.len(), 1);
        // at G = 21 the novice and the closed room still had their deadlines
        let mut w2 = world(4, 0);
        w2.rooms = w.rooms.clone();
        refresh_access(&mut w2, w.clock.at(21));
        assert!(matches!(w2.access.status.get(&1), Some((RoomStatus::Novice, Some(t))) if *t == novice_until));
        assert!(matches!(w2.access.status.get(&3), Some((RoomStatus::Closed, Some(_)))));
        assert_eq!(w2.access.accessible[0] & 0b1111, 0b0011, "novice rooms are accessible; unopened ones are not");
    }

    /// `engine.disabledCronjobs`: a named built-in job never runs, and the rest and the mod crons still do.
    #[test]
    fn disabled_cronjobs_never_run() {
        fn forced_update(disabled: &[&str]) -> bool {
            let (_tx, rx) = std::sync::mpsc::channel();
            let mut lp = TickLoop::new(Fake::new(Box::new(|_, _| Vec::new())), hooks(vec![("wake", 0, cron_wakes_room5)]), rx);
            let mut w = world(8, 0);
            w.config = Arc::new(EngineConfig { disabled_cronjobs: disabled.iter().map(|s| s.to_string()).collect(), ..EngineConfig::default() });
            for _ in 0..30 {
                lp.tick(&mut w);
            }
            assert_eq!(take_log().iter().filter(|l| l.starts_with("cron@")).count(), 30, "the mod cron");
            w.rooms[0].info.get(Atom::intern("nextForceUpdateTime")).is_some()
        }
        assert!(forced_update(&[]), "roomsForceUpdate runs by default");
        assert!(forced_update(&["genInvaders", "genStrongholds", "expandStrongholds"]));
        assert!(!forced_update(&["roomsForceUpdate"]));
    }

    fn cron_noise(w: &mut World, _: &mut engine::cron::CronCtx) {
        activate((w.time % 7) as RoomIdx);
    }

    /// 10 k ticks through the whole pipeline on fakes: skips, deactivation, billing, manual and global intents,
    /// notify, crons: no panic, bucket never above its cap.
    #[test]
    fn ten_thousand_ticks() {
        let plan: Plan = Box::new(|w, jobs| {
            let t = w.time;
            jobs.iter()
                .map(|j| {
                    let mut o = done(j.user, (t % 37) as u32, vec![((t % 5) as RoomIdx, vec![("o".into(), obj(r#"{"move":{"id":"undefined","direction":3}}"#))])]);
                    let ui = o.intents.as_mut().unwrap();
                    if t % 3 == 0 {
                        ui.global = Some(obj(r#"{"createOrder":[{}]}"#));
                    }
                    if t % 7 == 0 {
                        ui.notify.push(Val::Obj(obj(r#"{"message":"hi","groupInterval":5}"#)));
                    }
                    match (t + j.user as u64) % 11 {
                        0 => {
                            o.status = RunStatus::PartialError;
                            o.error = Some("Error: x".into());
                        }
                        1 => {
                            o.status = RunStatus::TotalError;
                            o.error = Some("Error: y".into());
                            o.intents = None;
                        }
                        _ => {}
                    }
                    o.used_clean_ms = 150.0 * ((t + j.user as u64) % 3) as f64;
                    o
                })
                .collect()
        });
        let (tx, rx) = std::sync::mpsc::channel();
        let mut fake = Fake::new(plan);
        fake.self_activate.extend(0..7);
        let mut lp = TickLoop::new(fake, hooks(vec![("noise", 20, cron_noise)]), rx);
        let mut w = world(7, 4);
        for t in 0..10_000u64 {
            if t % 13 == 0 {
                tx.send(command(move |w| {
                    w.manual_intents.push(((t % 7) as RoomIdx, (t % 4) as UserIdx, "x".into(), obj(r#"{"say":{"message":"m"}}"#)));
                    activate((t % 7) as RoomIdx);
                }))
                .unwrap();
            }
            if t == 5000 {
                w.index.presence[3] = Arc::default(); // user 3 loses its last object: RUN-DEACTIVATE
            }
            assert!(lp.tick(&mut w));
            w.global_intents.clear(); // P5's job
            take_log();
            for u in &w.users {
                let avail = u.doc.get(Atom::intern("cpuAvailable")).and_then(Val::as_f64).unwrap();
                assert!(avail <= 10000.0);
            }
        }
        assert_eq!(w.time, 10_000);
        assert_eq!(w.users[3].doc.get(Atom::intern("active")), Some(&Val::Num(0.0)));
        assert!(lp.stages.processed.len() > 10_000);
    }
}
