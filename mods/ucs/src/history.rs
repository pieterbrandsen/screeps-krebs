//! Room history as the UCS perf lanes' screepsmod-history fork (pieterbrandsen/screepsmod-history v1.6.0-status.2)
//! keeps and serves it, which ScreepsUserTracker, the lane reports and the stock client read:
//!
//! * chunks built as its worker builds them (`EngineHooks::history_fill_gaps`), `historyChunkSize` ticks each, written
//!   gzipped to `<data>/history/<room>/<base>.json.gz` (its file adapter);
//! * on a base divisible by 1000, the room's chunks older than `keepTicks` deleted (`HISTORY_KEEP_TICKS`);
//! * `GET /room-history?room=&time=` answering each case with its own status (status.js, roomHistory.js): 200 the
//!   chunk's JSON; 400 `missing_room`/`bad_room`/`bad_time`/`misaligned`; 425 `window_open` (the window has not
//!   closed) or `upload_pending` (the writer still holds the room's ticks for it); 410 `expired` (older than
//!   `keepTicks`); 404 `no_record`; 500 `read_failed`; 503 `clock_unavailable`. The reason is in `X-History-Status` as
//!   well as in the body `{error, room, time}`.
//!
//! Settings in `<data>/ucs.json`: `"history": {"chunkSize": 100, "keepTicks": 60000}` (defaults 100 and 200,000, as
//! the fork's); `engine.historyChunkSize` in krebs.json must be the same chunk size.

use backend::http::{Request, Response};
use backend::mods::Registry;
use backend::ApiCtx;
use std::path::PathBuf;
use std::sync::OnceLock;
use util::json::Json;

pub const DEFAULT_CHUNK: u64 = 100;
pub const DEFAULT_KEEP_TICKS: u64 = 200_000;
/// status.js BACKFILL_CHUNKS: how far past a window's end a missing chunk is still worth asking the writer about.
const BACKFILL_CHUNKS: u64 = 4;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HistorySettings {
    pub chunk: u64,
    pub keep_ticks: u64,
}

static SETTINGS: OnceLock<HistorySettings> = OnceLock::new();
static DIR: OnceLock<PathBuf> = OnceLock::new();

pub fn parse(ucs_json: &Json) -> HistorySettings {
    let h = ucs_json.get("history");
    let num = |k: &str, d: u64| h.and_then(|h| h.get(k)).and_then(Json::as_f64).filter(|v| *v > 0.0).map_or(d, |v| v as u64);
    HistorySettings { chunk: num("chunkSize", DEFAULT_CHUNK), keep_ticks: num("keepTicks", DEFAULT_KEEP_TICKS) }
}

pub fn register(r: &mut Registry, ucs_json: &Json) {
    let settings = parse(ucs_json);
    let _ = SETTINGS.set(settings);
    let _ = DIR.set(r.data_dir.join("history"));
    r.engine.history_fill_gaps = true;
    r.engine.save_room_history.push(save);
    r.server_data.push(("historyChunkSize", Json::Num(settings.chunk as f64)));
    r.routes.push(backend::Route { method: "GET", path: "/room-history", handler: room_history });
}

fn chunk_file(dir: &std::path::Path, room: &str, base: u64) -> PathBuf {
    dir.join(room).join(format!("{base}.json.gz"))
}

/// The file adapter's write, then on a base divisible by 1000 its cleanup (engine.js upload).
fn save(room: &str, base: u64, data: &Json) {
    let (Some(dir), Some(s)) = (DIR.get(), SETTINGS.get()) else { return };
    let file = chunk_file(dir, room, base);
    if let Some(parent) = file.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&file, util::deflate::gzip_compress(data.stringify().as_bytes()));
    if base % 1000 == 0 && base > s.keep_ticks {
        cleanup(&dir.join(room), base - s.keep_ticks);
    }
}

