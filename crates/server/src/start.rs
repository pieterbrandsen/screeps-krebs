//! `krebs` with no command, which is what a double-click runs. It does in one go what `new` and `serve` do apart, for
//! someone who has never seen a console: it works in the exe's own folder whatever the current directory is, makes the
//! world on the first start, says what to do next, and answers every failure with a plain sentence and a wait for Enter,
//! because the window would otherwise close before anyone could read it.
//!
//! Mods come from `mods` in krebs.json, as for `serve`. Where nothing names any, a build that has `PLAYERS_MOD` uses it, as
//! a lost krebs.json must not turn the game into the plain Screeps client. A build without it stays the plain server.
//! `--friends` listens on all interfaces. The CLI has no authentication, so it stays on this PC, and match servers never
//! see the flag: `mods/bare` starts them on 127.0.0.1, with no CLI at all.
//!
//! One server has one data folder (`krebs.lock`), as a second would clear the first one's matches and save over its world.
//!
//! A server other people can reach needs a Steam Web API key, so that Steam confirms each sign-in (`backend::steam_web`). A
//! start with no command asks for one, and `serve` refuses to go on without. Alone on this PC nothing needs one.
//!
//! Not covered: a fatal V8 error or an out-of-memory abort end the process without unwinding, so nothing here runs.

use crate::config::{Args, Config};
use backend::steam_web::{self, Answer};
use std::fs::{File, TryLockError};
use std::io::{BufRead, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream, ToSocketAddrs, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

const ALL_INTERFACES: &str = "0.0.0.0";
/// The names for this PC alone. Anything else in `http.host` is a server others may reach.
const LOCAL_HOSTS: [&str; 3] = ["127.0.0.1", "localhost", "::1"];
const THIS_PC: &str = "127.0.0.1";
const PLAYERS_MOD: &str = "bare";
/// In the data folder. What holds it is the lock on it, which goes with the process however it ends.
const LOCK_FILE: &str = "krebs.lock";
/// With the port open to a network, the room one caller can take up. A browser opens a handful of connections, so this
/// still fits a full lobby many times over.
const FRIENDS_MAX_CONNECTIONS: usize = 256;
const FRIENDS_REQUEST_TIMEOUT_S: u64 = 30;

const CRASHED: &str = "Krebs hit an internal error and had to stop. The lines above say where.\n\
Your last automatic save is still there. Start Krebs again to carry on from it.";

/// Without it every ticket is the same account, which is fine alone on this PC and the host's own account for anyone else.
const ONE_ACCOUNT: &str = "Friends cannot join without the bare mod: everyone who connected would be the same player.\n\
                           Put \"bare\" in mods in krebs.json, or use the download, which has it built in.\n\
                           A plain server with passwords can open by setting steam under auth in krebs.json to off.";
const NO_KEY: &str = "No key, so nobody else can join. Krebs.exe on its own still works for playing on this PC.";
const IN_THE_ZIP: &str = "Krebs was started from inside the zip, so Windows gave it a throwaway folder and left the other files out.\n\
Close this, right-click the zip and choose Extract All. Then start Krebs.exe from the new folder.";
const OPEN_CONSOLE: &str = "host under cli in krebs.json can only be this PC, as the admin console has no password and anyone who reached it could change anything.\n\
Set it to 127.0.0.1, or take that line out, and start Krebs again.";

static ACTIVE: AtomicBool = AtomicBool::new(false);

/// Whether this process was started with no command.
pub fn active() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}

/// Says what went wrong and keeps the window open until it has been read.
pub fn give_up(msg: &str) -> ! {
    eprintln!("\n{msg}\n");
    println!("Press Enter to close this window.");
    // with no console to read from this returns at once, so a script or a test never hangs here
    let _ = std::io::stdin().read_line(&mut String::new());
    std::process::exit(1)
}

/// Runs `f`, and a panic comes back as the sentence to show. Panics that something else catches (a connection, a room)
/// never get here, and the server keeps running through those.
fn guarded(f: impl FnOnce()) -> Result<(), &'static str> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).map_err(|_| CRASHED)
}

pub fn run(args: &Args) {
    ACTIVE.store(true, Ordering::Relaxed);
    if let Err(msg) = guarded(|| start(args)) {
        give_up(msg);
    }
}

