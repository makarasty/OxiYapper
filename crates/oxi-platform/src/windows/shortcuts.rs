//! The apps a person can start: `.lnk` shortcuts in the Start menu and on the desktop, per user and
//! for all users (item 16), then what the registry knows (Uninstall keys with an exe icon, App Paths).
//! A shortcut's file name is the app's friendly name; its target, or the app a launcher starts
//! (`Update.exe --processStart Discord.exe`, a Store app's package manifest), is the exe a paste
//! rule matches. A relative target is read from the shortcut's own folder.

use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

use windows::Win32::System::Com::{CoTaskMemFree, IBindCtx};
use windows::Win32::UI::Shell::{
    BHID_EnumItems, FOLDERID_AppsFolder, IEnumShellItems, IShellItem, KF_FLAG_DEFAULT,
    SHGetKnownFolderItem, SIGDN, SIGDN_NORMALDISPLAY, SIGDN_PARENTRELATIVEPARSING,
};
use windows_sys::Win32::Foundation::ERROR_MORE_DATA;
use windows_sys::Win32::Graphics::Gdi::{
    BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, DeleteObject, GetDC, GetDIBits,
    GetObjectW, ReleaseDC,
};
use windows_sys::Win32::Storage::FileSystem::GetDriveTypeW;
use windows_sys::Win32::System::Ole::{OleInitialize, OleUninitialize};
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, RRF_RT_REG_DWORD, RRF_RT_REG_EXPAND_SZ,
    RRF_RT_REG_SZ, RegCloseKey, RegEnumKeyExW, RegGetValueW, RegOpenKeyExW,
};
use windows_sys::Win32::UI::Shell::{
    CSIDL_COMMON_DESKTOPDIRECTORY, CSIDL_COMMON_PROGRAMS, CSIDL_DESKTOPDIRECTORY, CSIDL_PROGRAMS,
    ExtractIconExW, SHGetFolderPathW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, HICON, ICONINFO};

/// Folders below a Start menu root that are looked into; the Start menu nests two or three deep.
/// A desktop root is read at its top only: below it lie projects, `node_modules` and the like.
const MAX_DEPTH: usize = 6;
/// Shortcuts read per scan at most, so a folder full of them cannot stall the worker.
const MAX_LINKS: usize = 2000;
/// Folders opened per scan at most, shortcuts or not.
const MAX_DIRS: usize = 1000;
/// `GetDriveTypeW` for a local disk (`Win32_System_WindowsProgramming` holds the constant).
const DRIVE_FIXED: u32 = 3;
/// A real `.lnk` is a few KB; anything longer is cut there and fails to parse.
const MAX_LINK_BYTES: u64 = 64 * 1024;
/// Registry keys read per list, so a broken hive cannot stall the worker.
const MAX_KEYS: u32 = 4000;
/// An `AppxManifest.xml` is a few KB; a longer one is cut there.
const MAX_MANIFEST_BYTES: u64 = 256 * 1024;
/// Apps that start other things: a shortcut to one with arguments (Control Panel applets, a folder in
/// Explorer, a document in Notepad) is not the app itself.
const HOSTS: [&str; 5] = [
    "explorer.exe",
    "control.exe",
    "rundll32.exe",
    "mmc.exe",
    "notepad.exe",
];
const UNINSTALL: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall";
const UNINSTALL_32: &str = r"SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall";
const APP_PATHS: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths";
const APP_PATHS_32: &str = r"SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\App Paths";
const PACKAGES: &str = r"Software\Classes\Local Settings\Software\Microsoft\Windows\CurrentVersion\AppModel\Repository\Packages";

/// One app found by a scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct App {
    /// The shortcut's name, as the Start menu shows it.
    pub name: String,
    /// The exe file name a paste rule matches.
    pub exe: String,
    /// Where [`scan`] takes the icon from; `None` when the shortcut points off this computer.
    pub icon_source: Option<IconSource>,
    /// Filled by [`scan`]; `None` when the shell has none for it.
    pub icon: Option<Icon>,
}

/// Where an app's icon is read from. Always a path on a local disk, never a network one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IconSource {
    /// Icon number `index` in an exe, DLL or `.ico` the shortcut names or targets.
    Resource(PathBuf, i32),
}

/// An icon as straight (not premultiplied) RGBA rows, top row first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Icon {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// What a shortcut points at.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Link {
    pub target: String,
    pub args: String,
    /// The icon file the shortcut names, empty when it uses its target's.
    pub icon: String,
    /// The icon's number in that file.
    pub icon_index: i32,
}

/// The Start menu and desktop folders, the user's and the all-users ones, in that order; a folder
/// Windows does not name is left out.
pub fn roots() -> Vec<PathBuf> {
    [
        CSIDL_PROGRAMS,
        CSIDL_COMMON_PROGRAMS,
        CSIDL_DESKTOPDIRECTORY,
        CSIDL_COMMON_DESKTOPDIRECTORY,
    ]
    .into_iter()
    .filter_map(folder)
    .collect()
}

fn folder(csidl: u32) -> Option<PathBuf> {
    let mut buf = [0u16; 260];
    let csidl = i32::try_from(csidl).ok()?;
    // SAFETY: the call writes at most MAX_PATH (260) units into `buf`, which lives across it.
    let hr = unsafe {
        SHGetFolderPathW(
            std::ptr::null_mut(),
            csidl,
            std::ptr::null_mut(),
            0,
            buf.as_mut_ptr(),
        )
    };
    let len = buf.iter().position(|&c| c == 0)?;
    (hr == 0 && len > 0).then(|| PathBuf::from(String::from_utf16_lossy(&buf[..len])))
}

/// [`find`] in `roots`, then each app's icon. Blocking: run it on a worker, never the UI thread.
pub fn scan(roots: &[PathBuf]) -> Vec<App> {
    with_icons(find(roots))
}

/// [`scan`] over the Start menu and desktop folders, plus the apps the registry lists that have no
/// shortcut. Blocking: run it on a worker.
pub fn scan_installed() -> Vec<App> {
    let mut walk = walk_roots(&roots(), &desktops());
    let store = with_ole(|| store_apps(&mut walk.drives));
    walk.apps.extend(store);
    let registry = registry_apps(&mut walk.drives);
    walk.apps.extend(registry);
    with_icons(finish(walk.apps))
}

fn with_icons(mut apps: Vec<App>) -> Vec<App> {
    with_ole(|| {
        for app in &mut apps {
            app.icon = app.icon_source.as_ref().and_then(icon);
        }
    });
    apps
}

/// `f` with COM initialised on this thread (the shell needs it), undone after.
fn with_ole<T>(f: impl FnOnce() -> T) -> T {
    // SAFETY: no arguments.
    let ole = unsafe { OleInitialize(std::ptr::null()) } >= 0;
    let out = f();
    if ole {
        // SAFETY: pairs the successful `OleInitialize` above on this thread.
        unsafe { OleUninitialize() };
    }
    out
}

fn desktops() -> Vec<PathBuf> {
    [CSIDL_DESKTOPDIRECTORY, CSIDL_COMMON_DESKTOPDIRECTORY]
        .into_iter()
        .filter_map(folder)
        .collect()
}

/// Every app behind a `.lnk` under `roots`, one per exe (case-insensitive, a shortcut without
/// arguments before one with, then the first found), sorted by name. Uninstallers and shortcuts to
/// anything but an exe are left out. The desktop folders are read without their subfolders, the
/// rest [`MAX_DEPTH`] deep.
pub fn find(roots: &[PathBuf]) -> Vec<App> {
    find_in(roots, &desktops())
}

fn find_in(roots: &[PathBuf], desktops: &[PathBuf]) -> Vec<App> {
    finish(walk_roots(roots, desktops).apps)
}

fn walk_roots(roots: &[PathBuf], desktops: &[PathBuf]) -> Walk {
    let mut scan = Walk::default();
    for root in roots {
        let depth = if desktops.contains(root) {
            0
        } else {
            MAX_DEPTH
        };
        scan.walk(root, 0, depth);
    }
    scan
}

