//! Write commands: new, launch, start, got, note, ask, answer, ready, done, fail, close, set, prompt, ack, wait, doc set.
use taskr_core::ExitCode;

/// Runs a write command; `None` when the command is not a write.
pub fn dispatch(_json: bool, _args: &[String]) -> Option<ExitCode> {
    None
}
