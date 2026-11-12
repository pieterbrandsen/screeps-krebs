//! What the UCS perf lanes use of screepsmod-admin-utils 1.37.1 and screepsmod-auth 2.9.0 beyond HTTP routes:
//!
//! * console `utils.getCPULimit/setCPULimit/resetCPULimit` (gclToCPU/utils.js), `utils.getWhitelist/addWhitelistUser/
//!   removeWhitelistUser` (whitelist/utils.js), `utils.setShardName` and `utils.removeBots` (common.js), with the same
//!   return strings;
//! * console `auth.createAuthToken(username, description?)` (screepsmod-auth cli.js): a UUID that never expires,
//!   accepted as `X-Token` (backend authlib reads `authTokens`, which the engine keeps private like `password`);
//! * the crons, as a global-stage hook: `gcltocpu` (`cpu = fixedCPU || min(level * stepCPU + baseCPU, maxCPU)` for users
//!   with `active: 10000`, when `gclToCPU` is configured), `fixAllowed` (`blocked = banned || (whitelist non-empty and
//!   not a bot and not listed)`), every tick but writing only what changed, and `inactiveUserCleanup` every 60 s
//!   (`active: 0` for a user with no rooms and no creep or controller);
//! * `serverConfig.shardName`: `/api/version` names that one shard;
//! * `serverConfig.whitelist`: names listed from the start, as config.yml lists them (their case kept).
//!
//! Settings come from `<data>/ucs.json` (`{"shardName": "shard", "gclToCPU": {"maxCPU": 100, "baseCPU": 20,
//! "stepCPU": 10}, "whitelist": ["Observer", "Invader"]}`, all optional); the whitelist lives in
//! `<data>/ucs-whitelist.json`, admin-utils' env key here, with the configured names added at start.

use backend::mods::{CliCallCtx, Registry};
use backend::{edit_user, field, obj_set, str_field};
use engine::constants::{GCL_MULTIPLY, GCL_POW};
use engine::doc::Val;
use engine::processor::global::GlobalCtx;
use engine::world::{UserIdx, World};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use util::json::Json;

/// `serverConfig` of admin-utils that the lanes set.
#[derive(Default, Debug, Clone, PartialEq)]
pub struct Settings {
    pub shard_name: Option<String>,
    /// (maxCPU, baseCPU, stepCPU) when `gclToCPU` is on.
    pub gcl_to_cpu: Option<(f64, f64, f64)>,
    pub whitelist: Vec<String>,
}

static SETTINGS: OnceLock<Settings> = OnceLock::new();
static DATA_DIR: OnceLock<PathBuf> = OnceLock::new();
static WHITELIST: Mutex<Vec<String>> = Mutex::new(Vec::new());
static LAST_CLEANUP: Mutex<Option<Instant>> = Mutex::new(None);

/// `<data>/ucs.json`; absent or unreadable fields are off.
pub fn parse_settings(text: &str) -> Settings {
    let Ok(j) = Json::parse(text) else { return Settings::default() };
    let num = |o: &Json, k: &str, d: f64| o.get(k).and_then(Json::as_f64).unwrap_or(d);
    Settings {
        shard_name: j.get("shardName").and_then(Json::as_str).map(str::to_string),
        gcl_to_cpu: j.get("gclToCPU").filter(|g| matches!(g, Json::Obj(_))).map(|g| (num(g, "maxCPU", 300.0), num(g, "baseCPU", 20.0), num(g, "stepCPU", 10.0))),
        whitelist: j.get("whitelist").and_then(Json::as_array).map(|a| a.iter().filter_map(Json::as_str).map(str::to_string).collect()).unwrap_or_default(),
    }
}

pub fn register(r: &mut Registry) {
    let _ = DATA_DIR.set(r.data_dir.clone());
    let settings = parse_settings(&std::fs::read_to_string(r.data_dir.join("ucs.json")).unwrap_or_default());
    if let Some(name) = &settings.shard_name {
        r.server_data.push(("shards", Json::Arr(vec![Json::Str(name.clone())])));
    }
    let mut list = load_whitelist(&r.data_dir);
    merge_configured(&mut list, &settings.whitelist);
    *WHITELIST.lock().unwrap_or_else(|e| e.into_inner()) = list;
    let _ = SETTINGS.set(settings);
    r.engine.processor.global_stage.push(crons);
    r.cli_js.push(CLI_JS);
    r.cli_calls.push(("ucs.randomToken", random_token));
    r.cli_calls.push(("ucs.whitelist", whitelist_get));
    r.cli_calls.push(("ucs.whitelistAdd", whitelist_add));
    r.cli_calls.push(("ucs.whitelistRemove", whitelist_remove));
}

