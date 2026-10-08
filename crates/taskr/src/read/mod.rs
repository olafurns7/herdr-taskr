//! Read commands: status, asks, glance, search, log, handover, doc get/ls, version.
use taskr_core::ExitCode;

/// Runs a read command; `None` when the command is not a read.
pub fn dispatch(_json: bool, _args: &[String]) -> Option<ExitCode> {
    None
}
