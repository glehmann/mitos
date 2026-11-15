//! Differences between two versions of a passage, narrowed from lines to words
//! to individual punctuation and whitespace characters like Jujutsu's diffs.

use std::ops::Range;

use editor_core::Rope;
use gix_imara_diff::{Algorithm, Diff, InternedInput, Interner};

// Line counts alone do not bound refinement for minified or generated text.
const MAX_REFINE_BYTES: usize = 256 * 1024;

type Tokenizer = fn(&Rope, Range<usize>) -> Vec<Range<usize>>;
type Match = (Range<usize>, Range<usize>);

fn chars(text: &Rope, range: Range<usize>) -> impl Iterator<Item = (Range<usize>, char)> + '_ {
    let mut position = range.start;
    text.slice(range).chars().map(move |char| {
        let start = position;
        position += char.len_utf8();
        (start..position, char)
    })
}

/// Underscores separate words so that `snake_case` names differ by part.
fn is_word(char: char) -> bool {
    char.is_ascii_alphanumeric() || !char.is_ascii()
}

fn lines(text: &Rope, range: Range<usize>) -> Vec<Range<usize>> {
    let mut lines = Vec::new();
    let mut start = range.start;
    for (char_range, char) in chars(text, range.clone()) {
        if char == '\n' {
            lines.push(start..char_range.end);
            start = char_range.end;
        }
    }
    if start < range.end {
        lines.push(start..range.end);
    }
    lines
}

fn words(text: &Rope, range: Range<usize>) -> Vec<Range<usize>> {
    let mut words: Vec<Range<usize>> = Vec::new();
    for (range, char) in chars(text, range) {
        if !is_word(char) {
            continue;
        }
        match words.last_mut() {
            Some(word) if word.end == range.start => word.end = range.end,
            _ => words.push(range),
        }
    }
    words
}

fn separators(text: &Rope, range: Range<usize>) -> Vec<Range<usize>> {
    chars(text, range)
        .filter(|(_, char)| !is_word(*char))
        .map(|(range, _)| range)
        .collect()
}

/// Tokens common to both ranges, one pair each: tokens need not be contiguous.
fn common(text: &Rope, left: Range<usize>, right: Range<usize>, tokenize: Tokenizer) -> Vec<Match> {
    let (left, right) = (tokenize(text, left), tokenize(text, right));
    if left.is_empty() || right.is_empty() {
        return Vec::new();
    }
    let mut input = InternedInput {
        before: Vec::new(),
        after: Vec::new(),
        interner: Interner::new(left.len() + right.len()),
    };
    // Borrowed strings hash and compare faster than tree-backed slices.
    let token = |range: &Range<usize>| super::line_token(text.slice(range.clone()));
    input.update_before(left.iter().map(token));
    input.update_after(right.iter().map(token));
    let diff = Diff::compute(Algorithm::Histogram, &input);
    let mut common = Vec::new();
    let (mut before, mut after) = (0, 0);
    let end = (left.len(), right.len());
    for (hunk_before, hunk_after) in diff
        .hunks()
        .map(|hunk| {
            (
                hunk.before.start as usize..hunk.before.end as usize,
                hunk.after.start as usize..hunk.after.end as usize,
            )
        })
        .chain([(end.0..end.0, end.1..end.1)])
    {
        common.extend(
            left[before..hunk_before.start]
                .iter()
                .cloned()
                .zip(right[after..hunk_after.start].iter().cloned()),
        );
        (before, after) = (hunk_before.end, hunk_after.end);
    }
    common
}

