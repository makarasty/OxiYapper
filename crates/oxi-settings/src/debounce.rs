//! A config save that waits for a slider drag to rest.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use slint::{Timer, TimerMode, Weak};

use crate::{AppWindow, Ctx, report};

/// How long a slider rests before its value is saved.
const SAVE_DELAY: Duration = Duration::from_millis(300);

/// A config save not run yet. Dropping it runs it, so a window closed before the debounce
/// fires still saves.
struct PendingSave {
    weak: Weak<AppWindow>,
    ctx: Ctx,
}

impl Drop for PendingSave {
    fn drop(&mut self) {
        report(&self.weak, self.ctx.save());
    }
}

/// A drag fires on every step: [`SaveDebounce::touch`] saves once it rests, not one fsync per
/// step. Dropping this (the page is rebound or closed) runs a save still pending.
pub struct SaveDebounce {
    weak: Weak<AppWindow>,
    ctx: Ctx,
    timer: Timer,
    pending: Rc<RefCell<Option<PendingSave>>>,
}

impl SaveDebounce {
    pub fn new(weak: Weak<AppWindow>, ctx: Ctx) -> Self {
        Self {
            weak,
            ctx,
            timer: Timer::default(),
            pending: Rc::default(),
        }
    }

    /// The config changed in memory: save it once no more changes come for `SAVE_DELAY`.
    pub fn touch(&self) {
        self.pending
            .borrow_mut()
            .get_or_insert_with(|| PendingSave {
                weak: self.weak.clone(),
                ctx: self.ctx.clone(),
            });
        let pending = Rc::clone(&self.pending);
        self.timer
            .start(TimerMode::SingleShot, SAVE_DELAY, move || {
                let job = pending.borrow_mut().take();
                drop(job);
            });
    }
}
