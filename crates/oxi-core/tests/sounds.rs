//! Sound choices from the outside: the legacy ids per role and the Off choice of every role.

use oxi_core::config::{ConfigFile, Sounds};
use oxi_core::machine::Sound;
use oxi_core::sounds::{Builtin, Choice, OFF};

const ROLES: [Sound; 5] = [
    Sound::Start,
    Sound::Stop,
    Sound::Done,
    Sound::Cancel,
    Sound::Error,
];

// The owner's config has `pop` for start and stop: both used to become the "text ready" sound.
#[test]
fn a_legacy_id_on_start_and_stop_gives_two_different_role_defaults() {
    let start = Choice::parse_for(Sound::Start, "pop");
    let stop = Choice::parse_for(Sound::Stop, "pop");
    assert_eq!(
        start,
        Ok(Choice::Builtin(Builtin::default_for(Sound::Start)))
    );
    assert_eq!(stop, Ok(Choice::Builtin(Builtin::default_for(Sound::Stop))));
    assert_ne!(start, stop);
    assert_ne!(Choice::parse_for(Sound::Done, "pop"), start);
}

#[test]
fn every_role_default_is_what_the_config_defaults_to() {
    let s = Sounds::default();
    for (role, text) in ROLES
        .into_iter()
        .zip([&s.start, &s.stop, &s.done, &s.cancel, &s.error])
    {
        assert_eq!(text, Builtin::default_for(role).id(), "{role:?}");
    }
}

#[test]
fn off_is_a_choice_of_every_role() {
    for role in ROLES {
        assert_eq!(Choice::parse_for(role, OFF), Ok(Choice::Off), "{role:?}");
        assert_eq!(
            Choice::parse_for(role, " Off "),
            Ok(Choice::Off),
            "{role:?}"
        );
    }
}

#[test]
fn off_per_role_loads_without_a_warning_and_survives_a_save() {
    let text = "version = 2\n[sounds]\nstart = \"off\"\nstop = \"off\"\ncancel = \"off\"\nerror = \"off\"\n";
    let file = ConfigFile::parse(text).unwrap();
    assert_eq!(file.warnings, [] as [String; 0]);
    let s = &file.config.sounds;
    assert_eq!((s.start.as_str(), s.stop.as_str()), ("off", "off"));
    assert_eq!((s.cancel.as_str(), s.error.as_str()), ("off", "off"));
    assert_eq!(file.to_toml_string().unwrap(), text);
}
