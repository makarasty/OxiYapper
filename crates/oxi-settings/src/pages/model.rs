//! Model page: the curated models with their status on disk, download and cancel through the
//! daemon (the page itself never touches the network), "use my own .bin", the GPU and the idle
//! unload.

use std::cell::{Cell, RefCell};
use std::fs;
use std::path::Path;
use std::rc::{Rc, Weak as RcWeak};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;
use std::time::Duration;

use oxi_core::device::{Device, DeviceChoice};
use oxi_core::ipc::{DownloadResult, Push, Request};
use oxi_core::models::{self, MODELS, ModelEntry, ModelStatus};
use oxi_core::paths::part_path;
use slint::{ComponentHandle, Model, ModelRc, SharedString, Timer, TimerMode, VecModel, Weak};

use crate::debounce::SaveDebounce;
use crate::generated::{ModelRow, ModelState};
use crate::ipc::{Event, Link};
use crate::{AppWindow, Ctx, Page, report};

/// `[model] id` of "use my own .bin".
const CUSTOM: &str = "custom";
/// How often the page looks for the answer of a path check running on its worker.
const POLL: Duration = Duration::from_millis(25);

pub fn bind(window: &AppWindow, ctx: &Ctx) {
    let s = window.global::<ModelState>();
    let entries: Vec<&'static ModelEntry> = MODELS.iter().filter(|m| m.id != "tiny").collect();
    let dir = ctx.layout.models_dir();
    let rows: Vec<ModelRow> = entries.iter().map(|e| row(e, &dir)).collect();
    let (tx, rx) = channel();
    let page = Rc::new_cyclic(|me| PageState {
        me: me.clone(),
        window: window.as_weak(),
        ctx: ctx.clone(),
        entries,
        rows: Rc::new(VecModel::from(rows)),
        listed: RefCell::new(None),
        choices: RefCell::new(Vec::new()),
        idle_save: SaveDebounce::new(window.as_weak(), ctx.clone()),
        checks: Checks {
            latest: Cell::new(0),
            running: Cell::new(0),
            tx,
            rx,
            timer: Timer::default(),
        },
    });
    s.set_rows(ModelRc::from(page.rows.clone()));
    s.set_can_browse(cfg!(windows));
    s.set_warning(SharedString::new());
    page.push_config(&s);

    let p = Rc::clone(&page);
    s.on_shown(move || p.shown());
    let p = Rc::clone(&page);
    s.on_picked(move |i| p.picked(i));
    let p = Rc::clone(&page);
    s.on_download(move |i| p.download(i));
    let p = Rc::clone(&page);
    s.on_cancel(move |i| p.cancel(i));
    let p = Rc::clone(&page);
    s.on_custom_edited(move |text| p.custom_edited(&text));
    let p = Rc::clone(&page);
    s.on_browse(move || p.browse());
    let p = Rc::clone(&page);
    s.on_gpu_picked(move |i| p.gpu_picked(i));
    let p = Rc::clone(&page);
    s.on_idle_changed(move |v| p.idle_changed(v));

    // Held weakly: a rebind (Reset) replaces the callbacks above, which drops this page, so the
    // old subscriber goes quiet.
    // shortcut: `Ipc` has no unsubscribe, so each Reset leaves one dead closure behind.
    let weak = Rc::downgrade(&page);
    ctx.ipc.subscribe(move |event| {
        if let Some(page) = weak.upgrade() {
            page.event(event);
        }
    });
    // Reset to defaults rebinds while the page may be open; its `init` does not run again.
    if window.get_page() == Page::Model {
        page.shown();
    }
}

struct PageState {
    me: RcWeak<Self>,
    window: Weak<AppWindow>,
    ctx: Ctx,
    /// The registry models the page offers, in row order.
    entries: Vec<&'static ModelEntry>,
    rows: Rc<VecModel<ModelRow>>,
    /// The daemon's GPU list; `None` until it answers `ListDevices`.
    listed: RefCell<Option<Vec<Device>>>,
    /// What each GPU picker row writes.
    choices: RefCell<Vec<DeviceChoice>>,
    /// The idle slider saves once it rests.
    idle_save: SaveDebounce,
    checks: Checks,
}

