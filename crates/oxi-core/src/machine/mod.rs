//! State machine: phases, events, double tap, Esc (ARCHITECTURE section 3).
//!
//! Pure: no clock, no threads, no I/O. The coordinator feeds [`Event`]s with the current
//! time to [`step`], executes the returned [`Effect`]s, and sleeps until
//! [`State::next_deadline`]. Time-driven work happens on [`Event::Tick`].

mod state;
mod types;

pub use state::State;
pub use types::{
    Effect, Effects, EmptyAudio, EngineWarning, Event, JobId, MachineConfig, Mode, ModelFailure,
    ModelIdentity, Notice, OverlayState, Phase, PhaseCell, Sound,
};

use crate::types::Millis;

/// Feed one event into the machine and return what the coordinator must do.
pub fn step(state: &mut State, event: Event, now: Millis) -> Effects {
    state.handle(event, now)
}

#[cfg(test)]
mod edge_tests;