/// Byte ranges of `left` and of `right` that the other does not contain.
/// Oversized passages are not compared.
pub fn refine(
    text: &Rope,
    left: Range<usize>,
    right: Range<usize>,
) -> (Vec<Range<usize>>, Vec<Range<usize>>) {
    if left.len() + right.len() > MAX_REFINE_BYTES {
        return Default::default();
    }
    // Empty matches at both ends turn leading and trailing differences into gaps.
    let mut matches = vec![(left.start..left.start, right.start..right.start)];
    matches.extend(common(text, left.clone(), right.clone(), lines));
    matches.push((left.end..left.end, right.end..right.end));
    for tokenize in [words as Tokenizer, separators] {
        let mut refined = vec![matches[0].clone()];
        for pair in matches.windows(2) {
            let ((left, right), next) = (&pair[0], &pair[1]);
            if left.end < next.0.start && right.end < next.1.start {
                refined.extend(common(
                    text,
                    left.end..next.0.start,
                    right.end..next.1.start,
                    tokenize,
                ));
            }
            refined.push(next.clone());
        }
        matches = refined;
    }
    let mut differences = (Vec::new(), Vec::new());
    for pair in matches.windows(2) {
        let ((left, right), next) = (&pair[0], &pair[1]);
        if left.end < next.0.start {
            differences.0.push(left.end..next.0.start);
        }
        if right.end < next.1.start {
            differences.1.push(right.end..next.1.start);
        }
    }
    differences
}

#[cfg(test)]
mod tests {
    use super::*;

    fn differences(left: &str, right: &str) -> (Vec<String>, Vec<String>) {
        let text = Rope::from(format!("{left}{right}"));
        let (removed, added) = refine(&text, 0..left.len(), left.len()..text.len());
        let strings = |ranges: Vec<Range<usize>>| {
            ranges
                .into_iter()
                .map(|range| text.slice(range).to_string())
                .collect()
        };
        (strings(removed), strings(added))
    }

    #[test]
    fn narrows_differences_to_words_then_separators() {
        for (left, right, removed, added) in [
            ("hello world\n", "hello world\n", &[][..], &[][..]),
            ("hello world\n", "hello rust\n", &["world"], &["rust"]),
            ("foo bar\n", "baz qux\n", &["foo", "bar"], &["baz", "qux"]),
            ("", "added\n", &[], &["added\n"]),
            ("removed\n", "", &["removed\n"], &[]),
            // Unchanged lines anchor the comparison of the lines between them.
            (
                "same\none two\nend\n",
                "same\none 2\nextra\nend\n",
                &["two"],
                &["2", "extra\n"],
            ),
            ("old_name()\n", "new_name()\n", &["old"], &["new"]),
            ("f(a, b)\n", "f(a,  b);\n", &[], &[" ", ";"]),
            ("x = None\n}\n", "x = None\n    }\n", &[], &["    "]),
            ("naïve café\n", "naïve thé\n", &["café"], &["thé"]),
        ] {
            let expected = |strings: &[&str]| -> Vec<String> {
                strings.iter().map(|string| string.to_string()).collect()
            };
            assert_eq!(
                differences(left, right),
                (expected(removed), expected(added)),
                "{left:?} → {right:?}"
            );
        }
    }

    #[test]
    fn oversized_passages_are_not_compared() {
        let left = "a\n".repeat(MAX_REFINE_BYTES / 2);
        assert_eq!(differences(&left, "b\n"), Default::default());
    }

    #[test]
    fn reindented_and_rewrapped_code_differs_by_indentation_and_changed_words() {
        let base = "\
let location = if args.destination.is_none()
    && args.insert_after.is_none()
{
    None
} else {
    Some(location(args.destination.as_deref(), \"commits\")?)
};
";
        let rewrapped = "\
let location =
    if args.onto.is_none() && args.insert_after.is_none() {
        None
    } else {
        Some(location(args.onto.as_deref(), \"commits\")?)
    };
";
        let strings = |strings: &[&str]| -> Vec<String> {
            strings.iter().map(|string| string.to_string()).collect()
        };
        assert_eq!(
            differences(base, rewrapped),
            (
                strings(&["destination", "\n   ", "\n", "destination"]),
                strings(&["\n   ", "onto", " ", "    ", "    ", "    ", "onto", "    "]),
            )
        );
        assert_eq!(
            differences(base, &base.replace("commits", "revisions")),
            (strings(&["commits"]), strings(&["revisions"]))
        );
    }
}
