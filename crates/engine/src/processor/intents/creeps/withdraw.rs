//! mirror: reference/engine/src/processor/intents/creeps/withdraw.js
//! parity: FAITHFUL
//! Verified against the official processor by the fixtures in crates/engine/tests/fixtures/creeps (generate.js).
//! BUG-CRASH-WITHDRAW (C): a target without `store` crashes the room under `private` (as vanilla) and is a
//! no-op where the row is fixed.

use crate::atom;
use crate::constants::{EVENT_TRANSFER, LAB_ENERGY_CAPACITY, LAB_MINERAL_CAPACITY, PWR_DISRUPT_TERMINAL};
use crate::doc::Atom;
use crate::live::{to_number, truthy, Crash, O, V};
use crate::processor::{
    eq_str, get_or_undef, gt, js_add, js_min, lodash_find, lodash_match, loose_eq, lt, obj, obj1, or0, out_of_range, prop, push_event, put, resource_type, room_object,
    type_is, update, RoomScope,
};
use crate::utils;

/// CREEP-WITHDRAW; shared with power creeps.
pub fn run(object: &O, intent: &V, scope: &mut RoomScope) -> Result<(), Crash> {
    let rt_v = prop(intent, atom!("resourceType"))?;
    let Some(rt) = resource_type(&rt_v) else { return Ok(()) };
    let rt = Atom::intern(rt.as_str());

    let empty_space = to_number(&object.get(atom!("storeCapacity"))) - utils::calc_resources(&V::Obj(object.clone()))?;
    let mut amount = V::Num(js_min(to_number(&prop(intent, atom!("amount"))?), empty_space));

    if truthy(&object.get(atom!("spawning"))) || !truthy(&object.get(atom!("storeCapacity"))) || lt(&amount, &V::Num(0.0)) {
        return Ok(());
    }
    let game_time = V::Num(scope.game_time as f64);
    let user = object.get(atom!("user"));
    if let Some(c) = &scope.room_controller {
        if !loose_eq(&c.get(atom!("user")), &user) && gt(&c.get(atom!("safeMode")), &game_time) {
            return Ok(());
        }
    }
    let Some(target) = room_object(scope, &prop(intent, atom!("id"))?) else { return Ok(()) };
    if !loose_eq(&user, &target.get(atom!("user")))
        && scope.room_objects.near(&target.get(atom!("x")), &target.get(atom!("y"))).any(|i| {
            eq_str(&i.get(atom!("type")), "rampart")
                && !loose_eq(&i.get(atom!("user")), &user)
                && !truthy(&i.get(atom!("isPublic")))
                && loose_eq(&i.get(atom!("x")), &target.get(atom!("x")))
                && loose_eq(&i.get(atom!("y")), &target.get(atom!("y")))
        })
    {
        return Ok(());
    }
    if out_of_range(object, &target, 1.0) {
        return Ok(());
    }
    if type_is(&target, "nuker") || type_is(&target, "powerBank") {
        return Ok(());
    }
    if type_is(&target, "terminal") {
        let effect = lodash_find(&target.get(atom!("effects")), |e| lodash_match(e, atom!("power"), &V::Num(PWR_DISRUPT_TERMINAL as f64)));
        if let Some(effect) = effect {
            if gt(&prop(&effect, atom!("endTime"))?, &game_time) {
                return Ok(());
            }
        }
    }

    let target_store = target.get(atom!("store"));
    if !truthy(&target_store) && scope.ctx.compat.fix("BUG-CRASH-WITHDRAW") {
        return Ok(());
    }
    let have = prop(&target_store, rt)?;
    if gt(&amount, &have) {
        amount = have;
    }

    let store = object.get(atom!("store"));
    put(&store, rt, js_add(&or0(prop(&store, rt)?), &amount))?;
    update(scope, object, &obj1(atom!("store"), obj1(rt, get_or_undef(&object.get(atom!("store")), rt))))?;

    let target_store = target.get(atom!("store"));
    put(&target_store, rt, V::Num(to_number(&prop(&target_store, rt)?) - to_number(&amount)))?;
    update(scope, &target, &obj1(atom!("store"), obj1(rt, get_or_undef(&target.get(atom!("store")), rt))))?;
    if type_is(&target, "lab") && !eq_str(&rt_v, "energy") && !truthy(&prop(&target.get(atom!("store")), rt)?) {
        update(
            scope,
            &target,
            &obj! { "storeCapacityResource" => obj1(rt, V::Null), "storeCapacity" => V::Num((LAB_ENERGY_CAPACITY + LAB_MINERAL_CAPACITY) as f64) },
        )?;
    }

    push_event(scope, EVENT_TRANSFER, target.get(atom!("_id")), obj! { "targetId" => object.get(atom!("_id")), "resourceType" => rt_v.clone(), "amount" => amount.clone() });
    if scope.ctx.hooks.actionlog_amounts {
        let log = obj! { "x" => target.get(atom!("x")), "y" => target.get(atom!("y")), "amount" => amount, "resourceType" => rt_v, "targetId" => target.get(atom!("_id")) };
        crate::processor::put(&object.get(atom!("actionLog")), atom!("withdraw"), log)?;
    }
    Ok(())
}
