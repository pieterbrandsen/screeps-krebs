//! The privileged isolate (ARCHITECTURE.md §6.2, §8.6): the admin CLI's sessions, one context per connection
//! (CLI-SANDBOX) running the `js/cli` bundle (see js/cli/loader.js for its contract). The isolate starts from
//! V8's built-in snapshot (only one custom blob may be alive per process). A line runs to completion with no
//! deadline, as vanilla's `vm.runInContext`; promise results print once the microtasks after it are drained.
// ponytail: CLI only; the JS-mod server slot (routes, crons, tick events) joins this isolate when JS mods land.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

/// What the CLI's natives reach in the server (implemented over the world view, Commands and the backend).
pub trait CliHost: Send + Sync {
    /// A collection's docs as a JSON array (creation order), or None for an unknown collection.
    fn docs(&self, coll: &str) -> Option<String>;
    /// `[{op: 'insert', doc} | {op: 'replace', id, doc} | {op: 'remove', id}]` → the inserted docs (JSON array).
    fn write(&self, coll: &str, writes: &str) -> Result<String, String>;
    /// `storage.env[op](...args)` → the JSON result (None: undefined).
    fn env(&self, op: &str, args: &str) -> Result<Option<String>, String>;
    /// A server-side command by name (map, bots, strongholds, system) → JSON result.
    fn call(&self, name: &str, args: &str) -> Result<String, String>;
}

struct Session {
    context: v8::Global<v8::Context>,
    run: v8::Global<v8::Function>,
}

struct State {
    iso: v8::SharedIsolate,
    sessions: HashMap<u64, Session>,
    next: u64,
}

pub struct ServerIsolate {
    host: OnceLock<Arc<dyn CliHost>>,
    state: Mutex<Option<State>>,
    /// Mods' console JS (`Registry::cli_js`, screeps-krebs), run in every session once its sandbox is set up.
    mod_cli: OnceLock<Vec<String>>,
}

thread_local! {
    /// The host and the output of the line running on this thread.
    static CLI: RefCell<Option<(Arc<dyn CliHost>, String)>> = const { RefCell::new(None) };
}

/// The CLI bundle: the loader, then each `cli/*.js`, lodash and the constants as modules.
fn bundle() -> &'static str {
    static B: OnceLock<String> = OnceLock::new();
    B.get_or_init(|| {
        let files = crate::bundle::FILES;
        let mut s = String::from(files.iter().find(|(n, _)| *n == "cli/loader.js").map_or("", |(_, src)| *src));
        s.push('\n');
        let mut add = |name: &str, src: &str| {
            s.push_str("__chitinDefine(");
            util::json::write_quoted(&mut s, name);
            s.push_str(", function(module, exports, require) {\n");
            s.push_str(src);
            s.push_str("\n});\n");
        };
        for (name, src) in files.iter().filter(|(n, _)| (n.starts_with("cli/") && *n != "cli/loader.js") || *n == "vendor/lodash.js") {
            add(name, src);
        }
        add("common/constants.js", &format!("module.exports = {};", engine::constants::constants_json()));
        s
    })
}

impl Default for ServerIsolate {
    fn default() -> Self {
        ServerIsolate { host: OnceLock::new(), state: Mutex::new(None), mod_cli: OnceLock::new() }
    }
}

impl ServerIsolate {
    /// The host the CLI natives talk to; set once, before the first session.
    pub fn set_host(&self, host: Arc<dyn CliHost>) {
        let _ = self.host.set(host);
    }

    /// Mods' console JS (`Registry::cli_js`): each a JS function `(sandbox, call) => { … }`; set once, before the first
    /// session.
    pub fn set_mod_cli(&self, sources: Vec<String>) {
        let _ = self.mod_cli.set(sources);
    }

