//! Games and apps that no Start menu shortcut names (item 37): the libraries of Steam
//! (`libraryfolders.vdf`, `appmanifest_*.acf`), the Epic launcher's manifests, the install folders
//! Battle.net keeps in its `product.db` (Overwatch, Diablo), and the usual game folders. Each game is
//! its main exe, the one a paste rule matches. Files only: no registry and no launcher is started, so
//! a launcher that is not installed finds nothing and costs nothing. Blocking: run it on a worker.

use std::fs;
use std::path::{Component, Path, PathBuf};

/// Where an app was found, for the scanner's source filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Source {
    StartMenu,
    Store,
    Installed,
    Steam,
    BattleNet,
    Epic,
    Gog,
    GameFolder,
}

impl Source {
    pub const ALL: [Source; 8] = [
        Source::StartMenu,
        Source::Store,
        Source::Installed,
        Source::Steam,
        Source::BattleNet,
        Source::Epic,
        Source::Gog,
        Source::GameFolder,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Source::StartMenu => "Start menu",
            Source::Store => "Microsoft Store",
            Source::Installed => "Installed",
            Source::Steam => "Steam",
            Source::BattleNet => "Battle.net",
            Source::Epic => "Epic Games",
            Source::Gog => "GOG",
            Source::GameFolder => "Games folder",
        }
    }

    /// Whether what comes from here is a game (the scanner's Games filter).
    pub fn is_game(self) -> bool {
        matches!(
            self,
            Source::Steam | Source::BattleNet | Source::Epic | Source::Gog | Source::GameFolder
        )
    }
}

/// One game found: its main exe, its name, how much room it takes when the launcher says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Game {
    pub name: String,
    pub exe: PathBuf,
    pub source: Source,
    pub size: Option<u64>,
}

/// Where to look; each is `None` or empty when the machine has none of it.
#[derive(Debug, Clone, Default)]
pub struct Places {
    /// Steam's install folders (the one holding `steamapps`).
    pub steam: Vec<PathBuf>,
    /// `%ProgramData%`: the Epic manifests and Battle.net's `product.db` live below it.
    pub program_data: Option<PathBuf>,
    /// Folders whose subfolders are one game each.
    pub game_roots: Vec<PathBuf>,
}

/// Entries read below one game's folder when its exe is looked for, and when its size is added up.
const MAX_EXE_ENTRIES: usize = 2000;
const MAX_SIZE_ENTRIES: usize = 20_000;
/// A UE game keeps its real exe at `Game\Binaries\Win64\`, three folders down.
const EXE_DEPTH: usize = 3;
/// Files below this are not read whole: a manifest is a few KB, `product.db` some tens of KB.
const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;
/// Exes that are never the game: uninstallers, crash reporters, redistributables, anti-cheat and
/// web helpers.
const NOT_THE_GAME: [&str; 15] = [
    "unins",
    "crash",
    "redist",
    "setup",
    "install",
    "dxwebsetup",
    "vcredist",
    "dotnet",
    "helper",
    "easyanticheat",
    "battleye",
    "beservice",
    "report",
    "updater",
    "overlay",
];

/// Every game the launchers and folders in `places` list. `local` says whether a path is on this
/// computer's own disk: a path that is not is never opened (a manifest can name `\\host\share`).
pub fn find(places: &Places, local: &mut dyn FnMut(&Path) -> bool) -> Vec<Game> {
    let mut games = Vec::new();
    for steam in &places.steam {
        games.extend(steam_games(steam, local));
    }
    if let Some(data) = &places.program_data {
        games.extend(epic_games(data, local));
        games.extend(battle_net_games(data, local));
    }
    for root in &places.game_roots {
        games.extend(folder_games(root, local));
    }
    games
}

