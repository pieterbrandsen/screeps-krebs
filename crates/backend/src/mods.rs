//! Everything a mod can register (ARCHITECTURE.md §10.1, FROZEN). Owner: WP21.
//! screeps-krebs adds `cli_js` and `cli_calls`: console helpers, as vanilla mods add to `config.cli`.

use crate::socket::server::ChannelFn;
use crate::Route;

pub struct Manifest {
    pub name: &'static str,
    pub version: &'static str,
    pub dependencies: &'static [&'static str],
}

/// A console native of a mod (`Registry::cli_calls`): the JSON args `call` was given, what it returns as JSON.
pub type CliCallFn = fn(args: &util::json::Json, ctx: &CliCallCtx) -> Result<util::json::Json, String>;

/// What a console native can reach: the last published world, the tick thread's command channel (a change runs there
/// before the next tick, `crate::call`) and the server's data directory.
pub struct CliCallCtx<'a> {
    pub published: &'a crate::Published,
    pub commands: &'a std::sync::mpsc::Sender<engine::world::Command>,
    pub data_dir: &'a std::path::Path,
}

pub trait Mod: Sync {
    fn manifest(&self) -> Manifest;
    fn register(&self, r: &mut Registry);
}

#[derive(Default)]
pub struct Registry {
    pub engine: engine::hooks::EngineHooks,
    /// HTTP routes.
    pub routes: Vec<Route>,
    /// `config.backend.<path> = value` (vanilla mods' way): `("welcomeText", …)`, `("customObjectTypes.myobject",
    /// …)`, `("renderer.metadata.myobject", …)`, `("socketUpdateThrottle", …)`; /api/version's `serverData` and the
    /// socket throttle read the result.
    pub server_data: Vec<(&'static str, util::json::Json)>,
    /// (channel prefix, handler): offered every `subscribe` whose channel starts with the prefix, then called for
    /// each subscriber after every tick (`ChannelFn`).
    pub socket_channels: Vec<(&'static str, ChannelFn)>,
    /// Game-slot JS (§10.2), one `mods/<mod>.js` module each, evaluated in every player VM after the game API; its
    /// exports may carry `customObjectPrototypes` and `customIntentTypes`.
    pub js_game: Vec<&'static str>,
    /// Console JS (screeps-krebs): each source a JS function `(sandbox, call) => { … }`, run in every console session
    /// (line and HTTP) once the vanilla sandbox is set up, the way vanilla mods add `config.cli` helpers (`utils.*`,
    /// `auth.*`). `call(name, args)` runs one of `cli_calls` and returns its result (it throws the error text).
    pub cli_js: Vec<&'static str>,
    /// Console natives (screeps-krebs) that `cli_js` reaches through `call`: what JS in the sandbox cannot do itself
    /// (OS randomness, state a mod keeps, files).
    pub cli_calls: Vec<(&'static str, CliCallFn)>,
    /// The server's data directory (`--data`), where a mod keeps its files.
    pub data_dir: std::path::PathBuf,
    /// The host's Steam Web API key. Only a server other people can reach has one (`steam_web`).
    pub steam_key: Option<String>,
}