fn start(args: &Args) {
    let folder = go_to_exe_folder().unwrap_or_else(|e| give_up(&format!("Krebs could not open its own folder: {e}.\nMove the Krebs folder somewhere you own, such as Documents or the Desktop, and start it again.")));
    if unpacked_for_one_run(&folder) {
        give_up(IN_THE_ZIP);
    }
    let mut cfg = Config::load(args).unwrap_or_else(|e| give_up(&format!("Krebs could not read its settings: {e}.\nFix krebs.json, or delete it and Krebs uses its normal settings. Then start Krebs again.")));
    default_mods(&mut cfg, crate::mods::built_in(PLAYERS_MOD));
    if args.flag("friends").is_some() {
        open_to_friends(&mut cfg);
    }
    if let Some(problem) = cli_problem(&cfg).or_else(|| folder_problem(&cfg.data)) {
        give_up(&problem);
    }
    if needs_steam_key(&cfg) {
        make_sure_of_steam_key(&cfg.data);
    }
    // the ports come last: the key can take minutes, and nothing holds a port until the server does
    if let Some(problem) = steam_key(&cfg).err().or_else(|| port_problem(&cfg)) {
        give_up(&problem);
    }
    if !crate::spec_path(&cfg).exists() && !engine::persist::exists(&cfg.data) {
        println!("Creating your world. This only happens the first time.");
        crate::new_world(&cfg, false);
    }
    crate::serve(cfg, args);
}

fn go_to_exe_folder() -> std::io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let folder = exe.parent().unwrap_or(Path::new(".")).to_path_buf();
    std::env::set_current_dir(&folder)?;
    Ok(folder)
}

/// The folder Explorer (`Temp1_name.zip`), WinRAR (`Rar$EXa1.2`) or 7-Zip (`7zO12345`) unpacks one file into, for a
/// program started from inside an archive. It goes away, and the world would go with it. The zip has a top folder, so
/// this can be the folder above the exe's too.
fn unpacked_for_one_run(folder: &Path) -> bool {
    folder.ancestors().take(2).any(is_throwaway)
}

fn is_throwaway(folder: &Path) -> bool {
    let name = folder.file_name().map_or(String::new(), |n| n.to_string_lossy().to_ascii_lowercase());
    let explorer = name.strip_prefix("temp").and_then(|rest| rest.split_once('_')).is_some_and(|(n, rest)| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) && rest.ends_with(".zip"));
    let seven_zip = name.strip_prefix("7zo").is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_alphanumeric()));
    explorer || seven_zip || name.starts_with("rar$ex")
}

fn default_mods(cfg: &mut Config, players_mod_built_in: bool) {
    if !cfg.mods_named && players_mod_built_in {
        cfg.mods = vec![PLAYERS_MOD.to_string()];
    }
}

fn open_to_friends(cfg: &mut Config) {
    cfg.http_host = ALL_INTERFACES.into();
    cfg.cli_host = THIS_PC.into();
    cfg.max_connections = cfg.max_connections.min(FRIENDS_MAX_CONNECTIONS);
    cfg.request_timeout_s = cfg.request_timeout_s.min(FRIENDS_REQUEST_TIMEOUT_S);
}

/// Why the CLI cannot open where the config says, if so. It has no password, and only `--friends` ever moved it itself.
/// screeps-krebs: `cli.container` says a container's host keeps the console on its own loopback, so any host goes.
pub fn cli_problem(cfg: &Config) -> Option<String> {
    let this_pc = cfg.cli_host.parse::<IpAddr>().map_or(cfg.cli_host.eq_ignore_ascii_case("localhost"), |ip| ip.is_loopback());
    (!this_pc && !cfg.cli_container).then(|| OPEN_CONSOLE.to_string())
}

fn others_can_reach(cfg: &Config) -> bool {
    !LOCAL_HOSTS.contains(&cfg.http_host.as_str())
}

fn needs_steam_key(cfg: &Config) -> bool {
    others_can_reach(cfg) && cfg.mods.iter().any(|m| m == PLAYERS_MOD)
}

/// What `serve` gives the mods, or why it must not open. Alone on this PC there is no key, as no one else can use a ticket
/// and Steam being away should not keep the host out of their own game. The plain server may open to others only with its
/// ticket route off (`auth.steam`), and then it is on its own passwords.
pub fn steam_key(cfg: &Config) -> Result<Option<String>, String> {
    if others_can_reach(cfg) && !needs_steam_key(cfg) && cfg.steam.starts_with("single-user:") {
        return Err(ONE_ACCOUNT.to_string());
    }
    if !needs_steam_key(cfg) {
        return Ok(None);
    }
    let file = cfg.data.join(steam_web::KEY_FILE);
    steam_web::key_in(&cfg.data).map(Some).ok_or_else(|| {
        format!("Other PCs can reach this server, so it needs a Steam Web API key, for Steam to confirm who signs in.\nGet one at {} and put it in {}.", steam_web::KEY_PAGE, file.display())
    })
}

/// Holds the data folder for this process until it ends, or says who has it. With no folder there is no lock to take, and
/// the world's own check says what is missing. A file system with no locks is no reason to stop.
fn take_lock(data: &Path) -> Result<Option<File>, String> {
    let file = match File::options().create(true).truncate(false).write(true).open(data.join(LOCK_FILE)) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("Krebs could not use {}: {e}.", data.display())),
    };
    match file.try_lock() {
        Err(TryLockError::WouldBlock) => Err(in_use(data)),
        _ => Ok(Some(file)),
    }
}