/// The candidates (`true`: no arguments, the app itself) as one app per exe: a plain one wins.
fn finish(mut found: Vec<(App, bool)>) -> Vec<App> {
    found.sort_by_key(|(_, plain)| !plain);
    dedupe(found.into_iter().map(|(app, _)| app).collect())
}

/// One [`find`] in progress: what it has found and spent so far.
#[derive(Default)]
struct Walk {
    /// Each with whether its shortcut has no arguments.
    apps: Vec<(App, bool)>,
    links: usize,
    dirs: usize,
    drives: Drives,
}

impl Walk {
    fn walk(&mut self, dir: &Path, depth: usize, max_depth: usize) {
        if self.dirs >= MAX_DIRS {
            return;
        }
        self.dirs += 1;
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(fs::DirEntry::file_name);
        for entry in entries {
            if self.links >= MAX_LINKS {
                return;
            }
            // Not followed: a link to a folder above would loop.
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if kind.is_dir() {
                if depth < max_depth {
                    self.walk(&path, depth + 1, max_depth);
                }
            } else if kind.is_file()
                && path
                    .extension()
                    .is_some_and(|x| x.eq_ignore_ascii_case("lnk"))
            {
                self.links += 1;
                if let Some(app) = app_at(&path, &mut self.drives) {
                    self.apps.push(app);
                }
            }
        }
    }
}

fn app_at(path: &Path, drives: &mut Drives) -> Option<(App, bool)> {
    let mut bytes = Vec::new();
    File::open(path)
        .ok()?
        .take(MAX_LINK_BYTES)
        .read_to_end(&mut bytes)
        .ok()?;
    let name = path.file_stem()?.to_string_lossy().into_owned();
    let mut link = parse(&bytes)?;
    let folder = path.parent()?;
    link.target = absolutize(folder, &link.target);
    link.icon = absolutize(folder, &link.icon);
    // `plain`: the shortcut starts the app itself, not a host with arguments.
    let (exe, plain) = if let Some(aumid) = aumid_of(&link) {
        // A Store app: the shortcut starts Explorer, the exe lives in the package.
        let (exe, target) = store_exe(aumid)?;
        link.target = target;
        (exe, true)
    } else {
        let exe = app_of(&link)?;
        // A launcher that starts another exe (`--processStart`) is that app's shortcut.
        let target = link.target.rsplit(['\\', '/']).next().unwrap_or_default();
        let plain = link.args.is_empty() || !target.eq_ignore_ascii_case(&exe);
        (exe, plain)
    };
    let lower = |s: &str| s.to_lowercase();
    if lower(&name).contains("uninstall")
        || lower(&exe).starts_with("unins")
        || (!plain && HOSTS.contains(&lower(&exe).as_str()))
    {
        return None;
    }
    let icon_source = icon_source(&link, &mut |l| drives.fixed(l), Path::is_file);
    let app = App {
        name,
        exe,
        icon_source,
        icon: None,
    };
    Some((app, plain))
}

/// `rel` against `base`, `.` and `..` folded, unless it is empty, absolute (a drive, `\\`, `/`) or
/// starts with `%` (an environment name, expanded later). A `..` above the drive stops at it.
fn absolutize(base: &Path, rel: &str) -> String {
    let drive = matches!(rel.as_bytes(), [l, b':', ..] if l.is_ascii_alphabetic());
    if rel.is_empty() || drive || rel.starts_with(['\\', '/', '%']) {
        return rel.to_owned();
    }
    let base = base.to_string_lossy();
    let mut parts: Vec<&str> = base.split(['\\', '/']).filter(|p| !p.is_empty()).collect();
    for part in rel.split(['\\', '/']) {
        match part {
            "" | "." => {}
            ".." => {
                // The drive (`C:`) stays.
                if parts.len() > 1 {
                    parts.pop();
                }
            }
            p => parts.push(p),
        }
    }
    let joined = parts.join("\\");
    // A bare drive needs its slash.
    if parts.len() == 1 {
        joined + "\\"
    } else {
        joined
    }
}

/// The app id behind a Store shortcut: Explorer started with `shell:AppsFolder\<id>`.
fn aumid_of(link: &Link) -> Option<&str> {
    const PREFIX: &str = "shell:AppsFolder\\";
    let file = link.target.rsplit(['\\', '/']).next()?;
    if !file.eq_ignore_ascii_case("explorer.exe") {
        return None;
    }
    let args = link.args.trim().trim_matches('"');
    let (head, id) = args.split_at_checked(PREFIX.len())?;
    head.eq_ignore_ascii_case(PREFIX)
        .then_some(id)
        .filter(|id| !id.is_empty())
}

/// The exe file name the `Application` with `Id` `app_id` of a package manifest starts.
fn manifest_exe(xml: &str, app_id: &str) -> Option<String> {
    // `name="value"` in a tag, the name after a space.
    fn attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
        let key = format!(" {name}=\"");
        let at = tag.find(&key)? + key.len();
        tag[at..].split('"').next()
    }
    xml.split("<Application").skip(1).find_map(|rest| {
        let tag = &rest[..rest.find('>')?];
        let exe = attr(tag, "Executable").filter(|_| attr(tag, "Id") == Some(app_id))?;
        Some(exe.rsplit(['\\', '/']).next().unwrap_or(exe).to_owned())
    })
}

/// The exe file name and full path of the Store app `aumid` (`Family!AppId`): the package's folder
/// from the registry, its `AppxManifest.xml` for the exe. `None` when the package is gone.
fn store_exe(aumid: &str) -> Option<(String, String)> {
    let (family, app_id) = aumid.split_once('!')?;
    let (name, publisher) = family.rsplit_once('_')?;
    let (prefix, suffix) = (format!("{name}_"), format!("__{publisher}"));
    for package in reg_subkeys(HKEY_CURRENT_USER, PACKAGES) {
        if !(package.starts_with(&prefix) && package.ends_with(&suffix)) {
            continue;
        }
        let key = format!(r"{PACKAGES}\{package}");
        let Some(root) = reg_string(HKEY_CURRENT_USER, &key, Some("PackageRootFolder")) else {
            continue;
        };
        let mut xml = String::new();
        let read = File::open(Path::new(&root).join("AppxManifest.xml"))
            .and_then(|f| f.take(MAX_MANIFEST_BYTES).read_to_string(&mut xml));
        if read.is_err() {
            continue;
        }
        if let Some(exe) = manifest_exe(&xml, app_id) {
            // A desktop-bridge app keeps its exe below `app\`; the icon falls back to the
            // shortcut's when neither path exists.
            let top = Path::new(&root).join(&exe);
            let full = if top.is_file() {
                top
            } else {
                Path::new(&root).join("app").join(&exe)
            };
            return Some((exe, full.to_string_lossy().into_owned()));
        }
    }
    None
}

/// The packaged apps the shell lists in its `AppsFolder` (Windows Terminal, Calculator, a Store
/// `WhatsApp`), which have no shortcut file: each id with a `!` (`Family!AppId`) is resolved by
/// [`store_exe`]. Read-only; needs COM on this thread.
fn store_apps(drives: &mut Drives) -> Vec<(App, bool)> {
    fn text(item: &IShellItem, kind: SIGDN) -> Option<String> {
        // SAFETY: `item` is a live shell item; the returned string is ours to free, once.
        unsafe {
            let pwstr = item.GetDisplayName(kind).ok()?;
            let text = pwstr.to_string().ok();
            CoTaskMemFree(Some(pwstr.0.cast_const().cast()));
            text
        }
    }
    let mut out = Vec::new();
    // SAFETY: plain COM calls on live interfaces; the known-folder id outlives the call.
    let items = unsafe {
        SHGetKnownFolderItem::<IShellItem>(&FOLDERID_AppsFolder, KF_FLAG_DEFAULT, None)
            .and_then(|f| f.BindToHandler::<_, IEnumShellItems>(None::<&IBindCtx>, &BHID_EnumItems))
    };
    let Ok(items) = items else {
        return out;
    };
    for _ in 0..MAX_KEYS {
        let mut one = [None];
        let mut got = 0u32;
        // SAFETY: `one` holds the one item asked for and `got` the count; both outlive the call.
        let next = unsafe { items.Next(&mut one, Some(&raw mut got)) };
        let (Ok(()), 1, Some(item)) = (next, got, one[0].take()) else {
            break;
        };
        let (Some(id), Some(name)) = (
            text(&item, SIGDN_PARENTRELATIVEPARSING),
            text(&item, SIGDN_NORMALDISPLAY),
        ) else {
            continue;
        };
        if !id.contains('!') {
            continue;
        }
        let Some((exe, target)) = store_exe(&id) else {
            continue;
        };
        let link = Link {
            target,
            ..Link::default()
        };
        let icon_source = icon_source(&link, &mut |l| drives.fixed(l), Path::is_file);
        let app = App {
            name,
            exe,
            icon_source,
            icon: None,
        };
        out.push((app, true));
    }
    out
}

