//! The first-run setup wizard (SPEC Settings UI, ROADMAP M10). A step moves the window's page with
//! it, so a step that shows a page's parts gets that page's behaviour (the live meter, the hotkey
//! capture, the model rows); Finish and Skip setup set `ui.first_run_done` for good.

use slint::ComponentHandle;

use crate::generated::{Page, SetupState};
use crate::{AppWindow, Ctx, report};

/// The window's page under each step: welcome, model, hotkey, microphone, try it.
const PAGES: [Page; 5] = [
    Page::Home,
    Page::Model,
    Page::Hotkey,
    Page::Audio,
    Page::Home,
];

pub fn bind(window: &AppWindow, ctx: &Ctx) {
    let s = window.global::<SetupState>();
    let file = ctx.config.borrow();
    // A locked config cannot remember a finish: the wizard would come back at every start.
    s.set_done(file.config.ui.first_run_done || file.read_only);
    drop(file);
    let weak = window.as_weak();
    s.on_go(move |step| {
        if let Some(window) = weak.upgrade() {
            go(&window, step);
        }
    });
    let weak = window.as_weak();
    s.on_start(move || {
        if let Some(window) = weak.upgrade() {
            window.global::<SetupState>().set_open(true);
            go(&window, 0);
        }
    });
    let (weak, c) = (window.as_weak(), ctx.clone());
    s.on_finish(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };
        let s = window.global::<SetupState>();
        s.set_open(false);
        window.set_page(Page::Home);
        if !s.get_done() {
            s.set_done(true);
            c.config.borrow_mut().config.ui.first_run_done = true;
            report(&weak, c.save());
        }
    });
}

/// The page first, then the step: the step's parts ask their page's state as they appear.
fn go(window: &AppWindow, step: i32) {
    let i = usize::try_from(step).unwrap_or(0).min(PAGES.len() - 1);
    window.set_page(PAGES[i]);
    window
        .global::<SetupState>()
        .set_step(i32::try_from(i).unwrap_or(0));
}

