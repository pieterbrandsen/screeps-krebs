//! mirror: reference/engine/src/processor/intents/creeps/drop.js
//! parity: FAITHFUL
//! Verified against the official processor by the fixtures in crates/engine/tests/fixtures/creeps (generate.js).

use crate::atom;
use crate::doc::Atom;
use crate::live::{to_js_string, to_number, truthy, Crash, O, V};
use crate::processor::intents::_create_energy::create_energy;
use crate::processor::{ge, get_or_undef, gt, obj1, prop, put, resource_type, update, RoomScope};

/// CREEP-DROP; also the overflow drop of harvest, dismantle and `_drop-resources-without-space`.
pub fn run(object: &O, intent: &V, scope: &mut RoomScope) -> Result<(), Crash> {
    let rt_v = prop(intent, atom!("resourceType"))?;
    let Some(rt) = resource_type(&rt_v) else { return Ok(()) };
    let rt = Atom::intern(rt.as_str());
    let amount = prop(intent, atom!("amount"))?;
    let store = object.get(atom!("store"));
    if truthy(&object.get(atom!("spawning"))) || !truthy(&store) || !ge(&get_or_undef(&store, rt), &amount) {
        return Ok(());
    }
    if gt(&amount, &V::Num(0.0)) {
        put(&store, rt, V::Num(to_number(&get_or_undef(&store, rt)) - to_number(&amount)))?;
        let (x, y, room) = (to_number(&object.get(atom!("x"))), to_number(&object.get(atom!("y"))), to_js_string(&object.get(atom!("room"))));
        create_energy(x, y, &room, to_number(&amount), rt.as_str(), scope)?;
        if scope.ctx.hooks.actionlog_amounts {
            let log = crate::processor::obj! { "x" => object.get(atom!("x")), "y" => object.get(atom!("y")), "amount" => amount.clone(), "resourceType" => V::str(rt.as_str()) };
            crate::processor::put(&object.get(atom!("actionLog")), atom!("drop"), log)?;
        }
    }
    let value = get_or_undef(&object.get(atom!("store")), rt);
    update(scope, object, &obj1(atom!("store"), obj1(rt, value)))
}
