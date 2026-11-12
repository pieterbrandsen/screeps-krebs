//! mirror: reference/engine/src/processor/intents/creeps/pickup.js
//! parity: FAITHFUL
//! Verified against the official processor by the fixtures in crates/engine/tests/fixtures/creeps (generate.js).

use crate::atom;
use crate::doc::Atom;
use crate::live::{to_js_string, to_number, truthy, Crash, O, V};
use crate::processor::{bulk_remove, delete_room_object, ge, get_or_undef, js_add, js_min, obj1, or0, out_of_range, prop, put, room_object, type_is, update, RoomScope};
use crate::utils;

/// CREEP-PICKUP; shared with power creeps.
pub fn run(object: &O, intent: &V, scope: &mut RoomScope) -> Result<(), Crash> {
    if !truthy(&object.get(atom!("store"))) {
        object.set(atom!("store"), V::Obj(O::new()));
    }
    let carry = utils::calc_resources(&V::Obj(object.clone()))?;
    if truthy(&object.get(atom!("spawning"))) || ge(&V::Num(carry), &object.get(atom!("storeCapacity"))) {
        return Ok(());
    }
    let Some(target) = room_object(scope, &prop(intent, atom!("id"))?) else { return Ok(()) };
    if !type_is(&target, "energy") {
        return Ok(());
    }
    if out_of_range(object, &target, 1.0) {
        return Ok(());
    }

    let resource_type = match target.get(atom!("resourceType")) {
        v if truthy(&v) => v,
        _ => V::str("energy"),
    };
    let rt = Atom::intern(&to_js_string(&resource_type));
    let amount = js_min(to_number(&object.get(atom!("storeCapacity"))) - carry, to_number(&target.get(rt)));

    target.set(rt, V::Num(to_number(&target.get(rt)) - amount));
    let store = object.get(atom!("store"));
    put(&store, rt, js_add(&or0(get_or_undef(&store, rt)), &V::Num(amount)))?;

    if !truthy(&target.get(rt)) {
        bulk_remove(&mut scope.bulk, &target.get(atom!("_id")));
        delete_room_object(scope, &target.get(atom!("_id")));
    } else {
        update(scope, &target, &obj1(rt, target.get(rt)))?;
    }
    update(scope, object, &obj1(atom!("store"), obj1(rt, get_or_undef(&object.get(atom!("store")), rt))))?;
    if scope.ctx.hooks.actionlog_amounts {
        let log = crate::processor::obj! { "x" => target.get(atom!("x")), "y" => target.get(atom!("y")), "amount" => V::Num(amount), "resourceType" => resource_type, "targetId" => target.get(atom!("_id")) };
        crate::processor::put(&object.get(atom!("actionLog")), atom!("pickup"), log)?;
    }
    Ok(())
}
