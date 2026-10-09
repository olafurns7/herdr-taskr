//! `ask --question`: one AskUserQuestion-shaped object, validated and normalized for storage.
use serde_json::{Map, Value, json};
use taskr_core::store::{self, Error};

const MAX_BYTES: usize = 4096;

fn chars(s: &str) -> usize {
    s.chars().count()
}

fn text<'a>(o: &'a Map<String, Value>, key: &str, at: &str) -> Result<Option<&'a str>, Error> {
    match o.get(key) {
        None => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(_) => Err(store::usage(format!(
            "--question: {at}{key} must be a string"
        ))),
    }
}

fn flag(o: &Map<String, Value>, key: &str, at: &str) -> Result<bool, Error> {
    match o.get(key) {
        None => Ok(false),
        Some(Value::Bool(b)) => Ok(*b),
        Some(_) => Err(store::usage(format!(
            "--question: {at}{key} must be true or false"
        ))),
    }
}

fn known(o: &Map<String, Value>, keys: &[&str], at: &str) -> Result<(), Error> {
    match o.keys().find(|k| !keys.contains(&k.as_str())) {
        Some(k) => Err(store::usage(format!(
            "--question: unknown key {at}{k:?} (allowed: {})",
            keys.join(", ")
        ))),
        None => Ok(()),
    }
}

/// Validates the raw JSON and returns the normalized object that is stored in the ask's data.
pub fn parse(raw: &str) -> Result<Value, Error> {
    if raw.len() > MAX_BYTES {
        return Err(store::usage(format!(
            "--question: {} bytes, at most {MAX_BYTES}",
            raw.len()
        )));
    }
    let v: Value = serde_json::from_str(raw)
        .map_err(|e| store::usage(format!("--question: not JSON: {e}")))?;
    let Value::Object(o) = v else {
        return Err(store::usage("--question: want one JSON object"));
    };
    known(&o, &["header", "question", "multiSelect", "options"], "")?;
    let question = text(&o, "question", "")?.unwrap_or("");
    if question.trim().is_empty() {
        return Err(store::usage("--question: question must not be empty"));
    }
    let mut out = json!({"question": question, "multiSelect": flag(&o, "multiSelect", "")?});
    if let Some(h) = text(&o, "header", "")? {
        if chars(h) > 24 {
            return Err(store::usage(format!(
                "--question: header has {} characters, at most 24",
                chars(h)
            )));
        }
        if !h.trim().is_empty() {
            out["header"] = json!(h);
        }
    }
    let options = match o.get("options") {
        Some(Value::Array(a)) => a,
        _ => {
            return Err(store::usage(
                "--question: options must be an array of 2-4 objects",
            ));
        }
    };
    if !(2..=4).contains(&options.len()) {
        return Err(store::usage(format!(
            "--question: {} options, want 2-4",
            options.len()
        )));
    }
    let mut labels: Vec<&str> = vec![];
    let mut recommended = 0;
    let mut normalized = vec![];
    for (i, opt) in options.iter().enumerate() {
        let at = format!("options[{i}].");
        let Value::Object(opt) = opt else {
            return Err(store::usage(format!(
                "--question: options[{i}] must be an object"
            )));
        };
        known(
            opt,
            &["label", "description", "recommended", "preview"],
            &at,
        )?;
        let label = text(opt, "label", &at)?.unwrap_or("");
        if label.trim().is_empty() {
            return Err(store::usage(format!(
                "--question: {at}label must not be empty"
            )));
        }
        if chars(label) > 60 {
            return Err(store::usage(format!(
                "--question: {at}label has {} characters, at most 60",
                chars(label)
            )));
        }
        if labels.contains(&label) {
            return Err(store::usage(format!(
                "--question: {at}label {label:?} repeats an earlier label"
            )));
        }
        labels.push(label);
        let mut n = json!({"label": label});
        if let Some(d) = text(opt, "description", &at)? {
            if chars(d) > 500 {
                return Err(store::usage(format!(
                    "--question: {at}description has {} characters, at most 500",
                    chars(d)
                )));
            }
            if !d.is_empty() {
                n["description"] = json!(d);
            }
        }
        if flag(opt, "recommended", &at)? {
            recommended += 1;
            n["recommended"] = json!(true);
        }
        // `preview` (AskUserQuestion's mockup) is accepted so a question pastes as-is, then dropped.
        normalized.push(n);
    }
    if recommended > 1 {
        return Err(store::usage(format!(
            "--question: {recommended} options are recommended, at most 1"
        )));
    }
    out["options"] = Value::Array(normalized);
    Ok(out)
}