/// A path's existence check, run on a worker thread: a UNC path to an offline host can block
/// for a long time, and the window must not freeze with it.
struct Checks {
    /// Id of the newest check; an older answer is dropped.
    latest: Cell<u64>,
    /// Workers not answered yet; the poll timer runs while it is above zero.
    running: Cell<usize>,
    tx: Sender<Checked>,
    rx: Receiver<Checked>,
    timer: Timer,
}

struct Checked {
    id: u64,
    path: String,
    problem: Option<&'static str>,
    /// From an edit: a valid path is then saved and picks "use my own .bin".
    save: bool,
}

impl PageState {
    fn state(&self, f: impl FnOnce(&ModelState<'_>)) {
        if let Some(window) = self.window.upgrade() {
            f(&window.global::<ModelState>());
        }
    }

    /// Ask the daemon for its GPUs and warnings; dropped while it is not connected.
    fn ask(&self) {
        self.ctx.ipc.send(Request::ListDevices);
        self.ctx.ipc.send(Request::Status);
    }

    /// The page was opened: read the files again (a model may have been copied in) and ask.
    fn shown(&self) {
        self.refresh_rows();
        self.state(|s| self.push_config(s));
        self.ask();
    }

    /// Every config value the page shows.
    fn push_config(&self, s: &ModelState<'_>) {
        let file = self.ctx.config.borrow();
        let model = &file.config.model;
        let selected = if model.id == CUSTOM {
            Some(self.entries.len())
        } else {
            self.entries.iter().position(|e| e.id == model.id)
        };
        s.set_selected(selected.map_or(-1, int));
        s.set_custom_path(model.custom_path.as_str().into());
        #[expect(clippy::cast_precision_loss, reason = "minutes are far below 2^24")]
        s.set_idle_minutes(model.idle_unload_minutes as f32);
        let path = model.custom_path.clone();
        drop(file);
        self.check_custom(&path, false);
        self.push_gpus(s);
    }

    fn push_gpus(&self, s: &ModelState<'_>) {
        let current = self.ctx.config.borrow().config.model.gpu.clone();
        let (labels, choices) = gpu_rows(self.listed.borrow().as_deref(), &current);
        let index = choices.iter().position(|c| *c == current).unwrap_or(0);
        s.set_gpus(ModelRc::from(Rc::new(VecModel::from(labels))));
        s.set_gpu(int(index));
        *self.choices.borrow_mut() = choices;
    }

    fn update(&self, i: usize, f: impl FnOnce(&mut ModelRow)) {
        if let Some(mut r) = self.rows.row_data(i) {
            f(&mut r);
            self.rows.set_row_data(i, r);
        }
    }

    fn index_of(&self, id: &str) -> Option<usize> {
        self.entries.iter().position(|e| e.id == id)
    }

    fn picked(&self, i: i32) {
        let Ok(i) = usize::try_from(i) else { return };
        let id = match self.entries.get(i) {
            Some(entry) => entry.id,
            None if i == self.entries.len() => CUSTOM,
            None => return,
        };
        id.clone_into(&mut self.ctx.config.borrow_mut().config.model.id);
        self.state(|s| self.push_config(s));
        report(&self.window, self.ctx.save());
    }

    // shortcut: the row waits for the daemon's DownloadProgress pushes; the daemon does not
    // route DownloadModel and CancelDownload yet (another task), so Download shows nothing more
    // than "Starting the download" until it does.
    fn download(&self, i: i32) {
        let Some((i, entry)) = self.entry(i) else {
            return;
        };
        if self.ctx.ipc.link() != Link::Online {
            self.update(i, |r| r.error = "Start OxiYapper to download models".into());
            return;
        }
        self.ctx.ipc.send(Request::DownloadModel {
            id: entry.id.to_owned(),
        });
        self.update(i, |r| {
            r.downloading = true;
            r.progress = 0.0;
            r.status = "Starting the download".into();
            r.error = SharedString::new();
        });
    }

    /// The row stays in "downloading" until the daemon's `DownloadDone` says it stopped.
    fn cancel(&self, i: i32) {
        if let Some((_, entry)) = self.entry(i) {
            self.ctx.ipc.send(Request::CancelDownload {
                id: entry.id.to_owned(),
            });
        }
    }

    fn entry(&self, i: i32) -> Option<(usize, &'static ModelEntry)> {
        let i = usize::try_from(i).ok()?;
        Some((i, *self.entries.get(i)?))
    }

    /// A valid path is saved and picks "use my own .bin"; anything else only shows why not.
    fn custom_edited(&self, text: &str) {
        self.check_custom(clean(text), true);
    }

    /// Show why `path` cannot be a custom model. What the text alone decides shows at once;
    /// the file itself is looked at on a worker and shown (and saved, with `save`) when it
    /// answers.
    fn check_custom(&self, path: &str, save: bool) {
        let id = self.checks.latest.get() + 1;
        self.checks.latest.set(id);
        if let Some(problem) = shape_problem(path) {
            // An empty field is only an error while "use my own .bin" is picked.
            let custom = self.ctx.config.borrow().config.model.id == CUSTOM;
            let note = if custom || !path.is_empty() {
                problem
            } else {
                ""
            };
            self.state(|s| s.set_custom_error(note.into()));
            return;
        }
        self.state(|s| s.set_custom_error(SharedString::new()));
        let (tx, path) = (self.checks.tx.clone(), path.to_owned());
        self.checks.running.set(self.checks.running.get() + 1);
        thread::spawn(move || {
            let problem = custom_problem(&path);
            // The page is gone when the window closed first.
            let _ = tx.send(Checked {
                id,
                path,
                problem,
                save,
            });
        });
        let me = self.me.clone();
        self.checks.timer.start(TimerMode::Repeated, POLL, move || {
            if let Some(page) = me.upgrade() {
                page.poll();
            }
        });
    }

    fn poll(&self) {
        while let Ok(answer) = self.checks.rx.try_recv() {
            self.checks
                .running
                .set(self.checks.running.get().saturating_sub(1));
            if answer.id == self.checks.latest.get() {
                self.checked(&answer);
            }
        }
        if self.checks.running.get() == 0 {
            self.checks.timer.stop();
        }
    }

    fn checked(&self, answer: &Checked) {
        if answer.problem.is_none() && answer.save {
            let mut file = self.ctx.config.borrow_mut();
            answer
                .path
                .as_str()
                .clone_into(&mut file.config.model.custom_path);
            CUSTOM.clone_into(&mut file.config.model.id);
            drop(file);
            report(&self.window, self.ctx.save());
            self.state(|s| s.set_selected(int(self.entries.len())));
        }
        self.state(|s| s.set_custom_error(answer.problem.unwrap_or_default().into()));
    }

    #[cfg(windows)]
    fn browse(&self) {
        let picked = rfd::FileDialog::new()
            .add_filter("Whisper model", &["bin"])
            .pick_file();
        if let Some(path) = picked {
            let text = path.to_string_lossy().into_owned();
            self.state(|s| s.set_custom_path(text.as_str().into()));
            self.custom_edited(&text);
        }
    }

    /// No native dialog on this OS: `can-browse` hides the button.
    #[cfg(not(windows))]
    fn browse(&self) {}

    fn gpu_picked(&self, i: i32) {
        let choice = usize::try_from(i)
            .ok()
            .and_then(|i| self.choices.borrow().get(i).cloned());
        if let Some(choice) = choice {
            self.ctx.config.borrow_mut().config.model.gpu = choice;
            report(&self.window, self.ctx.save());
        }
    }

    fn idle_changed(&self, minutes: f32) {
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "the slider runs 0..60"
        )]
        let minutes = minutes.round().max(0.0) as u32;
        self.ctx
            .config
            .borrow_mut()
            .config
            .model
            .idle_unload_minutes = minutes;
        self.idle_save.touch();
    }

    fn event(&self, event: &Event) {
        match event {
            Event::Link(Link::Online) => self.ask(),
            // A lost daemon takes its downloads with it.
            Event::Link(_) => {
                for i in 0..self.rows.row_count() {
                    self.update(i, |r| r.downloading = false);
                }
                self.refresh_rows();
            }
            Event::Push(Push::Devices { gpus, .. }) => {
                *self.listed.borrow_mut() = Some(gpus.clone());
                self.state(|s| self.push_gpus(s));
            }
            Event::Push(Push::Status { warnings, .. }) => {
                self.state(|s| s.set_warning(warnings.join("\n").into()));
            }
            Event::Push(Push::DownloadProgress { id, done, total }) => {
                if let Some(i) = self.index_of(id) {
                    self.update(i, |r| {
                        r.downloading = true;
                        r.progress = fraction(*done, *total);
                        r.status = format!("Downloading {} of {}", gb(*done), gb(*total)).into();
                    });
                }
            }
            Event::Push(Push::DownloadDone { id, result }) => {
                if let Some(i) = self.index_of(id) {
                    self.update(i, |r| {
                        r.downloading = false;
                        r.error = match result {
                            DownloadResult::Failed { message } => message.as_str().into(),
                            DownloadResult::Ok | DownloadResult::Cancelled => SharedString::new(),
                        };
                    });
                    self.refresh_rows();
                }
            }
            Event::Push(_) => {}
        }
    }

    /// Status text of the rows not downloading, from the files.
    fn refresh_rows(&self) {
        let dir = self.ctx.layout.models_dir();
        for (i, entry) in self.entries.iter().enumerate() {
            self.update(i, |r| {
                if !r.downloading {
                    let fresh = row(entry, &dir);
                    r.status = fresh.status;
                    r.can_download = fresh.can_download;
                }
            });
        }
    }
}

