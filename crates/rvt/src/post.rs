use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::ado;
use crate::config::Config;
use crate::gitlab::{self, DiffRefs, FileDiff};
use crate::model::{Finding, Item, Review, Source};

const STALE: &str = "the change has new commits since the review; press r to review it again";

/// How a new-side line appears in a diff hunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Added,
    Context { old: u32 },
}

/// Finds new-side `line` in the hunks of one file's diff. Lines outside every hunk give `None`.
pub fn line_kind(diff: &str, line: u32) -> Option<LineKind> {
    let mut cursor: Option<(u32, u32)> = None;
    for text in diff.lines() {
        if let Some(hunk) = text.strip_prefix("@@ ") {
            cursor = hunk_start(hunk);
            continue;
        }
        let Some((old, new)) = cursor.as_mut() else {
            continue;
        };
        match text.chars().next() {
            Some('+') => {
                if *new == line {
                    return Some(LineKind::Added);
                }
                *new += 1;
            }
            Some('-') => *old += 1,
            Some('\\') => {}
            _ => {
                if *new == line {
                    return Some(LineKind::Context { old: *old });
                }
                *old += 1;
                *new += 1;
            }
        }
    }
    None
}

/// `-12,3 +14,5 @@ fn x()` starts at old line 12 and new line 14.
fn hunk_start(hunk: &str) -> Option<(u32, u32)> {
    let start = |sign: char| {
        hunk.split(' ')
            .find_map(|part| part.strip_prefix(sign))?
            .split(',')
            .next()?
            .parse::<u32>()
            .ok()
    };
    Some((start('-')?, start('+')?))
}

/// The comment text. A comment that is not on a line names the place it is about.
pub fn comment_body(finding: &Finding, on_line: bool) -> String {
    let title = format!("**{}**", finding.title);
    match finding.location().filter(|_| !on_line) {
        Some(location) => format!("{title}\n\n`{location}`\n\n{}", finding.body),
        None => format!("{title}\n\n{}", finding.body),
    }
}

/// The GitLab position for an inline finding, or `None` when its line is not in the MR's diff.
pub fn gitlab_position(finding: &Finding, files: &[FileDiff], refs: &DiffRefs) -> Option<Value> {
    let (path, line) = (finding.file.as_deref()?, finding.line?);
    let file = files.iter().find(|f| f.new_path == path)?;
    let mut position = json!({
        "position_type": "text",
        "base_sha": refs.base_sha,
        "start_sha": refs.start_sha,
        "head_sha": refs.head_sha,
        "old_path": file.old_path,
        "new_path": file.new_path,
        "new_line": line,
    });
    if let LineKind::Context { old } = line_kind(&file.diff, line)? {
        position["old_line"] = json!(old);
    }
    Some(position)
}

pub fn gitlab_discussion(finding: &Finding, files: &[FileDiff], refs: &DiffRefs) -> Value {
    let position = finding
        .inline
        .then(|| gitlab_position(finding, files, refs))
        .flatten();
    match position {
        Some(position) => json!({ "body": comment_body(finding, true), "position": position }),
        None => json!({ "body": comment_body(finding, false) }),
    }
}

pub fn ado_thread(finding: &Finding) -> Value {
    let on_line = finding.inline && finding.file.is_some() && finding.line.is_some();
    let mut thread = json!({
        "status": "active",
        "comments": [{
            "parentCommentId": 0,
            "commentType": "text",
            "content": comment_body(finding, on_line),
        }],
    });
    if let (true, Some(file), Some(line)) = (on_line, &finding.file, finding.line) {
        thread["threadContext"] = json!({
            "filePath": format!("/{}", file.trim_start_matches('/')),
            "rightFileStart": { "line": line, "offset": 1 },
            "rightFileEnd": { "line": line, "offset": 1 },
        });
    }
    thread
}

pub type Outcome = Vec<(usize, Result<(), String>)>;

/// Posts the findings at `picks` and reports how each went, by its index in the review. Nothing
/// is posted when the change has moved past the reviewed commit.
pub fn post(
    cfg: &Config,
    item: &Item,
    review: &Review,
    picks: &[usize],
    progress: &dyn Fn(&str),
) -> Outcome {
    progress("checking");
    let poster = match item.source {
        Source::Ado => ado_poster(cfg, item, review),
        Source::GitLab => gitlab_poster(cfg, item, review),
    };
    let poster = match poster {
        Ok(poster) => poster,
        Err(e) => {
            let error = format!("{e:#}");
            return picks.iter().map(|i| (*i, Err(error.clone()))).collect();
        }
    };
    picks
        .iter()
        .enumerate()
        .map(|(n, &index)| {
            progress(&format!("posting {} of {}", n + 1, picks.len()));
            let result = review
                .findings
                .get(index)
                .context("the finding is no longer in the review")
                .and_then(&poster);
            (index, result.map_err(|e| format!("{e:#}")))
        })
        .collect()
}

type Poster<'a> = Box<dyn Fn(&Finding) -> Result<()> + 'a>;

fn ado_poster<'a>(cfg: &'a Config, item: &'a Item, review: &Review) -> Result<Poster<'a>> {
    let ado = cfg.ado()?;
    if ado::head_commit(ado, item)? != review.head_sha {
        bail!(STALE);
    }
    Ok(Box::new(move |f| {
        ado::create_thread(ado, item, &ado_thread(f))
    }))
}

