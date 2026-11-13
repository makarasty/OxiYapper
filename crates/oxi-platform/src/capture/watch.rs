//! Watches the endpoint a recording opened, for the life of that recording (e09).
//!
//! A concrete endpoint gets no device monitor from cpal, and a 3.5 mm jack pulled out
//! (`DEVICE_STATE_UNPLUGGED`) or a removed USB or Bluetooth hands-free endpoint may leave its
//! stream giving silence with no error. Windows tells an `IMMNotificationClient`; which note means
//! the opened endpoint is gone is [`endpoint_lost`]. A default change is never one.

use std::sync::Arc;

use oxi_core::audio::{EndpointNote, endpoint_lost};
use windows::Win32::Foundation::{PROPERTYKEY, RPC_E_CHANGED_MODE};
use windows::Win32::Media::Audio::{
    DEVICE_STATE, DEVICE_STATE_ACTIVE, EDataFlow, ERole, IMMDeviceEnumerator,
    IMMNotificationClient, IMMNotificationClient_Impl, MMDeviceEnumerator,
};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
};
use windows::core::{HRESULT, PCWSTR, Result as WinResult};

/// Registered on [`EndpointWatch::start`], unregistered on drop.
pub(super) struct EndpointWatch {
    enumerator: IMMDeviceEnumerator,
    client: IMMNotificationClient,
}

// SAFETY: both are free-threaded COM objects (MMDevAPI's enumerator is "Both"); they are only
// used to unregister, on whichever thread drops the capture, after COM is initialised there.
unsafe impl Send for EndpointWatch {}

impl EndpointWatch {
    /// Calls `lost` (on a Windows notification thread) when the endpoint `opened` is unplugged,
    /// disabled or removed. `None` when Windows refuses: the recording runs unwatched, as before.
    pub(super) fn start(opened: &str, lost: Arc<dyn Fn() + Send + Sync>) -> Option<Self> {
        com_init();
        let client: IMMNotificationClient = Watcher {
            opened: opened.to_owned(),
            lost,
        }
        .into();
        // SAFETY: COM is initialised on this thread; both pointers live until the drop.
        let started = unsafe {
            CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .and_then(|e| e.RegisterEndpointNotificationCallback(&client).map(|()| e))
        };
        match started {
            Ok(enumerator) => Some(Self { enumerator, client }),
            Err(e) => {
                tracing::warn!("input device not watched: {e}");
                None
            }
        }
    }
}

impl Drop for EndpointWatch {
    fn drop(&mut self) {
        com_init();
        // SAFETY: registered in `start`. Synchronous: waits for a callback in progress, which
        // never drops the capture, so this cannot wait on itself.
        let _ = unsafe {
            self.enumerator
                .UnregisterEndpointNotificationCallback(&self.client)
        };
    }
}

#[allow(
    clippy::inline_always,
    clippy::ref_as_ptr,
    reason = "in the code `#[implement]` generates beside the struct"
)]
mod object {
    use std::sync::Arc;

    use windows::Win32::Media::Audio::IMMNotificationClient;
    use windows::core::implement;

    #[implement(IMMNotificationClient)]
    pub(super) struct Watcher {
        pub(super) opened: String,
        pub(super) lost: Arc<dyn Fn() + Send + Sync>,
    }
}
use object::{Watcher, Watcher_Impl};

impl Watcher {
    fn note(&self, note: EndpointNote<'_>) {
        if endpoint_lost(note, &self.opened) {
            (self.lost)();
        }
    }
}

/// The id Windows passed; empty when it passed none.
fn id(p: PCWSTR) -> String {
    if p.is_null() {
        return String::new();
    }
    // SAFETY: Windows passes a valid NUL-terminated string for the length of the call.
    unsafe { p.to_string() }.unwrap_or_default()
}

impl IMMNotificationClient_Impl for Watcher_Impl {
    fn OnDeviceStateChanged(&self, device: &PCWSTR, state: DEVICE_STATE) -> WinResult<()> {
        let active = state == DEVICE_STATE_ACTIVE;
        self.note(EndpointNote::State {
            id: &id(*device),
            active,
        });
        Ok(())
    }

