use super::*;
fn clean(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            match chars.next() {
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&c) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    while let Some(c) = chars.next() {
                        if c == '\x07' || c == '\x1b' && chars.peek() == Some(&'\\') {
                            if c == '\x1b' {
                                chars.next();
                            }
                            break;
                        }
                    }
                }
                _ => {}
            }
        } else if c.is_control() {
            out.push(' ');
        } else {
            out.push(c);
        }
    }
    out.trim().into()
}
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;
fn width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}
fn truncate(s: &str, w: usize) -> String {
    if width(s) <= w {
        return s.into();
    }
    if w == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0;
    for cluster in s.graphemes(true) {
        let next = width(cluster);
        if used + next > w - 1 {
            break;
        }
        out.push_str(cluster);
        used += next;
    }
    out.push('…');
    out
}
fn pad(s: &str, n: usize) -> String {
    let s = truncate(&clean(s), n);
    format!("{s}{}", " ".repeat(n.saturating_sub(width(&s))))
}
fn age(ms: i64) -> String {
    if ms < 60_000 {
        format!("{}s", ms.max(0) / 1000)
    } else if ms < 3_600_000 {
        format!("{}m", ms / 60_000)
    } else if ms < 172_800_000 {
        format!("{}h", ms / 3_600_000)
    } else {
        format!("{}d", ms / 86_400_000)
    }
}
pub(super) fn frame(v: &Value, w: usize, h: usize, age_ms: i64) -> Vec<String> {
    if w == 0 || h == 0 {
        return vec![];
    }
    let time = &s(v, "now")[11..16];
    let left = format!("taskr · {time}");
    let needs = v["needs_you"].as_array().unwrap();
    let attention = v["attention"].as_array().unwrap();
    let campaigns = v["campaigns"].as_array().unwrap();
    let right = match s(v, "verdict") {
        "rolling" => "✓ all rolling".into(),
        "needs_you" => format!("{} need you", needs.len()),
        "attention" => format!(
            "{} to check",
            attention.len() + n(&v["quiet"], "with_backlog") as usize
        ),
        _ => "? unknown".into(),
    };
    let mut full = format!("{left} · {}", age(age_ms));
    if width(&full) + width(&right) + 1 > w {
        full = left;
    }
    if width(&full) + width(&right) + 1 > w {
        full = time.into();
    }
    if width(&full) + width(&right) + 1 > w {
        full.clear();
    }
    let right = truncate(&right, w);
    let header = format!(
        "{full}{}{right}",
        " ".repeat(w - width(&full) - width(&right))
    );
    let rule = "─".repeat(w);
    let mut groups: [Vec<Vec<String>>; 3] = std::array::from_fn(|_| vec![]);
    for need in needs {
        let mut first = format!(
            "» {}  {}",
            clean(s(need, "campaign")),
            age(n(need, "age_ms") + age_ms)
        );
        let mut text = s(need, "text").to_string();
        if need["kind"] == "owner_todo" {
            first = format!(
                "! {}  {}",
                clean(s(need, "campaign")),
                age(n(need, "age_ms") + age_ms)
            );
            let items = need["items"].as_array().unwrap();
            text = items
                .first()
                .and_then(Value::as_str)
                .map(clean)
                .unwrap_or_default();
            if items.len() > 1 {
                text.push_str(&format!("  (+{} more)", items.len() - 1));
            }
        } else {
            if need["blocking"] == true {
                first.push_str("  BLOCKING");
            }
            if !s(need, "pane_id").is_empty() {
                first.push_str(&format!("  → {}", clean(s(need, "pane_id"))));
            }
            if let Some(also) = need["also"]
                .as_array()
                .and_then(|a| a.first())
                .and_then(Value::as_str)
            {
                first.push_str(&format!("  {}", clean(also)));
            }
        }
        groups[0].push(vec![
            truncate(&first, w),
            truncate(&format!("  {}", clean(&text)), w),
        ]);
    }
    for a in attention {
        let (sym, word) = match s(a, "kind") {
            "lane_failed" => ("✗", "failed"),
            "lane_blocked" => ("!", "blocked"),
            "lead_gone" => ("✗", "lead gone"),
            "lead_blocked" => ("!", "lead blocked"),
            "lead_unknown" => ("?", "lead unknown"),
            "owner_unclear" => ("?", "owner unclear"),
            "lane_missing" => ("?", "missing"),
            "results_waiting" => ("⌛", "waiting"),
            "host_stale" => ("⚠", "stale"),
            "daemon_unhealthy" => ("⚠", "daemon"),
            _ => ("?", "unknown"),
        };
        let name = format!("{}/{}", clean(s(a, "campaign")), clean(s(a, "lane")))
            .trim_matches('/')
            .to_string();
        let name = if name.is_empty() {
            if s(a, "host").is_empty() {
                "hub".into()
            } else {
                clean(s(a, "host"))
            }
        } else {
            name
        };
        let when = if !s(a, "since").is_empty() {
            format!(" {}", age(n(a, "age_ms") + age_ms))
        } else if a["kind"] == "lead_unknown" {
            " never".into()
        } else {
            String::new()
        };
        let first = if a["kind"] == "results_waiting" {
            format!(
                "{sym} {}: {} waiting{when}",
                clean(s(a, "recipient")),
                n(a, "count")
            )
        } else {
            format!("{sym} {name}  {word}{when}")
        };
        groups[1].push(vec![
            truncate(&first, w),
            truncate(&format!("  {}", clean(s(a, "text"))), w),
        ]);
    }
    for c in campaigns {
        let last = &c["last"];
        let (text, ms) = if last.is_null() {
            ("", n(c, "activity_age_ms"))
        } else {
            (s(last, "text"), n(last, "age_ms"))
        };
        groups[2].push(vec![truncate(
            &format!(
                "{} {} {} {} {}",
                if n(&c["lanes"], "working") > 0 {
                    "●"
                } else {
                    "○"
                },
                pad(s(c, "name"), 18),
                pad(
                    &format!("{}/{}", n(&c["lanes"], "working"), n(&c["lanes"], "open")),
                    6
                ),
                pad(&age(ms + age_ms), 4),
                clean(text)
            ),
            w,
        )]);
    }
    let quiet = if n(&v["quiet"], "count") > 0 {
        let base = truncate(&format!("· {} quiet", n(&v["quiet"], "count")), w);
        if n(&v["quiet"], "with_backlog") > 0 {
            format!(
                "{base}{}",
                truncate(
                    &format!(" ({} with backlog)", n(&v["quiet"], "with_backlog")),
                    w - width(&base)
                )
            )
        } else {
            base
        }
    } else {
        String::new()
    };
    let mut keep = [groups[0].len(), groups[1].len(), groups[2].len()];
    let compose = |keep: [usize; 3]| {
        let mut out = vec![header.clone(), rule.clone()];
        for (g, items) in groups.iter().enumerate() {
            for item in &items[..keep[g]] {
                out.extend(item.clone());
            }
            let remaining = items.len() - keep[g];
            if remaining > 0 {
                out.push(truncate(
                    &format!(
                        "{}{} more {}",
                        if g == 2 { "· +" } else { "+" },
                        remaining,
                        ["need you", "to check", "campaigns"][g]
                    ),
                    w,
                ));
            }
            let follows = !quiet.is_empty() || groups.iter().skip(g + 1).any(|v| !v.is_empty());
            if g < 2 && !items.is_empty() && follows {
                out.push(rule.clone());
            }
        }
        if !quiet.is_empty() {
            out.push(quiet.clone());
        }
        out
    };
    let mut out = compose(keep);
    while out.len() > h {
        let mut dropped = false;
        for g in (0..3).rev() {
            let before = keep[g];
            while keep[g] > 0 {
                keep[g] -= 1;
                let next = compose(keep);
                if next.len() < out.len() {
                    out = next;
                    dropped = true;
                    break;
                }
            }
            if dropped {
                break;
            }
            keep[g] = before;
        }
        if !dropped {
            break;
        }
    }
    let mut i = out.len();
    while out.len() > h && i > 1 {
        i -= 1;
        if out[i] == rule {
            out.remove(i);
        }
    }
    out.truncate(h);
    out
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn widths() {
        assert_eq!(truncate("東京abcdef", 5), "東京…");
        assert_eq!(clean("a\x1b[31mred\x1b[0m\n"), "ared");
        assert_eq!(pad("abc", 5), "abc  ");
    }
}

