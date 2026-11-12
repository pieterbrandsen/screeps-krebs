//! The UCS perf lanes' two local socket mods (perf/mods in the bot repo):
//!
//! * ucsbot-watch: which rooms, and whether the world map, each account has open in the client, kept as
//!   `{"r":[room, …],"m":mapOpen}` (rooms sorted) in that account's memory segment 98, or `""` when nothing is open. A
//!   room is `subscribe room:<room>` or `room:<shard>/<room>`, the map `mapVisual:<userId>` or `mapVisual:<userId>/<shard>`;
//!   an unsubscribe ends one, a closed connection all of its own. A segment is only written when its value changed. No
//!   tab survives a restart, so the first tick (or the first socket event, if that comes sooner) clears every segment 98
//!   that holds a value of this shape, and nothing else.
//! * ucsbot-recording-rooms: with `<data>/continuous-room-capture.enabled` present (the local mod's switch file), or
//!   `"recordingRooms": true` in ucs.json, one account may watch any number of rooms (rooms.js `USER_LIMIT` lifted).

use backend::mods::Registry;
use backend::socket::server::{SocketEvent, SocketEventCtx};
use engine::world::{Command, UserIdx, World};
use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use util::json::Json;

pub const SEGMENT: u8 = 98;

pub fn register(r: &mut Registry, ucs_json: &Json) {
    r.socket_events.push(on_event);
    r.engine.processor.global_stage.push(clear_on_boot);
    let switch = r.data_dir.join("continuous-room-capture.enabled").exists();
    if switch || matches!(ucs_json.get("recordingRooms"), Some(Json::Bool(true))) {
        backend::socket::rooms::set_user_limit(u32::MAX);
    }
}

struct Tab {
    user: UserIdx,
    rooms: BTreeSet<String>,
    map: bool,
}

/// Per connection its account and what it has open; per account the last value written.
#[derive(Default)]
pub struct Watch {
    tabs: HashMap<u64, Tab>,
    written: HashMap<UserIdx, String>,
}

/// `^room:(?:[^/]+\/)?([WE]\d+[NS]\d+)$`: the room a channel names.
pub fn room_of(channel: &str) -> Option<&str> {
    let rest = channel.strip_prefix("room:")?;
    let room = match rest.split_once('/') {
        Some((shard, room)) if !shard.is_empty() => room,
        Some(_) => return None,
        None => rest,
    };
    world_room(room).then_some(room)
}

fn world_room(name: &str) -> bool {
    let b = name.as_bytes();
    let digits = |s: &[u8]| !s.is_empty() && s.iter().all(u8::is_ascii_digit);
    if b.len() < 4 || !matches!(b[0], b'W' | b'E') {
        return false;
    }
    match b[1..].iter().position(|c| matches!(c, b'N' | b'S')) {
        Some(i) => digits(&b[1..1 + i]) && digits(&b[2 + i..]),
        None => false,
    }
}

/// A value this mod wrote: `{"r": [...], "m": boolean}` and nothing else.
pub fn is_watch_record(value: &str) -> bool {
    match Json::parse(value) {
        Ok(Json::Obj(fields)) => {
            fields.len() == 2 && matches!(fields.iter().find(|(k, _)| k == "r"), Some((_, Json::Arr(_)))) && matches!(fields.iter().find(|(k, _)| k == "m"), Some((_, Json::Bool(_))))
        }
        _ => false,
    }
}

impl Watch {
    fn record(&self, user: UserIdx) -> String {
        let mut rooms = BTreeSet::new();
        let mut map = false;
        for tab in self.tabs.values().filter(|t| t.user == user) {
            rooms.extend(tab.rooms.iter().cloned());
            map |= tab.map;
        }
        if rooms.is_empty() && !map {
            return String::new();
        }
        let r = Json::Arr(rooms.into_iter().map(Json::Str).collect());
        Json::Obj(vec![("r".into(), r), ("m".into(), Json::Bool(map))]).stringify()
    }

    /// The value to write for the account, if it changed.
    fn write(&mut self, user: UserIdx) -> Option<(UserIdx, String)> {
        let value = self.record(user);
        if self.written.get(&user) == Some(&value) {
            return None;
        }
        self.written.insert(user, value.clone());
        Some((user, value))
    }

    /// What the event changes: the segment to write, if any. `user` is the connection's account and its id.
    pub fn event(&mut self, conn: u64, user: Option<(UserIdx, &str)>, event: SocketEvent<'_>) -> Option<(UserIdx, String)> {
        let (channel, open) = match event {
            SocketEvent::Close => {
                let tab = self.tabs.remove(&conn)?;
                return self.write(tab.user);
            }
            SocketEvent::Subscribe(c) => (c, true),
            SocketEvent::Unsubscribe(c) => (c, false),
        };
        let (u, id) = user?;
        let room = room_of(channel);
        let map = channel.strip_prefix("mapVisual:").and_then(|rest| rest.strip_prefix(id)).is_some_and(|rest| rest.is_empty() || rest.starts_with('/'));
        if room.is_none() && !map {
            return None;
        }
        let tab = self.tabs.entry(conn).or_insert_with(|| Tab { user: u, rooms: BTreeSet::new(), map: false });
        match room {
            Some(room) if open => {
                tab.rooms.insert(room.to_string());
            }
            Some(room) => {
                tab.rooms.remove(room);
            }
            None => tab.map = open,
        }
        self.write(u)
    }
}

