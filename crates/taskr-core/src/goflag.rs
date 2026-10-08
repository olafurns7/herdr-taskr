//! Go flag-package parsing, including taskr's interspersed positional arguments.
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug)]
pub enum Value {
    Bool(bool),
    Int(i64),
    String(String),
    Strings(Vec<String>),
    Duration(i64),
}
#[derive(Clone, Debug)]
struct Flag {
    value: Value,
    default: Value,
    usage: String,
}
#[derive(Clone, Debug)]
pub struct FlagSet {
    pub name: String,
    flags: BTreeMap<String, Flag>,
    pub given: BTreeSet<String>,
    pub positional: Vec<String>,
}
impl FlagSet {
    pub fn new(name: &str, json: bool) -> Self {
        let mut fs = Self {
            name: name.into(),
            flags: BTreeMap::new(),
            given: BTreeSet::new(),
            positional: vec![],
        };
        fs.bool("json", json, "legacy JSON output");
        fs.bool("help", false, "print command help");
        fs.bool("h", false, "print command help");
        fs
    }
    fn add(&mut self, name: &str, value: Value, usage: &str) -> &mut Self {
        self.flags.insert(
            name.into(),
            Flag {
                default: value.clone(),
                value,
                usage: usage.into(),
            },
        );
        self
    }
    pub fn bool(&mut self, name: &str, default: bool, usage: &str) -> &mut Self {
        self.add(name, Value::Bool(default), usage)
    }
    pub fn int(&mut self, name: &str, default: i64, usage: &str) -> &mut Self {
        self.add(name, Value::Int(default), usage)
    }
    pub fn string(&mut self, name: &str, default: &str, usage: &str) -> &mut Self {
        self.add(name, Value::String(default.into()), usage)
    }
    pub fn strings(&mut self, name: &str, usage: &str) -> &mut Self {
        self.add(name, Value::Strings(vec![]), usage)
    }
    pub fn duration(&mut self, name: &str, nanos: i64, usage: &str) -> &mut Self {
        self.add(name, Value::Duration(nanos), usage)
    }
    pub fn get_bool(&self, name: &str) -> bool {
        matches!(
            self.flags.get(name).map(|f| &f.value),
            Some(Value::Bool(true))
        )
    }
    pub fn get_int(&self, name: &str) -> i64 {
        match &self.flags[name].value {
            Value::Int(v) | Value::Duration(v) => *v,
            _ => panic!("not integer"),
        }
    }
    pub fn get_string(&self, name: &str) -> &str {
        match &self.flags[name].value {
            Value::String(v) => v,
            _ => panic!("not string"),
        }
    }
    pub fn get_strings(&self, name: &str) -> &[String] {
        match &self.flags[name].value {
            Value::Strings(v) => v,
            _ => panic!("not strings"),
        }
    }
    pub fn was_set(&self, name: &str) -> bool {
        self.given.contains(name)
    }
    pub fn help(&self) -> bool {
        self.get_bool("help") || self.get_bool("h")
    }
    pub fn json(&self) -> bool {
        self.get_bool("json")
    }
    pub fn parse(&mut self, args: &[String], min: usize, max: usize) -> Result<(), String> {
        let mut i = 0;
        while i < args.len() {
            if args[i] == "--" {
                self.positional.extend_from_slice(&args[i + 1..]);
                break;
            }
            if !args[i].starts_with('-') || args[i] == "-" {
                self.positional.push(args[i].clone());
                i += 1;
                continue;
            }
            // taskr preflights one contiguous flag chunk before calling flag.Parse.
            let start = i;
            while i < args.len() && args[i].starts_with('-') && args[i] != "-" && args[i] != "--" {
                let (name, inline) = split(&args[i]);
                if let Some(f) = self.flags.get(name)
                    && !matches!(f.value, Value::Bool(_))
                    && inline.is_none()
                    && i + 1 < args.len()
                {
                    i += 1;
                }
                i += 1;
            }
            let end = i;
            i = start;
            while i < end {
                let (name, inline) = split(&args[i]);
                if let Some(f) = self.flags.get(name)
                    && !matches!(f.value, Value::Bool(_))
                {
                    let value = inline
                        .or_else(|| args.get(i + 1).filter(|_| i + 1 < end).map(String::as_str))
                        .unwrap_or("");
                    if value.starts_with('-') {
                        let (other, _) = split(value);
                        if self.flags.contains_key(other) {
                            return Err(format!("--{name} needs a value, got flag --{other}"));
                        }
                    }
                    if inline.is_none() && i + 1 < end {
                        i += 1;
                    }
                }
                i += 1;
            }
            i = start;
            while i < end {
                let arg = &args[i];
                let raw = arg.strip_prefix("--").unwrap_or(&arg[1..]);
                if raw.is_empty() || raw.starts_with(['-', '=']) {
                    return Err(format!("bad flag syntax: {arg}"));
                }
                let (name, inline) = raw
                    .split_once('=')
                    .map_or((raw, None), |(n, v)| (n, Some(v)));
                let Some(f) = self.flags.get_mut(name) else {
                    return Err(format!("flag provided but not defined: -{name}"));
                };
                let is_bool = matches!(f.value, Value::Bool(_));
                let value = if is_bool {
                    inline.unwrap_or("true")
                } else if let Some(v) = inline {
                    v
                } else {
                    i += 1;
                    if i >= end {
                        return Err(format!("flag needs an argument: -{name}"));
                    }
                    &args[i]
                };
                let bad = |reason: &str| {
                    if is_bool {
                        format!(
                            "invalid boolean value {} for -{name}: {reason}",
                            quote(value)
                        )
                    } else {
                        format!("invalid value {} for flag -{name}: {reason}", quote(value))
                    }
                };
                f.value = match &mut f.value {
                    Value::Bool(_) => match parse_bool(value) {
                        Some(v) => Value::Bool(v),
                        None => {
                            f.value = Value::Bool(false);
                            return Err(bad("parse error"));
                        }
                    },
                    Value::Int(_) => Value::Int(parse_int(value).map_err(bad)?),
                    Value::String(_) => Value::String(value.into()),
                    Value::Strings(v) => {
                        v.push(value.into());
                        Value::Strings(v.clone())
                    }
                    Value::Duration(_) => {
                        Value::Duration(parse_duration(value).map_err(|_| bad("parse error"))?)
                    }
                };
                self.given.insert(name.into());
                i += 1;
            }
        }
        if !self.help() && (self.positional.len() < min || self.positional.len() > max) {
            return Err(format!(
                "{}: expected {min} to {max} positional arguments, got {}",
                self.name,
                self.positional.len()
            ));
        }
        Ok(())
    }
    pub fn usage(&self, line: &str) -> String {
        let mut out = format!("{line}\n");
        for (name, f) in &self.flags {
            let mut usage = f.usage.clone();
            let mut ty = match f.value {
                Value::Bool(_) => "",
                Value::Int(_) => "int",
                Value::String(_) => "string",
                Value::Strings(_) => "value",
                Value::Duration(_) => "duration",
            }
            .to_string();
            if let Some(a) = usage.find('`')
                && let Some(b) = usage[a + 1..].find('`')
            {
                let b = a + 1 + b;
                ty = usage[a + 1..b].into();
                usage.remove(b);
                usage.remove(a);
            }
            let mut label = format!("  -{name}");
            if !ty.is_empty() {
                label.push(' ');
                label.push_str(&ty);
            }
            if label.len() <= 4 {
                label.push('\t');
            } else {
                label.push_str("\n    \t");
            }
            out.push_str(&label);
            out.push_str(&usage.replace('\n', "\n    \t"));
            let default = match &f.default {
                Value::Bool(true) => Some("true".into()),
                Value::Int(v) if *v != 0 => Some(v.to_string()),
                Value::String(v) if !v.is_empty() => Some(quote(v)),
                Value::Duration(v) if *v != 0 => Some(duration_text(*v)),
                _ => None,
            };
            if let Some(v) = default {
                out.push_str(&format!(" (default {v})"));
            }
            out.push('\n');
        }
        out
    }
}
fn split(arg: &str) -> (&str, Option<&str>) {
    let raw = arg.trim_start_matches('-');
    raw.split_once('=')
        .map_or((raw, None), |(a, b)| (a, Some(b)))
}
pub fn parse_bool(v: &str) -> Option<bool> {
    match v {
        "1" | "t" | "T" | "true" | "TRUE" | "True" => Some(true),
        "0" | "f" | "F" | "false" | "FALSE" | "False" => Some(false),
        _ => None,
    }
}
pub fn parse_int(v: &str) -> Result<i64, &'static str> {
    let (negative, unsigned) = if let Some(s) = v.strip_prefix('-') {
        (true, s)
    } else {
        (false, v.strip_prefix('+').unwrap_or(v))
    };
    let (base, digits, prefix) = if unsigned.starts_with("0x") || unsigned.starts_with("0X") {
        (16, &unsigned[2..], true)
    } else if unsigned.starts_with("0b") || unsigned.starts_with("0B") {
        (2, &unsigned[2..], true)
    } else if unsigned.starts_with("0o") || unsigned.starts_with("0O") {
        (8, &unsigned[2..], true)
    } else if unsigned.len() > 1 && unsigned.starts_with('0') {
        (8, unsigned, false)
    } else {
        (10, unsigned, false)
    };
    if digits.is_empty()
        || digits.ends_with('_')
        || digits.contains("__")
        || digits.starts_with('_') && !prefix
    {
        return Err("parse error");
    }
    let clean = digits.replace('_', "");
    if clean.is_empty() || !clean.chars().all(|c| c.is_digit(base)) {
        return Err("parse error");
    }
    let n = u64::from_str_radix(&clean, base).map_err(|_| "value out of range")?;
    if negative {
        if n > (1u64 << 63) {
            Err("value out of range")
        } else {
            Ok((n as i64).wrapping_neg())
        }
    } else {
        i64::try_from(n).map_err(|_| "value out of range")
    }
}
pub fn quote(v: &str) -> String {
    let mut out = String::from("\"");
    for c in v.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{7}' => out.push_str("\\a"),
            '\u{8}' => out.push_str("\\b"),
            '\u{b}' => out.push_str("\\v"),
            '\u{c}' => out.push_str("\\f"),
            c if c < '\u{20}' || c == '\u{7f}' => out.push_str(&format!("\\x{:02x}", c as u32)),
            c if c.is_control() || matches!(c, '\u{2028}' | '\u{2029}') => {
                out.push_str(&format!("\\u{:04x}", c as u32))
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
pub fn parse_duration(v: &str) -> Result<i64, String> {
    let bad = || format!("time: invalid duration {}", quote(v));
    let (negative, mut rest) = if let Some(r) = v.strip_prefix('-') {
        (true, r)
    } else {
        (false, v.strip_prefix('+').unwrap_or(v))
    };
    if rest == "0" {
        return Ok(0);
    }
    if rest.is_empty() {
        return Err(bad());
    }
    let mut total = 0u64;
    while !rest.is_empty() {
        let before = rest.bytes().take_while(u8::is_ascii_digit).count();
        let mut whole = 0u64;
        for digit in rest[..before].bytes() {
            whole = whole
                .checked_mul(10)
                .and_then(|n| n.checked_add((digit - b'0') as u64))
                .filter(|n| *n <= 1u64 << 63)
                .ok_or_else(bad)?;
        }
        rest = &rest[before..];
        let mut fraction = 0u64;
        let mut scale = 1.0f64;
        let mut after = 0;
        if let Some(r) = rest.strip_prefix('.') {
            after = r.bytes().take_while(u8::is_ascii_digit).count();
            let mut overflow = false;
            for digit in r[..after].bytes() {
                if overflow {
                    continue;
                }
                if fraction > (i64::MAX as u64) / 10 {
                    overflow = true;
                    continue;
                }
                let y = fraction * 10 + (digit - b'0') as u64;
                if y > 1u64 << 63 {
                    overflow = true;
                    continue;
                }
                fraction = y;
                scale *= 10.0;
            }
            rest = &r[after..];
        }
        if before == 0 && after == 0 {
            return Err(bad());
        }
        let at = rest
            .find(|c: char| c.is_ascii_digit() || c == '.')
            .unwrap_or(rest.len());
        let unit = &rest[..at];
        let multiplier = match unit {
            "ns" => 1,
            "us" | "µs" | "μs" => 1_000,
            "ms" => 1_000_000,
            "s" => 1_000_000_000,
            "m" => 60_000_000_000,
            "h" => 3_600_000_000_000,
            "" => return Err(format!("time: missing unit in duration {}", quote(v))),
            _ => {
                return Err(format!(
                    "time: unknown unit {} in duration {}",
                    quote(unit),
                    quote(v)
                ));
            }
        };
        if whole > (1u64 << 63) / multiplier {
            return Err(bad());
        }
        whole *= multiplier;
        if fraction > 0 {
            whole = whole
                .checked_add((fraction as f64 * (multiplier as f64 / scale)) as u64)
                .filter(|n| *n <= 1u64 << 63)
                .ok_or_else(bad)?;
        }
        total = total
            .checked_add(whole)
            .filter(|n| *n <= 1u64 << 63)
            .ok_or_else(bad)?;
        rest = &rest[at..];
    }
    if negative {
        Ok((total as i64).wrapping_neg())
    } else {
        i64::try_from(total).map_err(|_| bad())
    }
}
fn duration_text(ns: i64) -> String {
    if ns == 0 {
        "0s".into()
    } else if ns % 1_000_000_000 == 0 {
        let s = ns / 1_000_000_000;
        if s.abs() >= 60 {
            format!("{}m{}s", s / 60, s % 60)
        } else {
            format!("{s}s")
        }
    } else {
        format!("{}ns", ns)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn interspersed_flags_and_errors() {
        let mut fs = FlagSet::new("test", false);
        fs.int("as", 0, "task")
            .string("out", "", "path")
            .bool("open", false, "open");
        fs.parse(
            &["text", "--as", "0x10", "second", "--open=false"].map(String::from),
            2,
            2,
        )
        .unwrap();
        assert_eq!(fs.positional, ["text", "second"]);
        assert_eq!(fs.get_int("as"), 16);
        assert!(!fs.get_bool("open"));
        assert_eq!(
            fs.parse(&["--out", "--as"].map(String::from), 0, 0)
                .unwrap_err(),
            "--out needs a value, got flag --as"
        );
        assert_eq!(parse_int("08"), Err("parse error"));
        assert_eq!(parse_int("-0x8000000000000000"), Ok(i64::MIN));
        assert_eq!(parse_duration("1m2.5s").unwrap(), 62_500_000_000);
        assert_eq!(parse_duration("9223372036854775807ns").unwrap(), i64::MAX);
        assert_eq!(parse_duration("-9223372036854775808ns").unwrap(), i64::MIN);
        assert!(parse_duration("9223372036854775808ns").is_err());
        let mut invalid = FlagSet::new("test", true);
        assert!(invalid.parse(&["--json=invalid".into()], 0, 0).is_err());
        assert!(!invalid.json());
    }
}
