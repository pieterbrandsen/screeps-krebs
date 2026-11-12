//! V8 host: player isolates, system isolates, natives, deadlines, scheduling (ARCHITECTURE.md §6, API
//! FROZEN). Module tree: WP00/WP11; bodies: WP08.

pub mod bundle;
pub mod cpu;
mod doc_reuse;
pub mod governor;
pub mod host;
pub mod jsprof;
pub mod marshal;
pub mod natives;
pub mod natives_market;
pub mod natives_world;
mod pf_record;
pub mod sched;
pub mod server_isolate;
mod store;
pub mod watchdog;

/// Where the run phase spends its time, summed over the runner threads (ns), for `chitin bench`.
pub mod timing {
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
    use std::sync::Mutex;

    /// The last four split "JS tick" (natives.mark(0..3) in host/prelude.js and driver/runtime.js).
    pub const STAGES: [&str; 10] = [
        "build input", "create VM", "JS tick", "capture + microtasks", "store", "output",
        "  JS: input parse", "  JS: Game setup", "  JS: player loop", "  JS: Memory + out",
    ];
    /// `natives.mark(4..)`: ad-hoc probes for exploring a stage; the bench prints the ones used.
    pub const PROBES: usize = 32;
    pub(crate) static NS: [AtomicU64; 10 + PROBES] = [const { AtomicU64::new(0) }; 10 + PROBES];

    pub(crate) fn add(stage: usize, since: std::time::Instant) -> std::time::Instant {
        let now = std::time::Instant::now();
        NS[stage].fetch_add((now - since).as_nanos() as u64, Relaxed);
        now
    }

    thread_local!(static MARK: std::cell::Cell<std::time::Instant> = std::cell::Cell::new(std::time::Instant::now()));

    /// Set by the bench: every mark also reads the isolate's allocated-bytes counter (BYTES: what V8 allocated in
    /// each JS sub-stage and probe, in NS's slots), and each user's run time is kept (RUNS).
    pub static BENCH: AtomicBool = AtomicBool::new(false);
    static BYTES: [AtomicU64; 10 + PROBES] = [const { AtomicU64::new(0) }; 10 + PROBES];
    thread_local!(static MARK_BYTES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) });

    /// `natives.mark(i)`: the time (and with BENCH the bytes, `allocated` = the isolate's total so far) since the
    /// previous mark goes to JS sub-stage or probe `i` (none for -1).
    pub(crate) fn mark(sub: i32, allocated: Option<u64>) {
        let slot = usize::try_from(sub).ok().filter(|&i| i < 4 + PROBES).map(|i| 6 + i);
        MARK.with(|m| {
            let since = m.get();
            m.set(match slot {
                Some(s) => add(s, since),
                None => std::time::Instant::now(),
            });
        });
        if let Some(now) = allocated {
            let since = MARK_BYTES.with(|b| b.replace(now));
            if let Some(s) = slot {
                BYTES[s].fetch_add(now.saturating_sub(since), Relaxed);
            }
        }
    }

    /// Wall time of each user's job (build input through output), summed: user → (runs, ns).
    static RUNS: Mutex<BTreeMap<engine::world::UserIdx, (u64, u64)>> = Mutex::new(BTreeMap::new());

    pub(crate) fn add_run(user: engine::world::UserIdx, since: std::time::Instant) {
        if !BENCH.load(Relaxed) {
            return;
        }
        let mut runs = RUNS.lock().unwrap_or_else(|e| e.into_inner());
        let e = runs.entry(user).or_default();
        e.0 += 1;
        e.1 += since.elapsed().as_nanos() as u64;
    }

    /// Runs whose outMessage store.rs left to the system isolate.
    pub static STORE_FALLBACKS: AtomicU64 = AtomicU64::new(0);

    /// The sums since the last call (and resets them): STAGES, then the probes (`mark(4)` first).
    pub fn take() -> [u64; 10 + PROBES] {
        std::array::from_fn(|i| NS[i].swap(0, Relaxed))
    }

    /// BYTES since the last call (and resets them).
    pub fn take_bytes() -> [u64; 10 + PROBES] {
        std::array::from_fn(|i| BYTES[i].swap(0, Relaxed))
    }

    /// RUNS since the last call (and resets them).
    pub fn take_runs() -> BTreeMap<engine::world::UserIdx, (u64, u64)> {
        std::mem::take(&mut *RUNS.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

pub use natives_world::WorldAccess;
pub use server_isolate::ServerIsolate;

use engine::policy;
use engine::runner::{RunInput, RunJob, RunOutput};
use engine::world::{UserIdx, World};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::Mutex;

/// First call in main, before any other thread exists: TZ=UTC, V8 flags, platform(4), V8::initialize.
/// Later calls do nothing (tests share one process).
pub fn init_v8() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        std::env::set_var("TZ", "UTC");
        // An isolate per player multiplies V8's single-heap defaults (chitin bench, 200 bots): the first full GC
        // waits for ~30 MB of old space and freed pages sit in a pool for 8 s, ~37 MB per VM between them. With
        // these a VM cycles 5-10 MB used, 14 MB committed, at the same speed; heap_size_limit is unchanged.
        v8::V8::set_flags_from_string("--memory-pool-timeout=0 --initial-old-space-size=4");
        // Extra V8 flags for profiling, e.g. CHITIN_V8_FLAGS=--trace-gc.
        if let Ok(f) = std::env::var("CHITIN_V8_FLAGS") {
            v8::V8::set_flags_from_string(&f);
        }
        let platform = v8::new_default_platform(4, false).make_shared();
        v8::V8::initialize_platform(platform);
        v8::V8::initialize();
    });
}

