//! Client-host routing: hub discovery, RPC, request keys, the offline spool.
use taskr_core::ExitCode;

/// Sends the command to the hub when this host is a client; `None` runs it locally.
pub fn route(_json: bool, _args: &[String]) -> Option<ExitCode> {
    None
}