fn row(entry: &ModelEntry, dir: &Path) -> ModelRow {
    let status = models::status(entry, dir);
    // A cancelled or cut-off download leaves its `.part`; the next download resumes from it.
    let paused = status == ModelStatus::Missing
        && fs::metadata(part_path(&dir.join(entry.file_name))).is_ok_and(|m| m.len() > 0);
    ModelRow {
        id: entry.id.into(),
        name: entry.display_name.into(),
        size: gb(entry.size).into(),
        status: if paused {
            "Paused, Download resumes".into()
        } else {
            status_text(status).into()
        },
        can_download: matches!(status, ModelStatus::Missing | ModelStatus::Bad),
        downloading: false,
        progress: 0.0,
        error: SharedString::new(),
    }
}

const fn status_text(status: ModelStatus) -> &'static str {
    match status {
        ModelStatus::Missing => "Not downloaded",
        ModelStatus::Ready => "Ready",
        ModelStatus::NeedsHash => "Downloaded, checked before first use",
        ModelStatus::Bad => "Damaged or incomplete, download it again",
    }
}

/// Decimal gigabytes with one digit, the unit Hugging Face shows.
fn gb(bytes: u64) -> String {
    #[expect(clippy::cast_precision_loss, reason = "one decimal digit is shown")]
    let gb = bytes as f64 / 1e9;
    format!("{gb:.1} GB")
}