fn cleanup(room_dir: &std::path::Path, before: u64) {
    let Ok(entries) = std::fs::read_dir(room_dir) else { return };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if let Some(tick) = name.strip_suffix(".json.gz").and_then(|t| t.parse::<u64>().ok()) {
            if tick < before {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

/// What the request asks for, or the 400 it gets (status.js parseRequest).
pub fn parse_request(room: Option<&str>, time: Option<&str>, chunk: u64) -> Result<(String, u64), (u16, &'static str)> {
    let room = room.map(str::trim).unwrap_or("");
    if room.is_empty() {
        return Err((400, "missing_room"));
    }
    if room.len() > 32 || !room.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') {
        return Err((400, "bad_room"));
    }
    let raw = time.map(str::trim).unwrap_or("");
    let Some(time) = (!raw.is_empty()).then(|| raw.parse::<u64>().ok()).flatten() else { return Err((400, "bad_time")) };
    if chunk > 0 && time % chunk != 0 {
        return Err((400, "misaligned"));
    }
    Ok((room.to_string(), time))
}

/// status.js resolve, for a chunk that is not stored (or could not be read).
pub fn resolve_missing(read_failed: bool, time: u64, chunk: u64, game_time: Option<u64>, keep_ticks: u64, pending: bool) -> (u16, &'static str) {
    if read_failed {
        return (500, "read_failed");
    }
    let Some(now) = game_time else { return (503, "clock_unavailable") };
    if time + chunk > now {
        return (425, "window_open");
    }
    if pending {
        return (425, "upload_pending");
    }
    if keep_ticks > 0 && time < now.saturating_sub(keep_ticks) {
        return (410, "expired");
    }
    (404, "no_record")
}

/// Whether the writer still holds ticks of the room for the window `[time, time + chunk)` or later; only asked near the
/// clock (status.js shouldProbePending).
fn pending(room: &str, time: u64, chunk: u64, now: u64) -> bool {
    if now.saturating_sub(time) > chunk * BACKFILL_CHUNKS {
        return false;
    }
    engine::hooks::history_buffered().lock().unwrap_or_else(|e| e.into_inner()).get(room).is_some_and(|&lowest| lowest < time + chunk)
}

fn status(code: u16, reason: &str, room: &str, time: u64) -> Response {
    let mut res = Response::json(code, &Json::Obj(vec![
        ("error".into(), Json::Str(reason.into())),
        ("room".into(), Json::Str(room.into())),
        ("time".into(), Json::Num(time as f64)),
    ]));
    res.headers.push(("X-History-Status".into(), reason.into()));
    res
}

fn room_history(req: &Request, ctx: &ApiCtx) -> Response {
    let (Some(dir), Some(s)) = (DIR.get(), SETTINGS.get()) else { return Response::text(500, "text/plain", "history is not set up") };
    let (room, time) = match parse_request(req.param("room"), req.param("time"), s.chunk) {
        Ok(rt) => rt,
        Err((code, reason)) => {
            let mut res = Response::json(code, &Json::Obj(vec![("error".into(), Json::Str(reason.into()))]));
            res.headers.push(("X-History-Status".into(), reason.into()));
            return res;
        }
    };
    let file = chunk_file(dir, &room, time);
    let read_failed = match std::fs::read(&file) {
        Ok(gz) => match util::deflate::gzip_decompress(&gz, 512 * 1024 * 1024) {
            Ok(bytes) => return Response::text(200, "application/json; charset=utf-8", &String::from_utf8_lossy(&bytes)),
            Err(_) => true,
        },
        Err(e) => e.kind() != std::io::ErrorKind::NotFound,
    };
    let now = ctx.published.get().time;
    let (code, reason) = resolve_missing(read_failed, time, s.chunk, Some(now), s.keep_ticks, pending(&room, time, s.chunk, now));
    status(code, reason, &room, time)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_as_status_js() {
        assert_eq!(parse_request(None, Some("100"), 100), Err((400, "missing_room")));
        assert_eq!(parse_request(Some("../etc"), Some("100"), 100), Err((400, "bad_room")));
        assert_eq!(parse_request(Some("W7N3"), Some(""), 100), Err((400, "bad_time")));
        assert_eq!(parse_request(Some("W7N3"), Some("-1"), 100), Err((400, "bad_time")));
        assert_eq!(parse_request(Some("W7N3"), Some("150"), 100), Err((400, "misaligned")));
        assert_eq!(parse_request(Some(" W7N3 "), Some("200"), 100), Ok(("W7N3".into(), 200)));
    }

    #[test]
    fn missing_chunks_as_status_js() {
        let r = |t, now, pending| resolve_missing(false, t, 100, now, 60_000, pending);
        assert_eq!(r(900, Some(950), false), (425, "window_open"));
        assert_eq!(r(800, Some(950), true), (425, "upload_pending"));
        assert_eq!(r(800, Some(950), false), (404, "no_record"));
        assert_eq!(r(100, Some(70_000), false), (410, "expired"));
        assert_eq!(r(800, None, false), (503, "clock_unavailable"));
        assert_eq!(resolve_missing(true, 800, 100, Some(950), 60_000, false), (500, "read_failed"));
    }

    #[test]
    fn settings_default_to_the_fork() {
        assert_eq!(parse(&Json::Null), HistorySettings { chunk: 100, keep_ticks: 200_000 });
        let j = Json::parse(r#"{"history":{"chunkSize":100,"keepTicks":60000}}"#).unwrap();
        assert_eq!(parse(&j), HistorySettings { chunk: 100, keep_ticks: 60_000 });
    }
}
