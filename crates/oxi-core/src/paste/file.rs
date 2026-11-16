//! To-file mode: the daily file and the line one dictation adds to it (arch-plan 9.4). Pure; the
//! paste worker does the write.

use crate::config::FileFormat;
use crate::stats::Date;

/// The start of the status warning of a failed write (`<this>: <error kind>`); the settings
/// window finds the line by it.
pub const WRITE_FAILED: &str = "Dictations are not written to the file";

/// `YYYY-MM-DD.md` or `.txt`, the local day.
pub fn file_name(format: FileFormat, day: Date) -> String {
    let ext = match format {
        FileFormat::Md => "md",
        FileFormat::Txt => "txt",
    };
    format!("{day}.{ext}")
}

/// One dictation as it is appended, ending in a line break. `ms_of_day` is the local time.
/// `md`: `- HH:MM text`, a break inside the text continues on a line indented by two spaces.
/// `txt`: `HH:MM  text`, every break inside the text is a space.
pub fn line(format: FileFormat, ms_of_day: u32, text: &str) -> String {
    let (h, m) = (ms_of_day / 3_600_000 % 24, ms_of_day / 60_000 % 60);
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let text = text.trim_end_matches('\n');
    match format {
        FileFormat::Md => format!("- {h:02}:{m:02} {}\n", text.replace('\n', "\n  ")),
        FileFormat::Txt => format!("{h:02}:{m:02}  {}\n", crate::text::flatten(text)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AT: u32 = (9 * 60 + 5) * 60_000 + 59_999;

    #[test]
    fn the_name_is_the_local_day_with_the_format_extension() {
        let day = Date::from_ymd(2026, 11, 3).unwrap();
        assert_eq!(file_name(FileFormat::Md, day), "2026-11-03.md");
        assert_eq!(file_name(FileFormat::Txt, day), "2026-11-03.txt");
    }

    #[test]
    fn a_line_has_the_minute_and_ends_with_one_break() {
        assert_eq!(line(FileFormat::Md, AT, "hello"), "- 09:05 hello\n");
        assert_eq!(line(FileFormat::Txt, AT, "hello"), "09:05  hello\n");
        assert_eq!(line(FileFormat::Txt, 0, "x\n"), "00:00  x\n");
    }

    #[test]
    fn breaks_inside_the_text_are_spaces_in_txt_and_indented_in_md() {
        assert_eq!(line(FileFormat::Txt, AT, "a\r\nb\nc"), "09:05  a b c\n");
        assert_eq!(
            line(FileFormat::Md, AT, "a\r\nb\nc"),
            "- 09:05 a\n  b\n  c\n"
        );
    }
}
