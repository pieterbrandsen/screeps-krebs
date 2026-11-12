# Krebs

> **screeps-krebs:** this fork adds what the UCS bot's perf lanes need. See [SCREEPS-KREBS.md](SCREEPS-KREBS.md).

Krebs is a private server for the game Screeps, written in Rust. It runs on your own PC and adds lobbies and a Battle
Royale mode.

In Screeps you do not steer units by hand. You write a program in JavaScript, your bot, and it plays for you. In a match on
Krebs everyone brings a bot, and the bots fight it out on one map. No code of your own yet? A simple starter bot comes in the
download.

Krebs is not affiliated with Screeps or its makers. To play, you need the game
[Screeps: World](https://store.steampowered.com/app/464350/Screeps_World/) on Steam.

This is an early proof of concept. Matches run, but they have no final standings and no replays yet. Windows 10 or 11,
64-bit only.

## Play

1. Download the `Krebs-...-windows-x64.zip` file from the latest release, not the Source Code files. It is the same file on
   [GitHub](https://github.com/DefaultO/krebs/releases/latest) and on
   [Gitea](https://gitea.defaul.to/DefaultO/krebs/releases/latest), so use whichever opens.
2. Unzip it. Right-click the zip and choose Extract All. Do not start Krebs from inside the zip, and do not put it in
   Program Files. Documents or the Desktop are good places.
3. Open the `Krebs` folder that comes out of the zip, and double-click `Krebs.exe`. A black window opens and says how to
   connect. Keep it open while you play.
4. Open Screeps in Steam. On the server list, switch from **Community servers** to **Private server**. The list opens on
   Community servers every time, so switch first.
5. Enter Host `127.0.0.1` and Port `21025`, then press **Connect**.

To stop, close the black window. Your world is saved in the `data` folder beside `Krebs.exe`. Any match that is running ends
when you close it.

### "Windows protected your PC"

Windows shows this blue box for any program without a paid signature, and Krebs has none. Click **More info**, then click
**Run anyway**. You should only need to do this once.

### A match

Press **New lobby**, or join an open one. Up to 8 players fit. Pick the folder with your bot's Screeps code, or drop a folder or a
zip on the window, up to 10 MB. No bot yet? Pick the `starter-bot` folder from the zip. Press **Ready**. When everyone is
ready the match counts down, each player picks a room and places a spawn, and the bots play. Hold **Leave server** to go back
to the server list. One Krebs runs up to 4 matches at a time, and a lobby past that is told to wait for one to end.

You have 3 minutes to pick a room, and with none by then you get a free one. You then have 2 minutes to place a spawn, and
without one by then you are out of the match. You can have a seat in one match at a time. A match you left keeps your seat, so
you can go back to it, until you give the seat up, and it ends a minute after everyone has left it. The host can end any match:
type `stop` and its number in the black window, which shows that number when the match starts. `matches` lists them.

### Playing with friends

Double-click `Let friends join.bat` instead of `Krebs.exe`. The first time, it asks for a Steam Web API key and says where to
get one. It is free, you do it once, and Steam uses it to confirm who signs in. Steam only gives keys to accounts that have
spent at least 5 dollars there. Playing alone needs no key.

The window then lists this PC's addresses. A friend does steps 4 and 5 above with one of those as the Host. Everyone needs
their own copy of Screeps, and a bot: send them the `starter-bot` folder if they have none.

The limits, plainly:

- Those addresses work on your own network. For friends over the internet, you have to forward port 21025 to this PC on
  your router, and give them your public address. Krebs cannot do that for you.
- Windows Firewall may ask about Krebs. Allow it, or nobody else can connect. If friends cannot connect, look in Windows
  Defender Firewall with Advanced Security, under Inbound Rules, for a Block rule for `krebs.exe`, even if nothing ever asked
  you. Delete it and start `Let friends join.bat` again.
- Joining a server means trusting it with your PC, not only with your sign-in. The Screeps client runs code that a server
  sends it, which is how Krebs shows its lobbies at all, and a dishonest host could use that to run programs on your PC. Only
  join servers of people you trust. [docs/SECURITY.md](docs/SECURITY.md) says what you take on by hosting and by joining.
- Steam confirms every sign-in, so nobody can make one up or reuse an old one. The host of a server you join does see your
  sign-in, though, and a dishonest one could pass it on to another Krebs server. Nothing is encrypted, so only share your
  server with people you trust, and do not run a public one.
- Your key is in `data\steam-api-key.txt`, which Krebs keeps to your Windows account. Keep that file to yourself. While Steam
  cannot be reached, nobody can sign in.
- For each player, your PC asks Steam for their public profile picture.
- Every match runs on your PC, so more players means more work for it. A match can use 2 GB of memory plus 1 GB per seat. A bot
  that asks for more ends that match, which then says so, and your server carries on.

### When something goes wrong

Krebs says what went wrong in its window and waits for Enter. These are the messages you can get.

| Krebs says | What to do |
| --- | --- |
| `Krebs was started from inside the zip` | Right-click the zip, choose Extract All, and start `Krebs.exe` from the new folder. |
| `Krebs is already running on port 21025` | Look for its other window, or just connect. It may not let friends in: to do that, close it and start `Let friends join.bat`. |
| `Another Krebs is already using ...` | Look for its other window. To run a second one, copy the Krebs folder somewhere else and change the port number in its `krebs.json`. |
| `Port 21025 is already used by another program` | That may be another Screeps server. Close it and start Krebs again, or change the port number in `krebs.json`. |
| `Windows does not let Krebs use port 21025` | Something has reserved the port. Change the port number in `krebs.json`. |
| `Krebs could not open port 21025 on ...` | Check `host` and `port` under `http` in `krebs.json`, or delete that file and Krebs uses its normal settings. |
| `Krebs cannot write to ...` | Krebs keeps your world there, and Program Files or a read-only drive do not allow that. Move the folder to Documents or the Desktop. |
| `Krebs could not open its own folder` | Same: move the folder somewhere you own. |
| `Krebs could not read its settings` | `krebs.json` has a mistake. Fix it, or delete it, and Krebs uses its normal settings. |
| `Krebs could not save your world` | The disk may be full, or the `data` folder cannot be written to. Keep the window open and fix that: Krebs tries again by itself. If you close it first, you lose what happened since the last save. |
| `The saved world in ... could not be read` | Krebs did not change it. To start a new world, rename the `data` folder, for example to `data-old`, and start Krebs again. |
| `Check the mods list in krebs.json` | A mod named there is not built into this copy of Krebs. Put the `krebs.json` from the zip back, or delete it. |
| `Krebs hit an internal error and had to stop` | Your last automatic save is still there, so start Krebs again. |
| `The admin console is off` | Nothing to fix. Another program, often a second Krebs, holds port 21026. The game runs normally. |
| `Refused a sign-in: ...` | Someone could not get in, and the rest of the line says why. If everyone you invited got in, you can ignore it. |
| `Other PCs can reach this server` | Start `Let friends join.bat`, which asks for the key. Or set `host` under `http` in `krebs.json` back to `127.0.0.1`. |
| `host under cli in krebs.json can only be this PC` | The admin console has no password. Set `host` under `cli` in `krebs.json` to `127.0.0.1`, or take it out. |

`krebs.json` from the zip has the port in it, so there is a number to change:

```
{
  "http": { "port": 21030 },
  "mods": ["bare"]
}
```

One more that does not come from Krebs:

| You see | What to do |
| --- | --- |
| `An error occured while logging into your Steam account` | This comes from the Screeps client, and usually Steam has stopped answering. Restart Steam. If that does not help, the server may have refused you: ask the host to look in the Krebs window for `Refused a sign-in`. |

## Develop

You need Windows, [Rust](https://rustup.rs) and the
[C++ build tools](https://visualstudio.microsoft.com/visual-cpp-build-tools/). Rust's setup offers to install them.

```
git clone --recursive https://github.com/DefaultO/krebs.git
```

Krebs is on [GitHub](https://github.com/DefaultO/krebs) and on
[Gitea](https://gitea.defaul.to/DefaultO/krebs), and either one is enough: each has the code, the mods, the releases and the
V8 library, so you can clone, build and download with the other switched off. The files are the same on both, apart from
the commands in these READMEs, which name the site you read them on. The commit hashes differ, because each site's commits
carry that site's own address.

This gets the core and the mods that make Krebs what players see: lobbies, Battle Royale, the stripped client. Each mod is a
repository of its own, added as a git submodule. See [mods/README.md](mods/README.md), which also says how to add a mod.

Leave out `--recursive` for the plain core alone, a Screeps private server for the stock client. `mods/` then holds empty
folders, and `git submodule update --init` fetches the mods later.

The submodule URLs are relative, so a clone gets its mods from the site it came from, and a fork needs its own copies of the
mod repositories. Or point it at the originals, for each mod you want:

```
git config submodule.mods/bare.url https://github.com/DefaultO/krebs-bare.git
git submodule update --init mods/bare
```

`run.bat` builds the server if the code changed, makes a world on the first run and starts it. Extra arguments go to the
server. Page files in `mods/bare` reload on a reconnect.

With the mods here, a build lists them in `crates/modlist/Cargo.toml` and `Cargo.lock`, and neither list belongs in a commit.
So `run.bat` marks both files skip-worktree. If `git pull` then says your local changes to `Cargo.lock` would be overwritten
while `git status` shows nothing, undo that and pull. The next `run.bat` sets it again:

```
git update-index --no-skip-worktree Cargo.lock crates/modlist/Cargo.toml
git checkout -- Cargo.lock crates/modlist/Cargo.toml
git pull
```

```
tools\package.bat    the zip for players, in dist\, built in a clean copy, with everything in a Krebs folder
```

A release is built from a fresh clone at its tag, so that it holds what that commit says and nothing else.
`tools\package.bat` stops at anything a clone would not have: uncommitted changes, files `git status` has been told to skip
(which is what `run.bat` does to `Cargo.lock` and the mod list), a submodule that differs from its link, or a mod folder that
is not in the repository. Set `KREBS_ALLOW_DIRTY=1` for a trial build, which is not for publishing.

### The fast V8

Bots run on V8. Krebs uses its own build with pointer compression and profile-guided builtins, which makes ticks about 25%
faster. `run.bat` downloads that library once, as a prebuilt, and the matching bindings are in `vendor/v8/gen`. You do not
have to build it. `tools/v8-ptrcomp.ps1` is how it was made, which takes about 40 minutes. `tools/v8-download.bat` fetches it
from GitHub, or from Gitea if that fails, and a pinned sha256 has to match either way.

### Tests

```
cargo test
set RUSTY_V8_ARCHIVE=https://github.com/DefaultO/krebs/releases/download/v8-152.2.0-fast/rusty_v8.lib.gz
cargo test --workspace --release --features runtime/ptrcomp -j 4 --no-fail-fast
```

Those are for cmd: in PowerShell the middle line is `$env:RUSTY_V8_ARCHIVE="..."`. The address also works with
`gitea.defaul.to` in place of `github.com`. The first command runs the crates that need
no V8. The last runs everything, and has to be `--release`: debug V8 tests crash
in `Isolate::new`. `-j 4` is there because V8 crates take a lot of memory to link. A mod that is checked out is tested with
the rest.

`node tools/parity-report.js` regenerates `docs/parity-status.md`. The scripts in `tools/oracle` re-record the test fixtures
from the official server, which they find through `SCREEPS_ROOT`. Building and testing do not need them.

### The server

`krebs.exe` with no command does what a double-click does. These work as commands in cmd, from the folder that holds it, and
stay in that folder:

```
krebs new --world 2x2 --seed 1
krebs serve --mods bare
krebs import <db.json>
krebs export <db.json>
krebs user add <name> --password <password> --email <address>
krebs bench --bots 2 --world 2x2
krebs --help
```

`user add` needs a server running without the bare mod, which removes sign-up. Settings are in `krebs.json`, see
[krebs.example.json](krebs.example.json). `db.json` is an official Screeps database, so
worlds move to and from the official server. The admin console from the official server is on port 21026, on this PC only.
The mods `history` (room history for the stock client) and `devtools` (pause, step, speed) are built in.

The JavaScript that bots run on is largely the open source Screeps packages, kept where bots can see the difference, so that
bots written for the official server run unchanged. Everything else is written anew. Its own tests compare it against the
official code: [docs/parity-status.md](docs/parity-status.md) has the state per file, [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)
the design, and [docs/spec](docs/spec) the specs of the original server.

### The old name

Krebs used to be called chitin. Some internals and docs still say so. `krebs.exe` still reads a `chitin.json` if it finds no
`krebs.json`.

## Licence

Krebs is under the [MIT licence](LICENSE). Parts of it come from other projects and stay under their own licences, among them
the Screeps packages (ISC). See [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