/// For as long as this process runs.
pub fn lock_data(data: &Path) -> Result<(), String> {
    take_lock(data).map(std::mem::forget)
}

/// The one line `--password-stdin` waits for, which keeps a password off the command line and out of every file. A line
/// that never ended means whoever was sending it has gone.
pub fn password_from(input: &mut impl BufRead) -> Option<String> {
    let mut line = String::new();
    input.read_line(&mut line).ok()?;
    let password = line.strip_suffix('\n')?.trim_end_matches('\r');
    (!password.is_empty()).then(|| password.to_string())
}

/// `--announce-port`: on a line of its own, for whoever started this with port 0 and holds the other end of its stdout.
pub fn port_line(port: u16) -> String {
    format!("krebs-port {port}")
}

/// Tightens a key file this folder may hold from an older Krebs, or from someone's hands, without reading it: only a plain
/// file, as it is the file that is to be closed, and not whatever a link points at.
pub fn tighten_key(data: &Path) {
    let file = data.join(steam_web::KEY_FILE);
    if std::fs::symlink_metadata(&file).is_ok_and(|found| found.is_file()) {
        steam_web::make_private(&file);
    }
}

fn in_use(data: &Path) -> String {
    let place = std::path::absolute(data).unwrap_or_else(|_| data.to_path_buf());
    format!(
        "Another Krebs is already using {}, so this one has stopped before it could harm that one's matches.\n\
         Look for its other window. To run a second one, copy the Krebs folder somewhere else.",
        place.display()
    )
}

/// A saved key Steam still takes, or a new one from the person at the window.
fn make_sure_of_steam_key(data: &Path) {
    match steam_web::key_in(data).map(|key| steam_web::try_key(&key)) {
        Some(Answer::KeyRefused) => println!("\n  Steam no longer takes the key saved in {}.", data.join(steam_web::KEY_FILE).display()),
        Some(_) => return,
        None => {}
    }
    println!("{}", key_steps(data));
    // on a line of its own, as give_up reads stdin too and the lock has to be gone by then
    let key = read_key(&mut std::io::stdin().lock(), steam_web::try_key);
    let key = key.unwrap_or_else(|| give_up(NO_KEY));
    if let Err(e) = steam_web::save_key(data, &key) {
        give_up(&format!("Krebs could not save the key in {} ({e}).", data.display()));
    }
    // the key was on screen, and the next thing people do with this window is share what it says
    let _ = std::process::Command::new(steam_web::system_program("cmd")).args(["/c", "cls"]).status();
    println!("  Thanks, Krebs saved that key.\n");
}

fn key_steps(data: &Path) -> String {
    format!(
        "\n  To let other people join, Krebs needs a Steam Web API key, so that Steam can confirm who signs in.\n\
         \x20 It is free and you only do this once.\n\n\
         \x20   1. Open {} in your browser and sign in to Steam.\n\
         \x20   2. Enter any name as the Domain Name, like krebs.local, agree to the terms and press Register.\n\
         \x20   3. Copy the key, paste it here with a right-click, and press Enter.\n\n\
         \x20 Steam only gives keys to accounts that have spent at least 5 dollars on Steam.\n\
         \x20 Krebs keeps the key in {} on this PC. Keep that file to yourself.\n\
         \x20 To play alone, close this and start Krebs.exe instead. That needs no key.\n",
        steam_web::KEY_PAGE,
        data.join(steam_web::KEY_FILE).display()
    )
}

/// Lines from `input` until Steam takes one. None when the input ends, as a closed window or a script would.
fn read_key(input: &mut impl BufRead, try_key: impl Fn(&str) -> Answer) -> Option<String> {
    loop {
        print!("  Key: ");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        if input.read_line(&mut line).unwrap_or(0) == 0 {
            return None;
        }
        let key = line.trim();
        if !steam_web::is_key(key) {
            println!("  That is not a key. A key is 32 letters and digits, with nothing else around them. Try again.");
            continue;
        }
        match try_key(key) {
            Answer::KeyRefused => println!("  Steam did not take that key. Copy it again from the page and paste it here."),
            Answer::NoAnswer => println!("  Steam did not answer. Check this PC's internet, then paste the key again."),
            Answer::Good(_) | Answer::Bad => return Some(key.to_string()),
        }
    }
}

