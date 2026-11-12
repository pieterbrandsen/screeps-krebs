//! krebs.json + command-line flags (see krebs.example.json); CFG-ENGINE-DEFAULTS. Owner: WP11.
//!
//! Precedence: built-in defaults < krebs.json (`--config <file>`, else `./krebs.json`, else `./chitin.json`, the name
//! before Krebs) < flags.

use engine::policy::{Compat, Profile};
use engine::world::EngineConfig;
use std::path::{Path, PathBuf};
use util::json::Json;

/// Looked for in the working directory, in this order.
const CONFIG_FILES: [&str; 2] = ["krebs.json", "chitin.json"];

/// The first of `CONFIG_FILES` in `dir` as (name, text). A file that is there but cannot be read is an error: running
/// on the defaults would ignore what its owner wrote.
fn default_config(dir: &Path) -> Result<Option<(String, String)>, String> {
    for name in CONFIG_FILES {
        match std::fs::read_to_string(dir.join(name)) {
            Ok(text) => return Ok(Some((name.to_string(), text))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("{name}: {e}")),
        }
    }
    Ok(None)
}

#[derive(Clone)]
pub struct Config {
    pub http_host: String,
    pub http_port: u16,
    pub max_connections: usize,
    /// How long a connection may send nothing while a request is due, in seconds.
    pub request_timeout_s: u64,
    pub server_password: Option<String>,
    pub cli_host: String,
    pub cli_port: u16,
    /// No admin console and no port for one. A match's server has no use for one.
    pub cli_off: bool,
    /// The console port speaks screeps-launcher's HTTP CLI (`cli.http`).
    pub cli_http: bool,
    /// `single-user:<name>` | `trust` | `off`
    pub steam: String,
    pub data: PathBuf,
    pub world: WorldCfg,
    pub compat: Compat,
    pub shard_name: String,
    /// CFG-ENGINE-DEFAULTS (driver/lib/index.js:19-33).
    pub engine: EngineConfig,
    /// CFG-TICKRATE: initial `env.tick_rate_ms` (0 = as fast as possible).
    pub tick_rate_ms: u32,
    /// CLOCK-VIRTUAL `nominalTickMs` for new worlds.
    pub nominal_tick_ms: u32,
    pub runner: RunnerCfg,
    pub persist_interval_s: u64,
    pub mods: Vec<String>,
    /// Whether krebs.json or `--mods` named them. With neither, `start` has a default of its own.
    pub mods_named: bool,
}

#[derive(Clone)]
pub struct WorldCfg {
    /// Sectors (10×10 rooms) across and down, centred on the W/E and N/S seams.
    pub sectors: (u32, u32),
    pub seed: u64,
    pub wide_coords: bool,
    pub open: bool,
}

#[derive(Clone)]
pub struct RunnerCfg {
    /// 0 = physical cores (D3).
    pub threads: usize,
    pub heap_mb: usize,
    /// Each of a VM's two semi-spaces, in MB. 0 = isolated-vm's size, which is 2 MB at most at the default heap.
    pub semi_space_mb: usize,
    /// Game setup's object list and one Store handler. Match only, off on the game server: see `runtime::RuntimeConfig`.
    pub setup_list: bool,
    /// 0 = the runtime's default (75 % of RAM minus world memory).
    pub heap_budget_mb: usize,
    /// `thread` | `wall` | `fixed`
    pub meter: String,
    pub fixed_ms_per_tick: f64,
    pub hard_timeout_ms: u64,
    pub idle_evict_s: u64,
    pub watchdog_poll_us: u64,
}

/// Parsed command line: positional words (`serve`, `new`, `user add bob`, …) and the config.
pub struct Args {
    pub words: Vec<String>,
    pub flags: Vec<(String, String)>,
}

impl Args {
    /// `--key value` and `--key=value` anywhere; `--force` style switches take "true".
    pub fn parse(argv: &[String]) -> Args {
        const SWITCHES: &[&str] = &["force", "console", "friends", "password-stdin", "announce-port"];
        let (mut words, mut flags) = (Vec::new(), Vec::new());
        let mut it = argv.iter();
        while let Some(a) = it.next() {
            match a.strip_prefix("--") {
                Some(k) if k.contains('=') => {
                    let (k, v) = k.split_once('=').unwrap();
                    flags.push((k.to_string(), v.to_string()));
                }
                Some(k) if SWITCHES.contains(&k) => flags.push((k.to_string(), "true".into())),
                Some(k) => flags.push((k.to_string(), it.next().cloned().unwrap_or_default())),
                None => words.push(a.clone()),
            }
        }
        Args { words, flags }
    }

