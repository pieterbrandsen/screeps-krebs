//! mirror: reference/engine/src/processor/intents/creeps/transfer.js
//! parity: FAITHFUL
//! Verified against the official processor by the fixtures in crates/engine/tests/fixtures/creeps (generate.js).

use crate::atom;
use crate::constants::{EVENT_TRANSFER, LAB_MINERAL_CAPACITY};
use crate::doc::Atom;
use crate::live::{to_number, truthy, Crash, O, V};
use crate::processor::{eq_str, ge, get_or_undef, gt, js_add, lt, obj, obj1, or0, out_of_range, prop, push_event, put, resource_type, room_object, type_is, update, RoomScope};
use crate::utils;

/// CREEP-TRANSFER; shared with power creeps.
pub fn run(object: &O, intent: &V, scope: &mut RoomScope) -> Result<(), Crash> {
    let rt_v = prop(intent, atom!("resourceType"))?;
    let Some(rt) = resource_type(&rt_v) else { return Ok(()) };
    let rt = Atom::intern(rt.as_str());
    let intent_amount = prop(intent, atom!("amount"))?;
    let store = object.get(atom!("store"));
    if truthy(&object.get(atom!("spawning"))) || !truthy(&store) || !ge(&get_or_undef(&store, rt), &intent_amount) || lt(&intent_amount, &V::Num(0.0)) {
        return Ok(());
    }

    let Some(target) = room_object(scope, &prop(intent, atom!("id"))?) else { return Ok(()) };
    if type_is(&target, "creep") && truthy(&target.get(atom!("spawning"))) {
        return Ok(());
    }
    if out_of_range(object, &target, 1.0) {
        return Ok(());
    }

    let target_v = V::Obj(target.clone());
    let target_capacity = utils::capacity_for_resource(&target_v, rt.as_str());
    if !truthy(&target_capacity) {
        return Ok(());
    }
    let mut amount = intent_amount;
    let stored = if truthy(&target.get(atom!("storeCapacityResource"))) {
        prop(&target.get(atom!("store")), rt)?
    } else {
        V::Num(utils::calc_resources(&target_v)?)
    };
    if ge(&stored, &target_capacity) {
        return Ok(());
    }
    if gt(&js_add(&stored, &amount), &target_capacity) {
        amount = V::Num(to_number(&target_capacity) - to_number(&stored));
    }
    if !truthy(&amount) {
        return Ok(());
    }

    let target_store = target.get(atom!("store"));
    put(&target_store, rt, js_add(&or0(prop(&target_store, rt)?), &amount))?;
    update(scope, &target, &obj1(atom!("store"), obj1(rt, get_or_undef(&target.get(atom!("store")), rt))))?;

    let store = object.get(atom!("store"));
    put(&store, rt, V::Num(to_number(&get_or_undef(&store, rt)) - to_number(&amount)))?;
    update(scope, object, &obj1(atom!("store"), obj1(rt, get_or_undef(&object.get(atom!("store")), rt))))?;

    if type_is(&target, "lab") && !eq_str(&rt_v, "energy") && !truthy(&prop(&target.get(atom!("storeCapacityResource")), rt)?) {
        update(scope, &target, &obj! { "storeCapacityResource" => obj1(rt, V::Num(LAB_MINERAL_CAPACITY as f64)), "storeCapacity" => V::Null })?;
    }

    push_event(scope, EVENT_TRANSFER, object.get(atom!("_id")), obj! { "targetId" => target.get(atom!("_id")), "resourceType" => rt_v.clone(), "amount" => amount.clone() });
    if scope.ctx.hooks.actionlog_amounts {
        let log = obj! { "x" => target.get(atom!("x")), "y" => target.get(atom!("y")), "amount" => amount, "resourceType" => rt_v, "targetId" => target.get(atom!("_id")) };
        crate::processor::put(&object.get(atom!("actionLog")), atom!("transferEnergy"), log)?;
    }
    Ok(())
}