/// The first start: a config that was never set up opens on the wizard.
pub fn open_if_new(window: &AppWindow) {
    let s = window.global::<SetupState>();
    if !s.get_done() {
        s.invoke_start();
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use oxi_core::config::ConfigFile;
    use slint::ComponentHandle;

    use super::{PAGES, SetupState, open_if_new};
    use crate::testing::{has, linked};
    use crate::{AppWindow, ModelState, Page};

    fn state(window: &AppWindow) -> SetupState<'_> {
        window.global::<SetupState>()
    }

    fn saved_done(layout: &oxi_core::paths::Layout) -> bool {
        let text = fs::read_to_string(layout.config_file()).expect("saved");
        ConfigFile::parse(&text).unwrap().config.ui.first_run_done
    }

    fn settle() {
        slint::platform::update_timers_and_animations();
    }

    /// A new config opens on the welcome step; each step shows its parts on its page's state, and
    /// Next walks them all.
    #[test]
    fn a_new_config_opens_the_wizard_and_next_walks_every_step() {
        let (window, _ipc, _sent, _layout) = linked("setup", "walk", Page::Home);
        assert!(!state(&window).get_done(), "a new config is not set up");
        assert!(!state(&window).get_open(), "only the first start opens it");
        open_if_new(&window);
        settle();
        assert!(state(&window).get_open());
        assert_eq!(state(&window).get_step(), 0);
        assert!(has(&window, "Hold a key and speak"));
        assert!(has(&window, "Skip setup"));
        let labels = [
            "Hold a key and speak",
            "Models",
            "Change hotkey",
            "Record 3 s",
            "Try dictation here",
        ];
        for (step, label) in labels.iter().enumerate() {
            let i = i32::try_from(step).unwrap();
            assert_eq!(state(&window).get_step(), i);
            assert_eq!(window.get_page(), PAGES[step], "step {step}");
            assert!(has(&window, label), "step {step} shows {label}");
            assert!(has(&window, "Skip setup"), "Skip on step {step}");
            if step < labels.len() - 1 {
                state(&window).invoke_go(i + 1);
                settle();
            }
        }
        assert!(state(&window).get_open(), "the last step waits for Finish");
    }

    /// The model step checks the recommended model, also when the recommendation comes after the
    /// step opened (the hardware is read on a worker).
    #[test]
    fn the_model_step_checks_the_recommended_model() {
        let (window, _ipc, _sent, _layout) = linked("setup", "model", Page::Home);
        state(&window).invoke_start();
        let models = window.global::<ModelState>();
        state(&window).invoke_go(1);
        settle();
        // Built when looked at (no renderer here).
        assert!(has(&window, "Pick a model"));
        // The test box has no hardware to judge: the page found no recommendation.
        assert_eq!(models.get_recommended(), -1);
        models.set_recommended(2);
        settle();
        assert_eq!(models.get_viewed(), 2, "the recommended one is checked");
        models.set_recommended(3);
        settle();
        assert_eq!(models.get_viewed(), 3, "a later recommendation moves it");
    }

    /// Finish closes it, goes Home and saves the flag; it never opens on its own again.
    #[test]
    fn finish_saves_the_flag_and_the_wizard_does_not_come_back() {
        let (window, _ipc, _sent, layout) = linked("setup", "finish", Page::Home);
        open_if_new(&window);
        state(&window).invoke_go(4);
        settle();
        state(&window).invoke_finish();
        settle();
        assert!(!state(&window).get_open());
        assert_eq!(window.get_page(), Page::Home);
        assert!(state(&window).get_done());
        assert!(saved_done(&layout));
        assert!(!has(&window, "Skip setup"));

        // The next window on the same config: the flag is set, nothing opens.
        let again = crate::open_with(layout.clone(), crate::Ipc::fake().0).unwrap();
        again.show().unwrap();
        assert!(state(&again).get_done());
        open_if_new(&again);
        assert!(!state(&again).get_open(), "set up: no wizard");
    }

    /// Skip setup on any step sets the flag as Finish does.
    #[test]
    fn skip_on_a_middle_step_sets_the_flag() {
        let (window, _ipc, _sent, layout) = linked("setup", "skip", Page::Home);
        open_if_new(&window);
        state(&window).invoke_go(2);
        settle();
        crate::testing::one(&window, "Skip setup").invoke_accessible_default_action();
        settle();
        assert!(!state(&window).get_open());
        assert!(saved_done(&layout));
        assert_eq!(window.get_page(), Page::Home);
    }

    /// System > General reopens it on the welcome step; finishing again writes nothing new.
    #[test]
    fn system_reopens_the_wizard() {
        let (window, _ipc, _sent, layout) = linked("setup", "reopen", Page::System);
        open_if_new(&window);
        state(&window).invoke_finish();
        window.set_page(Page::System);
        settle();
        crate::testing::one(&window, "Run setup again").invoke_accessible_default_action();
        settle();
        assert!(state(&window).get_open());
        assert_eq!(state(&window).get_step(), 0);
        assert_eq!(window.get_page(), Page::Home);
        state(&window).invoke_go(3);
        settle();
        assert_eq!(window.get_page(), Page::Audio, "the meter's page");
        state(&window).invoke_finish();
        assert!(saved_done(&layout));
    }

    /// A locked config (newer or broken file) never opens the wizard: it could not save a finish.
    #[test]
    fn a_locked_config_does_not_open_the_wizard() {
        let layout = crate::testing::layout("setup", "locked");
        fs::create_dir_all(&layout.config_dir).unwrap();
        fs::write(
            layout.config_file(),
            "version = 99\n[ui]\nfirst_run_done = false\n",
        )
        .unwrap();
        i_slint_backend_testing::init_no_event_loop();
        let window = crate::open_with(layout, crate::Ipc::fake().0).unwrap();
        window.show().unwrap();
        open_if_new(&window);
        assert!(!state(&window).get_open());
    }
}