fn steam_games(steam: &Path, local: &mut dyn FnMut(&Path) -> bool) -> Vec<Game> {
    let mut libraries = vec![steam.to_path_buf()];
    if let Some(text) = read_text(&steam.join("steamapps").join("libraryfolders.vdf")) {
        libraries.extend(steam_libraries(&text));
    }
    let mut seen = Vec::new();
    let mut out = Vec::new();
    for lib in libraries {
        if seen.contains(&lib) || !local(&lib) {
            continue;
        }
        let apps = lib.join("steamapps");
        seen.push(lib);
        let Ok(entries) = fs::read_dir(&apps) else {
            continue;
        };
        let mut manifests: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                    n.starts_with("appmanifest_")
                        && Path::new(n)
                            .extension()
                            .is_some_and(|x| x.eq_ignore_ascii_case("acf"))
                })
            })
            .collect();
        manifests.sort();
        for manifest in manifests {
            let Some(app) = read_text(&manifest).and_then(|t| steam_manifest(&t)) else {
                continue;
            };
            let dir = apps.join("common").join(&app.installdir);
            if let Some(exe) = main_exe(&dir) {
                out.push(Game {
                    name: app.name,
                    exe,
                    source: Source::Steam,
                    size: app.size,
                });
            }
        }
    }
    out
}

/// The library folders `libraryfolders.vdf` lists, as `"path"` values.
pub fn steam_libraries(vdf: &str) -> Vec<PathBuf> {
    let tokens = vdf_tokens(vdf);
    tokens
        .windows(2)
        .filter(|w| w[0].eq_ignore_ascii_case("path") && !w[1].is_empty())
        .map(|w| PathBuf::from(&w[1]))
        .collect()
}

/// What an `appmanifest_*.acf` says of a game.
#[derive(Debug, PartialEq, Eq)]
pub struct SteamApp {
    pub name: String,
    pub installdir: String,
    pub size: Option<u64>,
}

/// `None` for a manifest with no name or install folder, and for Steam's own redistributables.
pub fn steam_manifest(acf: &str) -> Option<SteamApp> {
    let tokens = vdf_tokens(acf);
    let value = |key: &str| {
        tokens
            .windows(2)
            .find(|w| w[0].eq_ignore_ascii_case(key))
            .map(|w| w[1].clone())
    };
    let name = value("name").filter(|n| !n.is_empty())?;
    let installdir = value("installdir").filter(|d| !d.is_empty())?;
    // One plain folder name: no drive ("C:"), no `.`/`..`, no separators.
    let mut parts = Path::new(&installdir).components();
    let plain = matches!(parts.next(), Some(Component::Normal(_))) && parts.next().is_none();
    // Win32 strips trailing dots and spaces, so "..." or " " would open `common` itself.
    let plain = plain && !installdir.trim_end_matches(['.', ' ']).is_empty();
    if name.contains("Redistributable") || !plain {
        return None;
    }
    let size = value("SizeOnDisk").and_then(|s| s.parse().ok());
    Some(SteamApp {
        name,
        installdir,
        size,
    })
}

/// The quoted strings of a Valve text file, `\\` and `\"` undone; braces and layout are dropped, a
/// key is followed by its value.
fn vdf_tokens(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '"' {
            continue;
        }
        let mut token = String::new();
        while let Some(c) = chars.next() {
            match c {
                '"' => break,
                '\\' => match chars.next() {
                    Some('n') => token.push('\n'),
                    Some(other) => token.push(other),
                    None => break,
                },
                other => token.push(other),
            }
        }
        out.push(token);
    }
    out
}

fn epic_games(data: &Path, local: &mut dyn FnMut(&Path) -> bool) -> Vec<Game> {
    let dir = data
        .join("Epic")
        .join("EpicGamesLauncher")
        .join("Data")
        .join("Manifests");
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut items: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .is_some_and(|x| x.eq_ignore_ascii_case("item"))
        })
        .collect();
    items.sort();
    let mut out = Vec::new();
    for item in items {
        let Some(game) = read_text(&item).and_then(|t| epic_item(&t)) else {
            continue;
        };
        // DLC and tools name no exe: no tile. A folder with too few parts (`C:\`) is no game's.
        if game.launch.is_empty() || !own_folder(&game.dir) || !local(&game.dir) {
            continue;
        }
        // The biggest exe is the game's process (Fortnite's manifest names its bootstrapper); the
        // manifest's exe is the fallback, and only when it stays inside the folder on this disk.
        let named = Path::new(&game.launch.replace('/', "\\"))
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
            .then(|| game.dir.join(game.launch.replace('/', "\\")))
            .filter(|p| p.starts_with(&game.dir) && local(p) && p.is_file());
        if let Some(exe) = main_exe(&game.dir).or(named) {
            out.push(Game {
                name: game.name,
                exe,
                source: Source::Epic,
                size: game.size,
            });
        }
    }
    out
}

