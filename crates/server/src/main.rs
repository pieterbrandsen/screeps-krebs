//! `krebs`: launcher (config, init_v8 before any thread, `serve`, `new`, `import`, `export`, `user add`, and no command at all: `start`).
//! Module tree: WP00/WP11; launcher: WP11.

mod alloc;
mod bench;
mod cli;
mod config;
mod history;
mod mods;
#[cfg(windows)]
mod sampler;
mod start;
mod tick;

use backend::{BackendConfig, BackendDeps, Published};
use config::{Args, Config};
use engine::hooks::EngineHooks;
use engine::mapgen::{self, WorldSpec};
use engine::world::{EnvState, World};
use std::process::exit;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use util::json::Json;

const USAGE: &str = "usage:
  krebs [--friends]                                                       start here: world, server and what to do next
                                                                          --friends: let other PCs connect
  krebs new [--world WxH] [--seed N] [--profile private|mmo] [--force]    create a world in --data (default ./data)
  krebs serve [--host H] [--port P] [--tick-rate MS]                      run the server
  krebs user add <name> --password P --email E [--server HOST:PORT]       register on a running server
  krebs import <db.json> [--force] | krebs export <db.json>               official Loki db.json in or out
  krebs bench [--bots N] [--world WxH] [--ticks T] [--bot main.js|dir] [--max-memory-mb M] [--scenario F [--tile N] | --load D] [--save D] [--console] [--p3-repeat N]
                                                                          N bots on a generated world, timed
common flags: --config <krebs.json> --data <dir> --mods history,…";

/// Not for people: `start::tell_in_a_new_window` runs this program with it, and the text to show.
const TELL: &str = "tell";

#[global_allocator]
static GLOBAL: alloc::Cached = alloc::Cached;

fn main() {
    let args = Args::parse(&std::env::args().skip(1).collect::<Vec<_>>());
    if args.flag("help").is_some() {
        println!("{USAGE}");
        return;
    }
    if let [word, text] = args.words.as_slice() {
        if word == TELL {
            start::give_up(text);
        }
    }
    if args.words.is_empty() {
        return start::run(&args);
    }
    if args.flag("friends").is_some() {
        fail("--friends only works with no command: krebs --friends");
    }
    let cfg = Config::load(&args).unwrap_or_else(|e| fail(&e));
    let words: Vec<&str> = args.words.iter().map(String::as_str).collect();
    match words.as_slice() {
        ["serve"] => serve(cfg, &args),
        ["bench"] => bench::run(cfg, &args),
        ["new"] => new_world(&cfg, args.flag("force").is_some()),
        ["user", "add", name] => user_add(&cfg, &args, name),
        ["import", file] => import(&cfg, file, args.flag("force").is_some()),
        ["export", file] => export(&cfg, file),
        _ => fail("unknown command"),
    }
}

fn fail(msg: &str) -> ! {
    if start::active() {
        start::give_up(msg);
    }
    eprintln!("krebs: {msg}\n{USAGE}");
    exit(2)
}

// ---- world spec ----

/// `<data>/world.json`: what `new` decided. Until checkpoints exist (WP16) `serve` regenerates the world from
/// it; generation is deterministic by seed.
// ponytail: replaced by the first checkpoint once persist.rs lands; the spec file stays as provenance.
fn spec_path(cfg: &Config) -> std::path::PathBuf {
    cfg.data.join("world.json")
}

fn new_world(cfg: &Config, force: bool) {
    let path = spec_path(cfg);
    if path.exists() && !force {
        fail(&format!("{} exists (use --force to replace it)", path.display()));
    }
    let (sx, sy) = cfg.world.sectors;
    let spec = new_spec(cfg);
    let pool = util::pool::Pool::new(logical_cores());
    let w = generate(cfg, &spec, &pool);
    std::fs::create_dir_all(&cfg.data).unwrap_or_else(|e| fail(&format!("{}: {e}", cfg.data.display())));
    std::fs::write(&path, spec.stringify_pretty(2)).unwrap_or_else(|e| fail(&format!("{}: {e}", path.display())));
    engine::persist::save(&cfg.data, &w.view(), w.compat.profile).unwrap_or_else(|e| fail(&format!("{}: {e}", engine::persist::path(&cfg.data).display())));
    println!("world {sx}x{sy} sectors, seed {}: {} rooms -> {}", cfg.world.seed, w.rooms.len(), path.display());
}

