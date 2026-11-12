//! UCS bot perf mod (`mods: ["ucs"]`, or `--mods ucs`). What our lane tooling reads from a perf server that is not
//! in the vanilla backend, ported from screepsmod-admin-utils 1.37.1 (lib/backend.js):
//! * `GET /api/game/room-objects?room=<name>`: `{objects, users}`, every rooms.objects doc of the room as stored,
//!   and `users` keyed by id with `{_id, username, badge}` for every user a doc names, plus "2" (Invader) and "3"
//!   (Source Keeper). No `ok` field and no auth, as admin-utils answers it.
//! * `GET /api/game/shards/info`: `{ok, shards: [{cpuLimit, lastTicks, name, rooms, tick}]}`.
//! * the console helpers, crons and shard name in `admin` (admin-utils' and screepsmod-auth's);
//! * room history as our screepsmod-history fork keeps and serves it, in `history`.
//!
//! The speedrun rules (screepsmod-speedrun 1.0.3 and run.sh's stronghold patch) are world setup, not server code:
//! `engine.disabledCronjobs` turns off genInvaders, genStrongholds and expandStrongholds, and the lane runner removes
//! the NPC bots and lays the invaderCore ruins through the console.

pub mod admin;
pub mod history;

use backend::http::{Request, Response};
use backend::mods::{Manifest, Mod, Registry};
use backend::{ApiCtx, Route};
use engine::doc::{Atom, Val};
use engine::world::WorldView;
use util::json::Json;

pub struct Ucs;

/// What the server registers (modlist finds it by this name).
pub static MOD: Ucs = Ucs;

impl Mod for Ucs {
    fn manifest(&self) -> Manifest {
        Manifest { name: "ucs", version: env!("CARGO_PKG_VERSION"), dependencies: &[] }
    }

    fn register(&self, r: &mut Registry) {
        r.routes.push(Route { method: "GET", path: "/api/game/room-objects", handler: room_objects });
        r.routes.push(Route { method: "GET", path: "/api/game/shards/info", handler: shards_info });
        admin::register(r);
        let ucs_json = Json::parse(&std::fs::read_to_string(r.data_dir.join("ucs.json")).unwrap_or_default()).unwrap_or(Json::Null);
        history::register(r, &ucs_json);
    }
}

fn str_field<'a>(obj: &'a engine::doc::Obj, key: &str) -> Option<&'a str> {
    obj.get(Atom::intern(key)).and_then(Val::as_str)
}

/// The room's docs as admin-utils' `db['rooms.objects'].find({room})` returns them (an unknown room: none).
fn room_objects(req: &Request, ctx: &ApiCtx) -> Response {
    let w = ctx.published.get();
    let room = req.param("room").unwrap_or("");
    Response::text(200, "application/json; charset=utf-8", &room_objects_body(&w, room))
}

fn room_objects_body(w: &WorldView, room: &str) -> String {
    let docs = w.meta.by_name(room).map(|r| w.rooms[r as usize].objects.clone());
    let mut body = String::from("{\"objects\":[");
    let mut ids: Vec<String> = Vec::new();
    for (i, d) in docs.iter().flat_map(|a| a.iter()).enumerate() {
        if i > 0 {
            body.push(',');
        }
        body.push_str(d.json());
        if let Some(u) = str_field(&d.obj, "user") {
            if !u.is_empty() && !ids.iter().any(|x| x == u) {
                ids.push(u.to_string());
            }
        }
    }
    for npc in ["2", "3"] {
        if !ids.iter().any(|x| x == npc) {
            ids.push(npc.to_string());
        }
    }
    let mut users = Vec::new();
    for u in w.users.iter() {
        let Some(id) = str_field(&u.doc, "_id") else { continue };
        if !ids.iter().any(|x| x == id) {
            continue;
        }
        let mut fields = vec![("_id".to_string(), Json::Str(id.into()))];
        if let Some(name) = str_field(&u.doc, "username") {
            fields.push(("username".into(), Json::Str(name.into())));
        }
        if let Some(badge) = u.doc.get(Atom::intern("badge")) {
            fields.push(("badge".into(), badge.to_json()));
        }
        users.push((id.to_string(), Json::Obj(fields)));
    }
    body.push_str("],\"users\":");
    body.push_str(&Json::Obj(users).stringify());
    body.push('}');
    body
}

/// admin-utils' one-shard summary. `lastTicks` is empty: Krebs keeps no tick-duration list in env.
fn shards_info(_req: &Request, ctx: &ApiCtx) -> Response {
    let w = ctx.published.get();
    let shard = Json::Obj(vec![
        ("cpuLimit".into(), Json::Num(0.0)),
        ("lastTicks".into(), Json::Arr(vec![])),
        ("name".into(), Json::Null),
        ("rooms".into(), Json::Num(w.rooms.len() as f64)),
        ("tick".into(), Json::Null),
    ]);
    Response::ok(Json::Obj(vec![("shards".into(), Json::Arr(vec![shard]))]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registers_the_admin_utils_routes() {
        let mut r = Registry::default();
        MOD.register(&mut r);
        assert_eq!(MOD.manifest().name, "ucs");
        assert!(r.routes.iter().any(|x| x.method == "GET" && x.path == "/api/game/room-objects"));
        assert!(r.routes.iter().any(|x| x.method == "GET" && x.path == "/api/game/shards/info"));
    }
}
