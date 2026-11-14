//! "Keep the model ready" (hold item 41, candidate 10): three named memory choices over
//! `preload_at_start`, `idle_unload_minutes` and `unload_immediately`.

use super::schema::Model;

/// Minutes Balanced frees the model after: the default `idle_unload_minutes`.
const BALANCED_MINUTES: u32 = 5;

/// The one memory choice Model > Device and memory shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryMode {
    /// Loaded when the app starts and never freed: the first word comes fastest, the memory
    /// stays taken.
    AlwaysReady,
    /// Loaded at the first dictation, freed after 5 idle minutes (the default).
    Balanced,
    /// Freed right after each dictation: the memory comes back at once, every dictation loads.
    SaveMemory,
    /// Any other mix, set by hand in "Free memory after".
    Custom,
}

impl MemoryMode {
    pub const ALL: [Self; 4] = [
        Self::AlwaysReady,
        Self::Balanced,
        Self::SaveMemory,
        Self::Custom,
    ];
}

impl Model {
    #[must_use]
    pub fn memory(&self) -> MemoryMode {
        match (
            self.preload_at_start,
            self.unload_immediately,
            self.idle_unload_minutes,
        ) {
            (true, false, 0) => MemoryMode::AlwaysReady,
            (false, false, BALANCED_MINUTES) => MemoryMode::Balanced,
            (false, true, _) => MemoryMode::SaveMemory,
            _ => MemoryMode::Custom,
        }
    }

    /// Custom changes nothing: it only names a hand-set mix. Save memory keeps the minutes, so
    /// picking a time later starts from them (as "Right after dictation" does).
    pub fn set_memory(&mut self, mode: MemoryMode) {
        let (preload, now, minutes) = match mode {
            MemoryMode::AlwaysReady => (true, false, 0),
            MemoryMode::Balanced => (false, false, BALANCED_MINUTES),
            MemoryMode::SaveMemory => (false, true, self.idle_unload_minutes),
            MemoryMode::Custom => return,
        };
        self.preload_at_start = preload;
        self.unload_immediately = now;
        self.idle_unload_minutes = minutes;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_modes_set_the_three_keys_and_others_read_custom() {
        let mut m = Model::default();
        assert_eq!(
            m.memory(),
            MemoryMode::Balanced,
            "Balanced is today's default"
        );
        for mode in MemoryMode::ALL {
            m.set_memory(mode);
            if mode != MemoryMode::Custom {
                assert_eq!(m.memory(), mode);
            }
        }
        m.set_memory(MemoryMode::AlwaysReady);
        assert!(m.preload_at_start && !m.unload_immediately && m.idle_unload_minutes == 0);
        m.idle_unload_minutes = 10;
        m.set_memory(MemoryMode::SaveMemory);
        assert_eq!(m.idle_unload_minutes, 10, "Save memory keeps the minutes");
        m.unload_immediately = false;
        assert_eq!(m.memory(), MemoryMode::Custom, "10 minutes is set by hand");
        m.set_memory(MemoryMode::Custom);
        assert_eq!(m.memory(), MemoryMode::Custom, "Custom changes nothing");
        m.preload_at_start = true;
        m.idle_unload_minutes = BALANCED_MINUTES;
        assert_eq!(
            m.memory(),
            MemoryMode::Custom,
            "loaded at start and freed: by hand"
        );
    }
}
