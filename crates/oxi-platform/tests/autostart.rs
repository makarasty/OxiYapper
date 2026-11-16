//! Autostart register, query and unregister under a temp entry name, never the app's own
//! [`autostart::NAME`]. The entry is removed on drop, even when an assert fails.
#![cfg(windows)]

use oxi_platform::autostart;

struct Temp(String);

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = autostart::unregister(&self.0);
    }
}

#[test]
fn a_temp_entry_registers_reads_back_and_unregisters() {
    let entry = Temp(format!("OxiYapperTest-{}", std::process::id()));
    let name = entry.0.as_str();
    assert_ne!(name, autostart::NAME);
    let command = r#""C:\no\such\Synth Folder\OxiYapper.exe""#;
    assert_eq!(autostart::query(name).unwrap(), None);
    autostart::register(name, command).unwrap();
    assert_eq!(autostart::query(name).unwrap().as_deref(), Some(command));
    autostart::register(name, r#""C:\b.exe""#).unwrap();
    assert_eq!(
        autostart::query(name).unwrap().as_deref(),
        Some(r#""C:\b.exe""#)
    );
    autostart::unregister(name).unwrap();
    assert_eq!(autostart::query(name).unwrap(), None);
    // A second removal finds nothing and is fine.
    autostart::unregister(name).unwrap();
}