/// What `new` records in world.json (from the config and flags).
fn new_spec(cfg: &Config) -> Json {
    let (sx, sy) = cfg.world.sectors;
    let created = engine::policy::real_now_ms("CLOCK-VIRTUAL");
    Json::Obj(vec![
        ("sectors".into(), Json::Str(format!("{sx}x{sy}"))),
        ("seed".into(), Json::Num(cfg.world.seed as f64)),
        ("wideCoords".into(), Json::Bool(cfg.world.wide_coords)),
        ("open".into(), Json::Bool(cfg.world.open)),
        ("profile".into(), Json::Str(profile_name(cfg).into())),
        ("nominalTickMs".into(), Json::Num(cfg.nominal_tick_ms as f64)),
        ("createdMs".into(), Json::Num(created as f64)),
    ])
}

fn profile_name(cfg: &Config) -> &'static str {
    match cfg.compat.profile {
        engine::policy::Profile::Private => "private",
        engine::policy::Profile::Mmo => "mmo",
    }
}

/// The world `spec` describes, with this server's config applied.
fn generate(cfg: &Config, spec: &Json, pool: &util::pool::Pool) -> World {
    let (sx, sy) = config::parse_wxh(spec.get("sectors").and_then(Json::as_str).unwrap_or("2x2")).unwrap_or_else(|e| fail(&e));
    let ws = WorldSpec {
        sectors_x: config::sector_range(sx),
        sectors_y: config::sector_range(sy),
        seed: spec.get("seed").and_then(Json::as_f64).unwrap_or(1.0) as u64,
        wide_coords: spec.get("wideCoords").and_then(Json::as_bool).unwrap_or(false),
        open: spec.get("open").and_then(Json::as_bool).unwrap_or(true),
        portals: None,
    };
    let mut w = mapgen::generate_world(&ws, pool, &EngineHooks::default());
    engine::coll::add_default_users(&mut w);
    w.compat = Arc::new(world_compat(cfg, spec));
    w.config = Arc::new(copy_engine(&cfg.engine));
    w.clock = engine::policy::VClock {
        base_ms: spec.get("createdMs").and_then(Json::as_f64).unwrap_or(0.0) as i64,
        base_tick: 0,
        nominal_tick_ms: spec.get("nominalTickMs").and_then(Json::as_f64).unwrap_or(1000.0) as u32,
    };
    w.env = Arc::new(EnvState { paused: false, tick_rate_ms: cfg.tick_rate_ms, last_accessible_update: 0, status_dirty: true });
    w
}

/// The profile is world state (ID-LENGTH, ENV-GLOBALS-ABSENT): the one `new` recorded wins over the config's,
/// whose compat.override rows still apply.
fn world_compat(cfg: &Config, spec: &Json) -> engine::policy::Compat {
    let mut compat = cfg.compat.clone();
    if let Some(p) = spec.get("profile").and_then(Json::as_str).and_then(engine::policy::Profile::parse) {
        compat.profile = p;
    }
    compat
}

fn copy_engine(e: &engine::world::EngineConfig) -> engine::world::EngineConfig {
    engine::world::EngineConfig {
        main_loop_min_duration_ms: e.main_loop_min_duration_ms,
        main_loop_reset_interval_ms: e.main_loop_reset_interval_ms,
        cpu_max_per_tick: e.cpu_max_per_tick,
        cpu_bucket_size: e.cpu_bucket_size,
        history_chunk_size: e.history_chunk_size,
        socket_update_throttle_ms: e.socket_update_throttle_ms,
        history: e.history,
        disabled_cronjobs: e.disabled_cronjobs.clone(),
    }
}

