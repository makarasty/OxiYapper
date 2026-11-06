use oxi_core::paths::{BaseDirs, Layout, Os, layout, portable_layout};
use std::path::PathBuf;

fn p(s: &str) -> PathBuf {
    s.split('/').collect()
}

fn base() -> BaseDirs {
    BaseDirs {
        config: p("cfg"),
        data: p("dat"),
        logs: p("log"),
    }
}

#[test]
fn layout_per_os() {
    let cases = [
        (
            Os::Windows,
            Layout {
                config_dir: p("cfg/oxi-yapper/config"),
                data_dir: p("dat/oxi-yapper/data"),
                logs_dir: p("dat/oxi-yapper/data/logs"),
            },
        ),
        (
            Os::MacOs,
            Layout {
                config_dir: p("cfg/com.example.app"),
                data_dir: p("dat/com.example.app"),
                logs_dir: p("log/oxi-yapper"),
            },
        ),
        (
            Os::Linux,
            Layout {
                config_dir: p("cfg/oxi-yapper"),
                data_dir: p("dat/oxi-yapper"),
                logs_dir: p("log/oxi-yapper/logs"),
            },
        ),
    ];
    for (os, expected) in cases {
        assert_eq!(layout(os, &base(), "com.example.app"), expected, "{os:?}");
    }
}

#[test]
fn portable_puts_everything_under_exe_data() {
    let l = portable_layout(&p("app/bin"));
    assert_eq!(l.config_dir, p("app/bin/data"));
    assert_eq!(l.data_dir, p("app/bin/data"));
    assert_eq!(l.logs_dir, p("app/bin/data/logs"));
}

#[test]
fn file_locations_follow_the_table() {
    let l = layout(Os::Windows, &base(), "unused");
    assert_eq!(l.config_file(), p("cfg/oxi-yapper/config/config.toml"));
    assert_eq!(
        l.dictionary_file(),
        p("cfg/oxi-yapper/config/dictionary.txt")
    );
    assert_eq!(l.replace_file(), p("cfg/oxi-yapper/config/replace.txt"));
    assert_eq!(l.state_file(), p("dat/oxi-yapper/data/state.toml"));
    assert_eq!(l.models_dir(), p("dat/oxi-yapper/data/models"));
    assert_eq!(l.history_dir(), p("dat/oxi-yapper/data/history"));
}

#[test]
fn macos_config_and_data_share_a_dir_when_bases_match() {
    let b = BaseDirs {
        data: p("cfg"),
        ..base()
    };
    let l = layout(Os::MacOs, &b, "id");
    assert_eq!(l.config_file().parent(), l.state_file().parent());
}