#[cfg(test)]
mod parity_tests {
    use super::*;
    fn busy() -> Value {
        json!({"now":"2026-10-07T17:30:00.000Z","verdict":"needs_you","needs_you":[
            {"kind":"owner_ask","campaign":"copilot-modular","age_ms":720000,"blocking":true,"also":["lane failed"],"pane_id":"wN4:p1","text":"Merge #4840 now or wait for M3?"},
            {"kind":"owner_todo","campaign":"booked-vs-resolved","age_ms":3600000,"items":["1. approve prod deploy of #4833","2. approve next rollout"]}],
            "attention":[{"kind":"lane_failed","campaign":"mobile-screens","lane":"impl-tabs","since":"2026-10-07T17:26:00.000Z","age_ms":240000,"text":"lint gate exit 1"},{"kind":"results_waiting","recipient":"orch-hns2","count":3,"since":"2026-10-06T10:30:00.000Z","age_ms":111600000,"text":"reports ready to review"}],
            "campaigns":[{"name":"planner-ui","lanes":{"working":3,"open":5},"last":{"age_ms":120000,"text":"S4 merged"}},{"name":"copilot-modular","lanes":{"working":2,"open":2},"last":{"age_ms":840000,"text":"M1 review ok"}},{"name":"booked-vs-resolved","lanes":{"working":0,"open":1},"activity_age_ms":1680000}],
            "quiet":{"count":13,"with_backlog":4}})
    }
    #[test]
    fn existing_go_frame_goldens_and_all_widths() {
        let mut b = busy();
        b["attention"].as_array_mut().unwrap().push(json!({"kind":"lead_gone","campaign":"planner-ui","since":"2026-10-07T17:25:00.000Z","age_ms":300000,"text":"lead pane wN5:p1 is not in its host's agent list"}));
        let mut rolling = busy();
        rolling["verdict"] = json!("rolling");
        rolling["needs_you"] = json!([]);
        rolling["attention"] = json!([]);
        rolling["quiet"]["with_backlog"] = json!(0);
        for (v, w, h, want) in [
            (
                b.clone(),
                46,
                24,
                include_str!("../../../../../testdata/glance/busy-46.golden"),
            ),
            (
                b,
                80,
                24,
                include_str!("../../../../../testdata/glance/busy-80.golden"),
            ),
            (
                busy(),
                46,
                8,
                include_str!("../../../../../testdata/glance/overflow-8.golden"),
            ),
            (
                busy(),
                20,
                24,
                include_str!("../../../../../testdata/glance/narrow-20.golden"),
            ),
            (
                rolling,
                46,
                24,
                include_str!("../../../../../testdata/glance/rolling-46.golden"),
            ),
        ] {
            assert_eq!(frame(&v, w, h, 2000).join("\n") + "\n", want, "{w}x{h}");
        }
        let hostile = "\x1b[31mRED\x1b[0m\x1b]8;;https://evil.test\x1b\\link\x1b]8;;\x1b\\\t\n\r\x07 世界 👩‍💻 👋🏽";
        assert_eq!(clean(hostile), "REDlink     世界 👩‍💻 👋🏽");
        let mut v = busy();
        v["needs_you"][0]["campaign"] = json!(hostile);
        v["needs_you"][0]["text"] = json!(hostile);
        for w in 0..=120 {
            for h in [0, 1, 2, 4, 8, 24] {
                let rows = frame(&v, w, h, 0);
                assert!(rows.len() <= h);
                for row in rows {
                    assert!(width(&row) <= w, "{row:?}");
                    assert!(!row.contains('\x1b'));
                }
            }
        }
    }
}