/// What an Epic `.item` manifest says of a game.
#[derive(Debug, PartialEq, Eq)]
pub struct EpicItem {
    pub name: String,
    pub dir: PathBuf,
    /// The exe, relative to `dir`; empty for DLC and tools.
    pub launch: String,
    pub size: Option<u64>,
}

/// `None` for a manifest with no name or install folder.
pub fn epic_item(json: &str) -> Option<EpicItem> {
    let name = json_string(json, "DisplayName").filter(|n| !n.is_empty())?;
    let dir = json_string(json, "InstallLocation").filter(|d| !d.is_empty())?;
    Some(EpicItem {
        name,
        dir: PathBuf::from(dir),
        launch: json_string(json, "LaunchExecutable").unwrap_or_default(),
        size: json_number(json, "InstallSize"),
    })
}

/// The string value of the first `"key"` in `json`, escapes undone. Epic's manifests are flat
/// enough that no parser is needed; `\uXXXX` comes out as it is.
// shortcut: a key repeated in a nested object is read from its first place, enough for the keys
// above; use a JSON parser if one more is ever needed.
fn json_string(json: &str, key: &str) -> Option<String> {
    let at = json.find(&format!("\"{key}\""))? + key.len() + 2;
    let rest = json[at..].trim_start().strip_prefix(':')?.trim_start();
    let mut chars = rest.strip_prefix('"')?.chars();
    let mut out = String::new();
    loop {
        match chars.next()? {
            '"' => return Some(out),
            '\\' => match chars.next()? {
                'n' => out.push('\n'),
                other => out.push(other),
            },
            c => out.push(c),
        }
    }
}

fn json_number(json: &str, key: &str) -> Option<u64> {
    let at = json.find(&format!("\"{key}\""))? + key.len() + 2;
    let rest = json[at..].trim_start().strip_prefix(':')?.trim_start();
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

fn battle_net_games(data: &Path, local: &mut dyn FnMut(&Path) -> bool) -> Vec<Game> {
    let agent = data.join("Battle.net").join("Agent");
    // `.product.db` is the file of a current Battle.net, `product.db` of an older one.
    let bytes: Vec<u8> = [".product.db", "product.db"]
        .iter()
        .filter_map(|name| read_bytes(&agent.join(name)))
        .flatten()
        .collect();
    let mut out: Vec<Game> = Vec::new();
    for dir in install_paths(&bytes) {
        // Battle.net's own data (the Agent) and its client folder are in the db too: no games.
        let own = dir
            .file_name()
            .is_some_and(|n| n.eq_ignore_ascii_case("Battle.net"));
        if own || !own_folder(&dir) || dir.starts_with(data.join("Battle.net")) || !local(&dir) {
            continue;
        }
        let Some(exe) = main_exe(&dir) else { continue };
        if out.iter().any(|g| g.exe == exe) {
            continue;
        }
        // `_retail_` is a game's build folder, its name is the folder above.
        let folder = dir
            .file_name()
            .filter(|n| !n.to_string_lossy().starts_with('_'))
            .or_else(|| dir.parent().and_then(Path::file_name));
        let Some(name) = folder.map(|n| n.to_string_lossy().into_owned()) else {
            continue;
        };
        out.push(Game {
            name,
            size: dir_size(&dir),
            exe,
            source: Source::BattleNet,
        });
    }
    out
}

/// The absolute folders `product.db` (protobuf) holds as strings: a length byte under 128, then
/// `X:/...` (Battle.net writes forward slashes; `X:\...` too) that long. Anything else (a path
/// longer than that, other text) is skipped.
pub fn install_paths(bytes: &[u8]) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for at in 1..bytes.len().saturating_sub(3) {
        if !(bytes[at].is_ascii_alphabetic()
            && bytes[at + 1] == b':'
            && matches!(bytes[at + 2], b'\\' | b'/'))
        {
            continue;
        }
        let len = usize::from(bytes[at - 1]);
        let Some(text) = bytes
            .get(at..at + len)
            .filter(|t| len >= 4 && t.iter().all(|b| (0x20..0x7F).contains(b)))
        else {
            continue;
        };
        let path = PathBuf::from(
            String::from_utf8_lossy(text)
                .replace('/', "\\")
                .trim_end_matches('\\'),
        );
        if !out.contains(&path) {
            out.push(path);
        }
    }
    out
}