fn gitlab_poster<'a>(cfg: &'a Config, item: &'a Item, review: &Review) -> Result<Poster<'a>> {
    let gl = cfg.gitlab()?;
    let refs = gitlab::diff_refs(gl, item)?;
    if refs.head_sha != review.head_sha {
        bail!(STALE);
    }
    let files = gitlab::file_diffs(gl, item)?;
    Ok(Box::new(move |f| {
        gitlab::create_discussion(gl, item, &gitlab_discussion(f, &files, &refs))
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Severity;

    const HUNKS: &str = "\
@@ -10,4 +10,5 @@ int parse()
 a
-gone
+b
+c
 d
\\ No newline at end of file
@@ -40,2 +41,2 @@
 x
-y
+z
";

    #[test]
    fn line_kind_tells_added_from_context_lines() {
        assert_eq!(line_kind(HUNKS, 10), Some(LineKind::Context { old: 10 }));
        assert_eq!(line_kind(HUNKS, 11), Some(LineKind::Added));
        assert_eq!(line_kind(HUNKS, 12), Some(LineKind::Added));
        assert_eq!(line_kind(HUNKS, 13), Some(LineKind::Context { old: 12 }));
        assert_eq!(line_kind(HUNKS, 41), Some(LineKind::Context { old: 40 }));
        assert_eq!(line_kind(HUNKS, 42), Some(LineKind::Added));
        assert_eq!(line_kind(HUNKS, 14), None, "between hunks");
        assert_eq!(line_kind(HUNKS, 9), None, "before the first hunk");
    }

    fn finding(file: Option<&str>, line: Option<u32>, inline: bool) -> Finding {
        Finding {
            severity: Severity::High,
            file: file.map(str::to_string),
            line,
            title: "Overflow".into(),
            body: "The length can wrap.".into(),
            inline,
            selected: true,
            posted: false,
        }
    }

    fn refs() -> DiffRefs {
        DiffRefs {
            base_sha: "base".into(),
            start_sha: "start".into(),
            head_sha: "head".into(),
        }
    }

    fn files() -> Vec<FileDiff> {
        vec![FileDiff {
            old_path: "src/old.cpp".into(),
            new_path: "src/parse.cpp".into(),
            diff: HUNKS.into(),
        }]
    }

    #[test]
    fn comment_body_names_the_place_only_off_the_line() {
        let f = finding(Some("src/parse.cpp"), Some(11), true);
        assert_eq!(
            comment_body(&f, true),
            "**Overflow**\n\nThe length can wrap."
        );
        assert_eq!(
            comment_body(&f, false),
            "**Overflow**\n\n`src/parse.cpp:11`\n\nThe length can wrap."
        );
        assert_eq!(
            comment_body(&finding(None, None, false), false),
            "**Overflow**\n\nThe length can wrap."
        );
    }

    #[test]
    fn gitlab_discussions_carry_old_lines_only_for_context_lines() {
        let added = gitlab_discussion(
            &finding(Some("src/parse.cpp"), Some(11), true),
            &files(),
            &refs(),
        );
        assert_eq!(added["position"]["new_line"], 11);
        assert_eq!(added["position"]["old_path"], "src/old.cpp");
        assert_eq!(added["position"]["head_sha"], "head");
        assert!(added["position"].get("old_line").is_none(), "{added}");

        let context = gitlab_discussion(
            &finding(Some("src/parse.cpp"), Some(13), true),
            &files(),
            &refs(),
        );
        assert_eq!(context["position"]["old_line"], 12);
        assert_eq!(context["position"]["new_line"], 13);
    }

    #[test]
    fn gitlab_findings_off_the_diff_become_plain_comments() {
        for f in [
            finding(Some("src/parse.cpp"), Some(14), true),
            finding(Some("src/other.cpp"), Some(3), true),
            finding(Some("src/parse.cpp"), Some(11), false),
        ] {
            let discussion = gitlab_discussion(&f, &files(), &refs());
            assert!(discussion.get("position").is_none(), "{discussion}");
            assert!(
                discussion["body"].as_str().unwrap().contains('`'),
                "names the place: {discussion}"
            );
        }
    }

    #[test]
    fn ado_threads_sit_on_the_right_side_line_when_inline() {
        let inline = ado_thread(&finding(Some("src/parse.cpp"), Some(11), true));
        assert_eq!(inline["threadContext"]["filePath"], "/src/parse.cpp");
        assert_eq!(inline["threadContext"]["rightFileStart"]["line"], 11);
        assert_eq!(inline["threadContext"]["rightFileEnd"]["line"], 11);
        assert_eq!(inline["comments"][0]["parentCommentId"], 0);
        assert_eq!(
            inline["comments"][0]["content"],
            "**Overflow**\n\nThe length can wrap."
        );

        let general = ado_thread(&finding(Some("src/parse.cpp"), Some(90), false));
        assert!(general.get("threadContext").is_none(), "{general}");
        assert!(
            general["comments"][0]["content"]
                .as_str()
                .unwrap()
                .contains("`src/parse.cpp:90`")
        );
    }
}