    pub fn flag(&self, k: &str) -> Option<&str> {
        self.flags.iter().rev().find(|(f, _)| f == k).map(|(_, v)| v.as_str())
    }
}

impl Config {
    pub fn load(args: &Args) -> Result<Config, String> {
        let file = match args.flag("config") {
            Some(p) => Some((p.to_string(), std::fs::read_to_string(p).map_err(|e| format!("{p}: {e}"))?)),
            None => default_config(Path::new("."))?,
        };
        let j = match file {
            // Notepad can save a byte order mark, which JSON does not allow
            Some((name, text)) => Json::parse(text.trim_start_matches('\u{feff}')).map_err(|e| format!("{name}: {e}"))?,
            None => Json::Obj(Vec::new()),
        };
        let at = |path: &str| path.split('.').try_fold(&j, |o, k| o.get(k));
        let str_ = |path: &str, d: &str| at(path).and_then(Json::as_str).unwrap_or(d).to_string();
        let num = |path: &str, d: f64| at(path).and_then(Json::as_f64).unwrap_or(d);
        let bool_ = |path: &str, d: bool| at(path).and_then(Json::as_bool).unwrap_or(d);
        // What a hand-edited krebs.json most often gets wrong. Left alone, a port in quotes would be ignored in silence.
        let port = |path: &str, d: u16| match at(path) {
            None => Ok(d),
            Some(v) => v.as_f64().filter(|n| n.fract() == 0.0 && (0.0..=65535.0).contains(n)).map(|n| n as u16).ok_or(format!("{path} has to be a number from 1 to 65535, with no quotes round it")),
        };
        let mods = match at("mods") {
            None => Vec::new(),
            Some(v) => v.as_array().and_then(|a| a.iter().map(|m| m.as_str().map(str::to_string)).collect::<Option<Vec<_>>>()).ok_or("mods has to be a list of names, like [\"bare\"]")?,
        };

        let (profile, overrides) = match at("compat") {
            Some(Json::Obj(_)) => (str_("compat.profile", "private"), at("compat.override").and_then(Json::as_object).cloned().unwrap_or_default()),
            _ => (str_("compat", "private"), Vec::new()),
        };
        let profile = args.flag("profile").map(str::to_string).unwrap_or(profile);
        let mut compat = Compat::new(Profile::parse(&profile).ok_or(format!("compat: unknown profile {profile:?}"))?);
        for (id, v) in overrides {
            compat.set_override(&id, v.as_str() == Some("fix"))?;
        }

        let tick_rate = num("engine.tickRateMs", 1000.0);
        let mut c = Config {
            http_host: str_("http.host", "127.0.0.1"),
            http_port: port("http.port", 21025)?,
            max_connections: num("http.maxConnections", 2048.0) as usize,
            request_timeout_s: num("http.requestTimeoutS", 120.0) as u64,
            server_password: at("http.serverPassword").and_then(Json::as_str).map(str::to_string),
            cli_host: str_("cli.host", "127.0.0.1"),
            cli_port: port("cli.port", 21026)?,
            cli_off: bool_("cli.off", false),
            cli_http: bool_("cli.http", false),
            steam: str_("auth.steam", "single-user:player"),
            data: PathBuf::from(str_("data", "data")),
            world: WorldCfg {
                sectors: parse_wxh(&str_("world.sectors", "2x2"))?,
                seed: num("world.seed", 1.0) as u64,
                wide_coords: bool_("world.wideCoords", false),
                open: bool_("world.open", true),
            },
            compat,
            shard_name: str_("shardName", "shard0"),
            engine: EngineConfig {
                main_loop_min_duration_ms: tick_rate as u32,
                main_loop_reset_interval_ms: num("engine.mainLoopResetInterval", 5000.0) as u32,
                cpu_max_per_tick: num("engine.cpuMaxPerTick", 500.0) as u32,
                cpu_bucket_size: num("engine.cpuBucketSize", 10000.0) as u32,
                history_chunk_size: num("engine.historyChunkSize", 20.0) as u32,
                socket_update_throttle_ms: num("engine.socketUpdateThrottle", 200.0) as u32,
                history: false,
                disabled_cronjobs: cronjob_names(at("engine.disabledCronjobs"))?,
            },
            tick_rate_ms: tick_rate as u32,
            nominal_tick_ms: num("engine.nominalTickMs", 1000.0) as u32,
            runner: RunnerCfg {
                threads: num("runner.threads", 0.0) as usize,
                heap_mb: num("runner.heapMb", 256.0) as usize,
                semi_space_mb: num("runner.semiSpaceMb", 0.0) as usize,
                setup_list: bool_("runner.setupList", false),
                heap_budget_mb: num("runner.heapBudgetMb", 0.0) as usize,
                meter: str_("runner.meter", "thread"),
                fixed_ms_per_tick: num("runner.fixedMsPerTick", 1.0),
                hard_timeout_ms: num("runner.hardTimeoutMs", 5000.0) as u64,
                idle_evict_s: num("runner.idleEvictS", 180.0) as u64,
                watchdog_poll_us: num("runner.watchdogPollUs", 500.0) as u64,
            },
            persist_interval_s: num("persist.intervalS", 60.0) as u64,
            mods,
            mods_named: at("mods").is_some() || args.flag("mods").is_some(),
        };

        let int = |k: &str, v: &str| v.parse::<u64>().map_err(|_| format!("--{k}: not a number: {v:?}"));
        for (k, v) in &args.flags {
            match k.as_str() {
                "config" | "profile" | "force" | "friends" | "password-stdin" | "announce-port" | "password" | "email" | "server" | "bots" | "ticks" | "warmup" | "report-every" | "save-every" | "bot" | "max-memory-mb" | "scenario" | "tile" | "load" | "save" | "console" | "p3-repeat" => {}
                "data" => c.data = PathBuf::from(v),
                "mods" => c.mods = v.split(',').map(str::trim).filter(|m| !m.is_empty()).map(str::to_string).collect(),
                "host" => c.http_host = v.clone(),
                "port" => c.http_port = int(k, v)? as u16,
                "cli-port" => c.cli_port = int(k, v)? as u16,
                "tick-rate" => {
                    c.tick_rate_ms = int(k, v)? as u32;
                    c.engine.main_loop_min_duration_ms = c.tick_rate_ms; // CFG-TICKRATE: one value
                }
                "world" => c.world.sectors = parse_wxh(v)?,
                "seed" => c.world.seed = int(k, v)?,
                "steam" => c.steam = v.clone(),
                "runner-threads" => c.runner.threads = int(k, v)? as usize,
                "meter" => c.runner.meter = v.clone(),
                _ => return Err(format!("unknown flag --{k}")),
            }
        }
        if c.runner.threads == 0 {
            c.runner.threads = util::sys::physical_cores();
        }
        Ok(c)
    }
}

