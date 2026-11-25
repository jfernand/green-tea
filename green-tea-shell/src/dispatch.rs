//! The shell's whole-line commands and their one authoritative classification.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Line<'a> {
    Repeat(&'a str),
    Test,
    Trace(&'a str),
    Session,
    Replay,
    Harden,
    Regions,
    Pages(&'a str),
    Command(&'a str),
}

#[derive(Clone, Copy)]
enum WholeLine {
    Repeat,
    Test,
    Trace,
    Session,
    Replay,
    Harden,
    Regions,
    Pages,
}

fn whole_line(word: &str) -> Option<WholeLine> {
    match word {
        "repeat" => Some(WholeLine::Repeat),
        "test" => Some(WholeLine::Test),
        "trace" => Some(WholeLine::Trace),
        "session" => Some(WholeLine::Session),
        "replay" => Some(WholeLine::Replay),
        "harden" => Some(WholeLine::Harden),
        "regions" => Some(WholeLine::Regions),
        "pages" => Some(WholeLine::Pages),
        _ => None,
    }
}

pub fn is_whole_line_word(word: &str) -> bool {
    whole_line(word).is_some()
}

pub fn parse(text: &str) -> Line<'_> {
    let text = text.trim_ascii();
    let (word, rest) = text
        .split_once(' ')
        .unwrap_or((text, ""));
    match whole_line(word) {
        Some(WholeLine::Repeat) if !rest.is_empty() => Line::Repeat(rest.trim_ascii_start()),
        Some(WholeLine::Test) if rest.is_empty() => Line::Test,
        Some(WholeLine::Trace) if !rest.is_empty() => Line::Trace(rest.trim_ascii()),
        Some(WholeLine::Session) if rest.is_empty() => Line::Session,
        Some(WholeLine::Replay) if rest.is_empty() => Line::Replay,
        Some(WholeLine::Harden) if rest.is_empty() => Line::Harden,
        Some(WholeLine::Regions) if rest.is_empty() => Line::Regions,
        Some(WholeLine::Pages) => Line::Pages(rest.trim_ascii()),
        _ => Line::Command(text),
    }
}

#[cfg(test)]
mod tests {
    use super::{Line, is_whole_line_word, parse};

    #[test]
    fn classifies_whole_line_commands_and_arguments() {
        assert_eq!(parse("test"), Line::Test);
        assert_eq!(parse("trace on"), Line::Trace("on"));
        assert_eq!(parse(" repeat  3   echo hi "), Line::Repeat("3   echo hi"));
        assert_eq!(parse("session"), Line::Session);
        assert_eq!(parse("replay"), Line::Replay);
        assert_eq!(parse("harden"), Line::Harden);
        assert_eq!(parse("regions"), Line::Regions);
        assert_eq!(parse("pages"), Line::Pages(""));
        assert_eq!(parse("pages  12"), Line::Pages("12"));
    }

    #[test]
    fn keeps_prefixes_and_malformed_whole_line_calls_as_commands() {
        assert_eq!(parse("testing"), Line::Command("testing"));
        assert_eq!(parse("pagesX"), Line::Command("pagesX"));
        assert_eq!(parse("repeat"), Line::Command("repeat"));
        assert_eq!(parse("trace"), Line::Command("trace"));
        assert_eq!(parse("session extra"), Line::Command("session extra"));
    }

    #[test]
    fn repeat_rejects_each_whole_line_command() {
        for word in [
            "repeat", "test", "trace", "session", "replay", "harden", "regions", "pages",
        ] {
            assert!(is_whole_line_word(word));
        }
        assert!(!is_whole_line_word("testing"));
    }
}
