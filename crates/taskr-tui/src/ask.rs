//! Finds the options in an ask's text: `(A) ...; (B) ...`, as leads write them today.
//! A heuristic over free text, so the dialog always offers "write your own answer" too.

#[derive(Debug, PartialEq)]
pub(crate) struct Opt {
    pub key: char,
    pub text: String,
    pub recommended: bool,
}

#[derive(Debug, PartialEq)]
pub(crate) struct Parsed {
    /// The ask without its option list: what comes before it, then what comes after.
    pub context: String,
    pub options: Vec<Opt>,
}

pub(crate) fn parse(text: &str) -> Parsed {
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
                recommended,
            }
        })
        .collect();
    let context = format!("{}\n{}", text[..marks[0]].trim(), text[end..].trim());
    Parsed {
        context: context.trim().to_string(),
        options,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_in_a_lead_ask() {
        let p = parse(
            "PR #212 is ready (close warns). (A) merge and release now [recommended]; (B) merge, hold the release.",
        );
        assert_eq!(p.context, "PR #212 is ready (close warns).");
        assert_eq!(
            p.options,
            [
                Opt {
                    key: 'A',
                    text: "merge and release now".into(),
                    recommended: true
                },
                Opt {
                    key: 'B',
                    text: "merge, hold the release".into(),
                    recommended: false
                },
            ]
        );
    }

    #[test]
    fn prose_after_the_options_stays_in_the_context() {
        let p = parse(
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
            let p = parse(text);
            assert!(p.options.len() != 1, "{text}");
        }
        assert!(parse("Which host (A) or the other?").options.is_empty());
        assert_eq!(parse("é(A)ü(B)ö").options[1].text, "ö");
    }
}
