//! Blame through `jj file annotate`.
//!
//! Jujutsu cannot annotate the merged parents of a working-copy commit that has several,
//! so the working copy itself is annotated and its lines are mapped back to the parents'
//! version with the hunks of `jj diff`.

use std::{collections::HashMap, ops::Range, path::Path};

use anyhow::{Context, Result};

use super::{BlameRange, LineBlame};
use crate::jj::{jj, run, workspace_root};

/// What the blame of a file depends on.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Source {
    parents: Vec<u8>,
    hunks: Vec<Hunk>,
}

/// A change made by the working-copy commit.
#[derive(Debug, PartialEq, Eq)]
struct Hunk {
    /// Working-copy lines without a counterpart in the parents.
    added: Range<u32>,
    /// How much further the following lines are in the parents.
    shift: i64,
}

/// Every field ends with a NUL, which no commit metadata can contain. Lines continuing a
/// hunk leave them empty.
const TEMPLATE: &str = r#"if(first_line_in_hunk,
    commit.change_id() ++ "\0" ++ commit.change_id().shortest(8) ++ "\0"
    ++ commit.commit_id() ++ "\0" ++ commit.change_id().shortest().prefix() ++ "\0"
    ++ commit.commit_id().shortest().prefix() ++ "\0"
    ++ commit.author().name() ++ "\0" ++ commit.author().email() ++ "\0"
    ++ commit.author().timestamp().format("%s %Y-%m-%d") ++ "\0"
    ++ commit.description() ++ "\0" ++ if(commit.current_working_copy(), "@") ++ "\0",
    "\0\0\0\0\0\0\0\0\0\0")"#;
const FIELDS: usize = 10;

fn fileset(root: &Path, file: &Path) -> Result<String> {
    let relative = file
        .strip_prefix(root)?
        .to_str()
        .context("Jujutsu paths are UTF-8")?;
    Ok(format!("root-file:{relative:?}"))
}

pub(super) fn source(file: &Path, trust_full: bool) -> Result<Source> {
    let root = workspace_root(file, trust_full)?;
    Ok(Source {
        parents: run(jj(root).args([
            "log",
            "--no-graph",
            "--revisions",
            "@-",
            "--template",
            r#"commit_id ++ "\n""#,
        ]))?,
        hunks: parse_hunks(&String::from_utf8_lossy(&run(jj(root)
            .args(["diff", "--revision", "@", "--git", "--context", "0", "--"])
            .arg(fileset(root, file)?))?)),
    })
}

/// Read the `@@ -start,removed +start,added @@` headers of a diff without context lines.
fn parse_hunks(diff: &str) -> Vec<Hunk> {
    let side = |side: &str| {
        let (start, len) = side.split_once(',').unwrap_or((side, "1"));
        Some((start.parse::<u32>().ok()?, len.parse::<u32>().ok()?))
    };
    diff.lines()
        .filter_map(|line| {
            let mut sides = line.strip_prefix("@@ -")?.split(' ');
            let (_, removed) = side(sides.next()?)?;
            let (start, added) = side(sides.next()?.strip_prefix('+')?)?;
            // An empty side is located by the line before it.
            let start = if added == 0 { start } else { start - 1 };
            Some(Hunk {
                added: start..start + added,
                shift: i64::from(removed) - i64::from(added),
            })
        })
        .collect()
}

fn base_line(hunks: &[Hunk], line: u32) -> Option<u32> {
    let mut shift = 0;
    for hunk in hunks.iter().take_while(|hunk| hunk.added.start <= line) {
        if line < hunk.added.end {
            return None;
        }
        shift += hunk.shift;
    }
    u32::try_from(i64::from(line) + shift).ok()
}

pub(super) fn blame(
    file: &Path,
    trust_full: bool,
    source: &Source,
) -> Result<(Vec<BlameRange>, Vec<LineBlame>)> {
    let root = workspace_root(file, trust_full)?;
    let output = run(jj(root)
        .args([
            "file",
            "annotate",
            "--revision",
            "@",
            "--template",
            TEMPLATE,
        ])
        // A plain path, unlike the fileset that `jj diff` takes.
        .args(["--".as_ref(), file.strip_prefix(root)?]))?;
    Ok(parse_annotations(
        &String::from_utf8_lossy(&output),
        &source.hunks,
    ))
}

fn parse_annotations(output: &str, hunks: &[Hunk]) -> (Vec<BlameRange>, Vec<LineBlame>) {
    let fields: Vec<_> = output.split('\0').collect();
    let mut indices = HashMap::new();
    let mut commits = Vec::new();
    let mut ranges = Vec::<BlameRange>::new();
    let mut commit = None;
    for (line, record) in fields.chunks_exact(FIELDS).enumerate() {
        if !record[0].is_empty() {
            // The working-copy commit only holds lines that are not in the parents.
            commit = record[9].is_empty().then(|| {
                *indices.entry(record[0]).or_insert_with(|| {
                    commits.push(line_blame(record));
                    commits.len() - 1
                })
            });
        }
        let (Some(commit), Some(line)) = (commit, base_line(hunks, line as u32)) else {
            continue;
        };
        match ranges.last_mut() {
            Some(range) if range.commit == commit && range.lines.end == line => {
                range.lines.end += 1;
            }
            _ => ranges.push(BlameRange {
                lines: line..line + 1,
                commit,
            }),
        }
    }
    (ranges, commits)
}

