use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ExitCode {
    Ok = 0,
    Watch = 1,
    Usage = 2,
    Timeout = 3,
    Database = 4,
    Transport = 5,
    Rejected = 6,
    NotImplemented = 125,
}

pub fn escape_json(text: &str) -> String {
    text.replace('&', "\\u0026")
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
}

// Go encoding/json uses fixed notation for [1e-6, 1e21), shortest digits otherwise.
pub fn float64_text(value: f64) -> Result<String, serde_json::Error> {
    if !value.is_finite() {
        return Err(<serde_json::Error as serde::ser::Error>::custom(
            "non-finite float",
        ));
    }
    if value == 0.0 {
        return Ok(if value.is_sign_negative() { "-0" } else { "0" }.into());
    }
    let raw = serde_json::to_string(&value)?;
    let (mantissa, exp) = raw
        .split_once(['e', 'E'])
        .map_or((raw.as_str(), 0), |(m, e)| {
            (m, e.parse::<i32>().expect("serializer exponent"))
        });
    let negative = mantissa.starts_with('-');
    let mantissa = mantissa.trim_start_matches('-');
    let decimal = mantissa.find('.').unwrap_or(mantissa.len()) as i32 + exp;
    let raw_digits = mantissa.replace('.', "");
    let leading = raw_digits.len() - raw_digits.trim_start_matches('0').len();
    let decimal = decimal - leading as i32;
    let digits = raw_digits.trim_matches('0');
    let sign = if negative { "-" } else { "" };
    if (1e-6..1e21).contains(&value.abs()) {
        let body = if decimal <= 0 {
            format!("0.{}{}", "0".repeat((-decimal) as usize), digits)
        } else if decimal as usize >= digits.len() {
            format!("{}{}", digits, "0".repeat(decimal as usize - digits.len()))
        } else {
            let at = decimal as usize;
            format!("{}.{}", &digits[..at], &digits[at..])
        };
        Ok(format!("{sign}{body}"))
    } else {
        let exponent = decimal - 1;
        let tail = if digits.len() > 1 {
            format!(".{}", &digits[1..])
        } else {
            String::new()
        };
        Ok(format!("{sign}{}{tail}e{exponent:+}", &digits[..1]))
    }
}

pub fn compact_json(value: &Value) -> Result<String, serde_json::Error> {
    Ok(match value {
        Value::Number(n) if n.is_f64() => float64_text(n.as_f64().expect("float"))?,
        Value::Array(values) => format!(
            "[{}]",
            values
                .iter()
                .map(compact_json)
                .collect::<Result<Vec<_>, _>>()?
                .join(",")
        ),
        Value::Object(values) => {
            let mut keys: Vec<_> = values.keys().collect();
            keys.sort();
            format!(
                "{{{}}}",
                keys.into_iter()
                    .map(|k| Ok(format!(
                        "{}:{}",
                        escape_json(&serde_json::to_string(k)?),
                        compact_json(&values[k])?
                    )))
                    .collect::<Result<Vec<_>, serde_json::Error>>()?
                    .join(",")
            )
        }
        _ => escape_json(&serde_json::to_string(value)?),
    })
}

fn is_false(value: &bool) -> bool {
    !value
}

// Field order and omission match rpcDocPayload in rpc.go, not sorted map order.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Document {
    pub task: i64,
    pub kind: String,
    pub name: String,
    pub path: String,
    pub event_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sha256: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes: Option<i64>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reason: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub backfill: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub dry_run: bool,
}

pub fn request_json(
    argv: Option<&[String]>,
    document: Option<&Document>,
) -> Result<String, serde_json::Error> {
    let argv = escape_json(&serde_json::to_string(&argv)?);
    Ok(if let Some(document) = document {
        format!(
            "{{\"argv\":{argv},\"document\":{}}}",
            escape_json(&serde_json::to_string(document)?)
        )
    } else {
        argv
    })
}

