//! The seccomp filter everything inside runs under.
//!
//! A network namespace leaves two ways out. An `AF_VSOCK` socket addresses the
//! hypervisor's host under any VM guest, WSL2 included, whatever namespace it is made in;
//! io_uring can make one without the `socket` call a filter sees. The filter refuses
//! both with `EPERM`, the answer Codex's filter gives
//! (`codex-rs/linux-sandbox/src/seccomp.rs`), and allows every other call.
//!
//! [`program`] compiles the filter before the fork; [`install`] puts it in place between
//! fork and exec, and allocates nothing.

use crate::Error;
use seccompiler::{
    BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
    SeccompRule, TargetArch,
};
use std::collections::BTreeMap;

/// What a refused call returns.
const REFUSED: libc::c_int = libc::EPERM;

/// `__X32_SYSCALL_BIT`: on x86_64 a call numbered with it is the x32 ABI's, under the
/// same audit architecture as the native call.
const X32_SYSCALL_BIT: i64 = 0x4000_0000;

/// Refused whatever their arguments.
const IO_URING: [i64; 3] = [
    libc::SYS_io_uring_setup,
    libc::SYS_io_uring_enter,
    libc::SYS_io_uring_register,
];

/// Refused when the first argument, the address family, is `AF_VSOCK`.
const SOCKET: [i64; 2] = [libc::SYS_socket, libc::SYS_socketpair];

/// Each number the kernel takes for call `nr` under this architecture's audit value. The
/// filter's architecture check passes x32's numbers on x86_64, so they are refused too.
fn numbers(nr: i64) -> Vec<i64> {
    if cfg!(target_arch = "x86_64") {
        vec![nr, nr | X32_SYSCALL_BIT]
    } else {
        vec![nr]
    }
}

fn compile(e: impl std::fmt::Display) -> Error {
    Error::Confine(format!("seccomp: {e}"))
}

/// The filter, compiled for this architecture. Its first instructions check the audit
/// architecture and kill a process that calls under another one: an i386 call on
/// x86_64, or an aarch32 call on aarch64, has numbers of its own the rules do not name.
///
/// # Errors
///
/// When this architecture has no filter, or the filter does not compile.
pub(super) fn program() -> Result<BpfProgram, Error> {
    let arch = TargetArch::try_from(std::env::consts::ARCH).map_err(compile)?;
    let vsock = SeccompRule::new(vec![SeccompCondition::new(
        0,
        SeccompCmpArgLen::Dword,
        SeccompCmpOp::Eq,
        libc::AF_VSOCK as u64,
    )
    .map_err(compile)?])
    .map_err(compile)?;
    let mut rules: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();
    for nr in IO_URING.into_iter().flat_map(numbers) {
        // no rule: the call is refused whatever its arguments
        rules.insert(nr, Vec::new());
    }
    for nr in SOCKET.into_iter().flat_map(numbers) {
        rules.insert(nr, vec![vsock.clone()]);
    }
    let filter = SeccompFilter::new(
        rules,
        SeccompAction::Allow,
        SeccompAction::Errno(REFUSED as u32),
        arch,
    )
    .map_err(compile)?;
    BpfProgram::try_from(filter).map_err(compile)
}

/// Set `no_new_privs` and install `program` on the calling thread. Both hold across
/// every exec and fork after it. Allocates nothing, so it may run between fork and exec.
///
/// # Errors
///
/// The errno of the `prctl` or `seccomp` call that failed.
pub(super) fn install(program: &BpfProgram) -> std::io::Result<()> {
    // an unprivileged process may install a filter only under no_new_privs
    // SAFETY: prctl with integer arguments only.
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    seccompiler::apply_filter(program).map_err(|e| match e {
        seccompiler::Error::Prctl(io) | seccompiler::Error::Seccomp(io) => io,
        _ => std::io::Error::from_raw_os_error(libc::EINVAL),
    })
}
