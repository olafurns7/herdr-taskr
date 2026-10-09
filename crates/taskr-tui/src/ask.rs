//! An ask's options: a structured ask's own (`taskr ask --question`), or else those found
//! in its text, `(A) ...; (B) ...`, as leads write them today. The text is a heuristic, so
//! the dialog always offers "write your own answer" too.

use crate::model::Need;

#[derive(Debug, Default, PartialEq)]
pub(crate) struct Opt {
    pub key: char,
    pub text: String,
    /// A structured option's description; empty for one parsed from text.
    pub description: String,
    pub recommended: bool,
}

#[derive(Debug, PartialEq)]
pub(crate) struct Parsed {
    /// The ask without its option list: what comes before it, then what comes after.
    pub context: String,
    pub options: Vec<Opt>,
    /// The options came from the ask's question, not from its text.
    pub structured: bool,
    /// More than one option can be picked.
    pub multi: bool,
}

/// The ask's options: its question's when it has one, the text's otherwise.
pub(crate) fn parse(a: &Need) -> Parsed {
    let mut p = parse_text(&a.text);
    if let Some(q) = &a.question {
        // The summary ends in the options' labels (Q1); the heuristic still strips them.
        p.options = q
            .options
            .iter()
            .zip('A'..)
            .map(|(o, key)| Opt {
                key,
                text: o.label.clone(),
                description: o.description.clone(),
                recommended: o.recommended,
            })
            .collect();
        (p.structured, p.multi) = (true, q.multi_select);
    }
    p
}

/// The ask's mark on its row: ◆ structured, ◇ options parsed from its text, none without.
pub(crate) fn mark(a: &Need) -> &'static str {
    match parse(a) {
        p if p.structured => "◆",
        p if !p.options.is_empty() => "◇",
        _ => " ",
    }
}

/// The answer as sent: `A: label`, picks joined as `A: OAuth; C: SSO`, then ` — note`.
pub(crate) fn answer(options: &[Opt], picked: &[usize], note: &str) -> String {
    let mut out = picked
        .iter()
        .filter_map(|&i| options.get(i))
        .map(|o| format!("{}: {}", o.key, o.text))
        .collect::<Vec<_>>()
        .join("; ");
    if !note.trim().is_empty() {
        out = format!("{out} — {}", note.trim_start());
    }
    out
}

pub(crate) fn parse_text(text: &str) -> Parsed {
    // `(A)`, `(B)`, ... in order. A later `(B)` in prose ("otherwise I take (B)") is not an option.
    let mut marks = vec![];
    let mut want = b'A';
    for (i, _) in text.match_indices('(') {
        let b = text.as_bytes();
        if b.get(i + 1) == Some(&want) && b.get(i + 2) == Some(&b')') {
            marks.push(i);
            want += 1;
        }
    }
    if marks.len() < 2 {
        return Parsed {
            context: text.trim().to_string(),
            options: vec![],
            structured: false,
            multi: false,
        };
    }
    let last = marks[marks.len() - 1];
    let end = text[last..].find('\n').map_or(text.len(), |n| last + n);
    let options = marks
        .iter()
        .enumerate()
        .map(|(n, &start)| {
            let body = &text[start + 3..marks.get(n + 1).copied().unwrap_or(end)];
            let recommended = body.contains("[recommended]");
            let body = body.replace("[recommended]", "");
            Opt {
                key: (b'A' + n as u8) as char,
                text: body
                    .trim()
                    .trim_end_matches([';', '.', ',', ' '])
                    .to_string(),
                description: String::new(),
                recommended,
            }
        })
        .collect();
    let context = format!("{}\n{}", text[..marks[0]].trim(), text[end..].trim());
    Parsed {
        context: context.trim().to_string(),
        options,
        structured: false,
        multi: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_in_a_lead_ask() {
        let p = parse_text(
            "PR #212 is ready (close warns). (A) merge and release now [recommended]; (B) merge, hold the release.",
        );
        assert_eq!(p.context, "PR #212 is ready (close warns).");
        assert_eq!(
            p.options,
            [
                Opt {
                    key: 'A',
                    text: "merge and release now".into(),
                    recommended: true,
                    ..Opt::default()
                },
                Opt {
                    key: 'B',
                    text: "merge, hold the release".into(),
                    ..Opt::default()
                },
            ]
        );
    }

    #[test]
    fn prose_after_the_options_stays_in_the_context() {
        let p = parse_text(
            "Stopped.\nQuestion: (A) merge four (#1, #2); (B) wait.\nNo answer in 15 minutes: I take (B).",
        );
        assert_eq!(p.options.len(), 2);
        assert_eq!(p.options[1].text, "wait");
        assert_eq!(
            p.context,
            "Stopped.\nQuestion:\nNo answer in 15 minutes: I take (B)."
        );
    }

    #[test]
    fn free_text_has_no_options_and_odd_text_does_not_panic() {
        for text in [
            "Which host (A) or the other?",
            "",
            "(",
            "(A",
            "é(A)ü(B)ö",
            "(B) first (A) second",
        ] {
            let p = parse_text(text);
            assert!(p.options.len() != 1, "{text}");
        }
        assert!(
            parse_text("Which host (A) or the other?")
                .options
                .is_empty()
        );
        assert_eq!(parse_text("é(A)ü(B)ö").options[1].text, "ö");
    }

    fn structured(multi: bool) -> Need {
        let question = serde_json::json!({
            "header": "Auth", "question": "Which sign-in?", "multiSelect": multi,
            "options": [
                {"label": "OAuth", "description": "Hosted login.", "recommended": true},
                {"label": "Keys"},
                {"label": "SSO", "description": "Through the directory."},
            ],
        });
        Need {
            text: "Rollout is next. Auth: Which sign-in? (A) OAuth; (B) Keys; (C) SSO".into(),
            question: serde_json::from_value(question).ok(),
            ..Need::default()
        }
    }

    #[test]
    fn a_question_gives_the_options_and_skips_the_text() {
        let p = parse(&structured(true));
        assert!(p.structured && p.multi);
        assert_eq!(p.context, "Rollout is next. Auth: Which sign-in?");
        assert_eq!(p.options.len(), 3);
        assert_eq!(p.options[0].description, "Hosted login.");
        assert!(p.options[0].recommended && !p.options[2].recommended);
        assert_eq!((p.options[2].key, p.options[2].text.as_str()), ('C', "SSO"));
        // A text-only ask is parsed as before.
        let plain = Need {
            text: "Go? (A) yes [recommended]; (B) no".into(),
            ..Need::default()
        };
        assert_eq!(parse(&plain), parse_text(&plain.text));
        assert_eq!(
            [
                mark(&structured(false)),
                mark(&plain),
                mark(&Need::default())
            ],
            ["◆", "◇", " "]
        );
    }

    #[test]
    fn the_answer_text_is_canonical() {
        let p = parse(&structured(true));
        assert_eq!(answer(&p.options, &[0], ""), "A: OAuth");
        assert_eq!(answer(&p.options, &[0, 2], ""), "A: OAuth; C: SSO");
        assert_eq!(
            answer(&p.options, &[1], "rotate them monthly"),
            "B: Keys — rotate them monthly"
        );
        assert_eq!(answer(&p.options, &[0, 2], "  "), "A: OAuth; C: SSO");
    }
}