/// The runtime's settings from the config (`runner.*`).
fn runtime_config(cfg: &Config, profile: engine::policy::Profile) -> runtime::RuntimeConfig {
    let meter = match cfg.runner.meter.as_str() {
        "wall" => runtime::Meter::Wall,
        "fixed" => runtime::Meter::Fixed { ms_per_tick: cfg.runner.fixed_ms_per_tick },
        _ => runtime::Meter::Thread,
    };
    runtime::RuntimeConfig {
        runner_threads: cfg.runner.threads,
        heap_mb: cfg.runner.heap_mb,
        semi_space_mb: cfg.runner.semi_space_mb,
        setup_list: cfg.runner.setup_list,
        heap_budget_mb: cfg.runner.heap_budget_mb,
        meter,
        hard_timeout_ms: cfg.runner.hard_timeout_ms.max(cfg.engine.main_loop_reset_interval_ms as u64),
        idle_evict_s: cfg.runner.idle_evict_s,
        watchdog_poll_us: cfg.runner.watchdog_poll_us,
        profile,
        shard_name: cfg.shard_name.clone().into(),
    }
}

fn logical_cores() -> usize {
    std::thread::available_parallelism().map_or(1, |n| n.get())
}

// ---- serve ----

fn serve(mut cfg: Config, args: &Args) {
    // before anything else, so that without a password there is no server at all, and whoever started this can set things up
    // around it first, as nothing has run yet
    if args.flag("password-stdin").is_some() {
        cfg.server_password = Some(start::password_from(&mut std::io::stdin().lock()).unwrap_or_else(|| fail("--password-stdin: no password came on stdin")));
    }
    if let Some(problem) = start::cli_problem(&cfg) {
        fail(&problem);
    }
    // a key saved by an older Krebs, or put there by hand, takes the folder's permissions, needed or not
    start::tighten_key(&cfg.data);
    let steam_key = start::steam_key(&cfg).unwrap_or_else(|e| fail(&e));
    // first of all, as loading the mods can clear what the last server here left, and that could be a running one's
    start::lock_data(&cfg.data).unwrap_or_else(|e| fail(&e));
    // §2.1: TZ=UTC and V8 before any other thread (the pool, backend and watchdog threads lock isolates)
    runtime::init_v8();
    // world.json is what `new` decided; an imported world has only its checkpoint
    let has_checkpoint = engine::persist::exists(&cfg.data);
    let spec = match std::fs::read_to_string(spec_path(&cfg)) {
        Ok(t) => Json::parse(&t).unwrap_or_else(|e| fail(&start::world_problem(&cfg.data, &format!("{}: {e}", spec_path(&cfg).display())))),
        Err(_) if has_checkpoint => Json::Obj(Vec::new()),
        Err(e) => fail(&format!("{}: {e} (run `krebs new` or `krebs import` first)", spec_path(&cfg).display())),
    };
    let pool = util::pool::Pool::new(logical_cores());
    // the newest checkpoint, else the world `new` described (data directories from before checkpoints)
    let mut w = match engine::persist::load(&cfg.data, &import_opts(&cfg, &spec)) {
        Some(Ok((mut w, report))) => {
            say(&format!("loaded {} at tick {} ({} rooms, {} users)", engine::persist::path(&cfg.data).display(), w.time, w.rooms.len(), w.users.len()));
            report_import(&report);
            apply_config(&mut w, &cfg);
            if args.flag("tick-rate").is_some() {
                let e = &w.env;
                w.env = Arc::new(EnvState { tick_rate_ms: cfg.tick_rate_ms, paused: e.paused, last_accessible_update: e.last_accessible_update, status_dirty: e.status_dirty });
            }
            w
        }
        Some(Err(e)) => fail(&start::world_problem(&cfg.data, &e)),
        None => generate(&cfg, &spec, &pool),
    };
    let (mod_names, registry, game_js) = mods::load(&cfg.mods, &cfg.data, steam_key).unwrap_or_else(|e| fail(&start::mods_problem(&e)));
    w.mods = mod_names.iter().map(|&n| n.into()).collect();
    let history = !registry.engine.save_room_history.is_empty();
    let mut config = copy_engine(&w.config);
    config.history = history;
    w.config = Arc::new(config);
    let hooks: &'static EngineHooks = Box::leak(Box::new(registry.engine));

    let rt = Arc::new(runtime::Runtime::new(
        runtime_config(&cfg, w.compat.profile),
        runtime::Bundle { files: runtime::bundle::FILES, mod_game_js: game_js },
    ));
    let (tx, rx) = std::sync::mpsc::channel();
    let published = Arc::new(Published::new(w.view()));
    // the admin CLI: its host reaches the world through the same Commands as the backend
    let run_ticks = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let reset = {
        let (cfg, spec, mods) = (cfg.clone(), spec.clone(), w.mods.clone());
        Box::new(move || {
            let mut fresh = generate(&cfg, &spec, &util::pool::Pool::new(logical_cores()));
            fresh.mods = mods.clone();
            let mut config = copy_engine(&fresh.config);
            config.history = history;
            fresh.config = Arc::new(config);
            Ok(fresh)
        })
    };
    let mut host = cli::Host::new(published.clone(), tx.clone(), Some(rt.clone()), cfg.data.clone(), w.compat.profile, run_ticks.clone(), hooks, reset);
    // mods' console helpers (screeps-krebs): their JS in every session, their natives behind `call`
    host.mod_calls = registry.cli_calls;
    rt.server().set_mod_cli(registry.cli_js.iter().map(|s| s.to_string()).collect());
    rt.server().set_host(Arc::new(host));
    let backend = backend::start(BackendDeps {
        published: published.clone(),
        commands: tx,
        cli: if cfg.cli_off { Arc::new(cli::Closed) } else { Arc::new(cli::Cli(rt.clone())) },
        config: BackendConfig {
            host: cfg.http_host.clone(),
            port: cfg.http_port,
            cli_host: cfg.cli_host.clone(),
            // port 0 is no console at all
            cli_port: if cfg.cli_off { 0 } else { cfg.cli_port },
            cli_http: cfg.cli_http,
            max_connections: cfg.max_connections,
            request_timeout_s: cfg.request_timeout_s,
            server_password: cfg.server_password.clone(),
            steam: cfg.steam.clone(),
        },
        routes: registry.routes,
        socket_channels: registry.socket_channels,
        socket_events: registry.socket_events,
        server_data: registry.server_data,
    });
    let port = backend.local_addr().port();
    if args.flag("announce-port").is_some() {
        say(&start::port_line(port));
    }
    say(&format!("krebs {} serving {} rooms on http://{}:{port} (tick rate {} ms)", env!("CARGO_PKG_VERSION"), w.rooms.len(), cfg.http_host, cfg.tick_rate_ms));
    if start::active() {
        say(&start::welcome(&cfg));
    }

    let mut lp = tick::TickLoop::new(tick::Live { runtime: rt, pool, published, backend }, hooks, rx);
    let _ = tick::RUN_TICKS.set(run_ticks.clone());
    lp.run_ticks = run_ticks;
    let stop = lp.stop.clone();
    util::sys::on_ctrl_c(move || stop.store(true, Ordering::Relaxed));
    let every_s = cfg.persist_interval_s.max(1);
    let writer = std::rc::Rc::new(engine::persist::Writer::spawn(cfg.data.clone(), w.compat.profile, start::save_report(every_s)));
    let periodic = writer.clone();
    lp.checkpoint = Some((std::time::Duration::from_secs(every_s), Box::new(move |w: &World| {
        periodic.offer(w.view());
    })));
    lp.run(&mut w);
    lp.checkpoint = None;
    let saved = std::rc::Rc::try_unwrap(writer).map_or(Ok(()), |writer| writer.finish(w.view()));
    if let Err(e) = saved {
        let msg = start::last_save_failed(&cfg.data, &e);
        if start::active() {
            start::tell_in_a_new_window(&msg);
        } else {
            eprintln!("krebs: {msg}");
        }
        exit(1);
    }
    say(&format!("krebs stopped at tick {} (saved {})", w.time, engine::persist::path(&cfg.data).display()));
}

