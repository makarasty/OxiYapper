//! System page: `[system]` (autostart, update check) and `debug.log_text`. The theme picker on
//! the same page belongs to the shell. Autostart is written to the config only; registering it
//! with the OS is M10.

use std::path::Path;

use oxi_core::config::Config;
use slint::{ComponentHandle, Weak};

use crate::generated::SystemState;
use crate::{AppWindow, Ctx, Error, report};

pub fn bind(window: &AppWindow, ctx: &Ctx) {
    let s = window.global::<SystemState>();
    let file = ctx.config.borrow();
    s.set_autostart(file.config.system.autostart);
    s.set_update_check(file.config.system.update_check);
    s.set_log_text(file.config.debug.log_text);
    drop(file);

    let (w, c) = (window.as_weak(), ctx.clone());
    s.on_autostart_toggled(move |on| apply(&w, &c, |cfg| cfg.system.autostart = on));
    let (w, c) = (window.as_weak(), ctx.clone());
    s.on_update_check_toggled(move |on| apply(&w, &c, |cfg| cfg.system.update_check = on));
    let (w, c) = (window.as_weak(), ctx.clone());
    s.on_log_text_toggled(move |on| apply(&w, &c, |cfg| cfg.debug.log_text = on));
    let (w, dir) = (window.as_weak(), ctx.layout.config_dir.clone());
    s.on_open_settings_folder(move || open_folder(&w, &dir));
    let (w, dir) = (window.as_weak(), ctx.layout.logs_dir.clone());
    s.on_open_logs_folder(move || open_folder(&w, &dir));
}

fn open_folder(weak: &Weak<AppWindow>, dir: &Path) {
    report(weak, oxi_platform::open::open(dir).map_err(Error::from));
}

fn apply(weak: &Weak<AppWindow>, ctx: &Ctx, change: impl FnOnce(&mut Config)) {
    change(&mut ctx.config.borrow_mut().config);
    report(weak, ctx.save());
}