/// The console helpers, over the vanilla sandbox's `storage` (admin-utils' and screepsmod-auth's own code, ported).
const CLI_JS: &str = r#"(sandbox, call) => {
  const db = sandbox.storage.db;
  const utils = sandbox.utils || (sandbox.utils = {});
  const auth = sandbox.auth || (sandbox.auth = {});
  const help = (f, text) => { f._help = text; return f; };
  utils.getCPULimit = help(async (username) => {
    if (!username) return 'Username required';
    const user = await db.users.findOne({ usernameLower: username.toLowerCase() });
    if (!user) return 'Username not found';
    return `CPU limit is ${user.cpu} for user ${user.username}`;
  }, 'getCPULimit(username) - Get the CPU limit of a user');
  utils.setCPULimit = help(async (username, cpuLimit) => {
    if (!username) return 'Username required';
    if (!cpuLimit || cpuLimit <= 0) return 'Please set a value > 0';
    const user = await db.users.findOne({ usernameLower: username.toLowerCase() });
    if (!user) return 'Username not found';
    await db.users.update({ _id: user._id }, { $set: { cpu: cpuLimit, fixedCPU: cpuLimit } });
    return `Updated maximum cpu to ${cpuLimit} for user ${user.username}`;
  }, 'setCPULimit(username, cpuLimit) - Set a fixed CPU limit for a user');
  utils.resetCPULimit = help(async (username) => {
    if (!username) return 'Username required';
    const user = await db.users.findOne({ usernameLower: username.toLowerCase() });
    if (!user) return 'Username not found';
    await db.users.update({ _id: user._id }, { $set: { cpu: 100 }, $unset: { fixedCPU: true } });
    return `Reset maximum cpu to 100 for user ${user.username}`;
  }, 'resetCPULimit(username) - Reset the CPU limit of a user to 100');
  utils.getWhitelist = help(async () => call('ucs.whitelist'), 'getWhitelist() - Get the current whitelist object');
  utils.addWhitelistUser = help(async (user) => {
    call('ucs.whitelistAdd', [user]);
    return `${user.toLowerCase()} added to whitelist`;
  }, 'addWhitelistUser(username) - Add the given user to the whitelist');
  utils.removeWhitelistUser = help(async (user) => {
    call('ucs.whitelistRemove', [user]);
    return `${user.toLowerCase()} removed from whitelist`;
  }, 'removeWhitelistUser(username) - Remove the given user from the whitelist');
  utils.setShardName = help(async () => undefined, 'setShardName(value) - The shard name comes from <data>/ucs.json here');
  utils.removeBots = help(async () => 'screepsmod-auth removeUser() is not available', 'removeBots() Remove all bot users');
  auth.createAuthToken = help((username, description) => {
    if (!username) return 'Missing username';
    return db.users.findOne({ username }).then((user) => {
      if (!user) return 'No such user';
      const token = call('ucs.randomToken');
      const entry = { token, full: true, created: Date.now() };
      if (description) entry.description = description;
      const tokens = (Array.isArray(user.authTokens) ? user.authTokens : []).concat([entry]);
      return db.users.update({ _id: user._id }, { $set: { authTokens: tokens } }).then(() => token);
    });
  }, 'createAuthToken(username, description?) - Create a full auth token for the given user');
}"#;

fn random_token(_args: &Json, _ctx: &CliCallCtx) -> Result<Json, String> {
    Ok(Json::Str(uuid_v4()))
}

