//! CLI sessions in the server isolate over an in-memory host: the output rules of vanilla's sandbox.js/server.js
//! (CLI-OUTPUT), help texts (CLI-HELP), and the storage fixtures of tools/oracle/cli-storage.js (official LokiJS +
//! storage db.js results) replayed through js/cli/storage.js in V8 (CLI-STORAGE).

use runtime::server_isolate::{CliHost, ServerIsolate};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use util::json::Json;

#[derive(Default)]
struct Mem {
    colls: Mutex<HashMap<String, Vec<Json>>>,
    env: Mutex<HashMap<String, Json>>,
    calls: Mutex<Vec<String>>,
    gen: Mutex<u32>,
}

impl CliHost for Mem {
    fn docs(&self, coll: &str) -> Option<String> {
        Some(Json::Arr(self.colls.lock().unwrap().get(coll).cloned().unwrap_or_default()).stringify())
    }
    fn write(&self, coll: &str, writes: &str) -> Result<String, String> {
        let mut colls = self.colls.lock().unwrap();
        let docs = colls.entry(coll.to_string()).or_default();
        let mut inserted = Vec::new();
        for w in Json::parse(writes).unwrap().as_array().unwrap() {
            let op = w.get("op").and_then(Json::as_str).unwrap();
            if op == "insert" {
                let mut d = w.get("doc").unwrap().clone();
                let falsy = matches!(d.get("_id"), None | Some(Json::Null) | Some(Json::Bool(false))) || d.get("_id").and_then(Json::as_str) == Some("") || d.get("_id").and_then(Json::as_f64) == Some(0.0);
                if falsy {
                    let mut g = self.gen.lock().unwrap();
                    *g += 1;
                    if let Json::Obj(o) = &mut d {
                        o.retain(|(k, _)| k != "_id");
                        o.push(("_id".into(), Json::Str(format!("gen{g}"))));
                    }
                }
                docs.push(d.clone());
                inserted.push(d);
            } else {
                let id = w.get("id").unwrap();
                let Some(i) = docs.iter().position(|d| d.get("_id") == Some(id)) else { continue };
                if op == "remove" {
                    docs.remove(i);
                } else {
                    docs[i] = w.get("doc").unwrap().clone();
                }
            }
        }
        Ok(Json::Arr(inserted).stringify())
    }
    fn env(&self, op: &str, args: &str) -> Result<Option<String>, String> {
        let args = Json::parse(args).unwrap();
        let a = |i: usize| args.at(i).cloned().unwrap_or(Json::Null);
        let key = a(0).as_str().unwrap_or("").to_string();
        let mut env = self.env.lock().unwrap();
        Ok(match op {
            "get" => env.get(&key).map(Json::stringify),
            "set" => {
                env.insert(key, a(1));
                Some(a(1).stringify())
            }
            "sadd" => {
                let e = env.entry(key).or_insert(Json::Arr(vec![]));
                if let Json::Arr(v) = e {
                    v.push(a(1));
                }
                Some(e.stringify())
            }
            _ => None,
        })
    }
    fn call(&self, name: &str, args: &str) -> Result<String, String> {
        self.calls.lock().unwrap().push(format!("{name} {args}"));
        match name {
            "bots" => Ok(r#"{"simplebot":"/bots/simplebot"}"#.into()),
            "strongholdTemplates" => Ok(r#"{"bunker1":{"description":"Small stronghold"}}"#.into()),
            "cronjobs" => Ok(r#"["genInvaders"]"#.into()),
            "removeRoom" => Err("nope".into()),
            "closeRoom" => Ok(r#"{"modified":1}"#.into()),
            _ => Ok("null".into()),
        }
    }
}

fn session(host: Arc<Mem>) -> (ServerIsolate, u64) {
    runtime::init_v8();
    let s = ServerIsolate::default();
    s.set_host(host);
    let id = s.open_session().unwrap();
    (s, id)
}

/// CLI-OUTPUT and CLI-HELP: sandbox.js's result rules and server.js's writer.
#[test]
fn output_rules() {
    let host = Arc::new(Mem::default());
    let (s, id) = session(host.clone());
    assert!(s.greeting().starts_with("Screeps server v") && s.greeting().ends_with("\"help()\" to learn more about commands.\r\n< \r\n"));
    assert_eq!(s.eval(id, "1+1"), "< 2\r\n");
    assert_eq!(s.eval(id, "var x = 5"), "< ");
    assert_eq!(s.eval(id, "x"), "< 5\r\n");
    assert_eq!(s.eval(id, "'OK'"), "< OK\r\n");
    assert_eq!(s.eval(id, "Promise.resolve('OK')"), "< 'OK'\r\n");
    assert_eq!(s.eval(id, "Promise.resolve(0)"), "");
    assert_eq!(s.eval(id, "Promise.reject('nope')"), "< Error: nope\r\n");
    assert_eq!(s.eval(id, "print({a: 1, b: [1, 2]}, 'x')"), "{ a: 1, b: [ 1, 2 ] } 'x'\r\n< ");
    assert_eq!(s.eval(id, "print()"), "\r\n< ");
    assert_eq!(s.eval(id, "throw new TypeError('bad')"), "< TypeError: bad\r\n");
    assert_eq!(s.eval(id, "foo("), "< SyntaxError: Unexpected end of input\r\n");
    assert_eq!(s.eval(id, "help(x)"), "< There is no help page for this object.\r\n");
    let help = s.eval(id, "help()");
    assert!(help.starts_with("< The supported commands are:\r\n    * help() - Print this help text.\r\n"), "{help}");
    let sys = s.eval(id, "help(system)");
    assert!(sys.starts_with("< Available methods:\r\n - system.resetAllData() - Wipe all world data"), "{sys}");
    let db = s.eval(id, "help(storage.db)");
    assert!(db.contains(" - users.power_creeps\r\nAvailable methods: \r\n - find\r\n - findOne\r\n - by\r\n"), "{db}");
    assert!(s.eval(id, "help(strongholds)").contains("Stronghold templates:\r\n - bunker1 [Small stronghold]"));
    assert!(s.eval(id, "help(bots)").contains("Bot AIs:\r\n - simplebot [/bots/simplebot]"));
    // sessions are separate realms
    let id2 = s.open_session().unwrap();
    assert_eq!(s.eval(id2, "typeof x == 'undefined'"), "< true\r\n");
    assert_eq!(s.eval(id2, "typeof x"), "< ", "the string 'undefined' prints like undefined (server.js:16)");
    // server-side commands and their string rejections
    assert_eq!(s.eval(id, "system.pauseSimulation()"), "< 'OK'\r\n");
    assert_eq!(host.env.lock().unwrap().get("mainLoopPaused"), Some(&Json::Str("1".into())));
    assert_eq!(s.eval(id, "system.setTickDuration('x')"), "< Error: Invalid minimal duration x, integer expected.\r\n");
    assert_eq!(s.eval(id, "system.runCronjob('nope')"), "< Error: Cronjob \"nope\" not found\r\n");
    assert_eq!(s.eval(id, "system.runCronjob('genInvaders')"), "< 'OK'\r\n");
    host.colls.lock().unwrap().insert("rooms".into(), vec![Json::parse(r#"{"_id":"W1N1","status":"normal"}"#).unwrap()]);
    assert_eq!(s.eval(id, "map.removeRoom('W1N1')"), "< Error: nope\r\n");
    assert_eq!(s.eval(id, "map.removeRoom('W9N9')"), "< Error: The room W9N9 is not found\r\n");
    assert_eq!(s.eval(id, "map.closeRoom('W1N1')"), "< { modified: 1 }\r\n");
    assert!(host.calls.lock().unwrap().iter().any(|c| c == r#"closeRoom ["W1N1"]"#));
    s.close_session(id2);
}

/// screeps-launcher's HTTP CLI (`cli.http`, screeps-krebs): the whole body one expression, the result awaited and
/// written once, a falsy value too (a line session prints nothing for it), a string as it is however long (a line
/// session's util.inspect cuts it at 10,000 characters and quotes it), a throw as `Error: <stack>`.
#[test]
fn launcher_output_rules() {
    let host = Arc::new(Mem::default());
    let (s, _) = session(host.clone());
    let eval = |body: &str| {
        let id = s.open_launcher_session().unwrap();
        let out = s.eval(id, body);
        s.close_session(id);
        out
    };
    assert_eq!(eval("1 +\n1"), "2\n");
    assert_eq!(eval("Promise.resolve(0)"), "0\n");
    assert_eq!(eval("Promise.resolve(false)"), "false\n");
    assert_eq!(eval("Promise.resolve('')"), "\n");
    assert_eq!(eval("undefined"), "undefined\n");
    let long = eval("Promise.resolve('x'.repeat(20000) + \"'\")");
    assert_eq!(long.len(), 20_002, "a string comes back whole and unquoted");
    assert!(long.ends_with("'\n"));
    assert_eq!(eval("({ a: 1, b: [1, 2] })"), "{ a: 1, b: [ 1, 2 ] }\n");
    assert!(eval("nope.nope").starts_with("Error: ReferenceError: nope is not defined"));
    assert!(eval("Promise.reject(new Error('gone'))").starts_with("Error: Error: gone"));
    assert_eq!(eval("print('hi'); 42"), "'hi'\n42\n");
    // each request is a fresh realm
    eval("x = 5");
    assert_eq!(eval("typeof x"), "undefined\n");
}

/// tools/oracle/cli-storage.js fixtures replayed in V8.
#[test]
fn storage_fixtures() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cli");
    let text = std::fs::read_to_string(dir.join("storage-seed1.ndjson")).unwrap();
    let mut n = 0;
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let case = Json::parse(line).unwrap();
        let name = case.get("name").and_then(Json::as_str).unwrap();
        let sc = case.get("scenario").unwrap();
        let host = Arc::new(Mem::default());
        if let Some(Json::Obj(init)) = sc.get("initial") {
            for (k, v) in init {
                host.colls.lock().unwrap().insert(k.clone(), v.as_array().unwrap().to_vec());
            }
        }
        let (s, id) = session(host.clone());
        let mut results = Vec::new();
        for op in sc.get("ops").and_then(Json::as_array).unwrap() {
            let c = Json::Str(op.get("c").and_then(Json::as_str).unwrap().into()).stringify();
            let m = op.get("m").and_then(Json::as_str).unwrap();
            let j = |k: &str| op.get(k).map_or("undefined".to_string(), Json::stringify);
            let call = match m {
                "update" => format!("storage.db[{c}].update({}, {}, {})", j("query"), j("update"), j("params")),
                "bulk" => format!("storage.db[{c}].bulk({})", j("bulk")),
                "findEx" => format!("storage.db[{c}].findEx({}, {})", j("query"), j("opts")),
                _ => format!("storage.db[{c}].{m}(...{})", j("args")),
            };
            s.eval(id, &format!("R = undefined; {call}.then(v => {{ R = JSON.stringify({{ok: v === undefined ? null : v}}) }}, e => {{ R = JSON.stringify({{error: String(e)}}) }})"));
            let out = s.eval(id, "R");
            let json = out.trim_start_matches("< ").trim_end();
            results.push(Json::parse(json).unwrap_or_else(|e| panic!("{name}: {e}: {out}")));
        }
        let colls = host.colls.lock().unwrap();
        let fin = Json::Obj(["rooms.objects", "users", "market.orders"].iter().map(|k| (k.to_string(), Json::Arr(colls.get(*k).cloned().unwrap_or_default()))).collect());
        let ours = canon(&Json::Obj(vec![("results".into(), Json::Arr(results)), ("final".into(), fin)]));
        let r = case.get("result").unwrap();
        let theirs = Json::Obj(vec![("results".into(), r.get("results").unwrap().clone()), ("final".into(), r.get("final").unwrap().clone())]);
        assert_eq!(ours.stringify(), theirs.stringify(), "{name}");
        n += 1;
    }
    assert!(n > 100);
}

/// Generated ids (`gen<n>` here, genId there) named by first appearance, as the oracle does.
fn canon(v: &Json) -> Json {
    fn walk(x: &Json, names: &mut Vec<String>) -> Json {
        match x {
            Json::Arr(a) => Json::Arr(a.iter().map(|y| walk(y, names)).collect()),
            Json::Obj(o) => Json::Obj(
                o.iter()
                    .map(|(k, y)| {
                        let fresh = matches!(y, Json::Str(s) if k == "_id" && !s.starts_with(['r', 'u', 'm']) || k == "_id" && matches!(y, Json::Str(s) if s.starts_with("gen")));
                        if fresh {
                            let s = y.as_str().unwrap().to_string();
                            let i = names.iter().position(|n| *n == s).unwrap_or_else(|| {
                                names.push(s.clone());
                                names.len() - 1
                            });
                            (k.clone(), Json::Str(format!("#{}", i + 1)))
                        } else {
                            (k.clone(), walk(y, names))
                        }
                    })
                    .collect(),
            ),
            _ => x.clone(),
        }
    }
    walk(v, &mut Vec::new())
}
