//! The record lanes' @admon-dev/screepsmod-speedrun 1.0.3, with `"speedrun": true` in ucs.json:
//!
//! * when the server starts (`engine.on('init', 'main')`), every user with a `bot` field is respawned (its creeps,
//!   structures and sites go) and removed, with its code and Memory;
//! * its other half, deleting the genInvaders, genStrongholds and expandStrongholds cronjobs, is krebs.json's
//!   `engine.disabledCronjobs`, which the lane runner writes beside it.
//!
//! At start means before the lane runner spawns its own bot (which has a `bot` field too): the runner resets the
//! world and restarts the server, and spawns after that.

use backend::mods::Registry;
use engine::coll::{write, Write};
use engine::doc::{Atom, Val};
use engine::world::{UserIdx, World};
use util::json::Json;

pub fn register(r: &mut Registry, ucs_json: &Json) {
    if matches!(ucs_json.get("speedrun"), Some(Json::Bool(true))) {
        r.engine.init.push(remove_bots);
    }
}

/// The users `db.users.find({bot: {$exists: true}})` finds: a `bot` field of any value, null included.
pub fn bots(w: &World) -> Vec<(UserIdx, String)> {
    let bot = Atom::intern("bot");
    let removed = Atom::intern("$removed");
    w.users
        .iter()
        .enumerate()
        .filter(|(_, u)| u.doc.get(removed).is_none() && u.doc.get(bot).is_some())
        .map(|(i, u)| (i as UserIdx, u.id.as_str().to_string()))
        .collect()
}

fn remove_bots(w: &mut World) {
    for (u, id) in bots(w) {
        let name = w.users[u as usize].doc.get(Atom::intern("username")).and_then(Val::as_str).unwrap_or("").to_string();
        println!("[screepsmod-speedrun] removing bot \"{name}\"");
        if let Err(e) = backend::api::user::respawn_user(w, u) {
            eprintln!("[screepsmod-speedrun] could not respawn {name}: {e}");
            continue;
        }
        if let Err(e) = write(w, "users", vec![Write::Remove(id.into())]) {
            eprintln!("[screepsmod-speedrun] could not remove {name}: {e}");
        }
    }
}