/// A line for the window, if there still is one: println panics without, and a server has no need for anyone to read it.
fn say(line: &str) {
    use std::io::Write;
    let _ = writeln!(std::io::stdout(), "{line}");
}

/// This server's config over a loaded world. A checkpoint keeps the profile (world state) but no `engine.*`, and `new`
/// always writes one, so without this `krebs.json` would never reach a world.
fn apply_config(w: &mut World, cfg: &Config) {
    let mut compat = cfg.compat.clone();
    compat.profile = w.compat.profile;
    w.compat = Arc::new(compat);
    w.config = Arc::new(copy_engine(&cfg.engine));
}

/// Import options for this server: the world seed and profile `new` recorded (an official DB has neither).
fn import_opts(cfg: &Config, spec: &Json) -> engine::loki::ImportOpts {
    engine::loki::ImportOpts {
        seed: spec.get("seed").and_then(Json::as_f64).map_or(cfg.world.seed, |s| s as u64),
        profile: world_compat(cfg, spec).profile,
        now_ms: engine::policy::real_now_ms("CLOCK-VIRTUAL"),
        nominal_tick_ms: cfg.nominal_tick_ms,
    }
}

fn report_import(r: &engine::loki::ImportReport) {
    if r.orphan_objects > 0 {
        eprintln!("note: {} rooms.objects name no room in `rooms` and were left out", r.orphan_objects);
    }
    if r.pending_intents > 0 {
        eprintln!("note: {} pending intent docs (rooms.intents, users.intents) were dropped", r.pending_intents);
    }
}

