//! The prompt fingerprint behind a guarded `PaneSend`: a short hash of what a
//! pane is asking right now. The client fingerprints its mirror of the screen,
//! the server re-fingerprints the live grid at send time, and a mismatch means
//! the prompt moved on — so both sides must cut the same rows and hash them the
//! same way, across processes and builds (std's `DefaultHasher` promises
//! neither, hence FNV-1a inline).

use std::ops::Range;

/// How many of the screen's last non-empty rows the fingerprint covers. Twelve
/// spans an agent's whole permission dialog — the command or file it asks about,
/// the question, and the option list — so two prompts offering the same options
/// still differ, while the transcript further up (which reflows and carries live
/// status) stays out and cannot churn the fingerprint of a prompt that is
/// sitting still.
pub const PROMPT_LINES: usize = 12;

/// The screen rows the fingerprint covers: from the `PROMPT_LINES`-th last
/// non-empty row through the last non-empty one, blank rows between included
/// (the fingerprint skips them; a peek shows them for fidelity). Empty for a
/// blank screen. A row whose text is only whitespace counts as blank, and
/// trailing whitespace never counts: a written space and an empty cell look
/// alike, and a client's mirror — rebuilt from formatted snapshots and diffs,
/// not the original output — is not promised to keep the two apart.
pub fn prompt_rows<S: AsRef<str>>(rows: &[S]) -> Range<usize> {
    let filled: Vec<usize> = (0..rows.len())
        .filter(|&i| !rows[i].as_ref().trim_end().is_empty())
        .collect();
    let Some(&last) = filled.last() else {
        return 0..0;
    };
    filled[filled.len().saturating_sub(PROMPT_LINES)]..last + 1
}

/// The fingerprint of `rows` (one string per visible screen row, top to
/// bottom): FNV-1a 64 over the non-blank rows of `prompt_rows`, each with its
/// trailing whitespace trimmed, joined by `\n`, as 16 lowercase hex digits.
pub fn prompt_fingerprint<S: AsRef<str>>(rows: &[S]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let lines = rows[prompt_rows(rows)]
        .iter()
        .map(|row| row.as_ref().trim_end())
        .filter(|line| !line.is_empty());
    for (i, line) in lines.enumerate() {
        let sep: &[u8] = if i == 0 { b"" } else { b"\n" };
        for &byte in sep.iter().chain(line.as_bytes()) {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
    }
    format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_the_published_fnv1a_vectors() {
        // Pinned: a daemon and a client from different builds must agree, so
        // the algorithm can never drift silently.
        assert_eq!(prompt_fingerprint::<&str>(&[]), "cbf29ce484222325");
        assert_eq!(prompt_fingerprint(&["a"]), "af63dc4c8601ec8c");
        assert_eq!(prompt_fingerprint(&["foobar"]), "85944171f73967e8");
        assert_eq!(prompt_fingerprint(&["1. Yes", "2. No"]), "380ae48ff38db878");
    }

    #[test]
    fn ignores_blank_rows_and_trailing_whitespace() {
        let clean = prompt_fingerprint(&["Proceed?", "1. Yes", "2. No"]);
        assert_eq!(
            prompt_fingerprint(&["", "Proceed?   ", "", "  ", "1. Yes\t", "2. No", "", ""]),
            clean
        );
        // Leading whitespace is content: indentation is part of what is shown.
        assert_ne!(
            prompt_fingerprint(&["  Proceed?", "1. Yes", "2. No"]),
            clean
        );
    }

    #[test]
    fn changes_when_the_prompt_changes() {
        let asked = prompt_fingerprint(&["Run rm -rf build?", "❯ 1. Yes", "  2. No"]);
        assert_ne!(
            prompt_fingerprint(&["Run rm -rf dist?", "❯ 1. Yes", "  2. No"]),
            asked,
            "the subject of the question counts"
        );
        assert_ne!(
            prompt_fingerprint(&["Run rm -rf build?", "  1. Yes", "❯ 2. No"]),
            asked,
            "the highlighted option counts"
        );
        // Rows join with a separator, so a re-wrap is a change too.
        assert_ne!(
            prompt_fingerprint(&["ab", "c"]),
            prompt_fingerprint(&["a", "bc"])
        );
    }

    #[test]
    fn covers_only_the_last_prompt_lines_non_empty_rows() {
        let mut screen: Vec<String> = (0..20).map(|i| format!("line {i}")).collect();
        screen.insert(10, String::new());
        let fp = prompt_fingerprint(&screen);
        screen[0] = "the transcript scrolled".into();
        assert_eq!(
            prompt_fingerprint(&screen),
            fp,
            "rows above the region are out"
        );
        screen[20] = "line 19 changed".into();
        assert_ne!(prompt_fingerprint(&screen), fp, "the last row is in");
    }

    #[test]
    fn prompt_rows_spans_the_region_blank_rows_included() {
        assert_eq!(prompt_rows::<&str>(&[]), 0..0);
        assert_eq!(prompt_rows(&["", "  ", ""]), 0..0);
        assert_eq!(prompt_rows(&["", "a", "", "b", "", ""]), 1..4);
        let tall: Vec<String> = (0..30).map(|i| format!("{i}")).collect();
        assert_eq!(prompt_rows(&tall), 18..30);
    }
}