fn line_blame(record: &[&str]) -> LineBlame {
    let text = |text: &str| Some(text.trim_end().to_owned()).filter(|text| !text.is_empty());
    let (seconds, date) = record[7].split_once(' ').unwrap_or_default();
    let (title, body) = record[8].split_once('\n').unwrap_or((record[8], ""));
    LineBlame {
        commit_id: record[2].to_owned(),
        change_id: Some(record[0].to_owned()),
        unique_prefixes: (record[3].len(), record[4].len()),
        commit_hash: text(record[1]),
        author_name: text(record[5]),
        author_email: text(record[6]),
        commit_date: text(date),
        commit_title: text(title),
        commit_body: text(body.trim_start_matches('\n')),
        timestamp: seconds.parse().ok(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hunks_map_working_copy_lines_to_the_parents() {
        // Parents: a b c d e. Working copy: a new b d e.
        let hunks = parse_hunks("--- a/f\n+++ b/f\n@@ -1,0 +2,1 @@\n+new\n@@ -3 +3,0 @@\n-c\n");
        let lines: Vec<_> = (0..5).map(|line| base_line(&hunks, line)).collect();
        assert_eq!(lines, [Some(0), None, Some(1), Some(3), Some(4)]);
        // A removal before the first line.
        assert_eq!(base_line(&parse_hunks("@@ -1,2 +0,0 @@\n"), 0), Some(2));
    }

    #[test]
    fn working_copy_edits_on_a_merge_keep_the_blame_of_the_parents() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file.txt");
        let jj = |args: &[&str]| {
            std::process::Command::new("jj")
                .current_dir(dir.path())
                .args(args)
                .output()
                .is_ok_and(|output| output.status.success())
        };
        // Not every machine running the tests has Jujutsu.
        if !jj(&["git", "init"]) {
            return;
        }
        let commit = |text: &str, message: &str| {
            std::fs::write(&file, text).unwrap();
            assert!(jj(&["commit", "--message", message]));
        };
        commit("one\ntwo\nthree\nfour\nfive\n", "base\n\ndetails");
        commit("one\ntwo\nthree\nfour\nFIVE\n", "left");
        assert!(jj(&["new", "subject(base)"]));
        commit("ONE\ntwo\nthree\nfour\nfive\n", "right");
        assert!(jj(&["new", "subject(left)", "subject(right)"]));
        std::fs::write(&file, "ONE\nnew\ntwo\nfour\nFIVE\n").unwrap();

        let blame = crate::FileBlame::try_new(file.clone(), true).unwrap();
        // Lines of the merged parents; the third one is deleted in the working copy.
        let titles: Vec<_> = (0..6)
            .map(|line| Some(blame.blame_for_line(line)?.parse_format("{title}|{body}")))
            .collect();
        let title = |title: &str| Some(title.to_owned());
        assert_eq!(
            titles,
            [
                title("right"),
                title("base|details"),
                None,
                title("base|details"),
                title("left"),
                None
            ]
        );
        let right = blame.blame_for_line(0).unwrap();
        for id in [right.change_id.as_ref().unwrap(), &right.commit_id] {
            assert!(jj(&["show", id]), "{id}");
        }
        assert_ne!(right.change_id.as_ref(), Some(&right.commit_id));
        let (change, commit) = right.unique_prefixes;
        assert!((1..32).contains(&change) && (1..40).contains(&commit));

        let cached = crate::FileBlame::try_refresh(file.clone(), true, Some(blame.clone()));
        assert!(std::sync::Arc::ptr_eq(&blame, &cached.unwrap()));
        std::fs::write(&file, "ONE\ntwo\nfour\nFIVE\n").unwrap();
        let edited = crate::FileBlame::try_refresh(file, true, Some(blame.clone())).unwrap();
        assert!(!std::sync::Arc::ptr_eq(&blame, &edited));
        assert_eq!(
            edited.blame_for_line(1).unwrap().parse_format("{title}"),
            "base"
        );
    }

    #[test]
    fn annotations_skip_the_working_copy_and_share_commits() {
        let commit = |id: &str, working_copy: &str| {
            format!("{id}\0{id}\0hash\0{id}\0ha\0Ann\0ann@example.com\x001700000000 2023-11-14\0title\n\nbody\n\0{working_copy}\0")
        };
        let output = [
            commit("a", ""),
            "\0".repeat(FIELDS),
            commit("w", "@"),
            commit("a", ""),
            commit("b", ""),
        ]
        .concat();
        let hunks = parse_hunks("@@ -2,0 +3,1 @@\n");
        let (ranges, commits) = parse_annotations(&output, &hunks);
        let ranges: Vec<_> = ranges
            .iter()
            .map(|range| {
                (
                    range.lines.clone(),
                    commits[range.commit].change_id.as_deref(),
                )
            })
            .collect();
        assert_eq!(ranges, [(0..3, Some("a")), (3..4, Some("b"))]);
        assert_eq!(commits.len(), 2);
        assert_eq!(
            commits[0].parse_format("{commit} {author} {email} {date} {title} {body}"),
            "a Ann ann@example.com 2023-11-14 title body"
        );
    }
}
