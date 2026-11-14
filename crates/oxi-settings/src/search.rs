//! Settings search (owner item 17, ia-map 3.7): one static table of the pages, their tabs and every
//! setting row and group title with its "i" text and the old or everyday words people search with.
//! A page that is not open does not exist in Slint, so nothing is read from the live window; the
//! test `every_row_of_the_pages_is_in_the_index` reads every row and group title of
//! `ui/pages/*.slint` and fails when one is missing here, sits on another tab, or its text differs.
//! Picking a match opens its page and tab and sets `Shell.found`: the row whose `search-key` it is
//! scrolls into view and flashes (layout.slint `Flash`).

use std::{cell::RefCell, rc::Rc};

use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use crate::{AppWindow, AudioState, ModelState, Page, Shell, SystemState, TextState};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Page,
    Tab,
    Row,
}

/// One searchable place. `title` and `tip` are the page's own text as written in the `.slint`
/// source (escapes and `{}` placeholders included; the search drops them).
struct Entry {
    kind: Kind,
    page: Page,
    /// The sub-tab (`-1` for a page entry; `0` on a page without tabs).
    tab: i32,
    title: &'static str,
    tip: &'static str,
    /// Old names and everyday words (ia-map 3.7, RT R9), ranked with the tab names.
    words: &'static str,
    /// The `search-key` that flashes; empty opens the tab only (a row that exists only in a state).
    key: &'static str,
}

const fn page(page: Page, title: &'static str, words: &'static str) -> Entry {
    Entry {
        kind: Kind::Page,
        page,
        tab: -1,
        title,
        tip: "",
        words,
        key: "",
    }
}

const fn tab(page: Page, tab: i32, title: &'static str, words: &'static str) -> Entry {
    Entry {
        kind: Kind::Tab,
        page,
        tab,
        title,
        tip: "",
        words,
        key: "",
    }
}

const fn row(
    page: Page,
    tab: i32,
    title: &'static str,
    tip: &'static str,
    words: &'static str,
) -> Entry {
    Entry {
        kind: Kind::Row,
        page,
        tab,
        title,
        tip,
        words,
        key: title,
    }
}

/// A row that exists only in a state (the per-app profile pane): search opens its tab.
const fn inner(
    page: Page,
    tab: i32,
    title: &'static str,
    tip: &'static str,
    words: &'static str,
) -> Entry {
    Entry {
        kind: Kind::Row,
        page,
        tab,
        title,
        tip,
        words,
        key: "",
    }
}

use Page::{Audio, History, Home, Hotkey, Model, Overlay, Paste, System, Test, Text};

