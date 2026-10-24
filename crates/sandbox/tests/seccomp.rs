//! The seccomp filter of the namespaces backend: a process inside cannot open a vsock
//! socket, which a network namespace does not cover, or use io_uring, which can open one
//! without a `socket` call.

// An integration test is test code; a panic here is the failure being reported.
#![allow(clippy::unwrap_used)]
#![cfg(target_os = "linux")]

mod common;

use common::{opened, say};
use std::collections::BTreeMap;

/// What the filter answers each refused call with.
const REFUSED: libc::c_int = libc::EPERM;

/// Run inside the sandbox by the test below; on its own it only reports what it sees.
#[test]
#[ignore = "the program the seccomp test runs inside the sandbox"]
fn probe_refused_calls() {
    // SAFETY: each call passes valid pointers or none, and every descriptor returned is
    // closed by `opened` or right here.
    unsafe {
        let r = libc::prctl(libc::PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0);
        println!("{}no_new_privs {r}", common::REPORT);
        let stream = libc::SOCK_STREAM;
        say(
            "socket_vsock",
            opened(libc::socket(libc::AF_VSOCK, stream, 0).into()),
        );
        say(
            "socket_unix",
            opened(libc::socket(libc::AF_UNIX, stream, 0).into()),
        );
        say(
            "socket_inet",
            opened(libc::socket(libc::AF_INET, stream, 0).into()),
        );
        let mut pair = [-1; 2];
        let r = libc::socketpair(libc::AF_VSOCK, stream, 0, pair.as_mut_ptr());
        for fd in pair.into_iter().filter(|fd| *fd >= 0) {
            libc::close(fd);
        }
        say(
            "socketpair_vsock",
            if r < 0 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            },
        );
        // x32's numbering, open to any x86_64 process on a kernel built with it
        #[cfg(target_arch = "x86_64")]
        say(
            "socket_vsock_x32",
            opened(libc::syscall(
                0x4000_0000 | libc::SYS_socket,
                libc::AF_VSOCK,
                stream,
                0,
            )),
        );
        let mut params = [0u8; 120];
        say(
            "io_uring_setup",
            opened(libc::syscall(
                libc::SYS_io_uring_setup,
                1,
                params.as_mut_ptr(),
            )),
        );
        say(
            "io_uring_enter",
            opened(libc::syscall(libc::SYS_io_uring_enter, -1, 0, 0, 0, 0, 0)),
        );
        say(
            "io_uring_register",
            opened(libc::syscall(libc::SYS_io_uring_register, -1, 0, 0, 0)),
        );
    }
}

#[test]
fn inside_vsock_and_io_uring_are_refused_and_other_sockets_are_not() {
    if let Err(why) = dm_sandbox::linux::availability() {
        eprintln!("skipping: {why}");
        return;
    }
    let plan = common::plan("probe_refused_calls", Vec::new(), BTreeMap::new());
    let (code, out, _) = dm_sandbox::linux::run_captured(&plan).unwrap();
    assert_eq!(code, 0, "{}", String::from_utf8_lossy(&out));
    let refused = REFUSED.to_string();
    let mut want: BTreeMap<String, String> = [
        ("no_new_privs", "1"),
        ("socket_vsock", &refused),
        ("socketpair_vsock", &refused),
        ("io_uring_setup", &refused),
        ("io_uring_enter", &refused),
        ("io_uring_register", &refused),
        ("socket_unix", "ok"),
        ("socket_inet", "ok"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    if cfg!(target_arch = "x86_64") {
        want.insert("socket_vsock_x32".into(), refused.clone());
    }
    assert_eq!(common::report(&out), want);
}