fn fraction(done: u64, total: u64) -> f32 {
    if total == 0 {
        return 0.0;
    }
    #[expect(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        reason = "a bar position"
    )]
    let f = (done as f64 / total as f64).clamp(0.0, 1.0) as f32;
    f
}

fn int(i: usize) -> i32 {
    i32::try_from(i).unwrap_or(i32::MAX)
}

/// Explorer's "Copy as path" wraps the path in quotes.
fn clean(text: &str) -> &str {
    text.trim().trim_matches('"')
}

/// Why `path` cannot be a custom model, from the text alone; `None` when it may be.
fn shape_problem(path: &str) -> Option<&'static str> {
    if path.is_empty() {
        return Some("Choose a .bin model file");
    }
    let path = Path::new(path);
    if !path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("bin"))
    {
        return Some("The model file must end in .bin");
    }
    // The daemon resolves a relative path against its own working directory, not ours.
    if !path.is_absolute() {
        return Some("Use the full path to the file, from the drive letter");
    }
    None
}

/// Why `path` cannot be loaded as a custom model; `None` when it can. Reads the disk, which a
/// network path can stall: never on the UI thread.
fn custom_problem(path: &str) -> Option<&'static str> {
    if let Some(problem) = shape_problem(path) {
        return Some(problem);
    }
    match fs::metadata(path) {
        Ok(meta) if meta.is_file() => None,
        Ok(_) => Some("This is a folder, not a model file"),
        Err(_) => Some("File not found"),
    }
}