/// The world lives in `data`, so a folder that cannot be written to has to stop the start here, not at the first save.
fn folder_problem(data: &Path) -> Option<String> {
    let probe = data.join(format!("krebs-write-test-{}.tmp", std::process::id()));
    let result = std::fs::create_dir_all(data).and_then(|()| std::fs::write(&probe, b""));
    let _ = std::fs::remove_file(&probe);
    let e = result.err()?;
    let place = std::path::absolute(data).unwrap_or_else(|_| data.to_path_buf());
    Some(format!(
        "Krebs cannot write to {} ({e}).\nIt keeps your world there, and folders like Program Files or a read-only drive do not allow that.\n\
         Move the Krebs folder somewhere you own, such as Documents or the Desktop, and start it again.",
        place.display()
    ))
}

#[derive(Debug, PartialEq)]
enum Holder {
    Krebs,
    Other,
}

/// What answers on this PC at `port`, if anything. Binding alone would not tell: on Windows a listener on 127.0.0.1 and
/// one on all interfaces can share a port, and only one of them would then get the connections.
fn holder(port: u16) -> Option<Holder> {
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let mut socket = TcpStream::connect_timeout(&address, Duration::from_millis(500)).ok()?;
    let _ = socket.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = socket.set_write_timeout(Some(Duration::from_secs(2)));
    let _ = socket.write_all(b"GET /api/version HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
    let mut head = Vec::new();
    let _ = socket.take(4096).read_to_end(&mut head);
    let head = String::from_utf8_lossy(&head).to_ascii_lowercase();
    Some(if head.contains(&format!("\r\n{}:", backend::server::KREBS_HEADER.to_ascii_lowercase())) { Holder::Krebs } else { Holder::Other })
}

/// `friends`: this start was to let other PCs in, which the copy already running may not do.
fn taken(holder: &Holder, port: u16, friends: bool) -> String {
    match holder {
        Holder::Krebs if friends => format!(
            "Krebs is already running on port {port}, and that copy may not let friends in.\n\
             Close the other Krebs window first, then start this again."
        ),
        Holder::Krebs => format!(
            "Krebs is already running on port {port}, so this one has nothing to do.\n\
             Look for its other window, or just open Screeps and connect. To run a second one, copy the Krebs folder somewhere else and change the port number in its krebs.json."
        ),
        Holder::Other => format!(
            "Port {port} is already used by another program, so Krebs cannot start. It may be another Screeps server.\n\
             Close that program and start Krebs again, or change the port number in krebs.json. README.txt shows how."
        ),
    }
}

fn port_problem(cfg: &Config) -> Option<String> {
    let (host, port) = (cfg.http_host.as_str(), cfg.http_port);
    let friends = host == ALL_INTERFACES;
    if let Some(found) = holder(port) {
        return Some(taken(&found, port, friends));
    }
    let e = TcpListener::bind((host, port)).err()?;
    Some(match e.kind() {
        std::io::ErrorKind::AddrInUse => taken(&Holder::Other, port, friends),
        std::io::ErrorKind::PermissionDenied => format!("Windows does not let Krebs use port {port}: something has reserved it.\nChange the port number in krebs.json and start Krebs again. README.txt shows how."),
        _ => format!("Krebs could not open port {port} on {host}: {e}.\nCheck host and port under http in krebs.json, or delete that file and Krebs uses its normal settings."),
    })
}

/// For a world that is there but cannot be used. Nothing was changed, and that should be said.
pub fn world_problem(data: &Path, detail: &str) -> String {
    let place = std::path::absolute(data).unwrap_or_else(|_| data.to_path_buf());
    format!(
        "The saved world in {} could not be read: {detail}\nKrebs did not change it. To start a new world, rename that folder, for example to data-old, and start Krebs again.",
        place.display()
    )
}

pub fn mods_problem(detail: &str) -> String {
    format!("{detail}.\nCheck the mods list in krebs.json. A mod has to be built into this copy of Krebs to be used.")
}

fn save_failed(detail: &str, every_s: u64) -> String {
    format!(
        "\n  Krebs could not save your world: {detail}.\n\
         \x20 If you close this window now, you lose everything since the last save.\n\
         \x20 Free some disk space, and check that the data folder beside Krebs can be written to.\n\
         \x20 Krebs tries again every {every_s} seconds and says so here when it works.\n"
    )
}

const SAVING_AGAIN: &str = "\n  Krebs can save your world again.\n";

/// What the window says after each save: the same words every time one fails, and one line when they work again.
pub fn save_report(every_s: u64) -> impl FnMut(Result<(), &str>) + Send + 'static {
    let mut failing = false;
    // not the print macros, which panic with nobody at the other end, and this thread is what keeps saving
    move |saved| match saved {
        Err(detail) => {
            failing = true;
            let _ = writeln!(std::io::stderr(), "{}", save_failed(detail, every_s));
        }
        Ok(()) if failing => {
            failing = false;
            let _ = writeln!(std::io::stdout(), "{SAVING_AGAIN}");
        }
        Ok(()) => {}
    }
}