/// Apps the registry lists whose exe exists on a local disk: an Uninstall entry's `DisplayIcon`
/// (when it is an exe) under the entry's `DisplayName`, and each `App Paths` exe under its file
/// name. The Start menu is read first, so a name from there wins ([`finish`]).
fn registry_apps(drives: &mut Drives) -> Vec<(App, bool)> {
    let mut out = Vec::new();
    for (root, base) in [
        (HKEY_LOCAL_MACHINE, UNINSTALL),
        (HKEY_LOCAL_MACHINE, UNINSTALL_32),
        (HKEY_CURRENT_USER, UNINSTALL),
    ] {
        for sub in reg_subkeys(root, base) {
            let key = format!(r"{base}\{sub}");
            if reg_dword(root, &key, "SystemComponent") == Some(1) {
                continue;
            }
            let (Some(name), Some(icon)) = (
                reg_string(root, &key, Some("DisplayName")),
                reg_string(root, &key, Some("DisplayIcon")),
            ) else {
                continue;
            };
            let (file, index) = split_display_icon(&icon);
            out.extend(registry_app(name, file, index, drives));
        }
    }
    for (root, base) in [
        (HKEY_LOCAL_MACHINE, APP_PATHS),
        (HKEY_LOCAL_MACHINE, APP_PATHS_32),
        (HKEY_CURRENT_USER, APP_PATHS),
    ] {
        for sub in reg_subkeys(root, base) {
            let Some(stem) = sub
                .strip_suffix(".exe")
                .or_else(|| sub.strip_suffix(".EXE"))
            else {
                continue;
            };
            if let Some(path) = reg_string(root, &format!(r"{base}\{sub}"), None) {
                let (file, _) = split_display_icon(&path);
                // An App Paths exe with no icon is a helper or a service (winget, a driver tool).
                out.extend(
                    registry_app(stem.to_owned(), file, 0, drives)
                        .filter(|(app, _)| has_icon_resource(app)),
                );
            }
        }
    }
    out
}

/// Whether the exe of `app` holds at least one icon (asked of the shell, nothing extracted).
fn has_icon_resource(app: &App) -> bool {
    let Some(IconSource::Resource(file, _)) = &app.icon_source else {
        return false;
    };
    let path = shell_path(file);
    // SAFETY: `path` is NUL-terminated; index -1 with no icon outputs only counts the icons.
    unsafe {
        ExtractIconExW(
            path.as_ptr(),
            -1,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            0,
        ) > 0
    }
}

fn registry_app(
    name: String,
    file: String,
    index: i32,
    drives: &mut Drives,
) -> Option<(App, bool)> {
    let exe = file.rsplit(['\\', '/']).next()?.to_owned();
    let (lower, path) = (exe.to_lowercase(), file.to_lowercase());
    if Path::new(&lower).extension().is_none_or(|x| x != "exe")
        || lower.starts_with("unins")
        || name.to_lowercase().contains("uninstall")
        || is_installer(&lower, &path)
        || !is_local_path(&file, &mut |l| drives.fixed(l))
        || !Path::new(&file).is_file()
    {
        return None;
    }
    let app = App {
        name,
        exe,
        icon_source: Some(IconSource::Resource(PathBuf::from(file), index)),
        icon: None,
    };
    Some((app, true))
}