/// Picker labels and what each writes: Auto, CPU, the listed GPUs, then the configured GPU
/// when it is not listed, so it stays picked. Before the daemon answers (`listed` is `None`) a
/// configured name is not called missing.
fn gpu_rows(
    listed: Option<&[Device]>,
    current: &DeviceChoice,
) -> (Vec<SharedString>, Vec<DeviceChoice>) {
    let mut rows = vec![
        ("Auto".to_owned(), DeviceChoice::Auto),
        ("CPU".to_owned(), DeviceChoice::Cpu),
    ];
    for d in listed.unwrap_or_default() {
        let label = if d.description.is_empty() {
            d.name.clone()
        } else {
            format!("{} ({})", d.description, d.name)
        };
        rows.push((label, DeviceChoice::Named(d.name.clone())));
    }
    if let DeviceChoice::Named(name) = current
        && !rows.iter().any(|(_, c)| c == current)
    {
        let label = if listed.is_some() {
            format!("{name} (not connected)")
        } else {
            name.clone()
        };
        rows.push((label, current.clone()));
    }
    rows.into_iter()
        .map(|(l, c)| (SharedString::from(l), c))
        .unzip()
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::Receiver;

    use i_slint_backend_testing::{AccessibleRole, ElementHandle, mock_elapsed_time};
    use oxi_core::config::ConfigFile;
    use oxi_core::machine::{EngineWarning, Phase};
    use oxi_core::paths::Layout;
    use oxi_core::strings::engine_warning;

    use super::*;
    use crate::ipc::{Cmd, Ipc};

    /// The settings window on a fresh config dir, its Model page bound to a fake daemon link.
    fn window(name: &str) -> (AppWindow, Ipc, Receiver<Cmd>, Layout) {
        i_slint_backend_testing::init_no_event_loop();
        let dir = std::env::temp_dir().join(format!(
            "oxi-settings-lib-model-{}-{name}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let layout = Layout {
            config_dir: dir.clone(),
            data_dir: dir.clone(),
            logs_dir: dir,
        };
        let window = crate::open(layout.clone()).unwrap();
        let (ipc, sent) = Ipc::fake();
        let (mut ctx, _) = Ctx::load(layout.clone()).unwrap();
        ctx.ipc = ipc.clone();
        bind(&window, &ctx);
        window.show().unwrap();
        window.set_page(Page::Model);
        // Building the page runs its `init`, which asks the daemon.
        assert!(has(&window, "Use my own .bin"));
        (window, ipc, sent, layout)
    }

    fn with_role(window: &AppWindow, label: &str, role: AccessibleRole) -> ElementHandle {
        ElementHandle::find_by_accessible_label(window, label)
            .find(|e| e.accessible_role() == Some(role))
            .unwrap_or_else(|| panic!("no {role:?} labelled {label}"))
    }

    fn has(window: &AppWindow, label: &str) -> bool {
        ElementHandle::find_by_accessible_label(window, label)
            .next()
            .is_some()
    }

    /// Wait for the path worker: step the mocked clock (which runs the page's poll timer) until
    /// `ready`.
    fn wait_for(ready: impl Fn() -> bool) {
        for _ in 0..400 {
            mock_elapsed_time(POLL);
            if ready() {
                return;
            }
            thread::sleep(Duration::from_millis(5));
        }
        panic!("the path check did not answer");
    }

    fn requests(sent: &Receiver<Cmd>) -> Vec<Request> {
        sent.try_iter()
            .filter_map(|c| match c {
                Cmd::Send(r) => Some(r),
                Cmd::Start | Cmd::Restart => None,
            })
            .collect()
    }

    fn push(ipc: &Ipc, push: Push) {
        ipc.deliver(&Event::Push(push));
    }

    fn progress(id: &str, done: u64, total: u64) -> Push {
        Push::DownloadProgress {
            id: id.into(),
            done,
            total,
        }
    }

    fn done(id: &str, result: DownloadResult) -> Push {
        Push::DownloadDone {
            id: id.into(),
            result,
        }
    }

    fn download_large(window: &AppWindow) {
        with_role(window, "Download Whisper large-v3", AccessibleRole::Button)
            .invoke_accessible_default_action();
    }

    /// The large-v3 progress bar's value, `None` while it has none.
    fn bar(window: &AppWindow) -> Option<String> {
        ElementHandle::find_by_accessible_label(window, "Whisper large-v3 download")
            .find(|e| e.accessible_role() == Some(AccessibleRole::ProgressIndicator))
            .and_then(|e| e.accessible_value().map(String::from))
    }

    #[test]
    fn download_sends_its_id_and_progress_moves_the_bar() {
        let (window, ipc, sent, _) = window("progress");
        let asked = requests(&sent);
        assert!(asked.contains(&Request::ListDevices) && asked.contains(&Request::Status));

        download_large(&window);
        let want = Request::DownloadModel {
            id: "large-v3".into(),
        };
        assert_eq!(requests(&sent), [want]);
        assert_eq!(bar(&window).as_deref(), Some("0"));

        push(&ipc, progress("large-v3", 1_000_000_000, 4_000_000_000));
        assert_eq!(bar(&window).as_deref(), Some("0.25"));
        assert!(has(&window, "3.1 GB · Downloading 1.0 GB of 4.0 GB"));
        push(&ipc, progress("large-v3", 3_000_000_000, 4_000_000_000));
        assert_eq!(bar(&window).as_deref(), Some("0.75"));
        // Another model's progress leaves this bar alone.
        push(&ipc, progress("turbo", 1, 4));
        assert_eq!(bar(&window).as_deref(), Some("0.75"));
    }

    #[test]
    fn failed_download_shows_its_text() {
        let (window, ipc, _sent, _) = window("failed");
        let message = "connection reset (os error 10054)";
        push(&ipc, progress("large-v3", 5, 10));
        assert!(bar(&window).is_some());
        let failed = DownloadResult::Failed {
            message: message.into(),
        };
        push(&ipc, done("large-v3", failed));
        // The text is labelled with the model it belongs to.
        assert!(has(&window, &format!("Whisper large-v3: {message}")));
        assert_eq!(bar(&window), None);
        assert!(has(&window, "3.1 GB · Not downloaded"));
        // Trying again clears the old error.
        download_large(&window);
        assert!(!has(&window, &format!("Whisper large-v3: {message}")));
    }

    #[test]
    fn download_button_is_not_inside_the_radio() {
        let (window, _ipc, _sent, _) = window("nodes");
        let radio = with_role(&window, "Whisper large-v3", AccessibleRole::RadioButton);
        with_role(&window, "Download Whisper large-v3", AccessibleRole::Button);
        let inside = radio
            .query_descendants()
            .match_accessible_role(AccessibleRole::Button)
            .find_all();
        assert!(inside.is_empty(), "the Download button is inside the radio");
    }

    #[test]
    fn cancelled_download_with_a_part_file_shows_paused() {
        let (window, ipc, _sent, layout) = window("paused");
        push(&ipc, progress("large-v3", 5, 10));
        fs::create_dir_all(layout.models_dir()).unwrap();
        fs::write(layout.models_dir().join("ggml-large-v3.bin.part"), b"half").unwrap();
        push(&ipc, done("large-v3", DownloadResult::Cancelled));
        assert!(has(&window, "3.1 GB · Paused, Download resumes"));
        // A model with no `.part` stays "Not downloaded".
        assert!(has(&window, "1.6 GB · Not downloaded"));
        with_role(&window, "Download Whisper large-v3", AccessibleRole::Button);
    }

    #[test]
    fn relative_custom_path_is_refused_and_not_saved() {
        let (window, _ipc, _sent, layout) = window("relative");
        let field = with_role(&window, "Model file", AccessibleRole::TextInput);
        field.set_accessible_value("mine.bin".to_owned());
        assert!(has(&window, RELATIVE));
        assert!(!layout.config_file().exists(), "a relative path was saved");
    }

    #[test]
    fn an_answer_for_an_older_edit_is_dropped() {
        let (window, _ipc, _sent, layout) = window("stale");
        let good = layout.config_dir.join("good.bin");
        fs::write(&good, b"synthetic").unwrap();
        let field = with_role(&window, "Model file", AccessibleRole::TextInput);
        field.set_accessible_value(good.to_string_lossy().into_owned());
        field.set_accessible_value("notes.txt".to_owned());
        for _ in 0..20 {
            mock_elapsed_time(POLL);
            thread::sleep(Duration::from_millis(10));
        }
        assert!(has(&window, "The model file must end in .bin"));
        assert!(!layout.config_file().exists(), "the old answer was saved");
    }

    #[test]
    fn idle_slider_saves_once_it_rests() {
        let (window, _ipc, _sent, layout) = window("idle");
        let s = window.global::<ModelState>();
        s.invoke_idle_changed(12.0);
        s.invoke_idle_changed(30.0);
        assert!(!layout.config_file().exists(), "saved during the drag");
        mock_elapsed_time(Duration::from_millis(350));
        let text = fs::read_to_string(layout.config_file()).unwrap();
        let file = ConfigFile::parse(&text).unwrap();
        assert_eq!(file.config.model.idle_unload_minutes, 30);
    }

    #[test]
    fn cancel_waits_for_the_daemon_to_stop() {
        let (window, ipc, sent, _) = window("cancel");
        push(&ipc, progress("large-v3", 5, 10));
        let _ = requests(&sent);
        with_role(&window, "Cancel Whisper large-v3", AccessibleRole::Button)
            .invoke_accessible_default_action();
        let want = Request::CancelDownload {
            id: "large-v3".into(),
        };
        assert_eq!(requests(&sent), [want]);
        assert!(
            bar(&window).is_some(),
            "the bar went before the daemon stopped"
        );
        push(&ipc, done("large-v3", DownloadResult::Cancelled));
        assert_eq!(bar(&window), None);
        assert!(has(&window, "Download Whisper large-v3"));
    }

    #[test]
    fn lost_daemon_ends_the_downloads() {
        let (window, ipc, _sent, _) = window("lost");
        push(&ipc, progress("large-v3", 5, 10));
        ipc.deliver(&Event::Link(Link::Offline));
        assert_eq!(bar(&window), None);
        assert!(has(&window, "3.1 GB · Not downloaded"));
    }

    #[test]
    fn daemon_gpus_are_listed_by_name_and_picked() {
        let (window, ipc, _sent, layout) = window("gpus");
        let gpu = |name: &str, description: &str, uma| Device {
            name: name.into(),
            description: description.into(),
            uma,
        };
        let gpus = vec![
            gpu("Vulkan0", "NVIDIA GeForce RTX 4070 SUPER", false),
            gpu("Vulkan1", "AMD Radeon(TM) Graphics", true),
        ];
        push(
            &ipc,
            Push::Devices {
                mics: Vec::new(),
                gpus,
            },
        );
        let s = window.global::<ModelState>();
        let rows: Vec<SharedString> = s.get_gpus().iter().collect();
        let want = [
            "Auto",
            "CPU",
            "NVIDIA GeForce RTX 4070 SUPER (Vulkan0)",
            "AMD Radeon(TM) Graphics (Vulkan1)",
        ];
        assert_eq!(rows, want);
        s.invoke_gpu_picked(3);
        let text = fs::read_to_string(layout.config_file()).unwrap();
        let saved = ConfigFile::parse(&text).unwrap().config.model.gpu;
        assert_eq!(saved, DeviceChoice::Named("Vulkan1".into()));
    }

    #[test]
    fn status_warning_shows_the_cpu_note() {
        let (window, ipc, _sent, _) = window("warning");
        let note = engine_warning(&EngineWarning::CpuFallback {
            reason: "out of device memory".into(),
        });
        assert!(!has(&window, &note));
        let status = Push::Status {
            phase: Phase::Idle,
            model: "turbo".into(),
            device: DeviceChoice::Auto,
            backend: "cpu".into(),
            warnings: vec![note.clone()],
        };
        push(&ipc, status);
        assert!(has(&window, &note));
    }

    #[test]
    fn gpu_rows_keep_a_missing_choice() {
        type Case<'a> = (Option<&'a [Device]>, DeviceChoice, &'a [&'a str]);
        let listed = [Device {
            name: "Vulkan0".into(),
            description: String::new(),
            uma: false,
        }];
        let named = |n: &str| DeviceChoice::Named(n.into());
        let table: [Case<'_>; 5] = [
            (None, DeviceChoice::Auto, &["Auto", "CPU"]),
            (None, named("Vulkan1"), &["Auto", "CPU", "Vulkan1"]),
            (
                Some(&listed),
                DeviceChoice::Cpu,
                &["Auto", "CPU", "Vulkan0"],
            ),
            (Some(&listed), named("Vulkan0"), &["Auto", "CPU", "Vulkan0"]),
            (
                Some(&listed),
                named("Vulkan1"),
                &["Auto", "CPU", "Vulkan0", "Vulkan1 (not connected)"],
            ),
        ];
        for (listed, current, want) in table {
            let (labels, choices) = gpu_rows(listed, &current);
            assert_eq!(labels, want, "{current:?}");
            assert!(choices.contains(&current), "{current:?}");
        }
    }

    const RELATIVE: &str = "Use the full path to the file, from the drive letter";

    #[test]
    fn custom_path_problems() {
        let dir = std::env::temp_dir().join(format!(
            "oxi-settings-lib-model-{}-custom",
            std::process::id()
        ));
        fs::create_dir_all(dir.join("folder.bin")).unwrap();
        let path = |name: &str| dir.join(name).to_string_lossy().into_owned();
        fs::write(dir.join("mine.BIN"), b"not a model").unwrap();
        let (file, folder, missing) = (path("mine.BIN"), path("folder.bin"), path("gone.bin"));
        for (path, want) in [
            ("", Some("Choose a .bin model file")),
            (file.as_str(), None),
            (missing.as_str(), Some("File not found")),
            (folder.as_str(), Some("This is a folder, not a model file")),
            ("notes.txt", Some("The model file must end in .bin")),
            ("mine.bin", Some(RELATIVE)),
            ("models/mine.bin", Some(RELATIVE)),
        ] {
            assert_eq!(custom_problem(path), want, "{path:?}");
        }
        // The text alone decides these: no disk read on the UI thread.
        assert_eq!(shape_problem(&missing), None);
        assert_eq!(shape_problem("mine.bin"), Some(RELATIVE));
    }

    #[test]
    fn custom_path_answers_come_from_the_worker() {
        let (window, _ipc, _sent, layout) = window("worker");
        let field = with_role(&window, "Model file", AccessibleRole::TextInput);
        let missing = layout.config_dir.join("gone.bin");
        field.set_accessible_value(missing.to_string_lossy().into_owned());
        wait_for(|| has(&window, "File not found"));
        assert!(!layout.config_file().exists());

        let model = layout.config_dir.join("mine.bin");
        fs::write(&model, b"synthetic").unwrap();
        field.set_accessible_value(format!("\"{}\"", model.display()));
        wait_for(|| layout.config_file().exists());
        let text = fs::read_to_string(layout.config_file()).unwrap();
        let saved = ConfigFile::parse(&text).unwrap().config.model;
        assert_eq!(saved.id, "custom");
        assert_eq!(saved.custom_path, model.to_string_lossy());
        assert!(!has(&window, "File not found"));
        let radio = with_role(&window, "Use my own .bin", AccessibleRole::RadioButton);
        assert_eq!(radio.accessible_checked(), Some(true));
    }

    #[test]
    fn sizes_and_fractions() {
        assert_eq!(gb(1_624_555_275), "1.6 GB");
        assert_eq!(gb(0), "0.0 GB");
        assert!((fraction(1, 4) - 0.25).abs() < f32::EPSILON);
        assert!(fraction(1, 0).abs() < f32::EPSILON);
        assert!((fraction(9, 4) - 1.0).abs() < f32::EPSILON);
    }
}