/// For the save at the very end, when the window may already be closing.
pub fn last_save_failed(data: &Path, detail: &str) -> String {
    let place = std::path::absolute(data).unwrap_or_else(|_| data.to_path_buf());
    format!(
        "Krebs could not save your world when it closed: {detail}.\nEverything since the last good save is lost. That save is still in {}.\n\
         Free some disk space, check that you can write to that folder, and start Krebs again.",
        place.display()
    )
}

/// A closing console takes its window down whatever this process does, so saying this where it stays read needs a new
/// window: this same program, shown `msg` by the word `TELL`. Without one it goes to this window.
pub fn tell_in_a_new_window(msg: &str) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_CONSOLE: u32 = 0x10;
        let opened = std::env::current_exe().and_then(|exe| std::process::Command::new(exe).args([crate::TELL, msg]).creation_flags(CREATE_NEW_CONSOLE).spawn());
        if opened.is_ok() {
            return;
        }
    }
    give_up(msg);
}

/// This PC's addresses on its networks, most likely first. No packet leaves: connecting a UDP socket only picks the
/// interface a packet would use. The computer's own name then gives the others.
fn lan_addresses() -> Vec<Ipv4Addr> {
    let mut found = Vec::new();
    if let Ok(socket) = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)) {
        if let (Ok(()), Ok(SocketAddr::V4(me))) = (socket.connect(("10.255.255.255", 1)), socket.local_addr()) {
            found.push(*me.ip());
        }
    }
    let name = std::env::var("COMPUTERNAME").or_else(|_| std::env::var("HOSTNAME")).unwrap_or_default();
    for address in (name.as_str(), 0).to_socket_addrs().into_iter().flatten() {
        if let SocketAddr::V4(me) = address {
            if !found.contains(me.ip()) {
                found.push(*me.ip());
            }
        }
    }
    found.retain(|ip| !ip.is_loopback() && !ip.is_link_local() && !ip.is_unspecified());
    found
}

fn play_here(host: &str, port: u16) -> String {
    format!(
        "  To play on this PC:\n\
         \x20   1. Open Screeps in Steam.\n\
         \x20   2. On the server list, switch from Community servers to Private server.\n\
         \x20   3. Enter Host {host} and Port {port}, then press Connect.\n"
    )
}

/// `lan`: this PC's addresses, shown only when people can reach them.
fn welcome_text(host: &str, port: u16, friends: bool, lan: &[Ipv4Addr]) -> String {
    let mut text = String::from("\n  Krebs is running.\n\n");
    text += &play_here(host, port);
    if friends {
        text += "\n  To let other people join, give them the same steps with one of these as the Host:\n";
        if lan.is_empty() {
            text += "    (Krebs could not find this PC's address. Run ipconfig and use the IPv4 Address.)\n";
        }
        for ip in lan {
            text += &format!("    {ip}\n");
        }
        text += &format!(
            "  Those work for people on your own network. Try the first one first.\n\n\
             \x20 Windows Firewall may ask about Krebs. Allow it, or nobody else can connect. If friends cannot connect, README.txt says what to check.\n\
             \x20 Friends over the internet also need port {port} forwarded to this PC on your router, and then use your public address.\n\n\
             \x20 Steam confirms everyone who signs in. Nothing is encrypted, so only share this with people you trust.\n"
        );
    }
    text += "\n  Keep this window open while you play. Close it to stop. Your world is saved, and any match that is running ends.\n";
    text
}