/// The CPU meter behind `getUsed()`, deadlines and billing (D11).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Meter {
    /// OS thread CPU time (default).
    Thread,
    /// Wall clock, as the private server.
    Wall,
    /// +0.01 ms per `cpuTime()` call (reproducible runs).
    Fixed { ms_per_tick: f64 },
}

pub struct RuntimeConfig {
    pub runner_threads: usize,
    /// 256
    pub heap_mb: usize,
    /// Each of a VM's two semi-spaces, in MB; 0 keeps isolated-vm's young generation (see `host::create_params`).
    pub semi_space_mb: usize,
    /// Match only: Game setup's object list and one Store handler (`natives.setupList`, ARCHITECTURE.md "What differs in
    /// a match"). They show to a bot that patches its own prototypes, so the game server keeps this off.
    pub setup_list: bool,
    pub heap_budget_mb: usize,
    pub meter: Meter,
    pub hard_timeout_ms: u64,
    /// 180
    pub idle_evict_s: u64,
    pub watchdog_poll_us: u64,
    pub profile: policy::Profile,
    pub shard_name: Box<str>,
}

pub struct Bundle {
    pub files: &'static [(&'static str, &'static str)],
    /// (mod name, game-slot JS) in load order.
    pub mod_game_js: Vec<(Box<str>, Box<str>)>,
}

/// VMs, snapshot blobs, system isolates per worker, watchdog, scheduler.
pub struct Runtime {
    cfg: RuntimeConfig,
    snapshots: host::Snapshots,
    vms: Mutex<HashMap<UserIdx, host::Vm>>,
    /// One system isolate per worker thread, taken for a `__store` and put back.
    sys: Mutex<HashMap<std::thread::ThreadId, host::SysVm>>,
    /// store.rs ports the shipped host/system.js and common/system.js; a bundle with others runs them.
    native_store: bool,
    resets: AtomicU64,
    migrations: AtomicU64,
    server: ServerIsolate,
}

/// Heap totals, resets, migrations, parse/build times.
#[derive(Clone, Debug, Default)]
pub struct RuntimeStats {
    pub heap_used_mb: f64,
    pub heap_physical_mb: f64,
    pub heap_limit_mb: f64,
    pub live_blob_mb: f64,
    pub resets: u64,
    pub migrations: u64,
    pub parse_ms: f64,
    pub build_ms: f64,
}