/// `krebs import <db.json>`: an official (or Krebs) Loki db.json becomes this data directory's world.
fn import(cfg: &Config, file: &str, force: bool) {
    let target = engine::persist::path(&cfg.data);
    if engine::persist::exists(&cfg.data) && !force {
        fail(&format!("{} holds a world (use --force to replace it)", cfg.data.display()));
    }
    // a legacy checkpoint would be migrated over the import at the next start
    let _ = std::fs::remove_file(engine::persist::legacy_path(&cfg.data));
    let text = std::fs::read_to_string(file).unwrap_or_else(|e| fail(&format!("{file}: {e}")));
    let (w, report) = engine::loki::import(&text, &import_opts(cfg, &Json::Obj(Vec::new()))).unwrap_or_else(|e| fail(&format!("{file}: {e}")));
    report_import(&report);
    std::fs::create_dir_all(&cfg.data).unwrap_or_else(|e| fail(&format!("{}: {e}", cfg.data.display())));
    engine::persist::save(&cfg.data, &w.view(), w.compat.profile).unwrap_or_else(|e| fail(&format!("{}: {e}", target.display())));
    println!("imported {file}: tick {}, {} rooms, {} users -> {}", w.time, w.rooms.len(), w.users.len(), target.display());
}

/// `krebs export <db.json>`: the newest checkpoint as an official Loki db.json.
fn export(cfg: &Config, file: &str) {
    let src = engine::persist::path(&cfg.data);
    let (w, _) = engine::persist::load(&cfg.data, &import_opts(cfg, &Json::Obj(Vec::new())))
        .unwrap_or_else(|| fail(&format!("{}: no world (run `krebs new` or `krebs import` first)", cfg.data.display())))
        .unwrap_or_else(|e| fail(&e));
    std::fs::write(file, engine::loki::export(&w.view(), w.compat.profile)).unwrap_or_else(|e| fail(&format!("{file}: {e}")));
    println!("exported {} (tick {}) -> {file}", src.display(), w.time);
}

// ---- user add ----

/// Registers through the running server's `POST /api/register/submit` (AUTHMOD-REGISTER), the same path every
/// client uses, so accounts stay journaled by the server.
fn user_add(cfg: &Config, args: &Args, name: &str) {
    let password = args.flag("password").unwrap_or_else(|| fail("user add: --password is required"));
    let email = args.flag("email").unwrap_or_else(|| fail("user add: --email is required"));
    let server = args.flag("server").map(str::to_string).unwrap_or(format!("{}:{}", cfg.http_host, cfg.http_port));
    let body = vec![("username".to_string(), Json::Str(name.into())), ("password".to_string(), Json::Str(password.into())), ("email".to_string(), Json::Str(email.into()))];
    match http_post(&server, "/api/register/submit", &Json::Obj(body).stringify()) {
        Ok(r) if Json::parse(&r).ok().and_then(|j| j.get("ok").and_then(Json::as_f64)) == Some(1.0) => println!("user {name} registered"),
        Ok(r) if Json::parse(&r).is_err() => fail("user add: this server has no sign-up, as the bare mod removes it. Run it on a server started without --mods bare."),
        Ok(r) => fail(&format!("user add: server replied {r}")),
        Err(e) => fail(&format!("user add: {e} (is `krebs serve` running?)")),
    }
}