/// `engine.disabledCronjobs`: names from the built-in table. A misspelt name would leave the job running under a
/// config that says it is off, so it stops the server instead.
fn cronjob_names(v: Option<&Json>) -> Result<Vec<String>, String> {
    let Some(v) = v else { return Ok(Vec::new()) };
    let names = v.as_array().ok_or("engine.disabledCronjobs: expected an array of job names")?;
    let known = |n: &str| engine::cron::JOBS.iter().any(|j| j.0 == n);
    names.iter().map(|n| n.as_str().filter(|n| known(n)).map(str::to_string).ok_or_else(|| format!("engine.disabledCronjobs: no job {n}"))).collect()
}

/// `"2x2"` → (2, 2).
pub fn parse_wxh(s: &str) -> Result<(u32, u32), String> {
    let bad = || format!("world size {s:?}: expected WxH, e.g. 2x2");
    let (w, h) = s.split_once(['x', 'X']).ok_or_else(bad)?;
    let (w, h) = (w.parse::<u32>().map_err(|_| bad())?, h.parse::<u32>().map_err(|_| bad())?);
    if w == 0 || h == 0 {
        return Err(bad());
    }
    Ok((w, h))
}

/// Sector index range for `n` sectors centred on the seam: 2 → -1..1 (W0 and E0), 3 → -1..2.
pub fn sector_range(n: u32) -> std::ops::Range<i32> {
    let lo = -((n / 2) as i32);
    lo..lo + n as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Args {
        Args::parse(&s.split_whitespace().map(str::to_string).collect::<Vec<_>>())
    }

    #[test]
    fn flags_and_words() {
        let a = args("new --world 3x2 --seed=7 --force --data d");
        assert_eq!(a.words, ["new"]);
        assert_eq!(a.flag("world"), Some("3x2"));
        assert_eq!(a.flag("seed"), Some("7"));
        assert_eq!(a.flag("force"), Some("true"));
        let c = Config::load(&args("new --config ../../krebs.example.json --world 3x2 --seed=7 --data d")).unwrap();
        assert_eq!((c.world.sectors, c.world.seed, c.data, c.tick_rate_ms), ((3, 2), 7, PathBuf::from("d"), 1000));
        // CFG-ENGINE-DEFAULTS
        let e = &c.engine;
        assert_eq!((e.main_loop_min_duration_ms, e.main_loop_reset_interval_ms, e.cpu_max_per_tick, e.cpu_bucket_size, e.history_chunk_size), (1000, 5000, 500, 10000, 20));
        assert!(Config::load(&args("serve --bogus 1")).is_err());
        let c = Config::load(&args("serve --tick-rate 0")).unwrap();
        assert_eq!((c.tick_rate_ms, c.engine.main_loop_min_duration_ms), (0, 0));
        assert_eq!(sector_range(2), -1..1);
        assert_eq!(sector_range(3), -1..2);
        assert_eq!(sector_range(1), 0..1);
        assert!(parse_wxh("0x2").is_err() && parse_wxh("2").is_err());
    }

    #[test]
    fn krebs_json_comes_first_and_chitin_json_still_works() {
        let dir = std::env::temp_dir().join(format!("krebs-config-names-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(default_config(&dir), Ok(None));
        std::fs::write(dir.join("chitin.json"), "old").unwrap();
        assert_eq!(default_config(&dir), Ok(Some(("chitin.json".to_string(), "old".to_string()))));
        std::fs::write(dir.join("krebs.json"), "new").unwrap();
        assert_eq!(default_config(&dir), Ok(Some(("krebs.json".to_string(), "new".to_string()))));
        // saved by Notepad as "Unicode": there, but not text this can read
        std::fs::write(dir.join("krebs.json"), [0xff, 0xfe, 0x7b, 0x00]).unwrap();
        assert!(default_config(&dir).is_err(), "a file that cannot be read must not mean defaults");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_byte_order_mark_is_not_a_mistake() {
        let dir = std::env::temp_dir().join(format!("krebs-config-bom-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("krebs.json");
        std::fs::write(&file, "\u{feff}{\"http\":{\"port\":21555}}").unwrap();
        let cfg = Config::load(&Args::parse(&["serve".to_string(), "--config".to_string(), file.display().to_string()])).unwrap();
        assert_eq!(cfg.http_port, 21555);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn friends_is_a_switch() {
        let a = args("--friends --port 21555");
        assert!(a.words.is_empty());
        assert_eq!((a.flag("friends"), a.flag("port")), (Some("true"), Some("21555")));
    }

    #[test]
    fn a_match_takes_what_it_is_told_through_flags_that_are_switches() {
        let a = args("serve --password-stdin --announce-port --port 0");
        assert_eq!(a.words, ["serve"]);
        assert_eq!((a.flag("password-stdin"), a.flag("announce-port"), a.flag("port")), (Some("true"), Some("true"), Some("0")));
        assert_eq!(Config::load(&a).unwrap().server_password, None, "the flag itself holds no password");
    }

    #[test]
    fn disabled_cronjobs() {
        assert!(Config::load(&args("serve")).unwrap().engine.disabled_cronjobs.is_empty(), "default: every job runs");
        let names = |s: &str| cronjob_names(Some(&Json::parse(s).unwrap()));
        assert_eq!(names(r#"["genInvaders","genStrongholds","expandStrongholds"]"#).unwrap(), ["genInvaders", "genStrongholds", "expandStrongholds"]);
        assert_eq!(names("[]").unwrap(), Vec::<String>::new());
        assert_eq!(names(r#"["genStronghold"]"#).unwrap_err(), r#"engine.disabledCronjobs: no job "genStronghold""#);
        assert!(names("[1]").is_err() && names(r#""genInvaders""#).is_err());
    }
}