    /// The connection greeting (js/cli/server.js) with `{build} ` filled in.
    pub fn greeting(&self) -> String {
        const SERVER_JS_GREETING: &str = "Screeps server {build} running on port 21025.\r\nThis CLI interface contains a virtual JavaScript machine which you can use to invoke internal server commands. Any valid JavaScript code is allowed. Type \"help()\" to learn more about commands.\r\n";
        SERVER_JS_GREETING.replace("{build} ", &format!("v{} ", env!("CARGO_PKG_VERSION"))) + "< \r\n"
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<State>> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// One context per CLI connection (CLI-SANDBOX). Err: the bundle failed to start.
    pub fn open_session(&self) -> Result<u64, String> {
        self.open_session_as(false)
    }

    /// A context for one screeps-launcher HTTP CLI request (`POST /cli`): `eval` takes the whole body as one
    /// expression and returns what the launcher writes (js/cli/loader.js `__cliStart`, `natives.launcher`).
    pub fn open_launcher_session(&self) -> Result<u64, String> {
        self.open_session_as(true)
    }

    fn open_session_as(&self, launcher: bool) -> Result<u64, String> {
        let host = self.host.get().cloned().ok_or("the CLI has no host")?;
        let mut state = self.lock();
        if state.is_none() {
            let mut iso = v8::Isolate::new(v8::CreateParams::default());
            iso.set_microtasks_policy(v8::MicrotasksPolicy::Explicit);
            // SAFETY: nothing but plain-fn natives is attached to this isolate.
            let iso = unsafe { iso.try_into_shared() }.map_err(|e| e.to_string())?;
            *state = Some(State { iso, sessions: HashMap::new(), next: 1 });
        }
        let st = state.as_mut().unwrap();
        let (session, _) = with_host(host, || {
            let mut locker = st.iso.lock();
            v8::scope!(let scope, &mut *locker);
            let context = v8::Context::new(scope, Default::default());
            let scope = &mut v8::ContextScope::new(scope, context);
            v8::tc_scope!(let tc, scope);
            let src = v8::String::new(tc, bundle()).ok_or("bundle too large")?;
            macro_rules! fail {
                () => {
                    return Err(tc.exception().map(|e| crate::natives::error_text(tc, e)).unwrap_or_else(|| "CLI bundle failed".into()))
                };
            }
            let Some(script) = v8::Script::compile(tc, src, None) else { fail!() };
            if script.run(tc).is_none() {
                fail!();
            }
            let global = context.global(tc);
            let key = v8::String::new(tc, "__cliStart").unwrap();
            let start = global.get(tc, key.into()).and_then(|f| v8::Local::<v8::Function>::try_from(f).ok()).ok_or("no __cliStart")?;
            let natives = v8::Object::new(tc);
            crate::natives::set_fn(tc, natives, "docs", docs);
            crate::natives::set_fn(tc, natives, "write", write);
            crate::natives::set_fn(tc, natives, "env", env);
            crate::natives::set_fn(tc, natives, "call", call);
            crate::natives::set_fn(tc, natives, "out", out);
            if launcher {
                let key = v8::String::new(tc, "launcher").unwrap();
                let yes = v8::Boolean::new(tc, true);
                natives.set(tc, key.into(), yes.into());
            }
            let sources = self.mod_cli.get().map(Vec::as_slice).unwrap_or(&[]);
            let mods = v8::Array::new(tc, sources.len() as i32);
            for (i, src) in sources.iter().enumerate() {
                let s = v8::String::new(tc, src).ok_or("mod console JS too large")?;
                mods.set_index(tc, i as u32, s.into());
            }
            let key = v8::String::new(tc, "mods").unwrap();
            natives.set(tc, key.into(), mods.into());
            let Some(run) = start.call(tc, global.into(), &[natives.into()]) else { fail!() };
            let run = v8::Local::<v8::Function>::try_from(run).map_err(|_| "__cliStart returned no function")?;
            tc.perform_microtask_checkpoint();
            Ok(Session { context: v8::Global::new(tc, context), run: v8::Global::new(tc, run) })
        });
        let session = session?;
        let id = st.next;
        st.next += 1;
        st.sessions.insert(id, session);
        Ok(id)
    }

    /// Runs one line in the session; the output it wrote (CLI-OUTPUT), promise results included.
    pub fn eval(&self, session: u64, line: &str) -> String {
        let Some(host) = self.host.get().cloned() else { return String::new() };
        let mut state = self.lock();
        let Some(st) = state.as_mut() else { return String::new() };
        let Some(s) = st.sessions.get(&session) else { return String::new() };
        let (_, text) = with_host(host, || {
            let mut locker = st.iso.lock();
            v8::scope!(let scope, &mut *locker);
            let context = v8::Local::new(scope, &s.context);
            let scope = &mut v8::ContextScope::new(scope, context);
            v8::tc_scope!(let tc, scope);
            let run = v8::Local::new(tc, &s.run);
            let arg = v8::String::new(tc, line).unwrap();
            let recv = context.global(tc);
            if run.call(tc, recv.into(), &[arg.into()]).is_none() {
                // run() catches everything a line throws; this is a termination
                let text = tc.exception().map(|e| crate::natives::error_text(tc, e)).unwrap_or_default();
                print(&format!("< {text}\r\n"));
            }
            tc.perform_microtask_checkpoint();
        });
        text
    }

    pub fn close_session(&self, session: u64) {
        let mut state = self.lock();
        if let Some(st) = state.as_mut() {
            let s = st.sessions.remove(&session);
            if let Some(s) = s {
                // Globals drop under the isolate's lock
                let _locker = st.iso.lock();
                drop(s);
            }
        }
    }
}

/// Runs `f` with `host` reachable from the natives; returns its result and the output written.
fn with_host<R>(host: Arc<dyn CliHost>, f: impl FnOnce() -> R) -> (R, String) {
    CLI.with(|c| *c.borrow_mut() = Some((host, String::new())));
    let r = f();
    let text = CLI.with(|c| c.borrow_mut().take().map(|(_, t)| t).unwrap_or_default());
    (r, text)
}

fn host() -> Option<Arc<dyn CliHost>> {
    CLI.with(|c| c.borrow().as_ref().map(|(h, _)| h.clone()))
}

fn print(text: &str) {
    CLI.with(|c| {
        if let Some((_, t)) = c.borrow_mut().as_mut() {
            t.push_str(text);
        }
    });
}

fn arg(scope: &mut v8::PinScope, args: &v8::FunctionCallbackArguments, i: i32) -> String {
    args.get(i).to_rust_string_lossy(scope)
}

fn throw(scope: &mut v8::PinScope, msg: &str) {
    let m = v8::String::new(scope, msg).unwrap();
    let e = v8::Exception::error(scope, m);
    scope.throw_exception(e);
}

fn ret_str(scope: &mut v8::PinScope, rv: &mut v8::ReturnValue<v8::Value>, s: &str) {
    match v8::String::new(scope, s) {
        Some(v) => rv.set(v.into()),
        None => throw(scope, "result too large"),
    }
}

fn docs(scope: &mut v8::PinScope, args: v8::FunctionCallbackArguments, mut rv: v8::ReturnValue<v8::Value>) {
    let coll = arg(scope, &args, 0);
    if let Some(text) = host().and_then(|h| h.docs(&coll)) {
        ret_str(scope, &mut rv, &text);
    }
}

fn write(scope: &mut v8::PinScope, args: v8::FunctionCallbackArguments, mut rv: v8::ReturnValue<v8::Value>) {
    let (coll, writes) = (arg(scope, &args, 0), arg(scope, &args, 1));
    match host().map(|h| h.write(&coll, &writes)) {
        Some(Ok(text)) => ret_str(scope, &mut rv, &text),
        Some(Err(e)) => throw(scope, &e),
        None => throw(scope, "no CLI host"),
    }
}

fn env(scope: &mut v8::PinScope, args: v8::FunctionCallbackArguments, mut rv: v8::ReturnValue<v8::Value>) {
    let (op, a) = (arg(scope, &args, 0), arg(scope, &args, 1));
    match host().map(|h| h.env(&op, &a)) {
        Some(Ok(Some(text))) => ret_str(scope, &mut rv, &text),
        Some(Ok(None)) => {}
        Some(Err(e)) => throw(scope, &e),
        None => throw(scope, "no CLI host"),
    }
}

fn call(scope: &mut v8::PinScope, args: v8::FunctionCallbackArguments, mut rv: v8::ReturnValue<v8::Value>) {
    let (name, a) = (arg(scope, &args, 0), arg(scope, &args, 1));
    match host().map(|h| h.call(&name, &a)) {
        Some(Ok(text)) => ret_str(scope, &mut rv, &text),
        Some(Err(e)) => throw(scope, &e),
        None => throw(scope, "no CLI host"),
    }
}

fn out(scope: &mut v8::PinScope, args: v8::FunctionCallbackArguments, _rv: v8::ReturnValue<v8::Value>) {
    let text = arg(scope, &args, 0);
    print(&text);
}
