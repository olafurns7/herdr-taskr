use super::rpc::{Client, request};
use super::{Error, ExitCode, Result, exit, new_key, reply_error};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::time::Duration;

pub fn relay(
    cl: &Client,
    lead: &[String],
    args: &[String],
    cwd: &str,
    env: &Value,
    json_mode: bool,
) -> Result<Option<ExitCode>> {
    let mut f = taskr_core::goflag::FlagSet::new("prompt", json_mode);
    f.string("file", "", "")
        .string("text", "", "")
        .bool("confirm", false, "")
        .int("confirm-timeout", 60000, "")
        .int("receipt-timeout", 300000, "");
    if f.parse(args, 1, 1).is_err() {
        return Ok(None);
    }
    let file = f.get_string("file");
    let text = f.get_string("text");
    let receipt = f.get_int("receipt-timeout");
    let confirm = f.get_bool("confirm");
    if file.is_empty() == text.is_empty()
        || f.get_int("confirm-timeout") < 0
        || f.was_set("receipt-timeout")
            && (confirm || receipt < 0 || receipt > 0 && receipt < 60000)
    {
        return Ok(None);
    }
    let mut begin = vec![
        "--json".into(),
        "_prompt".into(),
        "begin".into(),
        f.positional[0].clone(),
        "--receipt-timeout".into(),
        if confirm {
            "0".into()
        } else {
            receipt.to_string()
        },
    ];
    if !file.is_empty() {
        begin.extend(["--file".into(), file.into()]);
        if let Ok(b) = std::fs::read(file) {
            begin.extend([
                "--sha256".into(),
                format!("{:x}", Sha256::digest(&b)),
                "--bytes".into(),
                b.len().to_string(),
            ]);
        }
    }
    if !text.is_empty() {
        begin.extend(["--text".into(), text.into()]);
    }
    let sock = crate::write::herdr::socket();
    if crate::write::herdr::up(&sock) {
        begin.push("--local-herdr".into());
    }
    let rep=cl.call(&request(&begin,cwd,&new_key()?,env.clone(),None),Duration::from_secs(40),true).map_err(|mut e| {if e.kind=="transport"{e.message=format!("server unreachable ({}); no attempt is known; inspect `taskr log` before any resend",e.message);}e})?;
    let b: Value = serde_json::from_str(
        rep["stdout"]
            .as_str()
            .unwrap_or("")
            .trim()
            .lines()
            .last()
            .unwrap_or(""),
    )
    .unwrap_or(Value::Null);
    if rep["exit"] != 0 {
        return Err(Error::new(
            exit(rep["exit"].as_i64().unwrap()),
            if b["kind"] == "usage" {
                "usage"
            } else if b["kind"] == "transport" {
                "transport"
            } else if b["kind"] == "herdr" {
                "herdr"
            } else {
                "rejected"
            },
            reply_error(&rep),
        ));
    }
    if b["route"] == "server" {
        return Ok(None);
    }
    let attempt = b["attempt_id"].as_i64().unwrap_or(0);
    let (outcome, detail) = crate::write::herdr::run_prompt(
        &sock,
        b["target"].as_str().unwrap_or(""),
        b["text"].as_str().unwrap_or(""),
        json_mode,
    );
    let mut argv: Vec<String> = lead
        .iter()
        .cloned()
        .chain([
            "_prompt".into(),
            "outcome".into(),
            attempt.to_string(),
            "--outcome".into(),
            outcome.into(),
            "--detail".into(),
            taskr_core::compact_json(&detail).unwrap(),
        ])
        .collect();
    if confirm {
        argv.extend([
            "--confirm".into(),
            "--confirm-timeout".into(),
            f.get_int("confirm-timeout").to_string(),
        ]);
    }
    let rep2=cl.call(&request(&argv,cwd,&new_key()?,env.clone(),None),super::rpc::budget(&argv)+Duration::from_secs(10),true).map_err(|e|Error::new(ExitCode::Transport,e.kind,format!("prompt attempt {attempt}: outcome {outcome} observed here, but the server did not record it ({}); inspect the agent before any resend",e.message)))?;
    print!("{}", rep2["stdout"].as_str().unwrap());
    eprint!("{}", rep2["stderr"].as_str().unwrap());
    if rep2["exit"] == 0 {
        let _ = super::doc::uploads(cl, &rep, cwd, env, None);
    }
    Ok(Some(exit(rep2["exit"].as_i64().unwrap())))
}