fn folder_games(root: &Path, local: &mut dyn FnMut(&Path) -> bool) -> Vec<Game> {
    if !local(root) {
        return Vec::new();
    }
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.path())
        .collect();
    dirs.sort();
    dirs.into_iter()
        .filter_map(|dir| {
            let name = dir.file_name()?.to_string_lossy().into_owned();
            Some(Game {
                name,
                exe: main_exe(&dir)?,
                source: Source::GameFolder,
                size: dir_size(&dir),
            })
        })
        .collect()
}

/// Whether `dir` is deeper than a drive's top: a prefix, a root and a folder (`C:\Games`). `C:`
/// (what `C:\` is left with once the slash is cut) is the process's current folder on C, and
/// a walk of it would list whatever the process runs in.
pub fn own_folder(dir: &Path) -> bool {
    dir.components().count() >= 3 && dir.has_root()
}

/// The exe of the app in `dir`: the biggest one in it and up to [`EXE_DEPTH`] folders down that is
/// not an uninstaller, installer or helper. A game's own exe is the biggest (a launcher stub or a
/// crash reporter is small), and it is the process a paste rule meets. Reads at most
/// [`MAX_EXE_ENTRIES`] entries; folders that are links are not followed.
pub fn main_exe(dir: &Path) -> Option<PathBuf> {
    let mut best: Option<(u64, PathBuf)> = None;
    // Shallow folders first: the budget runs out inside `Content\Paks`, never before `Binaries`.
    let mut todo = std::collections::VecDeque::from([(dir.to_path_buf(), 0usize)]);
    let mut seen = 0usize;
    while let Some((folder, depth)) = todo.pop_front() {
        let Ok(entries) = fs::read_dir(&folder) else {
            continue;
        };
        for entry in entries.flatten() {
            seen += 1;
            if seen > MAX_EXE_ENTRIES {
                return best.map(|(_, p)| p);
            }
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if kind.is_dir() {
                if depth < EXE_DEPTH {
                    todo.push_back((path, depth + 1));
                }
                continue;
            }
            let is_exe = path
                .extension()
                .is_some_and(|x| x.eq_ignore_ascii_case("exe"));
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            if !kind.is_file() || !is_exe || NOT_THE_GAME.iter().any(|w| name.contains(w)) {
                continue;
            }
            let len = entry.metadata().map_or(0, |m| m.len());
            if best.as_ref().is_none_or(|(b, _)| len > *b) {
                best = Some((len, path));
            }
        }
    }
    best.map(|(_, p)| p)
}

/// What the files below `dir` add up to, [`MAX_SIZE_ENTRIES`] entries at most (more is a lower
/// bound); folders that are links are not followed. `None` for a folder that cannot be read.
pub fn dir_size(dir: &Path) -> Option<u64> {
    fs::read_dir(dir).ok()?;
    let mut total = 0u64;
    let mut todo = vec![dir.to_path_buf()];
    let mut seen = 0usize;
    while let Some(folder) = todo.pop() {
        let Ok(entries) = fs::read_dir(&folder) else {
            continue;
        };
        for entry in entries.flatten() {
            seen += 1;
            if seen > MAX_SIZE_ENTRIES {
                return Some(total);
            }
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                todo.push(entry.path());
            } else if kind.is_file() {
                total += entry.metadata().map_or(0, |m| m.len());
            }
        }
    }
    Some(total)
}