/// The ask's summary: `[context ][header: ]question (A) label; (B) label`.
pub fn summary(context: &str, q: &Value) -> String {
    let mut s = String::new();
    if !context.trim().is_empty() {
        s.push_str(context.trim_end());
        s.push(' ');
    }
    if let Some(h) = q["header"].as_str() {
        s.push_str(h);
        s.push_str(": ");
    }
    s.push_str(q["question"].as_str().unwrap_or(""));
    let options: Vec<String> = q["options"]
        .as_array()
        .into_iter()
        .flatten()
        .zip('A'..)
        .map(|(o, c)| format!("({c}) {}", o["label"].as_str().unwrap_or("")))
        .collect();
    s.push(' ');
    s.push_str(&options.join("; "));
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(n: usize) -> Value {
        Value::Array((0..n).map(|i| json!({"label": format!("L{i}")})).collect())
    }

    fn err(v: Value) -> String {
        let e = parse(&v.to_string()).unwrap_err();
        assert_eq!(e.code, taskr_core::ExitCode::Usage, "{}", e.message);
        e.message
    }

    #[test]
    fn rejects_each_rule() {
        let long = |n| "x".repeat(n);
        for (v, want) in [
            (json!([]), "one JSON object"),
            (json!({"options": opts(2)}), "question must not be empty"),
            (
                json!({"question": " ", "options": opts(2)}),
                "question must not be empty",
            ),
            (
                json!({"question": 1, "options": opts(2)}),
                "question must be a string",
            ),
            (json!({"question": "q"}), "options must be an array"),
            (
                json!({"question": "q", "options": opts(1)}),
                "1 options, want 2-4",
            ),
            (
                json!({"question": "q", "options": opts(5)}),
                "5 options, want 2-4",
            ),
            (
                json!({"question": "q", "options": [{"label":"a"}, "b"]}),
                "options[1] must be an object",
            ),
            (
                json!({"question": "q", "options": [{"label":"a"}, {"label":""}]}),
                "options[1].label must not be empty",
            ),
            (
                json!({"question": "q", "options": [{"label":"a"}, {"description":"d"}]}),
                "options[1].label must not be empty",
            ),
            (
                json!({"question": "q", "options": [{"label":"a"}, {"label":long(61)}]}),
                "label has 61 characters, at most 60",
            ),
            (
                json!({"question": "q", "options": [{"label":"a"}, {"label":"a"}]}),
                "repeats an earlier label",
            ),
            (
                json!({"question": "q", "options": [{"label":"a","description":long(501)}, {"label":"b"}]}),
                "description has 501 characters, at most 500",
            ),
            (
                json!({"question": "q", "options": [{"label":"a","recommended":true}, {"label":"b","recommended":true}]}),
                "2 options are recommended, at most 1",
            ),
            (
                json!({"question": "q", "options": [{"label":"a","recommended":"yes"}, {"label":"b"}]}),
                "recommended must be true or false",
            ),
            (
                json!({"header": long(25), "question": "q", "options": opts(2)}),
                "header has 25 characters, at most 24",
            ),
            (
                json!({"question": "q", "multiSelect": "no", "options": opts(2)}),
                "multiSelect must be true or false",
            ),
            (
                json!({"question": "q", "options": opts(2), "extra": 1}),
                "unknown key \"extra\"",
            ),
            (
                json!({"question": "q", "options": [{"label":"a","lable":"x"}, {"label":"b"}]}),
                "unknown key options[0].\"lable\"",
            ),
            (
                json!({"question": long(4100), "options": opts(2)}),
                "bytes, at most 4096",
            ),
        ] {
            let got = err(v.clone());
            assert!(got.contains(want), "{v}: {got}");
        }
        assert!(parse("{").unwrap_err().message.contains("not JSON"));
    }

    #[test]
    fn normalizes_a_pasted_question() {
        // Multibyte text counts characters, not bytes: 60 × "é" is a valid label.
        let accent = "é".repeat(60);
        let raw = json!({"header":"Auth","question":"Which auth method?","multiSelect":true,
            "options":[{"label":"OAuth","description":"Use the IdP","recommended":true,"preview":"```\nmockup\n```"},
                       {"label":accent,"description":"","recommended":false}]});
        let q = parse(&raw.to_string()).unwrap();
        assert_eq!(
            q,
            json!({"header":"Auth","question":"Which auth method?","multiSelect":true,
                "options":[{"label":"OAuth","description":"Use the IdP","recommended":true},{"label":accent}]})
        );
        let plain =
            parse(r#"{"question":"Go?","options":[{"label":"Yes"},{"label":"No"}]}"#).unwrap();
        assert_eq!(
            plain,
            json!({"question":"Go?","multiSelect":false,"options":[{"label":"Yes"},{"label":"No"}]})
        );
    }

    #[test]
    fn derives_the_summary() {
        let q = parse(r#"{"header":"Auth","question":"Which auth method?","options":[{"label":"OAuth"},{"label":"Keys"},{"label":"SSO"}]}"#).unwrap();
        assert_eq!(
            summary("", &q),
            "Auth: Which auth method? (A) OAuth; (B) Keys; (C) SSO"
        );
        assert_eq!(
            summary("PR #12 needs login. ", &q),
            "PR #12 needs login. Auth: Which auth method? (A) OAuth; (B) Keys; (C) SSO"
        );
        let q = parse(r#"{"question":"Go?","options":[{"label":"Yes"},{"label":"No"}]}"#).unwrap();
        assert_eq!(summary("", &q), "Go? (A) Yes; (B) No");
    }
}
