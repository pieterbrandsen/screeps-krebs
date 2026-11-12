//! Engine-side mod hooks (ARCHITECTURE.md §10.1, FROZEN). Engine cannot see backend types; the full mod
//! registry is `backend::mods::Registry`.

use crate::cron::CronCtx;
use crate::live::{Crash, O, V};
use crate::mapgen;
use crate::processor::global::GlobalCtx;
use crate::processor::RoomScope;
use crate::system;
use crate::world::{UserIdx, World, WorldView};

#[derive(Default)]
pub struct ProcessorHooks {
    /// customIntentTypes
    pub intent_types: Vec<(&'static str, &'static [(&'static str, system::FieldType)])>,
    pub object_intents: Vec<(&'static str /*object type*/, &'static str /*intent*/, fn(&O, &V, &mut RoomScope) -> Result<(), Crash>)>,
    pub object_ticks: Vec<(&'static str /*custom type*/, fn(&O, &mut RoomScope) -> Result<(), Crash>)>,
    /// processObject (prescan)
    pub process_object: Vec<fn(&O, &mut RoomScope)>,
    pub pre_object_intents: Vec<fn(Option<&O>, UserIdx, &V, &mut RoomScope)>,
    pub post_object_intents: Vec<fn(&O, UserIdx, &V, &mut RoomScope)>,
    /// postProcessObject
    pub post_object: Vec<fn(&O, &mut RoomScope)>,
    /// processRoom
    pub room_end: Vec<fn(&mut RoomScope)>,
    pub activation_rules: Vec<fn(&O, &RoomScope) -> bool>,
    /// After market.
    pub global_stage: Vec<fn(&mut World, &GlobalCtx)>,
}

#[derive(Default)]
pub struct EngineHooks {
    pub processor: ProcessorHooks,
    /// (name, period s in vclock, job)
    pub crons: Vec<(&'static str, u32, fn(&mut World, &mut CronCtx))>,
    /// World-gen post-hook.
    pub gen_room: Vec<fn(&mut mapgen::GeneratedRoom, &mut util::rng::Rng)>,
    pub main_loop_stage: Vec<fn(Stage, &WorldView)>,
    pub next_tick_delay: Option<fn(used_ms: u64) -> u64>,
    /// `config.engine.on('saveRoomHistory', (roomName, baseTime, data) => …)` (FMT-HISTORY-CHUNK).
    pub save_room_history: Vec<fn(&str, u64, &util::json::Json)>,
    /// screeps-krebs: build chunks as screepsmod-history's worker does (`{}` for a tick the room did not save, as many
    /// ticks as it saved, `"undefined"` keys dropped) instead of vanilla's stop at the first missing tick.
    pub history_fill_gaps: bool,
}

/// screeps-krebs: the rooms whose history the writer holds and has not uploaded yet, each with the lowest tick it
/// holds; what tells "this chunk is not written yet" (screepsmod-history's `upload_pending`) from "nothing happened".
pub fn history_buffered() -> &'static std::sync::Mutex<std::collections::HashMap<Box<str>, u64>> {
    static BUFFERED: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<Box<str>, u64>>> = std::sync::OnceLock::new();
    BUFFERED.get_or_init(Default::default)
}

/// `mainLoopStage` event names (reference/engine/src/main.js:16-115, TICK-STAGE-EVENTS).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Stage {
    Start,
    GetUsers,
    AddUsersToQueue,
    WaitForUsers,
    GetRooms,
    AddRoomsToQueue,
    WaitForRooms,
    Commit1,
    Global,
    Commit2,
    IncrementGameTime,
    NotifyRoomsDone,
    Custom,
    Finish,
}
