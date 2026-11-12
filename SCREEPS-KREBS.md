# screeps-krebs

This fork of [Krebs](https://github.com/DefaultO/krebs) runs the UCS bot's perf lanes. It adds what the lanes' Node
server got from its mods: a few hooks in the core, and one mod, `ucs`, that uses them. Without `"mods": ["ucs"]` and
`cli.http`, the server behaves like upstream Krebs.

## Changes to the core

| Where | What |
| --- | --- |
| `cli.http` (krebs.json) | The console port speaks screeps-launcher's HTTP CLI. `POST /cli` evaluates the whole body in a fresh session, waits for the result and writes it once (falsy values and long strings included). `GET /greeting` answers too. It is off by default, and a request with an `Origin` header is refused. |
| Console isolate | Made from the players' startup snapshot. On a stock V8 (every Linux build), the old order aborted the server the first time a player VM was created after the console. |
| `Registry::cli_js`, `cli_calls` | Console helpers for mods, the way vanilla mods add to `config.cli`. |
| `Registry::socket_events` | A mod hears each `subscribe`, `unsubscribe` and closed connection, with the connection id, its user, the world and the command channel. These are vanilla's `onSubscribe`, `onUnsubscribe` and `conn.on('close')`. |
| `socket::rooms::set_user_limit` | A mod can change the 2-room limit per account. |
| `EngineHooks::history_fill_gaps`, `history_buffered` | History chunks built the way the lanes' screepsmod-history fork builds them, and which windows the writer still holds. |
| `ProcessorHooks::actionlog_amounts` | `pickup`, `withdraw`, `drop` and `transferEnergy` record `{x, y, amount, resourceType, targetId}` in actionLog, as the lanes' engine fork does. |
| Auth | Verifies screepsmod-auth password hashes (PBKDF2-HMAC-SHA256), and accepts long-lived API tokens from `users[].authTokens`. These stay private, as `password` does. `util::sha256` has the hashes. |
| `world.original` (krebs.json) | An official Loki db.json that `system.resetAllData()` reloads, as vanilla reloads `db.original.json`. `serve` also imports it into a data directory that has no world yet. The lanes use the launcher's seed world (`@screeps/launcher/init_dist/db.json`). |
| `storage.env.set(GAMETIME, t)` | Sets the world's clock, as vanilla's next tick reads it from env (stage lanes move a paused world to the stage's tick). The virtual wall clock does not jump. |
| `EngineHooks::init` | Runs once when the server starts, before the first tick: vanilla's `engine.on('init', 'main')`. |
| `/api/version` | `serverData.shards` comes from the backend config, so a mod can name the shard. |

## The `ucs` mod

| Lane server mod | In `mods/ucs` |
| --- | --- |
| screepsmod-admin-utils: `/api/game/room-objects`, `/api/game/shards/info` | `lib.rs` |
| screepsmod-admin-utils: `utils.setCPULimit` and the other CPU helpers, whitelist, gclToCPU, inactive-user cleanup, shard name | `admin.rs` |
| screepsmod-auth: `auth.createAuthToken`, password hashes | `admin.rs` and core auth |
| screepsmod-history fork (v1.6.0-status.2): gzipped 100-tick chunks, `/room-history` with its status codes | `history.rs` |
| Engine fork (v4.3.0-actionlog-real-amounts.1): actionLog amounts | core hook, switched on by the mod |
| screepsmod-client-new (v0.2.33-single-shard-room-channel.5): `/client`, plus the lanes' `/broadcast` page | `client.rs` |
| ucsbot-watch: open rooms and map in memory segment 98 | `watch.rs` |
| ucsbot-recording-rooms: no room limit on recording lanes | `watch.rs` |
| @admon-dev/screepsmod-speedrun 1.0.3: bots removed at start | `speedrun.rs` (`"speedrun": true`), with krebs.json `engine.disabledCronjobs` |
| config.yml `serverConfig.whitelist` | `admin.rs` (`whitelist` in ucs.json) |

Not ported, because no lane tooling reads them:

- screepsmod-server-stats
- milestones-history
- the rest of admin-utils

run.sh's stronghold patch (the invaderCore ruins) is world setup the lane runner lays through the console.

Not ported yet: screepsmod-dynamicmarket, which the non-record lanes load. Our bot never uses the market, but the
opponent bots may; the opponent profiles' calibration decides whether it matters.

## Settings

`krebs.json`:

```json
{
  "http": {"host": "127.0.0.1", "port": 21025},
  "cli": {"host": "127.0.0.1", "port": 21026, "http": true},
  "data": "data",
  "mods": ["ucs"],
  "world": {"original": "/krebs/seed-db.json"},
  "engine": {"historyChunkSize": 100}
}
```

`<data>/ucs.json` (every key is optional):

```json
{
  "shardName": "shard",
  "gclToCPU": {"maxCPU": 300, "baseCPU": 20, "stepCPU": 10},
  "history": {"chunkSize": 100, "keepTicks": 200000},
  "clientDir": "/path/to/screepsmod-client-new/dist/embedded",
  "recordingRooms": false,
  "speedrun": false,
  "whitelist": ["Observer", "Invader", "Source Keeper", "Screeps"]
}
```

Files under `<data>`:

| File | Purpose |
| --- | --- |
| `client/` | The client build, used when `clientDir` is not set. |
| `history/<room>/<base>.json.gz` | History chunks. |
| `ucs-whitelist.json` | The whitelist. |
| `continuous-room-capture.enabled` | The same switch file as the local mod. While it exists, as with `recordingRooms`, an account may watch any number of rooms. |

## Building

On Linux, build twice: the first build of a tree with a new mod stops with "mods folder changed".

```bash
cargo build --release -p server
```

```bash
cargo test --release --workspace
```