pub fn request_hash(
    argv: Option<&[String]>,
    document: Option<&Document>,
) -> Result<String, serde_json::Error> {
    Ok(format!(
        "{:x}",
        Sha256::digest(request_json(argv, document)?.as_bytes())
    ))
}

pub fn frozen_now() -> Result<time::OffsetDateTime, time::error::Parse> {
    #[cfg(feature = "contract")]
    if let Ok(now) = std::env::var("TASKR_FROZEN_NOW")
        && !now.is_empty()
    {
        return time::OffsetDateTime::parse(&now, &time::format_description::well_known::Rfc3339)
            .map(|t| t.to_offset(time::UtcOffset::UTC));
    }
    Ok(time::OffsetDateTime::now_utc())
}

// Event payloads decode through Go's interface{} path: every JSON number is float64.
pub fn event_data(value: Value) -> Value {
    match value {
        Value::Number(n) => Value::Number(
            serde_json::Number::from_f64(n.as_f64().expect("finite JSON number"))
                .expect("finite JSON number"),
        ),
        Value::Array(values) => Value::Array(values.into_iter().map(event_data).collect()),
        Value::Object(values) => Value::Object(
            values
                .into_iter()
                .map(|(k, v)| (k, event_data(v)))
                .collect(),
        ),
        other => other,
    }
}

pub mod db;
pub mod goflag;
pub mod schema;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn clock_follows_environment() {
        let before = time::OffsetDateTime::now_utc();
        let now = frozen_now().unwrap();
        let after = time::OffsetDateTime::now_utc();
        assert_eq!(now.offset(), time::UtcOffset::UTC);
        #[cfg(feature = "contract")]
        if let Ok(raw) = std::env::var("TASKR_FROZEN_NOW")
            && !raw.is_empty()
        {
            assert_eq!(
                now,
                time::OffsetDateTime::parse(&raw, &time::format_description::well_known::Rfc3339)
                    .unwrap()
            );
            return;
        }
        assert!(before <= now && now <= after);
    }
    #[cfg(not(feature = "contract"))]
    #[test]
    fn default_build_ignores_frozen_now() {
        for frozen in ["2001-01-01T00:00:00Z", "not a timestamp"] {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "tests::clock_follows_environment", "--nocapture"])
                .env("TASKR_FROZEN_NOW", frozen)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
    #[test]
    fn go_golden_vectors() {
        let vectors: Value =
            serde_json::from_str(include_str!("../../../testdata/contract/hash-vectors.json"))
                .unwrap();
        for vector in vectors["compact"].as_array().unwrap() {
            assert_eq!(
                compact_json(&event_data(vector["input"].clone())).unwrap(),
                vector["json"].as_str().unwrap(),
                "{}",
                vector["name"]
            );
        }
        for vector in vectors["integers"].as_array().unwrap() {
            assert_eq!(
                compact_json(&vector["input"]).unwrap(),
                vector["json"].as_str().unwrap()
            );
        }
        for vector in vectors["floats"].as_array().unwrap() {
            let bits = u64::from_str_radix(vector["bits"].as_str().unwrap(), 16).unwrap();
            assert_eq!(
                float64_text(f64::from_bits(bits)).unwrap(),
                vector["json"].as_str().unwrap(),
                "bits={bits:x}"
            );
        }
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(float64_text(bad).is_err());
        }
        #[derive(Deserialize)]
        struct Request {
            argv: Option<Vec<String>>,
            document: Option<Document>,
        }
        for vector in vectors["requests"].as_array().unwrap() {
            let request: Request = serde_json::from_value(vector["input"].clone()).unwrap();
            let argv = request.argv;
            let doc = request.document;
            assert_eq!(
                request_json(argv.as_deref(), doc.as_ref()).unwrap(),
                vector["json"].as_str().unwrap(),
                "{}",
                vector["name"]
            );
            assert_eq!(
                request_hash(argv.as_deref(), doc.as_ref()).unwrap(),
                vector["sha256"].as_str().unwrap(),
                "{}",
                vector["name"]
            );
        }
    }
}

pub mod store;

pub use time::Duration;