/// One HTTP/1.1 POST over std (no TLS); returns the body.
fn http_post(server: &str, path: &str, body: &str) -> Result<String, String> {
    use std::io::{Read, Write};
    let addr = server.trim_start_matches("http://").trim_end_matches('/');
    let mut s = std::net::TcpStream::connect(addr).map_err(|e| format!("{addr}: {e}"))?;
    let req = format!("POST {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
    s.write_all(req.as_bytes()).map_err(|e| e.to_string())?;
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&raw);
    let (head, rest) = text.split_once("\r\n\r\n").ok_or("malformed HTTP response")?;
    if !head.to_ascii_lowercase().contains("transfer-encoding: chunked") {
        return Ok(rest.to_string());
    }
    let (mut out, mut rest) = (String::new(), rest);
    while let Some((size, tail)) = rest.split_once("\r\n") {
        let n = usize::from_str_radix(size.trim(), 16).map_err(|_| "bad chunk")?;
        if n == 0 || tail.len() < n {
            break;
        }
        out.push_str(&tail[..n]);
        rest = tail[n..].trim_start_matches("\r\n");
    }
    Ok(out)
}


#[cfg(test)]
mod tests {
    use super::*;

    /// A world created `--profile mmo` stays mmo when served with the default (private) config.
    #[test]
    fn served_world_keeps_its_profile() {
        let cfg = Config::load(&Args::parse(&["serve".into()])).unwrap();
        let spec = Json::parse(r#"{"profile":"mmo"}"#).unwrap();
        assert_eq!(world_compat(&cfg, &spec).profile, engine::policy::Profile::Mmo);
        assert!(world_compat(&cfg, &spec).fix("ID-LENGTH"));
        assert_eq!(world_compat(&cfg, &Json::Obj(Vec::new())).profile, cfg.compat.profile);
    }

    /// `new` writes a checkpoint, so `serve` always loads one: `engine.*` must survive that branch, and a world with
    /// no such keys must keep vanilla's values.
    #[test]
    fn loaded_world_takes_the_engine_config() {
        let dir = std::env::temp_dir().join(format!("chitin-config-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("krebs.json");
        std::fs::write(&file, r#"{"engine":{"cpuMaxPerTick":123,"cpuBucketSize":456,"disabledCronjobs":["genStrongholds"]}}"#).unwrap();
        let load = |config: &[&str]| {
            let argv: Vec<String> = ["serve", "--world", "1x1", "--data"].iter().map(|s| s.to_string()).chain([dir.join("world").display().to_string()]).chain(config.iter().map(|s| s.to_string())).collect();
            Config::load(&Args::parse(&argv)).unwrap()
        };
        let cfg = load(&["--config", file.to_str().unwrap()]);
        let spec = new_spec(&cfg);
        let w = generate(&cfg, &spec, &util::pool::Pool::new(1));
        std::fs::create_dir_all(&cfg.data).unwrap();
        engine::persist::save(&cfg.data, &w.view(), w.compat.profile).unwrap();
        let (mut loaded, _) = engine::persist::load(&cfg.data, &import_opts(&cfg, &spec)).unwrap().unwrap();
        assert_eq!(loaded.config.cpu_max_per_tick, 500, "a checkpoint keeps no config");
        apply_config(&mut loaded, &cfg);
        assert_eq!((loaded.config.cpu_max_per_tick, loaded.config.cpu_bucket_size), (123, 456));
        assert_eq!(loaded.config.disabled_cronjobs, ["genStrongholds"]);
        // no krebs.json: what the owner's server runs with
        apply_config(&mut loaded, &load(&[]));
        let (e, d) = (&loaded.config, engine::world::EngineConfig::default());
        assert_eq!((e.main_loop_min_duration_ms, e.main_loop_reset_interval_ms, e.cpu_max_per_tick, e.cpu_bucket_size, e.history_chunk_size, e.socket_update_throttle_ms), (d.main_loop_min_duration_ms, d.main_loop_reset_interval_ms, d.cpu_max_per_tick, d.cpu_bucket_size, d.history_chunk_size, d.socket_update_throttle_ms));
        assert!(e.disabled_cronjobs.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