/// A random UUID v4 from the OS, as screepsmod-auth's tokens look.
pub fn uuid_v4() -> String {
    let mut b = [0u8; 16];
    util::sys::os_random(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

fn whitelist_path(dir: &std::path::Path) -> PathBuf {
    dir.join("ucs-whitelist.json")
}

fn load_whitelist(dir: &std::path::Path) -> Vec<String> {
    let text = std::fs::read_to_string(whitelist_path(dir)).unwrap_or_default();
    Json::parse(&text).ok().and_then(|j| j.as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())).unwrap_or_default()
}

fn save_whitelist(list: &[String]) -> Result<(), String> {
    let Some(dir) = DATA_DIR.get() else { return Ok(()) };
    let json = Json::Arr(list.iter().map(|u| Json::Str(u.clone())).collect()).stringify();
    std::fs::write(whitelist_path(dir), json).map_err(|e| e.to_string())
}

fn whitelist_get(_args: &Json, _ctx: &CliCallCtx) -> Result<Json, String> {
    Ok(Json::Arr(WHITELIST.lock().unwrap_or_else(|e| e.into_inner()).iter().map(|u| Json::Str(u.clone())).collect()))
}

fn user_arg(args: &Json) -> Result<String, String> {
    args.at(0).and_then(Json::as_str).map(str::to_lowercase).ok_or_else(|| "username required".to_string())
}

/// The configured names a list does not hold yet (in any case), added in their own case.
pub fn merge_configured(list: &mut Vec<String>, configured: &[String]) {
    for name in configured {
        if !list.iter().any(|w| w.eq_ignore_ascii_case(name)) {
            list.push(name.clone());
        }
    }
}

/// admin-utils pushes without a check, so a name can be listed twice; so here.
fn whitelist_add(args: &Json, ctx: &CliCallCtx) -> Result<Json, String> {
    let user = user_arg(args)?;
    let mut list = WHITELIST.lock().unwrap_or_else(|e| e.into_inner());
    list.push(user);
    save_whitelist(&list)?;
    drop(list);
    whitelist_get(args, ctx)
}

fn whitelist_remove(args: &Json, ctx: &CliCallCtx) -> Result<Json, String> {
    let user = user_arg(args)?;
    let mut list = WHITELIST.lock().unwrap_or_else(|e| e.into_inner());
    list.retain(|u| *u != user);
    save_whitelist(&list)?;
    drop(list);
    whitelist_get(args, ctx)
}

fn number(doc: &engine::doc::Obj, key: &str) -> Option<f64> {
    field(doc, key).and_then(Val::as_f64)
}

fn truthy(doc: &engine::doc::Obj, key: &str) -> bool {
    match field(doc, key) {
        None | Some(Val::Null) | Some(Val::Bool(false)) => false,
        Some(Val::Num(n)) => *n != 0.0 && !n.is_nan(),
        Some(Val::Str(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

/// `Game.gcl.level`: `floor((gcl / GCL_MULTIPLY) ^ (1 / GCL_POW)) + 1`.
pub fn gcl_level(points: f64) -> f64 {
    ((points.max(0.0) / f64::from(GCL_MULTIPLY)).powf(1.0 / GCL_POW)).floor() + 1.0
}

/// gclToCPU/cronjobs.js: `user.fixedCPU || min(level * stepCPU + baseCPU, maxCPU)`.
pub fn cpu_for(fixed: Option<f64>, gcl: f64, (max, base, step): (f64, f64, f64)) -> f64 {
    match fixed {
        Some(f) if f != 0.0 => f,
        _ => (gcl_level(gcl) * step + base).min(max),
    }
}

/// whitelist/cronjobs.js fixAllowed.
pub fn blocked(banned: bool, bot: bool, username: Option<&str>, whitelist: &[String]) -> bool {
    let listed = username.is_some_and(|u| whitelist.iter().any(|w| w.eq_ignore_ascii_case(u)));
    banned || (!whitelist.is_empty() && !bot && !listed)
}

fn crons(w: &mut World, _ctx: &GlobalCtx) {
    let settings = SETTINGS.get().cloned().unwrap_or_default();
    let whitelist = WHITELIST.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let cleanup_due = {
        let mut last = LAST_CLEANUP.lock().unwrap_or_else(|e| e.into_inner());
        let due = last.is_none_or(|t| t.elapsed() >= Duration::from_secs(60));
        if due {
            *last = Some(Instant::now());
        }
        due
    };
    let present: HashSet<String> = if cleanup_due {
        w.rooms
            .iter()
            .flat_map(|r| r.objects.iter())
            .filter(|d| matches!(str_field(&d.obj, "type"), Some("creep" | "controller")))
            .filter_map(|d| str_field(&d.obj, "user").map(str::to_string))
            .collect()
    } else {
        HashSet::new()
    };
    for u in 0..w.users.len() {
        let st = &w.users[u];
        let doc = &st.doc;
        let mut next = None::<engine::doc::Obj>;
        let mut set = |k: &str, v: Val| obj_set(next.get_or_insert_with(|| (**doc).clone()), k, v);
        if let Some(rule) = settings.gcl_to_cpu {
            if number(doc, "active") == Some(10_000.0) {
                let want = cpu_for(number(doc, "fixedCPU"), number(doc, "gcl").unwrap_or(0.0), rule);
                if number(doc, "cpu") != Some(want) {
                    set("cpu", Val::Num(want));
                }
            }
        }
        let want_blocked = blocked(truthy(doc, "banned"), field(doc, "bot").is_some_and(|b| !matches!(b, Val::Null)), str_field(doc, "username"), &whitelist);
        if !matches!(field(doc, "blocked"), Some(Val::Bool(b)) if *b == want_blocked) {
            set("blocked", Val::Bool(want_blocked));
        }
        if cleanup_due {
            let no_rooms = match field(doc, "rooms") {
                Some(Val::Arr(a)) => a.is_empty(),
                _ => true,
            };
            if no_rooms && !present.contains(st.id.as_str()) && number(doc, "active") != Some(0.0) {
                set("active", Val::Num(0.0));
            }
        }
        if let Some(next) = next {
            edit_user(w, u as UserIdx, |st| st.doc = Arc::new(next));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_from_ucs_json() {
        assert_eq!(parse_settings(""), Settings::default());
        let s = parse_settings(r#"{"shardName":"shard","gclToCPU":{"maxCPU":100,"baseCPU":20,"stepCPU":10}}"#);
        assert_eq!(s, Settings { shard_name: Some("shard".into()), gcl_to_cpu: Some((100.0, 20.0, 10.0)), whitelist: vec![] });
        assert_eq!(parse_settings(r#"{"whitelist":["Observer","Invader"]}"#).whitelist, vec!["Observer".to_string(), "Invader".into()]);
    }

    #[test]
    fn configured_names_join_the_list_once_in_their_case() {
        let mut list = vec!["w7n3".to_string(), "observer".to_string()];
        merge_configured(&mut list, &["Observer".into(), "Source Keeper".into()]);
        assert_eq!(list, vec!["w7n3".to_string(), "observer".into(), "Source Keeper".into()]);
    }

    #[test]
    fn gcl_to_cpu_as_admin_utils() {
        let rule = (100.0, 20.0, 10.0);
        // level 1 is 1*10+20 = 30: admin-utils adds a step at level 1 already
        assert_eq!(cpu_for(None, 0.0, rule), 30.0);
        assert_eq!(cpu_for(None, f64::from(GCL_MULTIPLY), rule), 40.0);
        assert_eq!(cpu_for(None, 1e12, rule), 100.0);
        assert_eq!(cpu_for(Some(100.0), 0.0, rule), 100.0, "fixedCPU wins (utils.setCPULimit)");
        assert_eq!(cpu_for(Some(0.0), 0.0, rule), 30.0, "a falsy fixedCPU is no fixed limit");
    }

    #[test]
    fn whitelist_blocks_as_fix_allowed() {
        let list = vec!["observer".to_string(), "invader".to_string()];
        assert!(!blocked(false, false, Some("Observer"), &list), "listed, any case");
        assert!(blocked(false, false, Some("stranger"), &list));
        assert!(!blocked(false, true, Some("W7N3"), &list), "a bot account is never blocked");
        assert!(!blocked(false, false, Some("stranger"), &[]), "an empty whitelist blocks nobody");
        assert!(blocked(true, false, Some("observer"), &list), "banned always");
    }

    #[test]
    fn tokens_look_like_screepsmod_auth_tokens() {
        let (a, b) = (uuid_v4(), uuid_v4());
        assert_ne!(a, b);
        let parts: Vec<&str> = a.split('-').collect();
        assert_eq!(parts.iter().map(|p| p.len()).collect::<Vec<_>>(), [8, 4, 4, 4, 12]);
        assert!(parts[2].starts_with('4'));
    }
}