/// Whether the exe (`file`, `path`: both lowercase) is an installer, not the app: it sits in
/// `Windows\Installer\{id}` or a `Package Cache` (VC++ redistributables, `python-3.x-amd64.exe`),
/// or its name says so.
fn is_installer(file: &str, path: &str) -> bool {
    path.contains(r"\windows\installer\")
        || path.contains(r"\package cache\")
        || ["setup", "install", "redist"]
            .iter()
            .any(|word| file.contains(word))
}

/// A `DisplayIcon` or default `App Paths` value, `"C:\a b\a.exe",0` or `C:\a.exe,-3`: the file
/// and the icon number (0 when there is none).
fn split_display_icon(value: &str) -> (String, i32) {
    let value = value.trim();
    let (file, number) = match value.strip_prefix('"') {
        Some(quoted) => quoted
            .split_once('"')
            .map_or((quoted, ""), |(f, rest)| (f, rest)),
        None => value.rsplit_once(',').unwrap_or((value, "")),
    };
    let index = number.trim_start_matches(',').trim().parse().unwrap_or(0);
    (file.trim().to_owned(), index)
}

fn reg_open(root: HKEY, path: &str) -> Option<HKEY> {
    let path = super::wide(path);
    let mut key: HKEY = std::ptr::null_mut();
    // SAFETY: `path` is NUL-terminated and `key` takes the handle; both outlive the call.
    let status = unsafe { RegOpenKeyExW(root, path.as_ptr(), 0, KEY_READ, &raw mut key) };
    (status == 0).then_some(key)
}

/// The names of the subkeys of `path`, at most [`MAX_KEYS`].
fn reg_subkeys(root: HKEY, path: &str) -> Vec<String> {
    let Some(key) = reg_open(root, path) else {
        return Vec::new();
    };
    let mut names = Vec::new();
    for index in 0..MAX_KEYS {
        let mut name = [0u16; 256];
        let mut len = 256u32;
        // SAFETY: `name` holds `len` units and both outlive the call; the optional outputs are null.
        let status = unsafe {
            RegEnumKeyExW(
                key,
                index,
                name.as_mut_ptr(),
                &raw mut len,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if status != 0 {
            break;
        }
        names.push(String::from_utf16_lossy(&name[..len as usize]));
    }
    // SAFETY: `key` came from `RegOpenKeyExW` and is closed once.
    unsafe { RegCloseKey(key) };
    names
}

/// A string value (`None`: the key's default), environment names expanded.
fn reg_string(root: HKEY, path: &str, value: Option<&str>) -> Option<String> {
    let (path, value) = (super::wide(path), value.map(super::wide));
    let mut data = vec![0u16; 512];
    // The second round has the size the first one asked for (`ERROR_MORE_DATA`).
    for _ in 0..2 {
        let mut len = u32::try_from(size_of_val(&data[..])).ok()?;
        // SAFETY: the names are NUL-terminated and outlive the call; `data` holds `len` bytes.
        let status = unsafe {
            RegGetValueW(
                root,
                path.as_ptr(),
                value.as_ref().map_or(std::ptr::null(), Vec::as_ptr),
                RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ,
                std::ptr::null_mut(),
                data.as_mut_ptr().cast(),
                &raw mut len,
            )
        };
        match status {
            0 => return utf16_text(&data, usize::try_from(len).ok()?),
            ERROR_MORE_DATA => data.resize(usize::try_from(len).ok()? / 2 + 1, 0),
            _ => return None,
        }
    }
    None
}

/// The text of a registry value: `bytes` of `data` (never past its end), up to the first NUL;
/// `None` when empty.
fn utf16_text(data: &[u16], bytes: usize) -> Option<String> {
    let text = &data[..(bytes / 2).min(data.len())];
    let end = text.iter().position(|&c| c == 0).unwrap_or(text.len());
    (end > 0).then(|| String::from_utf16_lossy(&text[..end]))
}

fn reg_dword(root: HKEY, path: &str, value: &str) -> Option<u32> {
    let (path, value) = (super::wide(path), super::wide(value));
    let mut data = 0u32;
    let mut len = 4u32;
    // SAFETY: the names are NUL-terminated and outlive the call; `data` is the 4 bytes of `len`.
    let status = unsafe {
        RegGetValueW(
            root,
            path.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            (&raw mut data).cast(),
            &raw mut len,
        )
    };
    (status == 0).then_some(data)
}

/// Where the icon of the app behind `link` is read from. When the icon file or the target is not
/// on a local disk (see [`is_local_path`]), or neither exists, there is none, and `is_file` is
/// never asked about the remote ones: any program can drop a shortcut to a `\\host\share` on the
/// desktop, and touching it would open SMB from this process and send the user's credentials
/// there. The `.lnk` itself is never handed to the shell: it reads data blocks `parse` never checks.
fn icon_source(
    link: &Link,
    fixed: &mut impl FnMut(u8) -> bool,
    is_file: impl Fn(&Path) -> bool,
) -> Option<IconSource> {
    // An empty icon is the target's own.
    let icon = (!link.icon.is_empty()).then(|| (expand(&link.icon), link.icon_index));
    let places: Vec<(String, i32)> = icon
        .into_iter()
        .chain([(expand(&link.target), 0)])
        .collect();
    if !places.iter().all(|(p, _)| is_local_path(p, fixed)) {
        return None;
    }
    places
        .into_iter()
        .map(|(p, index)| (PathBuf::from(p), index))
        .find(|(p, _)| is_file(p))
        .map(|(file, index)| IconSource::Resource(file, index))
}

/// Whether `path` is an absolute path on a local disk: `X:\` or `X:/` with `fixed(X)` true (`X`
/// uppercase). Anything that can reach another computer is not: a `\\` or `//` start in any form
/// (UNC, `\\?\UNC\`, `\\.\` devices), and a path that names no drive (relative, empty, `%VAR%`).
fn is_local_path(path: &str, fixed: &mut impl FnMut(u8) -> bool) -> bool {
    let slash = |c: u8| c == b'\\' || c == b'/';
    match path.as_bytes() {
        [a, b, ..] if slash(*a) && slash(*b) => false,
        [letter, b':', s, ..] if letter.is_ascii_alphabetic() && slash(*s) => {
            fixed(letter.to_ascii_uppercase())
        }
        _ => false,
    }
}

/// Which drive letters are local disks, asked of Windows once per letter.
#[derive(Default)]
struct Drives([Option<bool>; 26]);

impl Drives {
    /// `letter` is `A` to `Z`. A mapped network drive, a removable or a disconnected one is not
    /// fixed; `GetDriveTypeW` answers from the drive table without touching the network.
    fn fixed(&mut self, letter: u8) -> bool {
        let slot = usize::from(letter - b'A');
        *self.0[slot].get_or_insert_with(|| {
            let root = super::wide(format!("{}:\\", char::from(letter)));
            // SAFETY: `root` is NUL-terminated and lives across the call.
            unsafe { GetDriveTypeW(root.as_ptr()) == DRIVE_FIXED }
        })
    }
}

/// `%NAME%` replaced by that environment variable, kept as it is when unset.
fn expand(path: &str) -> String {
    let mut out = String::new();
    let mut rest = path;
    while let Some(start) = rest.find('%') {
        let Some(len) = rest[start + 1..].find('%') else {
            break;
        };
        let name = &rest[start + 1..start + 1 + len];
        out.push_str(&rest[..start]);
        match std::env::var(name) {
            Ok(value) if !name.is_empty() => out.push_str(&value),
            _ => out.push_str(&rest[start..start + len + 2]),
        }
        rest = &rest[start + len + 2..];
    }
    out.push_str(rest);
    out
}

/// One app per exe, case-insensitive, the first wins; sorted by name, then exe.
pub fn dedupe(apps: Vec<App>) -> Vec<App> {
    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<App> = apps
        .into_iter()
        .filter(|a| seen.insert(a.exe.to_lowercase()))
        .collect();
    out.sort_by_cached_key(|a| (a.name.to_lowercase(), a.exe.to_lowercase()));
    out
}

/// The exe a paste rule should match for `link`: the app a Squirrel launcher starts (Discord,
/// Slack and mods of them run `Update.exe --processStart Discord.exe`), else the target's file
/// name. `None` for a target that is not an exe.
pub fn app_of(link: &Link) -> Option<String> {
    let exe = |s: &str| s.to_lowercase().ends_with(".exe");
    let file = link.target.rsplit(['\\', '/']).next()?;
    if !exe(file) {
        return None;
    }
    // ASCII only, so a byte offset in it is one in `args`.
    let lower = link.args.to_ascii_lowercase();
    if let Some(at) = lower.find("--processstart") {
        let rest = link.args[at + "--processstart".len()..].trim_start();
        let started = match rest.strip_prefix('"') {
            Some(quoted) => quoted.split('"').next(),
            None => rest.split_whitespace().next(),
        };
        if let Some(started) = started.filter(|s| exe(s)) {
            return Some(
                started
                    .rsplit(['\\', '/'])
                    .next()
                    .unwrap_or(started)
                    .to_owned(),
            );
        }
    }
    Some(file.to_owned())
}

const HAS_ID_LIST: u32 = 1;
const HAS_LINK_INFO: u32 = 1 << 1;
const HAS_NAME: u32 = 1 << 2;
const HAS_RELATIVE_PATH: u32 = 1 << 3;
const HAS_WORKING_DIR: u32 = 1 << 4;
const HAS_ARGUMENTS: u32 = 1 << 5;
const HAS_ICON_LOCATION: u32 = 1 << 6;
const IS_UNICODE: u32 = 1 << 7;
const HEADER_SIZE: usize = 0x4C;
const ENV_BLOCK: u32 = 0xA000_0001;
const LINK_CLSID: [u8; 16] = [
    0x01, 0x14, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46,
];

/// Target and arguments of a shell link ([MS-SHLLINK]). The target is the local path from the
/// link info, else the environment-variable block's (`%ProgramFiles%\...`, unexpanded), else the
/// relative path. `None` when the bytes are not a shell link or name no target.
// shortcut: a link with only an item-id list (some MSI "advertised" shortcuts) gives no target and
// is skipped; resolve it with IShellLink once someone misses such an app.
pub fn parse(bytes: &[u8]) -> Option<Link> {
    if u32_at(bytes, 0)? != 0x4C || bytes.get(4..20)? != LINK_CLSID {
        return None;
    }
    let flags = u32_at(bytes, 0x14)?;
    let mut at = HEADER_SIZE;
    if flags & HAS_ID_LIST != 0 {
        at += 2 + usize::from(u16_at(bytes, at)?);
    }
    let mut target = None;
    if flags & HAS_LINK_INFO != 0 {
        let size = usize::try_from(u32_at(bytes, at)?).ok()?;
        target = link_info_path(bytes.get(at..at.checked_add(size)?)?);
        at += size;
    }
    let unicode = flags & IS_UNICODE != 0;
    let mut strings = [String::new(), String::new(), String::new(), String::new()];
    let present = [HAS_NAME, HAS_RELATIVE_PATH, HAS_WORKING_DIR, HAS_ARGUMENTS];
    for (slot, bit) in strings.iter_mut().zip(present) {
        if flags & bit != 0 {
            let (text, next) = string_at(bytes, at, unicode)?;
            *slot = text;
            at = next;
        }
    }
    let mut icon = String::new();
    if flags & HAS_ICON_LOCATION != 0 {
        (icon, at) = string_at(bytes, at, unicode)?;
    }
    let [_, relative, _, args] = strings;
    let target = target
        .or_else(|| env_target(bytes, at))
        .or_else(|| (!relative.is_empty()).then_some(relative))?;
    // ShellLinkHeader.IconIndex; meaningless without an icon location.
    let icon_index = if icon.is_empty() {
        0
    } else {
        i32::from_le_bytes(bytes.get(0x38..0x3C)?.try_into().ok()?)
    };
    Some(Link {
        target,
        args,
        icon,
        icon_index,
    })
}

/// The local base path (and common suffix) of a `LinkInfo` structure, Unicode when it has one.
fn link_info_path(info: &[u8]) -> Option<String> {
    let header = usize::try_from(u32_at(info, 4)?).ok()?;
    if u32_at(info, 8)? & 1 == 0 {
        return None;
    }
    let field = |offset: usize| usize::try_from(u32_at(info, offset)?).ok();
    let (base, suffix) = if header >= 0x24 {
        (wide_z(info, field(0x1C)?)?, wide_z(info, field(0x20)?)?)
    } else {
        (ansi_z(info, field(0x10)?)?, ansi_z(info, field(0x18)?)?)
    };
    let path = base + &suffix;
    (!path.is_empty()).then_some(path)
}

/// `TargetUnicode` of the `EnvironmentVariableDataBlock` among the extra data blocks at `at`.
fn env_target(bytes: &[u8], mut at: usize) -> Option<String> {
    loop {
        let size = usize::try_from(u32_at(bytes, at)?).ok()?;
        if size < 8 {
            return None;
        }
        if u32_at(bytes, at + 4)? == ENV_BLOCK {
            // BlockSize, BlockSignature, then 260 ANSI bytes, then 260 UTF-16 units.
            let path = wide_z(bytes.get(at..at.checked_add(size)?)?, 8 + 260)?;
            return (!path.is_empty()).then_some(path);
        }
        at = at.checked_add(size)?;
    }
}

/// A counted `StringData` entry at `at`, and where the next one starts.
fn string_at(bytes: &[u8], at: usize, unicode: bool) -> Option<(String, usize)> {
    let count = usize::from(u16_at(bytes, at)?);
    let start = at + 2;
    if unicode {
        let end = start.checked_add(count * 2)?;
        let units: Vec<u16> = bytes
            .get(start..end)?
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect();
        Some((String::from_utf16_lossy(&units), end))
    } else {
        let end = start.checked_add(count)?;
        Some((
            String::from_utf8_lossy(bytes.get(start..end)?).into_owned(),
            end,
        ))
    }
}

/// A NUL-terminated UTF-16 string at `at`; unterminated or out of bounds gives `None`.
fn wide_z(bytes: &[u8], at: usize) -> Option<String> {
    let units: Vec<u16> = bytes
        .get(at..)?
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .take_while(|&u| u != 0)
        .collect();
    // The terminator must be there too.
    bytes.get(at + units.len() * 2..at + units.len() * 2 + 2)?;
    Some(String::from_utf16_lossy(&units))
}

/// A NUL-terminated string in the system code page at `at`.
// shortcut: decoded as UTF-8, so a non-ASCII name comes out with U+FFFD; only the exe's file name
// is used and a shortcut written by Windows has the Unicode path too, which wins.
fn ansi_z(bytes: &[u8], at: usize) -> Option<String> {
    let rest = bytes.get(at..)?;
    let len = rest.iter().position(|&b| b == 0)?;
    Some(String::from_utf8_lossy(&rest[..len]).into_owned())
}

fn u16_at(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

/// A minimal Unicode shell link to `target` with `args`: header, link info with the local path, and
/// the arguments. Tests write synthetic shortcuts with it.
#[doc(hidden)]
pub fn link_bytes(target: &str, args: &str) -> Vec<u8> {
    link_bytes_with_icon(target, args, "", 0)
}

/// [`link_bytes`] with an icon location: the file `icon` and its number `index`.
#[doc(hidden)]
pub fn link_bytes_with_icon(target: &str, args: &str, icon: &str, index: i32) -> Vec<u8> {
    let utf16 = |s: &str| -> Vec<u8> { s.encode_utf16().flat_map(u16::to_le_bytes).collect() };
    let mut flags = HAS_LINK_INFO | IS_UNICODE;
    if !args.is_empty() {
        flags |= HAS_ARGUMENTS;
    }
    if !icon.is_empty() {
        flags |= HAS_ICON_LOCATION;
    }
    let mut out = vec![0u8; HEADER_SIZE];
    out[0..4].copy_from_slice(&0x4Cu32.to_le_bytes());
    out[4..20].copy_from_slice(&LINK_CLSID);
    out[0x14..0x18].copy_from_slice(&flags.to_le_bytes());
    out[0x38..0x3C].copy_from_slice(&index.to_le_bytes());
    // LinkInfo: a 0x24 header, an empty ANSI path and suffix, then the Unicode path and suffix.
    let mut path = utf16(target);
    path.extend([0, 0]);
    let header = 0x24u32;
    let ansi = header;
    let unicode = header + 2;
    let suffix = unicode + u32::try_from(path.len()).unwrap_or(0);
    let size = suffix + 2;
    for v in [size, header, 1, 0, ansi, 0, ansi + 1, unicode, suffix] {
        out.extend(v.to_le_bytes());
    }
    out.extend([0, 0]);
    out.extend(path);
    out.extend([0, 0]);
    if !args.is_empty() {
        let units = u16::try_from(args.encode_utf16().count()).unwrap_or(0);
        out.extend(units.to_le_bytes());
        out.extend(utf16(args));
    }
    if !icon.is_empty() {
        let units = u16::try_from(icon.encode_utf16().count()).unwrap_or(0);
        out.extend(units.to_le_bytes());
        out.extend(utf16(icon));
    }
    out.extend(0u32.to_le_bytes());
    out
}

/// The large icon for `source`, without the shortcut arrow. COM must be up
/// on this thread ([`scan`] does that). `None` is the default glyph.
fn icon(source: &IconSource) -> Option<Icon> {
    let mut hicon: HICON = std::ptr::null_mut();
    match source {
        IconSource::Resource(file, index) => {
            let path = shell_path(file);
            // SAFETY: `path` is NUL-terminated and `hicon` takes the one large icon asked for; both
            // outlive the call. No small icon is wanted: null.
            let got = unsafe {
                ExtractIconExW(
                    path.as_ptr(),
                    *index,
                    &raw mut hicon,
                    std::ptr::null_mut(),
                    1,
                )
            };
            if got != 1 {
                hicon = std::ptr::null_mut();
            }
        }
    }
    if hicon.is_null() {
        return None;
    }
    let pixels = icon_pixels(hicon);
    // SAFETY: the shell handed this icon to us; nothing uses it after this.
    unsafe { DestroyIcon(hicon) };
    pixels
}

/// `path` as the shell wants it: NUL-terminated UTF-16, backslashes (it finds nothing at a path
/// with forward slashes).
fn shell_path(path: &Path) -> Vec<u16> {
    super::wide(path.to_string_lossy().replace('/', "\\"))
}

fn icon_pixels(hicon: HICON) -> Option<Icon> {
    // SAFETY: ICONINFO is plain data; all zeros is a valid value.
    let mut parts: ICONINFO = unsafe { std::mem::zeroed() };
    // SAFETY: `hicon` is a live icon and `parts` lives across the call.
    if unsafe { GetIconInfo(hicon, &raw mut parts) } == 0 {
        return None;
    }
    let pixels = bitmap_rgba(parts.hbmColor);
    // SAFETY: GetIconInfo made both bitmaps for us to delete; a null one is refused harmlessly.
    unsafe {
        DeleteObject(parts.hbmColor);
        DeleteObject(parts.hbmMask);
    }
    pixels
}

fn bitmap_rgba(hbm: windows_sys::Win32::Graphics::Gdi::HBITMAP) -> Option<Icon> {
    if hbm.is_null() {
        return None;
    }
    // SAFETY: BITMAP is plain data; all zeros is a valid value.
    let mut bm: BITMAP = unsafe { std::mem::zeroed() };
    let bm_size = i32::try_from(size_of::<BITMAP>()).ok()?;
    // SAFETY: `bm` is `bm_size` bytes and lives across the call.
    if unsafe { GetObjectW(hbm, bm_size, (&raw mut bm).cast()) } == 0 {
        return None;
    }
    let (width, height) = (
        u32::try_from(bm.bmWidth).ok()?,
        u32::try_from(bm.bmHeight).ok()?,
    );
    if width == 0 || height == 0 || width > 256 || height > 256 {
        return None;
    }
    // SAFETY: BITMAPINFO is plain data; all zeros is a valid value.
    let mut bmi: BITMAPINFO = unsafe { std::mem::zeroed() };
    bmi.bmiHeader = BITMAPINFOHEADER {
        biSize: u32::try_from(size_of::<BITMAPINFOHEADER>()).ok()?,
        biWidth: bm.bmWidth,
        // Negative: top row first.
        biHeight: -bm.bmHeight,
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB,
        ..bmi.bmiHeader
    };
    let mut bgra = vec![0u8; usize::try_from(width * height * 4).ok()?];
    // SAFETY: no window: the screen's DC, released below.
    let dc = unsafe { GetDC(std::ptr::null_mut()) };
    // SAFETY: `bgra` holds `height` rows of `width` 32-bit pixels as `bmi` describes; the DC and
    // bitmap are live.
    let rows = unsafe {
        GetDIBits(
            dc,
            hbm,
            0,
            height,
            bgra.as_mut_ptr().cast(),
            &raw mut bmi,
            DIB_RGB_COLORS,
        )
    };
    // SAFETY: the DC came from GetDC with no window just above.
    unsafe { ReleaseDC(std::ptr::null_mut(), dc) };
    if u32::try_from(rows).ok()? != height {
        return None;
    }
    Some(Icon {
        width,
        height,
        rgba: rgba_of(bgra),
    })
}

/// BGRA rows as RGBA. An icon from before alpha (every alpha 0) is opaque.
// shortcut: such an icon's AND mask is ignored, so its transparent corners draw black; read the
// mask too if an old app shows a black box.
fn rgba_of(mut bgra: Vec<u8>) -> Vec<u8> {
    let opaque = bgra.as_chunks::<4>().0.iter().all(|p| p[3] == 0);
    for p in bgra.as_chunks_mut::<4>().0 {
        p.swap(0, 2);
        if opaque {
            p[3] = 255;
        }
    }
    bgra
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scratch::scratch_dir;

    fn dir(name: &str) -> PathBuf {
        scratch_dir(&format!("oxi-shortcuts-{}-{name}", std::process::id()))
    }

    fn write(dir: &Path, name: &str, target: &str, args: &str) {
        fs::write(dir.join(format!("{name}.lnk")), link_bytes(target, args)).unwrap();
    }

    #[test]
    fn a_synthetic_link_parses_back() {
        let link = parse(&link_bytes(r"C:\Synth\Synth App.exe", "--flag \"x y\"")).unwrap();
        assert_eq!(link.target, r"C:\Synth\Synth App.exe");
        assert_eq!(link.args, "--flag \"x y\"");
        assert_eq!(app_of(&link).as_deref(), Some("Synth App.exe"));
    }

    #[test]
    fn a_squirrel_launcher_resolves_to_the_app_it_starts() {
        let link = |args: &str| Link {
            target: r"C:\Users\synth\AppData\Local\SynthChat\Update.exe".into(),
            args: args.into(),
            icon: String::new(),
            icon_index: 0,
        };
        assert_eq!(
            app_of(&link("--processStart SynthChat.exe")).as_deref(),
            Some("SynthChat.exe")
        );
        assert_eq!(
            app_of(&link("--processStart \"Synth Chat.exe\" --a")).as_deref(),
            Some("Synth Chat.exe")
        );
        // Not an exe after the switch: the launcher itself.
        assert_eq!(
            app_of(&link("--processStart")).as_deref(),
            Some("Update.exe")
        );
    }

    #[test]
    fn a_target_that_is_not_an_exe_is_no_app() {
        let link = |target: &str| Link {
            target: target.into(),
            args: String::new(),
            icon: String::new(),
            icon_index: 0,
        };
        assert_eq!(app_of(&link(r"C:\Synth\readme.txt")), None);
        assert_eq!(app_of(&link(r"C:\Synth\folder")), None);
        assert_eq!(
            app_of(&link("%ProgramFiles%\\Synth\\synth.EXE")).as_deref(),
            Some("synth.EXE")
        );
    }

    #[test]
    fn garbage_and_cut_links_are_refused() {
        assert_eq!(parse(b""), None);
        assert_eq!(parse(&[0u8; 200]), None);
        let full = link_bytes(r"C:\Synth\a.exe", "--x");
        for len in [0x4C, 0x50, 0x60, full.len() - 6] {
            assert_eq!(parse(&full[..len]), None, "cut at {len}");
        }
    }

    #[test]
    fn the_environment_block_names_the_target_without_link_info() {
        // Header with HasExpString and IsUnicode only, then the extra data block.
        let mut bytes = link_bytes("x", "")[..HEADER_SIZE].to_vec();
        bytes[0x14..0x18].copy_from_slice(&(IS_UNICODE | 1 << 9).to_le_bytes());
        let mut block = vec![0u8; 0x314];
        block[0..4].copy_from_slice(&0x314u32.to_le_bytes());
        block[4..8].copy_from_slice(&ENV_BLOCK.to_le_bytes());
        let target: Vec<u8> = "%ProgramFiles%\\Synth\\synth.exe"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        block[268..268 + target.len()].copy_from_slice(&target);
        bytes.extend(block);
        bytes.extend(0u32.to_le_bytes());
        let link = parse(&bytes).unwrap();
        assert_eq!(app_of(&link).as_deref(), Some("synth.exe"));
    }

    #[test]
    fn find_dedupes_by_exe_skips_uninstallers_and_sorts_by_name() {
        let menu = dir("menu");
        let desk = dir("desk");
        fs::create_dir_all(menu.join("Synth Suite")).unwrap();
        write(
            &menu.join("Synth Suite"),
            "Zeta Synth",
            r"C:\S\zeta.exe",
            "",
        );
        write(
            &menu.join("Synth Suite"),
            "Uninstall Zeta",
            r"C:\S\zeta-remove.exe",
            "",
        );
        write(&menu, "Alpha Synth", r"C:\S\unins000.exe", "");
        write(
            &menu,
            "Synth Chat",
            r"C:\S\Update.exe",
            "--processStart SynthChat.exe",
        );
        write(&menu, "Synth Manual", r"C:\S\manual.pdf", "");
        // The desktop copy of an app already in the menu, in other case: the menu's name wins.
        write(&desk, "Chat on desktop", r"C:\S\SYNTHCHAT.EXE", "");
        write(&desk, "beta synth", r"C:\S\beta.exe", "");
        fs::write(desk.join("broken.lnk"), b"not a link").unwrap();
        let apps = find(&[menu, desk, PathBuf::from(r"C:\no\such\synth\folder")]);
        let names: Vec<(&str, &str)> = apps
            .iter()
            .map(|a| (a.name.as_str(), a.exe.as_str()))
            .collect();
        assert_eq!(
            names,
            [
                ("beta synth", "beta.exe"),
                ("Synth Chat", "SynthChat.exe"),
                ("Zeta Synth", "zeta.exe"),
            ]
        );
    }

    #[test]
    fn nothing_found_is_an_empty_list() {
        assert_eq!(find(&[dir("empty")]), []);
        assert_eq!(find(&[]), []);
    }

    #[test]
    fn environment_names_expand_and_unknown_ones_stay() {
        let windir = std::env::var("SystemRoot").unwrap();
        assert_eq!(
            expand(r"%SystemRoot%\notepad.exe"),
            format!(r"{windir}\notepad.exe")
        );
        assert_eq!(expand(r"%NO_SUCH_SYNTH_VAR%\a"), r"%NO_SUCH_SYNTH_VAR%\a");
        assert_eq!(expand("100% %%"), "100% %%");
    }

    #[test]
    fn old_icons_without_alpha_are_opaque() {
        assert_eq!(
            rgba_of(vec![1, 2, 3, 0, 4, 5, 6, 0]),
            [3, 2, 1, 255, 6, 5, 4, 255]
        );
        assert_eq!(
            rgba_of(vec![1, 2, 3, 0, 4, 5, 6, 9]),
            [3, 2, 1, 0, 6, 5, 4, 9]
        );
    }

    #[test]
    fn scan_reads_the_icon_of_a_synthetic_link_and_survives_a_missing_target() {
        let menu = dir("scan");
        write(&menu, "Synth Missing", r"C:\no\such\synth.exe", "");
        write(&menu, "Synth Pad", r"C:\Windows\System32\notepad.exe", "");
        // Forward slashes, as a joined test path may have them.
        let root = PathBuf::from(menu.to_string_lossy().replace('\\', "/"));
        let apps = scan(&[root]);
        assert_eq!(apps.len(), 2);
        for app in &apps {
            if let Some(icon) = &app.icon {
                assert_eq!(
                    icon.rgba.len(),
                    usize::try_from(icon.width * icon.height * 4).unwrap()
                );
            }
        }
        let pad = apps[1].icon.as_ref().expect("notepad has an icon");
        assert!(pad.width >= 16 && pad.rgba.as_chunks::<4>().0.iter().any(|p| p[3] > 0));
    }

    #[test]
    fn a_numbered_icon_is_taken_from_its_dll() {
        let dll = PathBuf::from(std::env::var("SystemRoot").unwrap()).join(r"System32\shell32.dll");
        let at = |index| icon(&IconSource::Resource(dll.clone(), index));
        let (first, other) = (at(0).expect("icon 0"), at(3).expect("icon 3"));
        assert_ne!(first.rgba, other.rgba, "the index was ignored");
        assert!(at(100_000).is_none());
    }

    /// `C:` is the only fixed drive.
    fn local(path: &str) -> bool {
        is_local_path(path, &mut |l| l == b'C')
    }

    #[test]
    fn only_an_absolute_path_on_a_fixed_drive_is_local() {
        for ok in [r"C:\x", r"C:\Synth\a.exe", "c:/synth/a.exe", r"C:/x\y"] {
            assert!(local(ok), "{ok}");
        }
        for bad in [
            r"\\host\share\a.exe",
            r"\\?\UNC\host\share\a.exe",
            r"\\?\C:\a.exe",
            r"\\.\pipe\synth",
            r"\\.\C:\a.exe",
            "//host/share/a.exe",
            r"\/host\share",
            r"/\host\share",
            r"D:\x",
            r"Z:\a.exe",
            r"\x",
            "/x",
            "a.exe",
            r"sub\a.exe",
            r"C:",
            r"C:a.exe",
            r"%ProgramFiles%\a.exe",
            "",
        ] {
            assert!(!local(bad), "{bad:?}");
        }
    }

    #[test]
    fn a_drive_is_asked_of_windows_once() {
        let mut drives = Drives::default();
        let system = std::env::var("SystemDrive").unwrap().as_bytes()[0].to_ascii_uppercase();
        assert!(drives.fixed(system), "the system drive is a local disk");
        assert_eq!(drives.0.iter().flatten().count(), 1);
        assert!(drives.fixed(system));
    }

    /// Icon decisions with no disk and no Windows: only `C:` is fixed, `exists` names the files.
    fn source(link: &Link, exists: &[&str]) -> Option<IconSource> {
        icon_source(link, &mut |l| l == b'C', |p| {
            exists.iter().any(|e| Path::new(e) == p)
        })
    }

    fn link_to(target: &str, icon: &str, icon_index: i32) -> Link {
        Link {
            target: target.into(),
            args: String::new(),
            icon: icon.into(),
            icon_index,
        }
    }

    #[test]
    fn a_remote_target_or_icon_is_never_looked_up() {
        // `is_file` would say yes to everything: a lookup of a remote path would show as an icon.
        let lookup = |link: &Link| icon_source(link, &mut |l| l == b'C', |_| true);
        assert_eq!(lookup(&link_to(r"\\host\share\a.exe", "", 0)), None);
        assert_eq!(lookup(&link_to(r"\\?\UNC\host\s\a.exe", "", 0)), None);
        // A remote icon file with a local target: no icon, not the target's.
        assert_eq!(
            lookup(&link_to(
                r"C:\Synth\a.exe",
                r"\\host\%NO_SYNTH_VAR%\x.ico",
                0
            )),
            None
        );
        // A local icon with a remote target: the shortcut is not handed to the shell either.
        assert_eq!(
            lookup(&link_to(r"\\host\share\a.exe", r"C:\Synth\x.ico", 0)),
            None
        );
        // A mapped or disconnected drive.
        assert_eq!(lookup(&link_to(r"Z:\a.exe", "", 0)), None);
        assert!(lookup(&link_to(r"C:\a.exe", "", 0)).is_some());
        // Local paths that exist nowhere: no icon, and never the `.lnk` for the shell to read.
        assert_eq!(source(&link_to(r"C:\a.exe", r"C:\i.ico", 0), &[]), None);
    }

    #[test]
    fn a_local_icon_is_the_file_and_its_number_else_the_target_else_none() {
        let (dll, exe) = (r"C:\Synth\res.dll", r"C:\Synth\a.exe");
        let dll_link = link_to(exe, dll, 7);
        assert_eq!(
            source(&dll_link, &[dll, exe]),
            Some(IconSource::Resource(dll.into(), 7))
        );
        // The icon file is gone: the target's own first icon.
        assert_eq!(
            source(&dll_link, &[exe]),
            Some(IconSource::Resource(exe.into(), 0))
        );
        assert_eq!(source(&dll_link, &[]), None);
    }

    #[test]
    fn the_icon_number_is_read_from_the_link() {
        let link = parse(&link_bytes_with_icon(
            r"C:\Synth\a.exe",
            "--x",
            r"C:\Synth\res.dll",
            7,
        ))
        .unwrap();
        assert_eq!(
            (link.icon.as_str(), link.icon_index, link.args.as_str()),
            (r"C:\Synth\res.dll", 7, "--x")
        );
        let by_id = parse(&link_bytes_with_icon(r"C:\a.exe", "", "i.dll", -105)).unwrap();
        assert_eq!(by_id.icon_index, -105);
        assert_eq!(parse(&link_bytes(r"C:\a.exe", "")).unwrap().icon_index, 0);
    }

    #[test]
    fn a_synthetic_link_to_a_unc_path_is_an_app_without_an_icon() {
        let menu = dir("unc");
        fs::write(
            menu.join("Synth Remote.lnk"),
            link_bytes_with_icon(
                r"\\synth-host.invalid\share\a.exe",
                "",
                r"\\synth-host.invalid\x.ico",
                2,
            ),
        )
        .unwrap();
        write(&menu, "Synth Local", r"C:\no\such\synth\b.exe", "");
        let apps = find(&[menu]);
        let sources: Vec<(&str, bool)> = apps
            .iter()
            .map(|a| (a.exe.as_str(), a.icon_source.is_some()))
            .collect();
        assert_eq!(sources, [("b.exe", false), ("a.exe", false)]);
    }

    #[test]
    fn a_desktop_is_read_at_its_top_and_the_start_menu_below_its_subfolders() {
        let menu = dir("depth-menu");
        let desk = dir("depth-desk");
        for root in [&menu, &desk] {
            write(root, "Top", r"C:\S\top.exe", "");
            fs::create_dir_all(root.join("Sub").join("Deeper")).unwrap();
            write(&root.join("Sub"), "Sub", r"C:\S\sub.exe", "");
            write(
                &root.join("Sub").join("Deeper"),
                "Deeper",
                r"C:\S\deeper.exe",
                "",
            );
        }
        let exes = |apps: Vec<App>| -> Vec<String> { apps.into_iter().map(|a| a.exe).collect() };
        assert_eq!(
            exes(find_in(
                std::slice::from_ref(&desk),
                std::slice::from_ref(&desk)
            )),
            ["top.exe"]
        );
        assert_eq!(
            exes(find_in(
                std::slice::from_ref(&menu),
                std::slice::from_ref(&desk)
            )),
            ["deeper.exe", "sub.exe", "top.exe"]
        );
        // A folder below the limit is not entered.
        let mut deep = menu.join("Beyond");
        for i in 0..=MAX_DEPTH {
            deep = deep.join(format!("d{i}"));
        }
        fs::create_dir_all(&deep).unwrap();
        write(&deep, "Beyond", r"C:\S\beyond.exe", "");
        assert!(!exes(find_in(&[menu], &[])).contains(&"beyond.exe".to_owned()));
    }

    #[test]
    fn a_tree_of_empty_folders_stops_at_the_folder_cap() {
        let menu = dir("dirs");
        for i in 0..MAX_DIRS + 5 {
            fs::create_dir(menu.join(format!("d{i:05}"))).unwrap();
        }
        write(&menu.join("d00000"), "First", r"C:\S\first.exe", "");
        write(
            &menu.join(format!("d{:05}", MAX_DIRS + 4)),
            "Last",
            r"C:\S\last.exe",
            "",
        );
        let apps = find(&[menu]);
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].exe, "first.exe");
    }

    #[test]
    fn a_relative_target_is_read_from_the_shortcuts_folder() {
        let base = Path::new(r"C:\ProgramData\Menu\Programs");
        assert_eq!(
            absolutize(base, r"..\..\..\Program Files (x86)\Synth\a.exe"),
            r"C:\Program Files (x86)\Synth\a.exe"
        );
        assert_eq!(
            absolutize(base, r".\sub\a.exe"),
            r"C:\ProgramData\Menu\Programs\sub\a.exe"
        );
        // Above the drive stops at it.
        assert_eq!(absolutize(base, r"..\..\..\..\..\..\a.exe"), r"C:\a.exe");
        for kept in [r"D:\a.exe", r"\\host\s\a.exe", "%SystemRoot%\\a.exe", ""] {
            assert_eq!(absolutize(base, kept), kept);
        }
    }

    #[test]
    fn a_relative_shortcut_finds_its_exe_and_icon() {
        let root = dir("relative");
        fs::create_dir_all(root.join("menu")).unwrap();
        fs::create_dir_all(root.join("bin")).unwrap();
        fs::write(root.join(r"bin\rel.exe"), b"MZ").unwrap();
        write(&root.join("menu"), "Synth Rel", r"..\bin\rel.exe", "");
        let apps = find(&[root.join("menu")]);
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].exe, "rel.exe");
        let Some(IconSource::Resource(file, 0)) = &apps[0].icon_source else {
            panic!("no icon source: {:?}", apps[0].icon_source);
        };
        assert_eq!(file, &root.join(r"bin\rel.exe"));
    }

    #[test]
    fn a_shortcut_with_arguments_to_a_host_is_no_app_and_a_plain_one_wins() {
        let menu = dir("hosts");
        write(
            &menu,
            "Synth Error log",
            r"C:\Windows\notepad.exe",
            r"C:\x\a.err",
        );
        write(&menu, "Synth Notepad", r"C:\Windows\notepad.exe", "");
        write(
            &menu,
            "Synth Applet",
            r"C:\Windows\control.exe",
            "/name Synth.Applet",
        );
        write(
            &menu,
            "Synth Folder",
            r"C:\Windows\explorer.exe",
            r"C:\Synth\",
        );
        write(&menu, "Synth Tool", r"C:\S\tool.exe", "--flag");
        let names: Vec<_> = find(&[menu]).into_iter().map(|a| a.name).collect();
        assert_eq!(names, ["Synth Notepad", "Synth Tool"]);
    }

    #[test]
    fn a_store_shortcut_names_its_app_id() {
        let link = |target: &str, args: &str| Link {
            target: target.into(),
            args: args.into(),
            ..Link::default()
        };
        let store = link(
            r"C:\Windows\explorer.exe",
            r"shell:AppsFolder\Synth_abc123!SynthApp",
        );
        assert_eq!(aumid_of(&store), Some("Synth_abc123!SynthApp"));
        assert_eq!(
            aumid_of(&link(r"C:\Synth\a.exe", r"shell:AppsFolder\X!Y")),
            None
        );
        assert_eq!(
            aumid_of(&link(r"C:\Windows\explorer.exe", r"C:\Synth\")),
            None
        );
        assert_eq!(
            aumid_of(&link(r"C:\Windows\explorer.exe", r"shell:AppsFolder\")),
            None
        );
    }

    #[test]
    fn the_manifest_gives_the_exe_of_the_application_by_id() {
        let xml = r#"<Package><Applications>
            <Application Id="Other" Executable="x\other.exe" EntryPoint="a">
            <Application Id="SynthApp" Executable="app\Synth.exe" EntryPoint="b">
            </Applications></Package>"#;
        assert_eq!(manifest_exe(xml, "SynthApp").as_deref(), Some("Synth.exe"));
        assert_eq!(manifest_exe(xml, "Other").as_deref(), Some("other.exe"));
        assert_eq!(manifest_exe(xml, "Missing"), None);
        assert_eq!(manifest_exe("", "SynthApp"), None);
    }

    #[test]
    fn a_long_registry_value_is_cut_at_the_buffer_not_past_it() {
        let data: Vec<u16> = "Synth App".encode_utf16().chain([0, 7, 7]).collect();
        assert_eq!(utf16_text(&data, 20).as_deref(), Some("Synth App"));
        // The size `RegGetValueW` reports for a value longer than the buffer: no panic.
        assert_eq!(utf16_text(&data[..3], 4000).as_deref(), Some("Syn"));
        assert_eq!(utf16_text(&data, 0), None);
        assert_eq!(utf16_text(&[0, 0], 4), None);
        assert_eq!(utf16_text(&[97; 3000], 6002), Some("a".repeat(3000)));
    }

    #[test]
    fn installers_and_package_cache_exes_are_no_apps() {
        let is = |path: &str| {
            let path = path.to_lowercase();
            is_installer(path.rsplit('\\').next().unwrap(), &path)
        };
        assert!(is(
            r"C:\ProgramData\Package Cache\{a}\python-3.14.10-amd64.exe"
        ));
        assert!(is(r"C:\Windows\Installer\{a}\icon.exe"));
        assert!(is(r"C:\x\winsdksetup.exe"));
        assert!(is(r"C:\x\Docker Desktop Installer.exe"));
        assert!(is(r"C:\x\vc_redist.x64.exe"));
        assert!(!is(r"C:\Program Files\Synth\Synth.exe"));
    }

    #[test]
    fn the_shell_lists_store_apps_with_an_exe() {
        // Read-only: the shell's own folder, then each package manifest.
        let mut drives = Drives::default();
        with_ole(|| {
            for (app, _) in store_apps(&mut drives) {
                assert!(app.exe.to_lowercase().ends_with(".exe"), "{}", app.name);
                assert_ne!(app.name, "");
            }
        });
    }

    #[test]
    fn a_display_icon_splits_into_file_and_number() {
        let split = |s: &str| split_display_icon(s);
        assert_eq!(split(r"C:\a\a.exe,3"), (r"C:\a\a.exe".to_owned(), 3));
        assert_eq!(
            split(r#""C:\a b\a.exe",-5"#),
            (r"C:\a b\a.exe".to_owned(), -5)
        );
        assert_eq!(split(r#""C:\a b\a.exe""#), (r"C:\a b\a.exe".to_owned(), 0));
        assert_eq!(split(r"C:\a.exe"), (r"C:\a.exe".to_owned(), 0));
    }

    #[test]
    fn the_registry_is_read_and_listed_apps_are_real_exes() {
        // The Windows key is on every machine and its subkeys are many; a missing one is none.
        assert!(reg_subkeys(HKEY_LOCAL_MACHINE, UNINSTALL).len() > 1);
        assert_eq!(
            reg_subkeys(HKEY_LOCAL_MACHINE, r"SOFTWARE\Synth\No\Such\Key").len(),
            0
        );
        let mut drives = Drives::default();
        for (app, _) in registry_apps(&mut drives) {
            let Some(IconSource::Resource(file, _)) = &app.icon_source else {
                panic!("{} has no file", app.name);
            };
            assert!(file.is_file(), "{}: {file:?}", app.name);
            assert!(app.exe.to_lowercase().ends_with(".exe"));
        }
    }
}