static WATCH: Mutex<Option<Watch>> = Mutex::new(None);
static CLEARED: AtomicBool = AtomicBool::new(false);

fn set_segment(w: &mut World, u: UserIdx, value: &str) {
    if (u as usize) < w.users.len() {
        backend::edit_user(w, u, |st| {
            let mut m = (*st.segments).clone();
            m.insert(SEGMENT, util::jsstr::JsStr::from_str(value));
            st.segments = std::sync::Arc::new(m);
        });
    }
}

/// Every segment 98 in this mod's shape becomes `""`.
fn clear(w: &mut World) {
    let stale: Vec<UserIdx> = (0..w.users.len()).filter(|&i| w.users[i].segments.get(&SEGMENT).is_some_and(|v| is_watch_record(&v.to_string_lossy()))).map(|i| i as UserIdx).collect();
    for u in stale {
        set_segment(w, u, "");
    }
}

fn clear_on_boot(w: &mut World, _ctx: &engine::processor::global::GlobalCtx) {
    if !CLEARED.swap(true, Ordering::Relaxed) {
        clear(w);
    }
}

fn send(commands: &std::sync::mpsc::Sender<Command>, apply: impl FnOnce(&mut World) + Send + 'static) {
    let _ = commands.send(Command {
        apply: Box::new(move |w| {
            apply(w);
            Ok(Json::Null)
        }),
        reply: None,
    });
}

fn on_event(event: SocketEvent<'_>, ctx: &SocketEventCtx<'_>) {
    let Some(commands) = ctx.commands else { return };
    if !CLEARED.swap(true, Ordering::Relaxed) {
        send(commands, clear);
    }
    let user = ctx.user.and_then(|u| ctx.view.users.get(u as usize).map(|s| (u, s.id.as_str())));
    let change = WATCH.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(Watch::default).event(ctx.conn, user, event);
    if let Some((u, value)) = change {
        send(commands, move |w| set_segment(w, u, &value));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "a11ce0000000001";

    #[test]
    fn rooms_as_the_local_mods_regex() {
        assert_eq!(room_of("room:W7N3"), Some("W7N3"));
        assert_eq!(room_of("room:shard0/E12S4"), Some("E12S4"));
        assert_eq!(room_of("room:/W7N3"), None);
        assert_eq!(room_of("room:a/b/W7N3"), None);
        assert_eq!(room_of("room:W7N3x"), None);
        assert_eq!(room_of("roomMap2:W7N3"), None);
    }

    #[test]
    fn watch_records_only_in_the_mods_shape() {
        assert!(is_watch_record(r#"{"r":["W1N1"],"m":false}"#));
        assert!(!is_watch_record(r#"{"r":["W1N1"],"m":false,"x":1}"#));
        assert!(!is_watch_record(r#"{"r":"W1N1","m":false}"#));
        assert!(!is_watch_record(""));
        assert!(!is_watch_record("bot data"));
    }

    #[test]
    fn tabs_add_up_per_account_and_close_alone() {
        let mut w = Watch::default();
        let me = Some((0, ID));
        assert_eq!(w.event(1, me, SocketEvent::Subscribe("room:W2N1")), Some((0, r#"{"r":["W2N1"],"m":false}"#.into())));
        assert_eq!(w.event(1, me, SocketEvent::Subscribe("room:shard/W1N1")), Some((0, r#"{"r":["W1N1","W2N1"],"m":false}"#.into())));
        assert_eq!(w.event(2, me, SocketEvent::Subscribe(&format!("mapVisual:{ID}/shard"))), Some((0, r#"{"r":["W1N1","W2N1"],"m":true}"#.into())));
        assert_eq!(w.event(2, me, SocketEvent::Subscribe("room:W2N1")), None, "an unchanged value is not written again");
        assert_eq!(w.event(1, me, SocketEvent::Subscribe("mapVisual:someoneelse")), None);
        assert_eq!(w.event(1, None, SocketEvent::Subscribe("room:W3N3")), None);
        assert_eq!(w.event(1, me, SocketEvent::Close), Some((0, r#"{"r":["W2N1"],"m":true}"#.into())));
        assert_eq!(w.event(2, me, SocketEvent::Unsubscribe(&format!("mapVisual:{ID}"))), Some((0, r#"{"r":["W2N1"],"m":false}"#.into())));
        assert_eq!(w.event(2, me, SocketEvent::Unsubscribe("room:W2N1")), Some((0, String::new())));
        assert_eq!(w.event(2, me, SocketEvent::Close), None);
        assert_eq!(w.event(9, me, SocketEvent::Close), None, "a connection that opened nothing");
    }
}