fn read_bytes(path: &Path) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut bytes = Vec::new();
    fs::File::open(path)
        .ok()?
        .take(MAX_FILE_BYTES)
        .read_to_end(&mut bytes)
        .ok()?;
    Some(bytes)
}

fn read_text(path: &Path) -> Option<String> {
    read_bytes(path).map(|b| String::from_utf8_lossy(&b).into_owned())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::scratch::scratch_dir;

    fn dir(name: &str) -> PathBuf {
        scratch_dir(&format!("oxi-launchers-{}-{name}", std::process::id()))
    }

    /// An exe file of `len` synthetic bytes, folders made on the way.
    fn exe(path: &Path, len: usize) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, vec![0u8; len]).unwrap();
    }

    fn all(_: &Path) -> bool {
        true
    }

    #[test]
    fn libraries_and_manifests_are_read_from_valve_text() {
        let vdf = "\"libraryfolders\"\n{\n\t\"0\"\n\t{\n\t\t\"path\"\t\t\"C:\\\\Program Files (x86)\\\\Steam\"\n\t}\n\t\"1\"\n\t{\n\t\t\"path\"\t\t\"D:\\\\SteamLibrary\"\n\t}\n}";
        assert_eq!(
            steam_libraries(vdf),
            [
                PathBuf::from(r"C:\Program Files (x86)\Steam"),
                PathBuf::from(r"D:\SteamLibrary")
            ]
        );
        let acf = "\"AppState\"\n{\n\t\"appid\"\t\"1\"\n\t\"name\"\t\"Synth Game\"\n\t\"installdir\"\t\"SynthGame\"\n\t\"SizeOnDisk\"\t\"123456\"\n}";
        assert_eq!(
            steam_manifest(acf),
            Some(SteamApp {
                name: "Synth Game".into(),
                installdir: "SynthGame".into(),
                size: Some(123_456)
            })
        );
        assert_eq!(
            steam_manifest(
                "\"AppState\" { \"name\" \"Steamworks Common Redistributables\" \"installdir\" \"Steamworks Shared\" }"
            ),
            None
        );
        for bad in [
            "C:", "..", ".", "...", " ", ". .", "a/b", "a\\\\b", "C:\\\\x",
        ] {
            let acf = format!("\"AppState\" {{ \"name\" \"G\" \"installdir\" \"{bad}\" }}");
            assert_eq!(steam_manifest(&acf), None, "{bad}");
        }
    }

    #[test]
    fn a_steam_game_in_a_second_library_is_found_by_its_biggest_exe() {
        let root = dir("steam");
        let (steam, lib) = (root.join("Steam"), root.join("Lib"));
        fs::create_dir_all(steam.join("steamapps")).unwrap();
        let vdf = format!(
            "\"libraryfolders\" {{ \"1\" {{ \"path\" \"{}\" }} }}",
            lib.to_string_lossy().replace('\\', "\\\\")
        );
        fs::write(steam.join("steamapps").join("libraryfolders.vdf"), vdf).unwrap();
        fs::create_dir_all(lib.join("steamapps")).unwrap();
        fs::write(
            lib.join("steamapps").join("appmanifest_7.acf"),
            "\"AppState\" { \"name\" \"Synth Shooter\" \"installdir\" \"Shooter\" \"SizeOnDisk\" \"5000\" }",
        )
        .unwrap();
        let game = lib.join("steamapps").join("common").join("Shooter");
        exe(&game.join("Shooter.exe"), 100);
        exe(
            &game
                .join("Binaries")
                .join("Win64")
                .join("Shooter-Win64-Shipping.exe"),
            900,
        );
        exe(&game.join("UnityCrashHandler64.exe"), 5000);
        exe(&game.join("_redist").join("vcredist_x64.exe"), 9000);
        let places = Places {
            steam: vec![steam],
            ..Places::default()
        };
        let games = find(&places, &mut all);
        assert_eq!(games.len(), 1);
        assert_eq!(games[0].name, "Synth Shooter");
        assert_eq!(games[0].source, Source::Steam);
        assert_eq!(games[0].size, Some(5000));
        assert!(games[0].exe.ends_with("Shooter-Win64-Shipping.exe"));
    }

    /// `Places` for Epic manifests, one `.item` file per `(DisplayName, InstallLocation,
    /// LaunchExecutable)`, under a scratch `ProgramData`.
    fn epic(name: &str, items: &[(&str, &str, &str)]) -> Places {
        let data = dir(name);
        let manifests = data
            .join("Epic")
            .join("EpicGamesLauncher")
            .join("Data")
            .join("Manifests");
        fs::create_dir_all(&manifests).unwrap();
        for (n, (title, at, launch)) in items.iter().enumerate() {
            let at = at.replace('\\', "\\\\");
            fs::write(
                manifests.join(format!("I{n}.item")),
                format!(
                    "{{\"DisplayName\": \"{title}\", \"InstallLocation\": \"{at}\", \"LaunchExecutable\": \"{launch}\"}}"
                ),
            )
            .unwrap();
        }
        Places {
            program_data: Some(data),
            ..Places::default()
        }
    }

    #[test]
    fn an_epic_game_is_its_biggest_exe_not_the_bootstrapper_its_manifest_names() {
        let data = dir("epic-fortnite");
        let game = data.join("Fortnite");
        let bin = game.join("FortniteGame").join("Binaries").join("Win64");
        exe(&bin.join("FortniteBootstrapper.exe"), 10);
        exe(&bin.join("FortniteClient-Win64-Shipping.exe"), 900);
        let places = epic(
            "epic-fortnite-items",
            &[(
                "Fortnite",
                &game.to_string_lossy(),
                "FortniteGame/Binaries/Win64/FortniteBootstrapper.exe",
            )],
        );
        let games = find(&places, &mut all);
        assert_eq!(games.len(), 1);
        assert!(games[0].exe.ends_with("FortniteClient-Win64-Shipping.exe"));
    }

    #[test]
    fn an_epic_dlc_that_exists_on_disk_is_no_tile() {
        let data = dir("epic-dlc");
        let dlc = data.join("Content");
        exe(&dlc.join("FortniteClient-Win64-Shipping.exe"), 900);
        let places = epic("epic-dlc-items", &[("DLC", &dlc.to_string_lossy(), "")]);
        assert_eq!(find(&places, &mut all), []);
    }

    #[test]
    fn an_epic_manifest_exe_outside_its_folder_or_off_this_disk_is_never_used() {
        let data = dir("epic-out");
        let game = data.join("Game");
        // Only a helper in the folder (no main exe), and a real exe beside the folder.
        exe(&game.join("setup-helper.exe"), 10);
        exe(&data.join("outside").join("cmd.exe"), 10);
        let at = game.to_string_lossy();
        let places = epic(
            "epic-out-items",
            &[
                ("Up", &at, "../outside/cmd.exe"),
                ("Unc", &at, "//host/share/x.exe"),
                ("Drive", &at, "C:/Windows/System32/cmd.exe"),
                ("Inside", &at, "setup-helper.exe"),
            ],
        );
        let games = find(&places, &mut all);
        assert_eq!(games.len(), 1, "{games:?}");
        assert_eq!(games[0].name, "Inside");
        // A joined exe the callback calls remote is not opened either.
        assert_eq!(find(&places, &mut |p| p == game), []);
    }

    #[test]
    fn a_drive_top_is_no_epic_or_battle_net_game_folder() {
        for top in ["C:\\", "C:", "D:/"] {
            assert!(!own_folder(Path::new(top)), "{top}");
            assert_eq!(
                find(&epic("epic-root", &[("Root", top, "a.exe")]), &mut all),
                []
            );
        }
        assert!(own_folder(Path::new(r"C:\Games")));
    }

    #[test]
    fn an_epic_game_is_started_by_the_exe_its_manifest_names() {
        let data = dir("epic");
        let manifests = data
            .join("Epic")
            .join("EpicGamesLauncher")
            .join("Data")
            .join("Manifests");
        fs::create_dir_all(&manifests).unwrap();
        let game = data.join("Games").join("Fort");
        exe(&game.join("Fort").join("Binaries").join("Fort.exe"), 10);
        let item = format!(
            "{{\n  \"DisplayName\": \"Synth Fort\",\n  \"InstallLocation\": \"{}\",\n  \"LaunchExecutable\": \"Fort/Binaries/Fort.exe\",\n  \"InstallSize\": 777\n}}",
            game.to_string_lossy().replace('\\', "\\\\")
        );
        fs::write(manifests.join("A1.item"), item).unwrap();
        // DLC: no launch exe and no folder on disk: nothing.
        fs::write(
            manifests.join("A2.item"),
            "{\"DisplayName\": \"DLC\", \"InstallLocation\": \"C:\\\\no\\\\such\\\\epic\", \"LaunchExecutable\": \"\"}",
        )
        .unwrap();
        let places = Places {
            program_data: Some(data),
            ..Places::default()
        };
        let games = find(&places, &mut all);
        assert_eq!(games.len(), 1);
        assert_eq!(games[0].name, "Synth Fort");
        assert_eq!(games[0].source, Source::Epic);
        assert_eq!(games[0].size, Some(777));
        assert!(games[0].exe.ends_with("Fort.exe"));
    }

    #[test]
    fn battle_net_install_folders_come_out_of_the_protobuf_strings() {
        let data = dir("bnet");
        let overwatch = data.join("Games").join("Overwatch");
        exe(&overwatch.join("_retail_").join("Overwatch.exe"), 800);
        exe(&overwatch.join("Overwatch Launcher.exe"), 100);
        // Battle.net writes forward slashes.
        let path = overwatch.to_string_lossy().replace('\\', "/");
        // field 1 "ow", field 2 the path (length byte first), field 3 a number.
        let mut db = vec![0x0A, 2, b'o', b'w', 0x12, u8::try_from(path.len()).unwrap()];
        db.extend(path.as_bytes());
        db.extend([0x1A, 0x01, 0x05]);
        // The Agent's own data under ProgramData is no game.
        let own = data.join("Battle.net").join("Agent");
        fs::create_dir_all(&own).unwrap();
        let own_path = own.to_string_lossy().replace('\\', "/");
        db.extend([0x22, u8::try_from(own_path.len()).unwrap()]);
        db.extend(own_path.as_bytes());
        fs::write(own.join(".product.db"), &db).unwrap();
        assert_eq!(
            install_paths(&db),
            [
                PathBuf::from(path.replace('/', "\\")),
                PathBuf::from(own_path.replace('/', "\\"))
            ]
        );
        let places = Places {
            program_data: Some(data),
            ..Places::default()
        };
        let games = find(&places, &mut all);
        assert_eq!(games.len(), 1, "{games:?}");
        assert_eq!(games[0].name, "Overwatch");
        assert_eq!(games[0].source, Source::BattleNet);
        assert!(games[0].exe.ends_with("Overwatch.exe"));
        assert_eq!(games[0].size, Some(900));
    }

    #[test]
    fn game_folders_give_one_game_per_subfolder_and_missing_launchers_nothing() {
        let root = dir("folders");
        exe(&root.join("Games").join("Alpha").join("alpha.exe"), 10);
        fs::create_dir_all(root.join("Games").join("Empty")).unwrap();
        let places = Places {
            steam: vec![root.join("no-steam")],
            program_data: Some(root.join("no-data")),
            game_roots: vec![root.join("Games"), root.join("No Games")],
        };
        let games = find(&places, &mut all);
        assert_eq!(games.len(), 1);
        assert_eq!(games[0].name, "Alpha");
        assert_eq!(games[0].source, Source::GameFolder);
        assert_eq!(games[0].size, Some(10));
        assert_eq!(find(&Places::default(), &mut all), []);
    }

    #[test]
    fn a_path_that_is_not_local_is_never_opened() {
        let root = dir("remote");
        exe(&root.join("Games").join("Alpha").join("alpha.exe"), 10);
        let places = Places {
            game_roots: vec![root.join("Games")],
            ..Places::default()
        };
        assert_eq!(find(&places, &mut |_| false), []);
    }
}
