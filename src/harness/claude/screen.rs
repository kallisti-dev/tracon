//! What tracon recognises on Claude Code's screen; [`crate::screen`] says how
//! the list uses it. The patterns are tested against real screens in
//! `tests/screens/`, one directory per Claude Code version.

use crate::screen::Sighting;

/// The glyphs Claude Code's spinner cycles through.
const SPINNER_GLYPHS: [char; 6] = ['·', '✢', '✳', '✶', '✻', '✽'];

/// Text only a dialog shows, compared without case.
const DIALOG_MARKERS: [&str; 4] = [
    "esc to cancel",
    "tab to amend",
    "allow once",
    "no, and tell claude what to do differently",
];

/// A dialog outranks a spinner, and a spinner outranks an interrupt notice
/// left above a prompt that is still being worked on.
pub fn read(text: &str) -> Option<Sighting> {
    let lines: Vec<&str> = text.lines().map(str::trim_end).collect();
    if dialog(&lines) {
        Some(Sighting::Prompt)
    } else if lines.iter().any(|line| spinner(line)) {
        Some(Sighting::Working)
    } else if interrupted(&lines) {
        Some(Sighting::Interrupted)
    } else {
        None
    }
}

/// A dialog's footer or options, or a question above numbered choices.
fn dialog(lines: &[&str]) -> bool {
    let lower: Vec<String> = lines.iter().map(|line| line.to_lowercase()).collect();
    lower
        .iter()
        .any(|line| DIALOG_MARKERS.iter().any(|marker| line.contains(marker)))
        || (lower.iter().any(|line| line.contains("do you want"))
            && lines
                .iter()
                .any(|line| line.trim_start().starts_with("❯ 1.")))
}

/// Claude Code's line while it works, at the start of a row: a glyph, a verb
/// and an ellipsis, then a timer once one is due, as in `· Nebulizing…` or
/// `✢ Blanching… (6s · ↓ 258 tokens)`. A finished turn leaves `✻ Crunched
/// for 28s · done 9:19 PM` instead.
fn spinner(line: &str) -> bool {
    let mut chars = line.chars();
    if !chars.next().is_some_and(|c| SPINNER_GLYPHS.contains(&c)) {
        return false;
    }
    let Some(rest) = chars.as_str().strip_prefix(' ') else {
        return false;
    };
    let Some((verb, after)) = rest.trim_start().split_once('…') else {
        return false;
    };
    let words = verb
        .split([' ', '-'])
        .all(|word| !word.is_empty() && word.chars().all(char::is_alphabetic));
    words && (after.is_empty() || (after.starts_with(" (") && after.ends_with(')')))
}

/// The notice a refused prompt or an interrupt leaves, `⎿  Interrupted ·
/// What should Claude do instead?`, in the latest turn: below the last
/// prompt sent and above the input box.
fn interrupted(lines: &[&str]) -> bool {
    let Some(input) = input_box(lines) else {
        return false;
    };
    let turn = &lines[..input];
    let start = turn
        .iter()
        .rposition(|line| line.starts_with('❯'))
        .map_or(0, |index| index + 1);
    turn[start..].iter().any(|line| {
        line.trim_start()
            .strip_prefix('⎿')
            .is_some_and(|rest| rest.trim_start().starts_with("Interrupted"))
    })
}

/// The row of the input box's top border: a rule directly above a line that
/// starts with `❯`.
fn input_box(lines: &[&str]) -> Option<usize> {
    (1..lines.len())
        .rev()
        .find(|&row| lines[row].starts_with('❯') && rule(lines[row - 1]))
        .map(|row| row - 1)
}

fn rule(line: &str) -> bool {
    let line = line.trim();
    !line.is_empty() && line.chars().all(|c| c == '─')
}

#[cfg(test)]
mod tests {
    use super::*;

    macro_rules! screen {
        ($name:literal) => {
            (
                $name,
                include_str!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/tests/screens/claude-2.1.300/",
                    $name,
                    ".txt"
                )),
            )
        };
    }

    #[test]
    fn real_screens_are_read() {
        let cases = [
            (screen!("dialog"), Some(Sighting::Prompt)),
            (screen!("dialog-no-selected"), Some(Sighting::Prompt)),
            (screen!("starting"), Some(Sighting::Working)),
            (screen!("running"), Some(Sighting::Working)),
            (screen!("queued-prompt"), Some(Sighting::Working)),
            (screen!("refused-esc"), Some(Sighting::Interrupted)),
            (screen!("refused-no"), Some(Sighting::Interrupted)),
            (screen!("interrupted-esc"), Some(Sighting::Interrupted)),
            (screen!("interrupted-ctrl-c"), Some(Sighting::Interrupted)),
            (screen!("finished"), None),
        ];
        for ((name, text), expected) in cases {
            assert_eq!(read(text), expected, "{name}");
        }
    }

    #[test]
    fn a_spinner_beside_a_dialog_is_a_prompt() {
        let (_, dialog) = screen!("dialog");
        let both = format!("✶ Kerfuffling… (3s · ↓ 9 tokens)\n{dialog}");
        assert_eq!(read(&both), Some(Sighting::Prompt));
    }

    #[test]
    fn spinner_lines_are_recognised() {
        for line in [
            "✢ Moseying… (1m 5s · ↑ 2.1k tokens · esc to interrupt)",
            "✻ Sautéing…",
            "✶ Re-reading the docs…",
        ] {
            assert!(spinner(line), "{line}");
        }
        for line in [
            "  ✶ Kerfuffling…",
            "✻ Crunched for 28s · done 9:19 PM",
            "- Loading…",
            "✶ Kerfuffling… then more",
            "✶ …",
        ] {
            assert!(!spinner(line), "{line}");
        }
    }
}