pub fn welcome(cfg: &Config) -> String {
    let friends = needs_steam_key(cfg);
    // one address named in krebs.json is then the only one that answers, for this PC too
    let named = cfg.http_host.parse::<Ipv4Addr>().ok().filter(|ip| friends && !ip.is_unspecified());
    let lan = match named {
        Some(ip) => vec![ip],
        None if friends => lan_addresses(),
        None => Vec::new(),
    };
    welcome_text(&named.map_or(THIS_PC.to_string(), |ip| ip.to_string()), cfg.http_port, friends, &lan)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        Config::load(&Args::parse(&["serve".to_string()])).unwrap()
    }

    fn config_from(name: &str, text: &str, flags: &[&str]) -> Config {
        let dir = std::env::temp_dir().join(format!("krebs-start-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("krebs.json");
        std::fs::write(&file, text).unwrap();
        let argv: Vec<String> = ["serve", "--config", file.to_str().unwrap()].iter().chain(flags).map(|s| s.to_string()).collect();
        let cfg = Config::load(&Args::parse(&argv)).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        cfg
    }

    #[test]
    fn with_no_mods_named_the_players_mod_is_used_where_it_is_built_in() {
        let mut lost = config();
        default_mods(&mut lost, true);
        assert_eq!(lost.mods, ["bare"], "a lost krebs.json keeps the game");
        let mut plain = config();
        default_mods(&mut plain, false);
        assert!(plain.mods.is_empty(), "the core without mods stays the plain server");
        let mut port_only = config_from("port", r#"{"http":{"port":21555}}"#, &[]);
        default_mods(&mut port_only, true);
        assert_eq!(port_only.mods, ["bare"], "a file that only sets the port is not a choice about mods");
        let mut chosen = config_from("empty", r#"{"mods":[]}"#, &[]);
        default_mods(&mut chosen, true);
        assert!(chosen.mods.is_empty(), "a list in the file is a choice");
        let mut flag = config_from("flag", "{}", &["--mods", "history"]);
        default_mods(&mut flag, true);
        assert_eq!(flag.mods, ["history"]);
    }

    #[test]
    fn a_throwaway_folder_is_told_from_a_real_one() {
        for folder in ["C:/Users/Ann/AppData/Local/Temp/Temp1_Krebs-v0.1.0-windows-x64.zip", "C:/Temp/Temp12_krebs.ZIP", "C:/Temp/7zO8A2C1F31", "C:/Temp/Rar$EXa1234.5678"] {
            assert!(unpacked_for_one_run(Path::new(folder)), "{folder}");
        }
        for folder in ["C:/Users/Ann/Documents/Krebs-v0.1.0-windows-x64", "C:/Temp/krebs", "C:/Temp/Temp_x.zip", "C:/Temp/Temp1_notes", "C:/Temp/7zip-stuff", "C:/"] {
            assert!(!unpacked_for_one_run(Path::new(folder)), "{folder}");
        }
        for folder in ["C:/Users/Ann/AppData/Local/Temp/Temp1_Krebs-v0.3.0-windows-x64.zip/Krebs", "C:/Temp/Rar$EXa1234.5678/Krebs"] {
            assert!(unpacked_for_one_run(Path::new(folder)), "the zip's top folder comes along: {folder}");
        }
        assert!(!unpacked_for_one_run(Path::new("C:/Users/Ann/Downloads/Krebs-v0.3.0-windows-x64/Krebs")), "Extract All makes this");
    }

    #[test]
    fn a_data_folder_has_one_server() {
        let dir = std::env::temp_dir().join(format!("krebs-start-lock-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let held = take_lock(&dir).unwrap();
        assert!(held.is_some());
        let said = take_lock(&dir).unwrap_err();
        assert!(said.contains("Another Krebs is already using") && said.contains("copy the Krebs folder"), "{said}");
        drop(held);
        assert!(take_lock(&dir).unwrap().is_some(), "gone with what held it, and a lock file left behind is nothing");
        assert!(take_lock(&dir.join("none")).unwrap().is_none(), "no folder: the world's own check comes next");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_password_is_one_whole_line() {
        let read = |text: &str| password_from(&mut text.as_bytes());
        assert_eq!(read("s3cret\n").as_deref(), Some("s3cret"));
        assert_eq!(read("s3cret\r\nmore\n").as_deref(), Some("s3cret"), "only the one line");
        assert_eq!(read("s3cret"), None, "cut off, so whoever sent it has gone");
        assert_eq!(read(""), None);
        assert_eq!(read("\n"), None);
    }

    #[test]
    fn a_key_file_is_tightened_whenever_it_is_there_and_never_read() {
        let dir = std::env::temp_dir().join(format!("krebs-start-tighten-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        tighten_key(&dir);
        assert!(!dir.join(steam_web::KEY_FILE).exists(), "nothing there, nothing made");
        // a folder in its place: this would fail on reading it, and icacls would have nothing to refuse
        std::fs::create_dir(dir.join(steam_web::KEY_FILE)).unwrap();
        tighten_key(&dir);
        assert!(dir.join(steam_web::KEY_FILE).is_dir());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_console_stays_on_this_pc() {
        let mut cfg = config();
        for host in ["127.0.0.1", "127.0.0.2", "localhost", "LOCALHOST", "::1"] {
            cfg.cli_host = host.to_string();
            assert_eq!(cli_problem(&cfg), None, "{host}");
        }
        for host in ["0.0.0.0", "::", "192.168.1.20", "203.0.113.7", "example.com", ""] {
            cfg.cli_host = host.to_string();
            assert_eq!(cli_problem(&cfg), Some(OPEN_CONSOLE.to_string()), "{host}");
        }
        open_to_friends(&mut cfg);
        assert_eq!(cli_problem(&cfg), None, "--friends puts it back itself");
        let mut boxed = config();
        boxed.cli_host = "0.0.0.0".into();
        boxed.cli_container = true;
        assert_eq!(cli_problem(&boxed), None, "cli.container: the host publishes it on its loopback");
    }

    #[test]
    fn a_save_that_fails_says_so_in_plain_words() {
        let said = save_failed("Access is denied. (os error 5)", 60);
        for words in ["could not save your world", "Access is denied", "lose everything since the last save", "disk space", "every 60 seconds"] {
            assert!(said.contains(words), "{words}: {said}");
        }
        let closing = last_save_failed(Path::new("data"), "x");
        for words in ["could not save your world when it closed", "last good save is lost", "start Krebs again"] {
            assert!(closing.contains(words), "{words}: {closing}");
        }
    }

    /// A one-shot server that answers with `head`.
    fn answer(head: &'static str) -> u16 {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut request = [0u8; 256];
            let _ = s.read(&mut request);
            let _ = s.write_all(head.as_bytes());
        });
        port
    }

    #[test]
    fn a_free_port_has_no_holder() {
        let port = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap().local_addr().unwrap().port();
        assert_eq!(holder(port), None);
    }

    #[test]
    fn krebs_is_told_from_another_program() {
        assert_eq!(holder(answer("HTTP/1.1 200 OK\r\nX-Krebs: 0.1.0\r\nContent-Length: 0\r\n\r\n")), Some(Holder::Krebs));
        assert_eq!(holder(answer("HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")), Some(Holder::Other));
        assert_eq!(holder(answer("not http at all")), Some(Holder::Other));
    }

    #[test]
    fn a_taken_port_is_explained() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let mut cfg = config();
        cfg.http_port = listener.local_addr().unwrap().port();
        let said = port_problem(&cfg).unwrap();
        assert!(said.contains(&format!("Port {} is already used by another program", cfg.http_port)), "{said}");
        assert!(taken(&Holder::Krebs, 21025, false).contains("already running"));
    }

    #[test]
    fn a_running_copy_is_not_called_enough_for_friends() {
        let alone = taken(&Holder::Krebs, 21025, false);
        assert!(alone.contains("just open Screeps and connect"), "{alone}");
        let friends = taken(&Holder::Krebs, 21025, true);
        assert!(friends.contains("Close the other Krebs window first") && !friends.contains("just open"), "{friends}");
        assert_eq!(taken(&Holder::Other, 21025, true), taken(&Holder::Other, 21025, false));
    }

    #[test]
    fn a_folder_that_cannot_be_written_is_explained() {
        let dir = std::env::temp_dir().join(format!("krebs-start-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(folder_problem(&dir.join("data")), None, "a new folder is made");
        assert!(dir.join("data").is_dir());
        assert_eq!(std::fs::read_dir(dir.join("data")).unwrap().count(), 0, "the test file is gone again");
        // a file where the folder should be fails the same way on every system
        std::fs::write(dir.join("taken"), "").unwrap();
        let said = folder_problem(&dir.join("taken")).unwrap();
        assert!(said.contains("Krebs cannot write to") && said.contains("Documents"), "{said}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_panic_becomes_a_sentence() {
        assert_eq!(guarded(|| {}), Ok(()));
        assert_eq!(guarded(|| panic!("this panic is part of the test")), Err(CRASHED));
    }

    #[test]
    fn friends_open_the_game_port_and_nothing_else() {
        let mut cfg = config();
        assert_eq!((cfg.http_host.as_str(), cfg.cli_host.as_str()), (THIS_PC, THIS_PC), "off by default");
        cfg.cli_host = ALL_INTERFACES.into();
        open_to_friends(&mut cfg);
        assert_eq!((cfg.http_host.as_str(), cfg.cli_host.as_str()), (ALL_INTERFACES, THIS_PC));
        assert_eq!((cfg.max_connections, cfg.request_timeout_s), (FRIENDS_MAX_CONNECTIONS, FRIENDS_REQUEST_TIMEOUT_S), "the limits tighten with the port open");
        cfg.max_connections = 8;
        open_to_friends(&mut cfg);
        assert_eq!(cfg.max_connections, 8, "and never loosen what the file set");
    }

    #[test]
    fn only_a_server_others_can_reach_needs_a_steam_key() {
        let mut cfg = config();
        cfg.mods = vec![PLAYERS_MOD.to_string()];
        assert!(!needs_steam_key(&cfg), "alone on this PC");
        assert_eq!(steam_key(&cfg), Ok(None));
        for host in LOCAL_HOSTS {
            cfg.http_host = host.to_string();
            assert!(!needs_steam_key(&cfg), "{host}");
        }
        open_to_friends(&mut cfg);
        assert!(needs_steam_key(&cfg));
        cfg.http_host = "192.168.1.20".to_string();
        assert!(needs_steam_key(&cfg), "http.host in krebs.json counts too");
        cfg.mods.clear();
        assert!(!needs_steam_key(&cfg), "the plain server has no Steam sign-in of its own");
    }

    #[test]
    fn the_plain_server_does_not_open_its_one_account_to_others() {
        let mut cfg = config();
        cfg.mods.clear();
        assert_eq!(steam_key(&cfg), Ok(None), "alone on this PC that account is fine");
        cfg.http_host = ALL_INTERFACES.to_string();
        assert_eq!(steam_key(&cfg), Err(ONE_ACCOUNT.to_string()));
        cfg.http_host = "192.168.1.20".to_string();
        assert_eq!(steam_key(&cfg), Err(ONE_ACCOUNT.to_string()), "whatever the address");
        cfg.steam = "off".to_string();
        assert_eq!(steam_key(&cfg), Ok(None), "with the ticket route off there is no such account");
    }

    #[test]
    fn a_server_others_can_reach_will_not_serve_without_a_key() {
        let dir = std::env::temp_dir().join(format!("krebs-start-key-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut cfg = config();
        cfg.mods = vec![PLAYERS_MOD.to_string()];
        cfg.data = dir.clone();
        open_to_friends(&mut cfg);
        let said = steam_key(&cfg).unwrap_err();
        for words in ["Steam Web API key", steam_web::KEY_PAGE, steam_web::KEY_FILE] {
            assert!(said.contains(words), "{words}: {said}");
        }
        std::fs::write(dir.join(steam_web::KEY_FILE), "0123456789ABCDEF0123456789ABCDEF").unwrap();
        assert_eq!(steam_key(&cfg), Ok(Some("0123456789ABCDEF0123456789ABCDEF".to_string())));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_key_is_asked_for_until_steam_takes_one() {
        const GOOD: &str = "0123456789ABCDEF0123456789ABCDEF";
        const WRONG: &str = "FEDCBA9876543210FEDCBA9876543210";
        let steam = |key: &str| if key == GOOD { Answer::Bad } else { Answer::KeyRefused };
        let mut input = format!("\nnot a key\n{WRONG}\n  {GOOD}\r\nnever read\n");
        assert_eq!(read_key(&mut input.as_bytes(), steam).as_deref(), Some(GOOD));
        input = "not a key\n".to_string();
        assert_eq!(read_key(&mut input.as_bytes(), steam), None, "the window was closed");
        assert_eq!(read_key(&mut "".as_bytes(), steam), None);
        let away = std::cell::Cell::new(true);
        let comes_back = |_: &str| if away.replace(false) { Answer::NoAnswer } else { Answer::Bad };
        input = format!("{GOOD}\n{GOOD}\n");
        assert_eq!(read_key(&mut input.as_bytes(), comes_back).as_deref(), Some(GOOD), "Steam away for a moment");
    }

    #[test]
    fn one_address_in_krebs_json_is_the_one_the_welcome_gives() {
        let mut cfg = config();
        cfg.mods = vec![PLAYERS_MOD.to_string()];
        cfg.http_host = "192.168.1.20".to_string();
        let said = welcome(&cfg);
        assert!(said.contains("Host 192.168.1.20 and Port") && !said.contains(THIS_PC), "{said}");
        cfg.http_host = THIS_PC.to_string();
        assert!(welcome(&cfg).contains("Host 127.0.0.1 and Port"));
    }

    #[test]
    fn the_welcome_says_what_to_do_next() {
        let alone = welcome_text(THIS_PC, 21025, false, &[]);
        for words in ["Open Screeps in Steam", "Private server", "Host 127.0.0.1 and Port 21025", "Keep this window open", "Close it to stop", "any match that is running ends"] {
            assert!(alone.contains(words), "{words}: {alone}");
        }
        assert!(!alone.contains("Firewall") && !alone.contains("trust"), "{alone}");
        let friends = welcome_text(THIS_PC, 21500, true, &[Ipv4Addr::new(192, 168, 1, 20)]);
        for words in ["192.168.1.20", "Windows Firewall", "README.txt says what to check", "port 21500 forwarded", "Steam confirms everyone", "people you trust"] {
            assert!(friends.contains(words), "{words}: {friends}");
        }
        assert!(welcome_text(THIS_PC, 21500, true, &[]).contains("ipconfig"));
    }

    #[test]
    fn the_words_stay_plain() {
        let here = std::env::current_dir().unwrap().display().to_string();
        for text in [welcome_text(THIS_PC, 21025, true, &[]), key_steps(Path::new("d")), NO_KEY.to_string(), CRASHED.to_string(), IN_THE_ZIP.to_string(), SAVING_AGAIN.to_string(), OPEN_CONSOLE.to_string(), in_use(Path::new("d")), taken(&Holder::Krebs, 1, false), taken(&Holder::Krebs, 1, true), taken(&Holder::Other, 1, false), world_problem(Path::new("d"), "x"), mods_problem("x"), save_failed("x", 60), last_save_failed(Path::new("d"), "x")] {
            // The folder in a message is the user's own, in whatever language it is.
            let text = text.replace(&here, "");
            assert!(text.is_ascii(), "a console may not show it: {text}");
        }
    }

    #[test]
    fn lan_addresses_are_never_this_pc_alone() {
        assert!(lan_addresses().iter().all(|ip| !ip.is_loopback()));
    }
}
