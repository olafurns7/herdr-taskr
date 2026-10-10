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
        } else if matches!(c, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
            && c != '\u{200d}'
        {
            // Zero-width and bidi controls: invisible, and they can reorder what follows.
            // The zero-width joiner stays: emoji sequences such as 👩‍💻 need it.
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
pub(super) fn pad(s: &str, n: usize) -> String {
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
/// `brief` is the hub's text: ids on every row, names padded to the longest and never cut,
/// quotes clipped to 60, blank lines between blocks; `--watch` passes false and stays
/// byte-identical.
pub(super) fn frame(v: &Value, w: usize, h: usize, age_ms: i64, brief: bool) -> Vec<String> {
    if w == 0 || h == 0 {
        return vec![];
    }
    let time = &s(v, "now")[11..16];
    let left = format!("taskr · {time}");
    let needs = v["needs_you"].as_array().unwrap();
    let attention = v["attention"].as_array().unwrap();
    let campaigns = v["campaigns"].as_array().unwrap();
    let right = match s(v, "verdict") {
        "rolling" => "✓ no owner action".into(),
        "needs_you" if brief => {
            format!(
                "{} owner ask{}",
                needs.len(),
                if needs.len() == 1 { "" } else { "s" }
            )
        }
        "needs_you" => format!("{} need you", needs.len()),
        "attention" | "unknown" => format!("{} to check", attention.len()),
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
    let header = if brief {
        format!("taskr · {time}Z · cursor={} · {right}", n(v, "cursor"))
    } else {
        format!(
            "{full}{}{right}",
            " ".repeat(w - width(&full) - width(&right))
        )
    };
    let rule = "─".repeat(w);
    // Lines that carry an id are never cut in brief: the hub passes the ids back.
    let cut = |s: String| if brief { s } else { truncate(&s, w) };
    let mut groups: [Vec<Vec<String>>; 3] = std::array::from_fn(|_| vec![]);
    for need in needs {
        let ids = if brief {
            format!("  root={} ask={}", n(need, "root_id"), n(need, "ask_id"))
        } else {
            String::new()
        };
        let mut first = format!(
            "» {}{ids}  {}",
            clean(s(need, "campaign")),
            age(n(need, "age_ms") + age_ms)
        );
        let text = s(need, "text");
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
        groups[0].push(vec![cut(first), truncate(&format!("  {}", clean(text)), w)]);
    }
    for a in attention {
        let (sym, mut word) = match s(a, "kind") {
            "lead_gone" => ("✗", "lead gone".to_string()),
            "lead_blocked" => ("!", "lead blocked".to_string()),
            "lead_unknown" => ("?", "lead unknown".to_string()),
            "lead_idle_results" => ("⌛", "waiting".to_string()),
            "parked_active" => ("?", "parked but active".to_string()),
            "lead_unregistered_silent" => ("?", "unregistered".to_string()),
            "host_stale" => ("⚠", "stale".to_string()),
            "daemon_unhealthy" => ("⚠", "daemon".to_string()),
            _ => ("?", "unknown".to_string()),
        };
        let mut name = clean(s(a, "campaign"));
        if name.is_empty() {
            name = if s(a, "host").is_empty() {
                "hub".into()
            } else {
                clean(s(a, "host"))
            };
        }
        let when = if !s(a, "since").is_empty() {
            format!(" {}", age(n(a, "age_ms") + age_ms))
        } else if a["kind"] == "lead_unknown" {
            " never".into()
        } else {
            String::new()
        };
        if a["kind"] == "lead_idle_results" {
            name = clean(s(a, "recipient"));
            word = format!("idle · {} results", n(a, "count"));
        }
        let first = if brief {
            let mut first = format!("{sym} {name}  ");
            if n(a, "root_id") > 0 {
                first.push_str(&format!("root={}  ", n(a, "root_id")));
            }
            first.push_str(&format!("{word}{when}"));
            if !s(a, "pane_id").is_empty() {
                first.push_str(&format!("  → {}", clean(s(a, "pane_id"))));
            }
            first
        } else {
            let name = truncate(
                &name,
                width(&name)
                    .min(8)
                    .max(w.saturating_sub(width(&format!("{sym}   {word}{when}")))),
            );
            truncate(&format!("{sym} {name}  {word}{when}"), w)
        };
        let text = match s(a, "kind") {
            "lead_unregistered_silent" => "lead silent with open lanes",
            "parked_active" => "new activity; re-park to hold again",
            _ => s(a, "text"),
        };
        groups[1].push(vec![first, truncate(&format!("  {}", clean(text)), w)]);
    }
    let quote = |t: &str| {
        if brief {
            clip(&clean(&line(t)), 60)
        } else {
            clean(t)
        }
    };
    let fill = |s: String, n: usize| format!("{s}{}", " ".repeat(n.saturating_sub(width(&s))));
    let root = |c: &Value| format!("root={}", n(c, "id"));
    let lanes = |c: &Value| {
        let l = &c["lanes"];
        format!("{}/{} working", n(l, "working"), n(l, "open"))
    };
    let widest =
        |f: &dyn Fn(&Value) -> String| campaigns.iter().map(|c| width(&f(c))).max().unwrap_or(0);
    let widths = [
        widest(&|c| clean(s(c, "name"))),
        widest(&root),
        widest(&lanes),
    ];
    for c in campaigns {
        let mut sym = if n(&c["lanes"], "working") > 0 {
            "●"
        } else {
            "○"
        };
        let mut text = s(&c["last"], "text").to_string();
        let mut detail = String::new();
        if !c["owner_note"].is_null() {
            let note = &c["owner_note"];
            let text = owner_value(s(note, "text")).unwrap_or_else(|| s(note, "text").into());
            detail = format!("  {} · {}", age(n(note, "age_ms") + age_ms), quote(&text));
        }
        if c["parked"] == true {
            sym = "·";
            text = format!("parked · {}", age(n(c, "park_age_ms") + age_ms));
            if c["parked_active"] == true {
                text = if w < 60 {
                    "parked active"
                } else {
                    "parked but active"
                }
                .into();
            }
        } else if matches!(s(c, "lead"), "waiting" | "unregistered") {
            if detail.is_empty() && !text.is_empty() {
                detail = format!("  {}", quote(&text));
            }
            text = if c["lead"] == "waiting" {
                "lead waiting"
            } else {
                "unregistered"
            }
            .into();
        }
        let ago = age(n(c, "activity_age_ms") + age_ms);
        if brief && let Some(bytes) = c["tmp_bytes"].as_u64() {
            text = format!(
                "{}{}tmp {}",
                text,
                if text.is_empty() { "" } else { " · " },
                store::tmp::compact(bytes)
            );
        }
        let mut row = vec![if brief {
            format!(
                "{sym} {}  {}  {}  {} {}",
                fill(clean(s(c, "name")), widths[0]),
                fill(root(c), widths[1]),
                fill(lanes(c), widths[2]),
                fill(ago, 4),
                quote(&text)
            )
        } else {
            truncate(
                &format!(
                    "{sym} {} {} {} {}",
                    pad(s(c, "name"), 18),
                    pad(
                        &format!("{}/{}", n(&c["lanes"], "working"), n(&c["lanes"], "open")),
                        6
                    ),
                    pad(&ago, 4),
                    quote(&text)
                ),
                w,
            )
        }];
        if !detail.is_empty() {
            row.push(truncate(&detail, w));
        }
        groups[2].push(row);
    }
    // `unchanged` is set only by brief's `--since`: active campaigns it hid.
    let quiet = [
        ("unchanged", n(v, "unchanged")),
        ("quiet", n(&v["quiet"], "count")),
    ]
    .iter()
    .filter(|(_, count)| *count > 0)
    .map(|(word, count)| format!(" · {count} {word}"))
    .collect::<String>();
    let quiet = truncate(quiet.strip_prefix(' ').unwrap_or_default(), w);
    let pending = (n(v, "owner_notes_pending") > 0).then(|| {
        let word = if n(v, "owner_notes_pending") == 1 {
            "note still carries"
        } else {
            "notes still carry"
        };
        truncate(
            &format!("{} {word} OWNER items", n(v, "owner_notes_pending")),
            w,
        )
    });
    if brief {
        let mut out: Vec<String> = [header].into_iter().chain(pending).collect();
        for (g, items) in groups.iter().enumerate() {
            let mut block: Vec<String> = items.iter().flatten().cloned().collect();
            if g == 2 && !quiet.is_empty() {
                block.push(quiet.clone());
            }
            if !block.is_empty() {
                out.push(String::new());
                out.extend(block);
            }
        }
        for row in &mut out {
            row.truncate(row.trim_end_matches(' ').len());
        }
        return out;
    }
    let mut keep = [groups[0].len(), groups[1].len(), groups[2].len()];
    let compose = |keep: [usize; 3]| {
        let mut out = vec![header.clone(), rule.clone()];
        out.extend(pending.clone());
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
    for row in &mut out {
        row.truncate(row.trim_end_matches(' ').len());
    }
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
            {"kind":"owner_ask","campaign":"checkout-redesign","root_id":12,"age_ms":720000,"blocking":true,"also":["lane failed"],"pane_id":"wDemoA:p1","text":"Merge #104 now or wait for M3?"},
            {"kind":"owner_ask","campaign":"billing-fix","root_id":13,"age_ms":3600000,"text":"approve prod deploy of #103"}],
            "attention":[{"kind":"lead_blocked","campaign":"settings-form","lane":"impl-tabs","since":"2026-10-07T17:26:00.000Z","age_ms":240000,"text":"lint gate exit 1"},{"kind":"lead_idle_results","recipient":"lead-docs","count":3,"since":"2026-10-06T10:30:00.000Z","age_ms":111600000,"text":"reports ready to review"}],
            "campaigns":[{"id":11,"name":"docs-site","activity_age_ms":120000,"activity_id":69850,"lanes":{"working":3,"open":5},"last":{"age_ms":120000,"text":"S4 merged"}},{"id":12,"name":"checkout-redesign","activity_age_ms":840000,"activity_id":69840,"lanes":{"working":2,"open":2},"last":{"age_ms":840000,"text":"M1 review ok"}},{"id":13,"name":"billing-fix","lanes":{"working":0,"open":1},"activity_age_ms":1680000,"activity_id":69790}],
            "quiet":{"count":13}})
    }
    #[test]
    fn existing_go_frame_goldens_and_all_widths() {
        let mut b = busy();
        b["attention"].as_array_mut().unwrap().push(json!({"kind":"lead_gone","campaign":"docs-site","since":"2026-10-07T17:25:00.000Z","age_ms":300000,"text":"lead pane wDemoB:p1 is not in its host's agent list"}));
        let mut rolling = busy();
        rolling["verdict"] = json!("rolling");
        rolling["needs_you"] = json!([]);
        rolling["attention"] = json!([]);
        rolling["quiet"]["with_backlog"] = json!(0);
        let trust = json!({"now":"2026-10-07T17:30:00.000Z","verdict":"attention","needs_you":[],"owner_notes_pending":1,
            "attention":[{"kind":"lead_unregistered_silent","campaign":"checkout-redesign","since":"synthetic","age_ms":10800000},{"kind":"lead_idle_results","recipient":"checkout-redesign","count":3,"since":"synthetic","age_ms":1860000,"text":"worker ready: synthetic report"},{"kind":"parked_active","campaign":"billing-fix","since":"synthetic","age_ms":60000}],
            "campaigns":[{"name":"held","parked":true,"park_age_ms":10800000,"activity_age_ms":60000},{"name":"billing-fix","parked":true,"parked_active":true,"activity_age_ms":60000},{"name":"waiting","lead":"waiting","activity_age_ms":10000,"owner_note":{"text":"OWNER: approve demo","age_ms":10800000}},{"name":"unregistered","lead":"unregistered","activity_age_ms":60000,"owner_note":{"text":"OWNER: review demo","age_ms":18000000}}],"quiet":{"count":0}});
        let unregistered = json!({"now":"2026-10-07T17:30:00.000Z","verdict":"attention","needs_you":[],"campaigns":[],"quiet":{"count":0},
            "attention":[{"kind":"lead_unregistered_silent","campaign":"checkout-redesign","since":"synthetic","age_ms":3600000,"text":"lead silent with open lanes"},{"kind":"lead_unknown","campaign":"never-observed","text":"lead has never been observed"},{"kind":"host_stale","host":"mac","text":"no heartbeat recorded"}]});
        for (v, w, h, want) in [
            (
                trust,
                46,
                30,
                include_str!("../../../../../testdata/glance/trust-46.golden"),
            ),
            (
                unregistered,
                46,
                24,
                include_str!("../../../../../testdata/glance/unregistered-46.golden"),
            ),
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
            assert_eq!(
                frame(&v, w, h, 2000, false).join("\n") + "\n",
                want,
                "{w}x{h}"
            );
        }
        let hostile = "\x1b[31mRED\x1b[0m\x1b]8;;https://evil.test\x1b\\link\x1b]8;;\x1b\\\t\n\r\x07 世界 👩‍💻 👋🏽";
        assert_eq!(clean(hostile), "REDlink     世界 👩‍💻 👋🏽");
        let mut v = busy();
        v["needs_you"][0]["campaign"] = json!(hostile);
        v["needs_you"][0]["text"] = json!(hostile);
        for w in 0..=120 {
            for h in [0, 1, 2, 4, 8, 24] {
                let rows = frame(&v, w, h, 0, false);
                assert!(rows.len() <= h);
                for row in rows {
                    assert!(width(&row) <= w, "{row:?}");
                    assert!(!row.contains('\x1b'));
                }
            }
        }
    }
    /// `busy()` plus ask and root ids, a lead pane, billing-fix exactly 30m old (the
    /// `--since 30m` boundary) and a fourth campaign whose last text has two lines.
    fn brief_fixture() -> Value {
        let mut v = busy();
        v["needs_you"][0]["ask_id"] = json!(70100);
        v["needs_you"][1]["ask_id"] = json!(70101);
        v["attention"][0]["root_id"] = json!(15);
        v["attention"][0]["pane_id"] = json!("wDemoC:p1");
        v["attention"][1]["root_id"] = json!(16);
        v["campaigns"][2]["activity_age_ms"] = json!(1_800_000);
        v["campaigns"].as_array_mut().unwrap().push(json!({"id":14,"name":"release-train","activity_age_ms":7200000,"activity_id":69700,"lanes":{"working":0,"open":2},"last":{"age_ms":7200000,"text":"tag v0.9.3\nsecond line is not quoted"}}));
        v
    }
    #[test]
    fn brief_goldens() {
        let mut rolling = brief_fixture();
        rolling["verdict"] = json!("rolling");
        rolling["needs_you"] = json!([]);
        rolling["attention"] = json!([]);
        let empty = json!({"now":"2026-10-07T17:30:00.000Z","verdict":"rolling","needs_you":[],"attention":[],"campaigns":[],"quiet":{"count":0}});
        for (v, since, want) in [
            (brief_fixture(), "", "brief-busy"),
            (rolling, "", "brief-rolling"),
            // 69790 is billing-fix's own activity id: equal to the cursor is not newer.
            (brief_fixture(), "69790", "brief-since-cursor"),
            (brief_fixture(), "30m", "brief-since-30m"),
            (empty, "", "brief-empty"),
        ] {
            let path = format!(
                "{}/testdata/glance/{want}.golden",
                env!("CARGO_MANIFEST_DIR")
            );
            let got = brief(v, since).unwrap();
            if std::env::var_os("TASKR_UPDATE_GOLDEN").is_some() {
                std::fs::write(&path, &got).unwrap();
            }
            assert_eq!(got, std::fs::read_to_string(&path).unwrap(), "{path}");
        }
        for bad in ["-5m", "0s", "soon", "99999999999999999999", "5"] {
            assert_eq!(
                since_filter(bad).err().map(|e| e.code),
                (bad != "5").then_some(ExitCode::Usage),
                "{bad}"
            );
        }
    }
    /// 2 KB is the owner's target for typical ASCII text, not a guarantee: names, Unicode
    /// quotes and owner-note lines can exceed it.
    #[test]
    fn brief_15_campaigns_under_2k() {
        let quote = "x".repeat(60);
        let campaigns: Vec<Value> = (0..15)
            .map(|i| json!({"id":100_000+i,"name":format!("campaign-with-a-long-name-{i}"),"activity_age_ms":3_540_000,"activity_id":999_990+i,"lanes":{"working":10,"open":10},"last":{"text":quote}}))
            .collect();
        let v = json!({"now":"2026-10-07T17:30:00.000Z","verdict":"rolling","needs_you":[],"attention":[],"campaigns":campaigns,"quiet":{"count":99}});
        let out = brief(v, "").unwrap();
        assert_eq!(out.lines().count(), 18, "{out}");
        assert!(out.len() < 2048, "{} bytes:\n{out}", out.len());
        assert!(
            out.contains("  root=100000  ") && out.contains("  root=100001  "),
            "{out}"
        );
    }
    #[test]
    fn brief_size_follows_primary_text_and_keeps_urgent_rows() {
        let mut v = brief_fixture();
        let plain = brief(v.clone(), "").unwrap();
        assert!(!plain.contains("tmp "));
        v["campaigns"][0]["last"]["text"] = json!("x".repeat(200));
        v["campaigns"][0]["tmp_bytes"] = json!(0);
        let out = brief(v.clone(), "").unwrap();
        assert!(out.contains(&format!("{}…", "x".repeat(59))), "{out}");
        assert!(
            !out.contains("tmp 0"),
            "secondary size must yield to clipping: {out}"
        );
        assert!(out.contains("BLOCKING"), "{out}");
        v["campaigns"][0]["last"]["text"] = json!("primary milestone");
        let out = brief(v, "").unwrap();
        assert!(out.contains("primary milestone · tmp 0"), "{out}");
    }
    #[test]
    fn brief_clips_overlong_quote() {
        let v = json!({"now":"2026-10-07T17:30:00.000Z","verdict":"rolling","needs_you":[],"attention":[],"campaigns":[{"id":14,"name":"overlong-quote","activity_age_ms":7_200_000,"activity_id":69700,"lanes":{"working":1,"open":2},"last":{"age_ms":7_200_000,"text":"x".repeat(200)}}],"quiet":{"count":0}});
        let out = brief(v, "").unwrap();
        assert!(
            out.contains(&format!("{}\u{2026}", "x".repeat(59))),
            "{out}"
        );
        assert!(!out.contains(&"x".repeat(60)), "{out}");
    }
    #[test]
    fn brief_strips_control_zero_width_and_bidi() {
        let mut v = busy();
        v["campaigns"][0]["last"]["text"] =
            json!("\u{202e}evil\u{200b}\u{2066}x\u{2069}\u{200f}\x1b[31m red\x07\u{85}end");
        let out = brief(v, "").unwrap();
        assert!(out.contains("evilx red  end"), "{out}");
        for c in [
            '\u{202e}', '\u{200b}', '\u{2066}', '\u{2069}', '\u{200f}', '\x1b', '\x07', '\u{85}',
        ] {
            assert!(!out.contains(c), "{c:?}");
        }
    }
}
