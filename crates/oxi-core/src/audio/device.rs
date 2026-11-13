//! Which input device a recording opens (ARCHITECTURE section 9, step 1; e09).
//!
//! Decided at every key-down from what is present then, so a Windows default changed while the
//! app runs, a mic plugged in or one that came back is used by the next recording. The config is
//! only read: a missing device is fallen back from, never written over.

/// One input device present at the open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputDevice<'a> {
    pub name: &'a str,
    /// It is the system default input now.
    pub is_default: bool,
}

/// What to open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputPick {
    /// The device at this index of the present list; `fell_back` when it is the default standing
    /// in for a configured device that is not present (the user is told).
    Open { index: usize, fell_back: bool },
    /// No device to open: the recording is refused with that reason.
    NoDevice,
}

/// The device for `configured` (`audio.device`, empty: the system default) among `present`.
/// A configured name that is present wins (of two with that name, the default one); else the
/// current default; with no default, nothing.
#[must_use]
pub fn pick_input(configured: &str, present: &[InputDevice<'_>]) -> InputPick {
    let named = |d: &InputDevice<'_>| !configured.is_empty() && d.name == configured;
    let best = present
        .iter()
        .position(|d| named(d) && d.is_default)
        .or_else(|| present.iter().position(named));
    if let Some(index) = best {
        return InputPick::Open {
            index,
            fell_back: false,
        };
    }
    present
        .iter()
        .position(|d| d.is_default)
        .map_or(InputPick::NoDevice, |index| InputPick::Open {
            index,
            fell_back: !configured.is_empty(),
        })
}

/// What the OS said about an audio endpoint while a recording ran on `opened` (Windows:
/// `IMMNotificationClient`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointNote<'a> {
    /// The endpoint `id` changed state; `active` is `DEVICE_STATE_ACTIVE` (a 3.5 mm jack pulled
    /// out is `UNPLUGGED`, a disabled one `DISABLED`, a gone USB mic `NOTPRESENT`).
    State { id: &'a str, active: bool },
    /// The endpoint `id` left the system (USB, Bluetooth hands-free profile ended).
    Removed { id: &'a str },
    /// The system default moved.
    DefaultChanged,
}

/// Whether `note` means the endpoint the recording opened (`opened`, its id) is gone, so the
/// recording ends as a lost device. A new default never ends it: the recording keeps its endpoint
/// to the key-up and the next press opens the new default (e09).
#[must_use]
pub fn endpoint_lost(note: EndpointNote<'_>, opened: &str) -> bool {
    let ours = |id: &str| !opened.is_empty() && id.eq_ignore_ascii_case(opened);
    match note {
        EndpointNote::State { id, active } => !active && ours(id),
        EndpointNote::Removed { id } => ours(id),
        EndpointNote::DefaultChanged => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(name: &str, is_default: bool) -> InputDevice<'_> {
        InputDevice { name, is_default }
    }

    #[test]
    fn the_pick_table() {
        let open = |index, fell_back| InputPick::Open { index, fell_back };
        let (usb, laptop, bt) = ("USB Mic", "Laptop Mic", "Headset");
        let cases: [(&str, &str, Vec<InputDevice<'_>>, InputPick); 9] = [
            (
                "default: the current default, wherever it sits",
                "",
                vec![dev(usb, false), dev(laptop, true)],
                open(1, false),
            ),
            (
                "default changed (or a new mic became it): the next open follows",
                "",
                vec![dev(usb, true), dev(laptop, false), dev(bt, false)],
                open(0, false),
            ),
            (
                "picked and present: that one, not the default",
                usb,
                vec![dev(laptop, true), dev(usb, false)],
                open(1, false),
            ),
            (
                "picked and gone: the default, and the user is told",
                usb,
                vec![dev(laptop, true)],
                open(0, true),
            ),
            (
                "picked device back: it is used again (config never rewritten)",
                usb,
                vec![dev(laptop, true), dev(bt, false), dev(usb, false)],
                open(2, false),
            ),
            (
                "two with the picked name: the default one",
                usb,
                vec![dev(usb, false), dev(laptop, false), dev(usb, true)],
                open(2, false),
            ),
            (
                "no input device at all: refused",
                "",
                vec![],
                InputPick::NoDevice,
            ),
            (
                "picked, gone, and nothing else: refused",
                usb,
                vec![],
                InputPick::NoDevice,
            ),
            (
                "devices but no default (Windows between two): refused, no guess",
                "",
                vec![dev(usb, false)],
                InputPick::NoDevice,
            ),
        ];
        for (what, configured, present, want) in cases {
            assert_eq!(pick_input(configured, &present), want, "{what}");
        }
    }

    #[test]
    fn the_endpoint_lost_table() {
        let (ours, other) = ("{0.0.1.00000000}.{AB-12}", "{0.0.1.00000000}.{CD-34}");
        let state = |id, active| EndpointNote::State { id, active };
        let cases = [
            (
                "our endpoint UNPLUGGED, DISABLED or NOTPRESENT",
                state(ours, false),
                ours,
                true,
            ),
            (
                "our endpoint back to ACTIVE",
                state(ours, true),
                ours,
                false,
            ),
            (
                "another endpoint unplugged",
                state(other, false),
                ours,
                false,
            ),
            (
                "our endpoint removed",
                EndpointNote::Removed { id: ours },
                ours,
                true,
            ),
            (
                "another endpoint removed",
                EndpointNote::Removed { id: other },
                ours,
                false,
            ),
            (
                "default changed: the recording keeps its endpoint",
                EndpointNote::DefaultChanged,
                ours,
                false,
            ),
            (
                "ids differ only in case",
                EndpointNote::Removed {
                    id: "{0.0.1.00000000}.{ab-12}",
                },
                ours,
                true,
            ),
            (
                "opened id unknown: never a guess",
                state("", false),
                "",
                false,
            ),
        ];
        for (what, note, opened, want) in cases {
            assert_eq!(endpoint_lost(note, opened), want, "{what}");
        }
    }
}
