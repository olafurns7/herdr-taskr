use std::io;

fn procargs2(raw: &[u8]) -> io::Result<(String, Vec<String>)> {
    let short = || io::Error::other("short kern.procargs2");
    let argc = i32::from_ne_bytes(raw.get(..4).ok_or_else(short)?.try_into().unwrap());
    if argc <= 0 || argc as usize > raw.len() - 4 {
        return Err(io::Error::other("invalid kern.procargs2 argc"));
    }
    let rest = &raw[4..];
    let end = rest
        .iter()
        .position(|b| *b == 0)
        .filter(|i| *i > 0)
        .ok_or_else(|| io::Error::other("kern.procargs2 has no exec path"))?;
    let exe = String::from_utf8_lossy(&rest[..end]).into_owned();
    let mut rest = &rest[end..];
    while rest.first() == Some(&0) {
        rest = &rest[1..];
    }
    let mut argv = Vec::new();
    for _ in 0..argc {
        let end = rest
            .iter()
            .position(|b| *b == 0)
            .ok_or_else(|| io::Error::other("kern.procargs2 argv is truncated"))?;
        argv.push(String::from_utf8_lossy(&rest[..end]).into_owned());
        rest = &rest[end + 1..];
    }
    Ok((exe, argv))
}

fn kinfo(raw: &[u8], pid: i32) -> io::Result<(String, u32)> {
    // Darwin's 64-bit kinfo_proc ABI (arm64 and x86_64); libc does not expose it.
    if raw.len() != 648 {
        return Err(io::Error::other("unexpected kern.proc.pid size"));
    }
    let actual = i32::from_ne_bytes(raw[40..44].try_into().unwrap());
    if actual != pid {
        return Err(io::Error::other(format!("no process {pid}")));
    }
    let sec = i64::from_ne_bytes(raw[..8].try_into().unwrap());
    let usec = i32::from_ne_bytes(raw[8..12].try_into().unwrap());
    let uid = u32::from_ne_bytes(raw[420..424].try_into().unwrap());
    if !(0..1_000_000).contains(&usec) {
        return Err(io::Error::other("invalid kern.proc.pid start time"));
    }
    Ok((format!("{sec}.{usec:06}"), uid))
}

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
fn sysctl(mib: &mut [libc::c_int]) -> io::Result<Vec<u8>> {
    let mut len = 0;
    // SAFETY: mib and len are valid for their supplied lengths; null oldp queries size, null newp is read-only.
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as _,
            std::ptr::null_mut(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    let mut raw = vec![0u8; len];
    // SAFETY: raw has len writable bytes and the MIB is valid; null newp cannot change kernel state.
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as _,
            raw.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    if len > raw.len() {
        return Err(io::Error::other("sysctl result grew"));
    }
    raw.truncate(len);
    Ok(raw)
}

#[cfg(target_os = "macos")]
pub(super) fn proc_identity(pid: i32) -> io::Result<super::identity::Proc> {
    let raw = sysctl(&mut [libc::CTL_KERN, libc::KERN_PROCARGS2, pid])?;
    let (exe, argv) = procargs2(&raw)?;
    let raw = sysctl(&mut [libc::CTL_KERN, libc::KERN_PROC, libc::KERN_PROC_PID, pid])?;
    let (start, uid) = kinfo(&raw, pid)?;
    Ok(super::identity::Proc {
        exe,
        argv,
        start,
        uid,
    })
}

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
pub(super) fn hup_ignored() -> bool {
    let mut action = std::mem::MaybeUninit::<libc::sigaction>::uninit();
    // SAFETY: old action points to writable sigaction storage; null new action only queries disposition.
    let result = unsafe { libc::sigaction(libc::SIGHUP, std::ptr::null(), action.as_mut_ptr()) };
    if result != 0 {
        return false;
    }
    // SAFETY: successful sigaction initialized the entire old action.
    unsafe { action.assume_init().sa_sigaction == libc::SIG_IGN }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn args(argc: i32, bytes: &[u8]) -> Vec<u8> {
        [argc.to_ne_bytes().as_slice(), bytes].concat()
    }
    #[test]
    fn procargs_preserves_argv_and_ignores_environment() {
        let (exe, argv) = procargs2(&args(
            4,
            b"/tmp/taskr\0\0\0taskr\0daemon\0\0a b\0SECRET=ignored\0",
        ))
        .unwrap();
        assert_eq!(exe, "/tmp/taskr");
        assert_eq!(argv, ["taskr", "daemon", "", "a b"]);
    }
    #[test]
    fn procargs_rejects_missing_and_truncated_fields() {
        for raw in [
            vec![],
            vec![1, 0, 0],
            args(0, b"/taskr\0"),
            args(-1, b"/taskr\0"),
            args(i32::MAX, b"/taskr\0"),
            args(1, b"\0taskr\0"),
            args(1, b"/taskr"),
            args(2, b"/taskr\0taskr\0daemon"),
        ] {
            assert!(procargs2(&raw).is_err(), "{raw:?}");
        }
    }
    #[test]
    fn kinfo_checks_pid_size_and_formats_kernel_identity() {
        let mut raw = [0u8; 648];
        raw[..8].copy_from_slice(&123i64.to_ne_bytes());
        raw[8..12].copy_from_slice(&7i32.to_ne_bytes());
        raw[40..44].copy_from_slice(&42i32.to_ne_bytes());
        raw[420..424].copy_from_slice(&501u32.to_ne_bytes());
        assert_eq!(kinfo(&raw, 42).unwrap(), ("123.000007".into(), 501));
        assert!(kinfo(&raw, 43).is_err());
        assert!(kinfo(&raw[..647], 42).is_err());
        raw[8..12].copy_from_slice(&1_000_000i32.to_ne_bytes());
        assert!(kinfo(&raw, 42).is_err());
    }
}