/// The pages in rail order, each with its tabs and then its rows. A row added to, moved on or
/// renamed in `ui/pages/*.slint` fails `every_row_of_the_pages_is_in_the_index`, which prints the
/// line to paste here.
#[rustfmt::skip]
static ENTRIES: &[Entry] = &[
    page(Home, "Home", "start stats statistics streak activity"),

    page(Model, "Model", "whisper engine"),
    tab(Model, 0, "Models", "download delete gallery whisper turbo large tiny base small medium distil recommended"),
    tab(Model, 1, "Languages", "language auto"),
    tab(Model, 2, "Speed", "advanced engine performance"),
    tab(Model, 3, "Accuracy", "advanced quality mistakes errors"),
    row(Model, 0, "Models folder", r"Where the downloaded models are kept. The size counts the finished models; the free space is that of its drive.", "disk space size storage"),
    row(Model, 0, "Unfinished downloads", r"Downloads that stopped before the end. They are kept so the download goes on from there.", "partial downloads resume"),
    row(Model, 1, "Dictation language", "", "language auto russian english"),
    row(Model, 1, "Model for each language", r"When you dictate in a language listed here, its model runs. With Auto the model of the first row runs. An app rule with its own model uses that model; an app rule with its own language uses that language's row.", "model by language per language"),
    row(Model, 2, "Device and memory", "", "gpu ram"),
    row(Model, 2, "Run the model on", r"Where the model does its work. Auto picks the graphics card when there is one, else the processor. A graphics card is much faster; the processor works everywhere.", "run on gpu graphics card cpu processor device vulkan cuda"),
    row(Model, 2, "Free memory after", r"Takes the model out of memory when you have not dictated for this long. The next dictation then starts a little slower.", "unload after idle ram vram"),
    row(Model, 2, "Load the model at start", r"Loads the model when the app starts, so even the first dictation starts at once. Off: it loads when you first dictate.", "preload ready memory ram vram always"),
    row(Model, 2, "Speed tuning", r"For experts. Auto means the app decides: the grey value is what it uses now.", "recognition reset to auto"),
    row(Model, 2, "Processor use", r"How many processor threads transcription may use. Auto: 4 now, a graphics card does the heavy work. Max: all of them, the fastest, other programs may slow down. Half: half of them.", "cpu cores"),
    row(Model, 2, "Speed boost", r"A quicker way to compute (flash attention). Auto: on now, a graphics card is in use. Changing it loads the model again for the next dictation.", ""),
    row(Model, 3, "Accuracy tuning", r"For experts. Auto means the app decides: the grey value is what it uses now.", "filters reset to auto"),
    row(Model, 3, "Careful mode", r"Auto: Off, the quickest. On is slower and can be more exact. The number is how many wordings are compared before one is picked.", "speed or accuracy beam greedy decoding"),
    row(Model, 3, "Word variety", r"Auto: 0, the most predictable words. Varied and Very varied let Whisper try less likely words.", "temperature"),
    row(Model, 3, "Second try", r"Auto: on. A phrase Whisper is not sure of is tried again with a little more variety. The next two rows say when.", "retry doubtful segments fallback"),
    row(Model, 3, "Second try when confidence is low", r"Auto: -1.0. A phrase Whisper is less sure of than this is tried again. Stricter retries more phrases, Looser fewer.", "doubt check logprob threshold"),
    row(Model, 3, "Second try when a phrase repeats", r"Auto: 2.4. A phrase that repeats itself more than this is tried again. Stricter retries sooner, Looser later.", "repeat check compression threshold"),
    row(Model, 3, "Silence cut-off", r"Auto: 0.6. A phrase more likely to be silence than this is dropped. Keep more drops only near-certain silence, Cut more drops doubtful quiet too.", "no speech threshold"),
    row(Model, 3, "Skip non-speech sounds", r"Auto: off. On drops sounds written as [MUSIC] or (laughs).", "blank"),
    row(Model, 3, "Hint for the model", r"Words or a sentence Whisper reads before it listens, to set the style or the spelling. Your dictionary: its words alone. Custom: your text, then your dictionary. Reset to Auto keeps it.", "prompt initial prompt"),

    page(Hotkey, "Hotkey", "shortcut keys keyboard"),
    row(Hotkey, 0, "Shortcut", "", "hotkey keys"),
    row(Hotkey, 0, "Keys", r"Click keys or press them. Left and right modifiers both count.", "hotkey shortcut change"),
    row(Hotkey, 0, "Recording mode", "", "push to talk toggle hold press"),
    row(Hotkey, 0, "Cancel on another key", r"The key always reaches the app. Auto cancels only if you have not spoken yet.", ""),
    row(Hotkey, 0, "Esc cancels a recording", "", "escape"),
    row(Hotkey, 0, "Continue if pressed again within", r"Let go of the keys and press them again within this time, and the same dictation goes on. Off starts a new one.", "hold gap resume"),

    page(Audio, "Audio", "sound"),
    tab(Audio, 0, "Microphone", "mic input"),
    tab(Audio, 1, "Recording", "record"),
    tab(Audio, 2, "Sounds", "beep start stop text ready cancel error chime"),
    row(Audio, 0, "Device", "", "microphone mic input"),
    row(Audio, 0, "Remember for this microphone", r"Keeps the boost, noise and rumble settings, stereo side and speech sensitivity for each microphone, and brings them back when you pick it.", "per device headset laptop webcam profile"),
    row(Audio, 0, "Level", r"How loud your microphone is right now. Speak to see it move.", "test record 3 s meter check"),
    row(Audio, 0, "Microphone boost", r"Makes your voice louder or quieter in the recording Whisper hears. Record 3 s checks the level.", "gain volume quiet loud check suggestion"),
    row(Audio, 0, "Level bar sensitivity", r"Only changes how far the Level bar moves; the recording stays as it is.", "meter sensitivity"),
    row(Audio, 0, "Stereo side", r"Which side of a stereo microphone is used. Both averages them; pick one when the other is noisy or silent.", "channel mix left right"),
    row(Audio, 1, "Don't cut my words", r"Careful keeps quieter starts and ends and listens longer after you let go; Tight cuts closer. Custom: the two rows below set by hand.", "edges cut words careful tight preset tail first last word"),
    row(Audio, 1, "Speech sensitivity", r"How easily quiet speech is counted as speech. Raise it when short phrases are lost.", "vad voice detection"),
    row(Audio, 1, "Keep recording after release", r"Keeps listening this long after you let go, so the last word is not cut.", "tail after release"),
    row(Audio, 1, "Cut silence at the ends", r"Removes the quiet before and after you speak: fewer made-up words, a faster result.", "trim silence"),
    row(Audio, 1, "Even out loudness", r"Makes quiet and loud recordings equally loud before Whisper hears them.", "normalise normalize level volume"),
    row(Audio, 1, "Reduce background noise", r"Removes steady noise like a fan or hum. Light and Medium keep part of the original sound; Auto cleans only when the room is noisy.", "noise suppression denoise strength light medium strong auto"),
    row(Audio, 1, "Cut low rumble", r"Removes deep hum and thumps below the voice: fans, desk knocks, air conditioning. Speech stays as it is.", "high pass filter hum bass rumble fan"),
    row(Audio, 1, "Longest recording", r"A recording stops at this length; the overlay warns 30 s before.", "max length limit duration"),
    row(Audio, 2, "Own sounds", r"Choose one WAV or OGG file, or a folder: every file directly in it (up to 200) becomes a tile below.", "custom sound file folder"),
    row(Audio, 2, "Picked sound", "", "preview"),
    row(Audio, 2, "Volume", "", "sound loud quiet"),

    page(Text, "Text", ""),
    tab(Text, 0, "Dictionary", "words names rare tokens vocabulary suggestions doubted misheard"),
    tab(Text, 1, "Replacements", "misheard fix correct wrong word heard wanted"),
    tab(Text, 2, "Words", "filler snippets"),
    tab(Text, 3, "Formatting", "processing"),
    tab(Text, 4, "Voice commands", "new line paragraph delete that scratch that"),
    row(Text, 2, "Remove filler words", r#"Removes the words below, with their comma: \"Ну, э, привет\" becomes \"Ну, привет\"; \"Um, hello\" becomes \"Hello\"."#, "fillers"),
    row(Text, 2, "Filler words", r"Filler words to drop, like um or ну: one per line, any case, whole words only.", ""),
    row(Text, 2, "Phrase shortcuts", r#"When you say only the trigger, the text takes its place: \"мой адрес => ул. Ленина, 1\" or \"my email => me@example.com\". The trigger must be the whole dictation: inside a longer sentence it does nothing (a replacement fixes a word anywhere). Write a backslash and n for a line break in the text."#, "snippets"),
    row(Text, 3, "Punctuation", "", ""),
    row(Text, 3, "Punctuation from pauses", r#"A long pause ends the sentence: a full stop and a capital letter. \"привет как дела\" with a pause becomes \"привет. Как дела\"; \"hello how are you\" becomes \"hello. How are you\"."#, "pause punctuation period"),
    row(Text, 3, "Sentence pause", r"A pause at least this long ends a sentence.", ""),
    row(Text, 3, "Paragraph pause", r"A pause at least this long also leaves a blank line. Typed text gets a space instead: a line break would send the message.", ""),
    row(Text, 3, "No full stop at the end", r#"Drops the full stop at the very end of a dictation: \"See you.\" becomes \"See you\". An ellipsis, ! and ? stay; a phrase shortcut's text is never changed."#, "period dot final punctuation style suggestion habit"),
    row(Text, 3, "Small first letter", r#"Starts a dictation with a small letter: \"Hello there\" becomes \"hello there\". A one-letter word (I) and a word with more capitals (USB) stay as they are."#, "lowercase capital style suggestion habit"),
    row(Text, 3, "Unsure words", "", "doubtful"),
    row(Text, 3, "Hold back unsure words", r"A part Whisper was unsure of is not pasted; its text stays in the History record. Needs History saving.", "hold doubtful words back confidence"),
    row(Text, 3, "Certainty needed", r"A part is held back when Whisper is less sure of it than this.", "doubt level confidence"),
    row(Text, 4, "Use voice commands", r"The switch for every app: off, none of the commands below works. An app rule on the Paste page can turn it on or off for one app.", "voice command new line paragraph delete that scratch that"),
    row(Text, 4, "New line", r#"Ends the line. \"Hello. New line. World.\" becomes Hello. and World. on two lines."#, "enter break phrase trigger words"),
    row(Text, 4, "New paragraph", r"Leaves a blank line and starts the next sentence with a capital letter.", "blank line break phrase trigger words"),
    row(Text, 4, "Delete the last sentence", r#"Removes the sentence you said just before it, in this dictation only. \"Keep. Drop this one. Delete that. More.\" becomes Keep. More."#, "scratch that undo remove phrase trigger words"),

    page(Paste, "Paste", "output insert"),
    tab(Paste, 0, "Paste", "clipboard ctrl+v type"),
    tab(Paste, 1, "Write to file", "to-file save markdown"),
    tab(Paste, 2, "Per-app rules", "apps programs games profile exceptions"),
    row(Paste, 0, "Paste mode", r"Clipboard puts your own copy back after the paste. Type also works where Ctrl+V does not, like remote desktops.", "clipboard type ctrl+v"),
    row(Paste, 0, "Add a space after the text", r"Adds a space after the pasted text.", "trailing space"),
    row(Paste, 0, "Press Enter after pasting", r"One Enter once the whole text is in. Not sent if, since you started speaking, the window or the field changed, you pressed a key, or the PC was locked or asleep.", "auto enter send submit"),
    row(Paste, 0, "Typing speed", r"How fast Type mode types. Safe waits a little between keys, for apps and games that drop fast keys. An app's rule can change it.", ""),
    row(Paste, 0, "Keep the last dictation in Win+V", r"A new dictation replaces the one before.", "clipboard history"),
    row(Paste, 0, "Give your copy back after", r"How long the dictation stays on Ctrl+V after it is pasted.", "restore your copy clipboard"),
    row(Paste, 1, "Write each dictation to a file", r"Each dictation is added to a file of the day, YYYY-MM-DD, in the folder. With Write only nothing is pasted. Without a folder nothing is written.", "also write to a file to-file"),
    row(Paste, 1, "Folder", r"Choose the folder the files go to.", "to-file"),
    row(Paste, 1, "File type", r"Markdown adds a list line, - 09:05 text; Text adds 09:05 text.", "md txt"),
    row(Paste, 2, "Join lines in command prompts", r"Only a classic console window with nothing but cmd.exe running in it, where a line break runs the line as a command: there the lines are joined with spaces. Claude Code, PowerShell or any other program started in it, Windows Terminal and every other app get them as dictated. An app's profile can turn it on or off for that app.", "console terminal"),
    inner(Paste, 2, "Language", r"This app's language. A language with its own model (Model > Languages) runs that model; Auto runs the model of the first row there.", "per app profile"),
    inner(Paste, 2, "Model", r"This app's model. Changing between two apps with different models loads a model each time (1 to 4 seconds, while you speak).", "per app profile"),
    inner(Paste, 2, "Write to a file", r"Needs the folder on Paste > Write to file.", "per app profile"),
    inner(Paste, 2, "Typing speed", r"How fast this app gets typed text. It matters only when the app types its text instead of pasting it.", "per app profile"),
    inner(Paste, 2, "Dictionary", r"Off sends Whisper no dictionary words for this app.", "per app profile"),
    inner(Paste, 2, "Replacements", r"Off skips the replacements of Text > Replacements for this app.", "per app profile"),
    inner(Paste, 2, "Voice commands", r"All voice commands on or off for this app, whatever Text > Voice commands says.", "per app profile"),
    inner(Paste, 2, "Remove filler words", r"On or off for this app, whatever Text > Words says.", "per app profile"),
    inner(Paste, 2, "Punctuation from pauses", r"On or off for this app, whatever Text > Formatting says.", "per app profile pause punctuation"),
    inner(Paste, 2, "Join lines", r#"On joins the lines with spaces in this app, Off keeps them, whatever the \"Join lines in command prompts\" switch above the rules says."#, "per app profile"),

    page(Overlay, "Overlay", "pill indicator bubble"),
    row(Overlay, 0, "Show the overlay", "", "pill indicator"),
    row(Overlay, 0, "Hide over fullscreen apps", "", "games videos"),
    row(Overlay, 0, "Position", r"Click a position to see the overlay there.", "place corner"),

    page(History, "History", "recordings past dictations"),
    page(Test, "Test", "compare models benchmark"),

    page(System, "System", "general"),
    tab(System, 0, "General", "theme"),
    tab(System, 1, "History and files", "recordings and files storage disk"),
    tab(System, 2, "About", "version licence author github"),
    row(System, 0, "Appearance", "", "theme dark light"),
    row(System, 0, "Accent", "", "colour color"),
    row(System, 0, "App language", r"Menus and labels of this window. It does not change the language you dictate in.", "interface"),
    row(System, 0, "Startup and updates", "", ""),
    row(System, 0, "Start at sign-in", "", "autostart startup boot"),
    row(System, 0, "Check for updates", r"Once a day OxiYapper asks GitHub for the latest version. Nothing installs on its own.", ""),
    row(System, 1, "Save dictations to History", "", "save recordings keep"),
    row(System, 1, "Keep for", r"Keeps audio and text in the folder below up to these limits. Older ones are deleted, even while off.", "days age"),
    row(System, 1, "Size limit", r"When the folder below grows past this size, the oldest recordings are deleted.", "gb disk"),
    row(System, 1, "Recordings folder", "", "where browse"),
    row(System, 1, "Open a folder", r"Settings holds config.toml and your dictionary; Logs holds the log files.", "config"),
    row(System, 1, "Log dictated text", r"Writes what you dictate into the log files, to help find a bug. Off by default.", "debug"),
    row(System, 2, "Built with", "", "libraries credits"),
    row(System, 2, "Author", "", ""),
];

/// Lower case, accents off, `\` escapes and `{}` placeholders dropped.
fn fold(text: &str) -> String {
    const PLAIN: [(&str, char); 15] = [
        ("àáâãäåāăą", 'a'),
        ("çćč", 'c'),
        ("ď", 'd'),
        ("èéêëēėęě", 'e'),
        ("ìíîïīį", 'i'),
        ("ł", 'l'),
        ("ñńň", 'n'),
        ("òóôõöøōő", 'o'),
        ("ř", 'r'),
        ("śš", 's'),
        ("ť", 't'),
        ("ùúûüūůű", 'u'),
        ("ÿý", 'y'),
        ("źżž", 'z'),
        ("ё", 'е'),
    ];
    let mut out = String::with_capacity(text.len());
    let mut placeholder = false;
    for c in text.chars().flat_map(char::to_lowercase) {
        match c {
            '{' => placeholder = true,
            '}' => placeholder = false,
            '\\' => {}
            _ if placeholder => {}
            _ => out.push(
                PLAIN
                    .iter()
                    .find(|(from, _)| from.contains(c))
                    .map_or(c, |&(_, to)| to),
            ),
        }
    }
    out
}

/// The words of folded text: letters and digits, split at everything else ("Win+V" is "win", "v").
fn words(folded: &str) -> impl Iterator<Item = &str> {
    folded
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
}

/// Whether some word of `text` starts with `word`: "gain" finds "Gain", not "again".
fn has(text: &str, word: &str) -> bool {
    words(&fold(text)).any(|w| w.starts_with(word))
}

/// How well `entry` matches the folded query words: 0 every word in the title, 1 some only in a tab
/// or page name or the extra words, 2 some only in the tip; `None` when a word is nowhere.
fn rank(entry: &Entry, query: &[&str]) -> Option<u8> {
    // A page or tab name ranks under a setting title (title > tab > tip).
    let title_tier = u8::from(entry.kind != Kind::Row);
    query.iter().try_fold(0, |worst, word| {
        let tier = if has(entry.title, word) {
            title_tier
        } else if has(entry.words, word) {
            1
        } else if has(entry.tip, word) {
            2
        } else {
            return None;
        };
        Some(worst.max(tier))
    })
}

/// The entries matching `query`, best first (rank, then a title that starts with the query, then
/// table order). An empty query lists the pages and tabs, a map to browse.
fn find(query: &str) -> Vec<usize> {
    let query = fold(query.trim());
    let words: Vec<&str> = words(&query).collect();
    if words.is_empty() {
        return (0..ENTRIES.len())
            .filter(|&i| ENTRIES[i].kind != Kind::Row)
            .collect();
    }
    let mut hits: Vec<(u8, bool, usize)> = ENTRIES
        .iter()
        .enumerate()
        .filter_map(|(i, e)| Some((rank(e, &words)?, !fold(e.title).starts_with(&query), i)))
        .collect();
    hits.sort_unstable();
    hits.into_iter().map(|(.., i)| i).collect()
}

/// Where an entry is, as the popup shows it: "Page > Tab" for a row, the page for a tab.
fn place(entry: &Entry) -> String {
    let name = |kind: Kind, tab: i32| {
        ENTRIES
            .iter()
            .find(|e| {
                e.kind == kind && e.page == entry.page && (kind == Kind::Page || e.tab == tab)
            })
            .map(|e| e.title)
    };
    let page = name(Kind::Page, -1).unwrap_or_default();
    match (entry.kind, name(Kind::Tab, entry.tab)) {
        (Kind::Page, _) => String::new(),
        (Kind::Row, Some(tab)) => format!("{page} > {tab}"),
        _ => page.to_owned(),
    }
}

/// Opens the entry's page and tab, then names its row to `Shell.found` so it flashes.
fn open(window: &AppWindow, entry: &Entry) {
    window.set_page(entry.page);
    if entry.tab >= 0 {
        match entry.page {
            Model => window.global::<ModelState>().set_tab(entry.tab),
            Audio => window.global::<AudioState>().set_view(entry.tab),
            Text => window.global::<TextState>().set_tab(entry.tab),
            Paste => window.global::<TextState>().set_paste_tab(entry.tab),
            System => window.global::<SystemState>().set_tab(entry.tab),
            _ => {}
        }
    }
    if !entry.key.is_empty() {
        let shell = window.global::<Shell>();
        shell.set_found(entry.key.into());
        shell.set_found_serial(shell.get_found_serial() + 1);
    }
}

/// The picked row has an anchor in front of its controls and holds the focus: one Tab key moves it to
/// the first control (a Switch, Slider, Combo ...). `quiet-focus` keeps the ring off, as a pick is no
/// keyboard walk. A row with no enabled control of its own (a text-only row too) keeps the focus on its
/// own tail stop (layout.slint): the Tab never leaves the found row. The
/// quiet ends by `quiet-release`, queued after the Tab, so a text field's late `changed has-focus` still
/// sees it.
fn focus_control(window: &AppWindow) {
    let shell = window.global::<Shell>();
    shell.set_quiet_focus(true);
    let tab = SharedString::from(slint::platform::Key::Tab);
    window
        .window()
        .dispatch_event(slint::platform::WindowEvent::KeyPressed { text: tab.clone() });
    window
        .window()
        .dispatch_event(slint::platform::WindowEvent::KeyReleased { text: tab });
    shell.set_quiet_release(shell.get_quiet_release() + 1);
}

/// Answers the window's search popup.
pub(crate) fn bind(window: &AppWindow) {
    window.global::<Shell>().on_found_ready({
        let weak = window.as_weak();
        move || {
            if let Some(window) = weak.upgrade() {
                focus_control(&window);
            }
        }
    });
    let shown: Rc<RefCell<Vec<usize>>> = Rc::default();
    window.on_search({
        let (weak, shown) = (window.as_weak(), shown.clone());
        move |query| {
            let Some(window) = weak.upgrade() else { return };
            let hits = find(&query);
            let titles: Vec<SharedString> = hits.iter().map(|&i| ENTRIES[i].title.into()).collect();
            let places: Vec<SharedString> =
                hits.iter().map(|&i| place(&ENTRIES[i]).into()).collect();
            window.set_search_items(ModelRc::new(VecModel::from(titles)));
            window.set_search_notes(ModelRc::new(VecModel::from(places)));
            let pages: Vec<Page> = hits.iter().map(|&i| ENTRIES[i].page).collect();
            window.set_search_pages(ModelRc::new(VecModel::from(pages)));
            *shown.borrow_mut() = hits;
        }
    });
    window.on_search_picked({
        let weak = window.as_weak();
        move |i| {
            let hit = usize::try_from(i)
                .ok()
                .and_then(|i| shown.borrow().get(i).copied());
            if let (Some(window), Some(hit)) = (weak.upgrade(), hit) {
                open(&window, &ENTRIES[hit]);
            }
        }
    });
}

#[cfg(test)]
mod tests;