impl Runtime {
    /// Builds the player and system snapshots; `init_v8` must have run.
    pub fn new(cfg: RuntimeConfig, bundle: Bundle) -> Runtime {
        watchdog::set_poll_us(cfg.watchdog_poll_us);
        let snapshots = host::snapshots(&bundle);
        let shipped = |name: &str| {
            let file = |files: &[(&str, &'static str)]| files.iter().find(|f| f.0 == name).map(|f| f.1);
            file(bundle.files).is_some() && file(bundle.files) == file(bundle::FILES)
        };
        let native_store = shipped("host/system.js") && shipped("common/system.js");
        let server = ServerIsolate::with_snapshot(snapshots.blob);
        Runtime {
            cfg,
            snapshots,
            vms: Mutex::new(HashMap::new()),
            sys: Mutex::new(HashMap::new()),
            native_store,
            resets: AtomicU64::new(0),
            migrations: AtomicU64::new(0),
            server,
        }
    }

    pub fn run_phase(&self, pool: &util::pool::Pool, w: &World, jobs: &[RunJob]) -> Vec<RunOutput> {
        sched::run_phase(self, pool, w, jobs)
    }

    /// One prepared run on the calling thread (what `run_phase` does per job after `build_input`).
    #[doc(hidden)]
    pub fn run_input(&self, w: &dyn WorldAccess, input: RunInput) -> RunOutput {
        host::run(self, w, input)
    }

    /// Governor + idle eviction.
    // ponytail: idle eviction only; the heap budget policy is governor.rs (WP19).
    pub fn after_phase(&self, _w: &World) {
        let idle = std::time::Duration::from_secs(self.cfg.idle_evict_s);
        let evicted: Vec<host::Vm> = {
            let mut vms = self.lock_vms();
            let old: Vec<UserIdx> = vms.iter().filter(|(_, v)| v.last_used.elapsed() >= idle).map(|(&u, _)| u).collect();
            old.into_iter().filter_map(|u| vms.remove(&u)).collect()
        };
        self.resets.fetch_add(evicted.len() as u64, Relaxed);
        evicted.into_iter().for_each(host::dispose);
    }

    /// Every VM (a world reset: users are not the same any more).
    pub fn reset_all(&self, reason: &str) {
        let all: Vec<host::Vm> = self.lock_vms().drain().map(|(_, v)| v).collect();
        if !all.is_empty() {
            eprintln!("runtime: reset {} VMs: {reason}", all.len());
        }
        self.resets.fetch_add(all.len() as u64, Relaxed);
        all.into_iter().for_each(host::dispose);
    }

    pub fn reset_user(&self, user: UserIdx, reason: &str) {
        let vm = self.lock_vms().remove(&user);
        if let Some(vm) = vm {
            eprintln!("runtime: reset VM of user {user}: {reason}");
            self.resets.fetch_add(1, Relaxed);
            host::dispose(vm);
        }
    }

    /// CLI + JS-mod server slot (§10).
    pub fn server(&self) -> &ServerIsolate {
        &self.server
    }

    /// `chitin bench`, `CHITIN_JS_PROFILE=cpu:…`: adds every VM's V8 CPU profile to `jsprof::report`.
    pub fn collect_cpu_profiles(&self) {
        for vm in self.lock_vms().values_mut() {
            host::collect_cpu_profile(vm);
        }
    }

    pub fn stats(&self) -> RuntimeStats {
        let vms = self.lock_vms();
        const MB: f64 = 1048576.0;
        RuntimeStats {
            heap_used_mb: vms.values().map(|v| v.heap_used as f64).sum::<f64>() / MB,
            heap_physical_mb: vms.values().map(|v| v.heap_physical as f64).sum::<f64>() / MB,
            heap_limit_mb: vms.values().map(|v| v.heap_limit as f64).sum::<f64>() / MB,
            live_blob_mb: marshal::LIVE_EXTERNAL_BYTES.load(Relaxed) as f64 / MB,
            resets: self.resets.load(Relaxed),
            migrations: self.migrations.load(Relaxed),
            parse_ms: 0.0,
            build_ms: 0.0,
        }
    }

    fn lock_vms(&self) -> std::sync::MutexGuard<'_, HashMap<UserIdx, host::Vm>> {
        self.vms.lock().unwrap_or_else(|e| e.into_inner())
    }
}