    fn OnDeviceAdded(&self, _: &PCWSTR) -> WinResult<()> {
        Ok(())
    }

    fn OnDeviceRemoved(&self, device: &PCWSTR) -> WinResult<()> {
        self.note(EndpointNote::Removed { id: &id(*device) });
        Ok(())
    }

    fn OnDefaultDeviceChanged(&self, _: EDataFlow, _: ERole, _: &PCWSTR) -> WinResult<()> {
        self.note(EndpointNote::DefaultChanged);
        Ok(())
    }

    fn OnPropertyValueChanged(&self, _: &PCWSTR, _: &PROPERTYKEY) -> WinResult<()> {
        Ok(())
    }
}

/// COM on this thread, once, released at thread exit (as cpal does for its own calls).
fn com_init() {
    struct Com(HRESULT);
    impl Drop for Com {
        fn drop(&mut self) {
            if self.0.is_ok() {
                // SAFETY: balances the successful `CoInitializeEx` of this thread.
                unsafe { CoUninitialize() };
            }
        }
    }
    thread_local! {
        // SAFETY: a plain init of this thread; another mode already set is fine for MMDevAPI.
        static COM: Com = Com(unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) });
    }
    COM.with(|c| {
        if c.0.is_err() && c.0 != RPC_E_CHANGED_MODE {
            tracing::warn!("COM not initialised: {:?}", c.0);
        }
    });
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use windows::Win32::Media::Audio::{DEVICE_STATE_UNPLUGGED, eCapture, eConsole};
    use windows::core::HSTRING;

    use super::*;

    /// The COM object, called through its vtable as Windows calls it: only the opened endpoint
    /// going away calls `lost`; a default change does not.
    #[test]
    fn only_the_opened_endpoint_going_calls_lost() {
        let calls = Arc::new(AtomicU32::new(0));
        let counted = Arc::clone(&calls);
        let client: IMMNotificationClient = Watcher {
            opened: "{0.0.1.00000000}.{ours}".into(),
            lost: Arc::new(move || {
                counted.fetch_add(1, Ordering::Relaxed);
            }),
        }
        .into();
        let (ours, other) = (
            HSTRING::from("{0.0.1.00000000}.{ours}"),
            HSTRING::from("{0.0.1.00000000}.{other}"),
        );
        let (ours, other) = (PCWSTR(ours.as_ptr()), PCWSTR(other.as_ptr()));
        // SAFETY: valid strings for the length of each call, as Windows passes them.
        unsafe {
            client
                .OnDefaultDeviceChanged(eCapture, eConsole, other)
                .unwrap();
            client
                .OnDeviceStateChanged(other, DEVICE_STATE_UNPLUGGED)
                .unwrap();
            client.OnDeviceRemoved(other).unwrap();
            client
                .OnDeviceStateChanged(ours, DEVICE_STATE_ACTIVE)
                .unwrap();
            client.OnDeviceRemoved(PCWSTR::null()).unwrap();
            assert_eq!(calls.load(Ordering::Relaxed), 0);
            client
                .OnDeviceStateChanged(ours, DEVICE_STATE_UNPLUGGED)
                .unwrap();
            client.OnDeviceRemoved(ours).unwrap();
        }
        assert_eq!(
            calls.load(Ordering::Relaxed),
            2,
            "once each; the capture dedups"
        );
    }

    /// Register and unregister on two threads, as a capture opened on the audio worker and
    /// finished on the end thread. No device is opened. Without the audio service there is no
    /// watch, and nothing to check.
    #[test]
    fn a_watch_unregisters_on_another_thread() {
        let Some(watch) = EndpointWatch::start("{0.0.1.00000000}.{none}", Arc::new(|| {})) else {
            eprintln!("skipped: no endpoint notifications here");
            return;
        };
        std::thread::spawn(move || drop(watch)).join().unwrap();
    }
}
