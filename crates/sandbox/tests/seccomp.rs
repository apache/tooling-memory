//! The seccomp filter of the namespaces backend: a process inside cannot open a vsock
//! socket, which a network namespace does not cover, or use io_uring, which can open one
//! without a `socket` call. Nor does it inherit a socket the launcher left open.

// An integration test is test code; a panic here is the failure being reported.
#![allow(clippy::unwrap_used)]
#![cfg(target_os = "linux")]

mod common;

use common::{opened, say};
use std::collections::BTreeMap;
use std::os::unix::process::CommandExt;
use std::process::Command;

/// What the filter answers each refused call with.
const REFUSED: libc::c_int = libc::EPERM;

/// `__X32_SYSCALL_BIT`: x32's numbering, open to any x86_64 process on a kernel built
/// with it.
const X32: libc::c_long = 0x4000_0000;

/// In the probe's environment: descriptors the test left open without `FD_CLOEXEC`, as
/// `name=fd` separated by spaces.
const LEFT_OPEN: &str = "MEMORY_TEST_LEFT_OPEN";

/// Why the backend cannot install its filter, as it says it.
const NO_SECCOMP: &str = "the sandbox cannot install its seccomp filter here: it needs a \
                          kernel with seccomp filters, and a seccomp profile that allows \
                          seccomp(2)";

/// The result of a raw syscall that returns zero or more on success.
fn zero(r: libc::c_long) -> Result<(), std::io::Error> {
    if r < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// `socketpair` with `AF_VSOCK`, by call number `nr`, closing what it returns.
fn socketpair_vsock(nr: libc::c_long) -> Result<(), std::io::Error> {
    let mut pair: [libc::c_int; 2] = [-1; 2];
    // SAFETY: a pointer to two descriptors for the kernel to fill.
    let r = unsafe { libc::syscall(nr, libc::AF_VSOCK, libc::SOCK_STREAM, 0, pair.as_mut_ptr()) };
    let result = zero(r);
    for fd in pair.into_iter().filter(|fd| *fd >= 0) {
        // SAFETY: closing a descriptor the call above returned.
        unsafe { libc::close(fd) };
    }
    result
}

/// Report each call the filter refuses, by call number `bit | nr`: `bit` is zero for
/// the native numbering.
fn refused_calls(suffix: &str, bit: libc::c_long) {
    let mut params = [0u8; 120];
    // SAFETY: each call passes valid pointers or none, and every descriptor returned is
    // closed by `opened`.
    unsafe {
        say(
            &format!("socket_vsock{suffix}"),
            opened(libc::syscall(
                bit | libc::SYS_socket,
                libc::AF_VSOCK,
                libc::SOCK_STREAM,
                0,
            )),
        );
        say(
            &format!("io_uring_setup{suffix}"),
            opened(libc::syscall(
                bit | libc::SYS_io_uring_setup,
                1,
                params.as_mut_ptr(),
            )),
        );
        say(
            &format!("io_uring_enter{suffix}"),
            opened(libc::syscall(
                bit | libc::SYS_io_uring_enter,
                -1,
                0,
                0,
                0,
                0,
                0,
            )),
        );
        say(
            &format!("io_uring_register{suffix}"),
            opened(libc::syscall(
                bit | libc::SYS_io_uring_register,
                -1,
                0,
                0,
                0,
            )),
        );
    }
    say(
        &format!("socketpair_vsock{suffix}"),
        socketpair_vsock(bit | libc::SYS_socketpair),
    );
}

/// Run inside the sandbox by the test below; on its own it only reports what it sees.
#[test]
#[ignore = "the program the seccomp test runs inside the sandbox"]
fn probe_refused_calls() {
    // SAFETY: prctl with integer arguments only.
    let r = unsafe { libc::prctl(libc::PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0) };
    println!("{}no_new_privs {r}", common::REPORT);
    refused_calls("", 0);
    if cfg!(target_arch = "x86_64") {
        refused_calls("_x32", X32);
    }
    // SAFETY: each socket returned is closed by `opened`.
    unsafe {
        let stream = libc::SOCK_STREAM;
        say(
            "socket_unix",
            opened(libc::socket(libc::AF_UNIX, stream, 0).into()),
        );
        say(
            "socket_inet",
            opened(libc::socket(libc::AF_INET, stream, 0).into()),
        );
    }
    let left = std::env::var(LEFT_OPEN).unwrap_or_default();
    for (name, fd) in left.split_whitespace().filter_map(|w| w.split_once('=')) {
        let fd: libc::c_int = fd.parse().unwrap();
        // SAFETY: fcntl F_GETFD on a number, which fails on a descriptor not open.
        let r = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        say(&format!("left_open_{name}"), zero(r.into()));
    }
}

/// A socket of `family` left open across exec, at a number of 100 or more, so no
/// descriptor the probe opens itself takes its number.
fn left_open(family: libc::c_int) -> Option<libc::c_int> {
    // SAFETY: a socket without SOCK_CLOEXEC, duplicated with F_DUPFD, which does not set
    // FD_CLOEXEC either; the first descriptor is closed.
    unsafe {
        let fd = libc::socket(family, libc::SOCK_STREAM, 0);
        if fd < 0 {
            return None;
        }
        let high = libc::fcntl(fd, libc::F_DUPFD, 100);
        libc::close(fd);
        (high >= 0).then_some(high)
    }
}

/// Whether `kernel.io_uring_disabled` refuses io_uring_setup to everyone here, with the
/// same errno as the filter.
fn io_uring_disabled() -> bool {
    std::fs::read_to_string("/proc/sys/kernel/io_uring_disabled").is_ok_and(|v| v.trim() != "0")
}

#[test]
fn inside_vsock_and_io_uring_are_refused_and_other_sockets_are_not() {
    if let Err(why) = dm_sandbox::linux::availability() {
        eprintln!("skipping: {why}");
        return;
    }
    let refused = REFUSED.to_string();
    let mut want: BTreeMap<String, String> = [
        ("no_new_privs", "1"),
        ("socket_unix", "ok"),
        ("socket_inet", "ok"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    let mut suffixes = vec![""];
    if cfg!(target_arch = "x86_64") {
        suffixes.push("_x32");
    }
    for suffix in suffixes {
        for call in [
            "socket_vsock",
            "socketpair_vsock",
            "io_uring_setup",
            "io_uring_enter",
            "io_uring_register",
        ] {
            want.insert(format!("{call}{suffix}"), refused.clone());
        }
    }
    let mut left = Vec::new();
    for (name, family) in [("unix", libc::AF_UNIX), ("vsock", libc::AF_VSOCK)] {
        if let Some(fd) = left_open(family) {
            left.push(format!("{name}={fd}"));
            want.insert(format!("left_open_{name}"), libc::EBADF.to_string());
        }
    }
    let env = BTreeMap::from([(LEFT_OPEN.to_string(), left.join(" "))]);
    let plan = common::plan("probe_refused_calls", Vec::new(), env);
    let ran = dm_sandbox::linux::run_captured(&plan);
    for fd in left.iter().filter_map(|w| w.split_once('=')) {
        // SAFETY: closing a descriptor left_open returned.
        unsafe { libc::close(fd.1.parse().unwrap()) };
    }
    let (code, out, _) = ran.unwrap();
    assert_eq!(code, 0, "{}", String::from_utf8_lossy(&out));
    let mut seen = common::report(&out);
    if io_uring_disabled() {
        // the host refuses it with the filter's errno, so it says nothing of the filter
        for map in [&mut want, &mut seen] {
            map.retain(|k, _| !k.starts_with("io_uring_setup"));
        }
    }
    assert_eq!(seen, want);
}

/// Run by the test below, under a seccomp filter that refuses `seccomp(2)`: say what
/// [`dm_sandbox::linux::availability`] answers, and whether a launch starts.
#[test]
#[ignore = "the host side of the seccomp test, under a profile that refuses seccomp(2)"]
fn probe_availability_without_seccomp() {
    let said = match dm_sandbox::linux::availability() {
        Ok(()) => "ok".to_string(),
        Err(why) => why,
    };
    println!("{}availability {said}", common::REPORT);
    let plan = common::plan("probe_refused_calls", Vec::new(), BTreeMap::new());
    let launch = match dm_sandbox::linux::run_captured(&plan) {
        Ok(_) => "started",
        Err(_) => "refused",
    };
    println!("{}launch {launch}", common::REPORT);
}

/// `AUDIT_ARCH_*` of this target, which the filter below checks before the number.
#[cfg(target_arch = "x86_64")]
const AUDIT_ARCH: u32 = 0xC000_003E;
#[cfg(target_arch = "aarch64")]
const AUDIT_ARCH: u32 = 0xC000_00B7;

fn bpf(code: u32, k: u32, jt: u8, jf: u8) -> libc::sock_filter {
    libc::sock_filter {
        code: u16::try_from(code).unwrap(),
        jt,
        jf,
        k,
    }
}

/// This binary's ignored test `probe`, under a seccomp filter that answers call `nr`
/// with `errno` and allows every other call, as a container runtime's profile may.
fn under_filter(probe: &str, nr: libc::c_long, errno: libc::c_int) -> Command {
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args([
        "--exact",
        probe,
        "--ignored",
        "--nocapture",
        "--test-threads=1",
    ]);
    let nr = u32::try_from(nr).unwrap();
    let errno = libc::SECCOMP_RET_ERRNO | u32::try_from(errno).unwrap();
    let program = [
        bpf(libc::BPF_LD | libc::BPF_W | libc::BPF_ABS, 4, 0, 0),
        bpf(
            libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K,
            AUDIT_ARCH,
            0,
            3,
        ),
        bpf(libc::BPF_LD | libc::BPF_W | libc::BPF_ABS, 0, 0, 0),
        bpf(libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K, nr, 0, 1),
        bpf(libc::BPF_RET | libc::BPF_K, errno, 0, 0),
        bpf(libc::BPF_RET | libc::BPF_K, libc::SECCOMP_RET_ALLOW, 0, 0),
    ];
    // SAFETY: between fork and exec the child makes two syscalls on a program built
    // before the fork, and allocates nothing.
    unsafe {
        cmd.pre_exec(move || {
            let prog = libc::sock_fprog {
                len: program.len() as u16,
                filter: program.as_ptr().cast_mut(),
            };
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0
                || libc::syscall(libc::SYS_seccomp, libc::SECCOMP_SET_MODE_FILTER, 0, &prog) != 0
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    cmd
}

#[test]
fn without_seccomp_availability_says_so_and_a_launch_does_not_start() {
    if let Err(why) = dm_sandbox::linux::availability() {
        eprintln!("skipping: {why}");
        return;
    }
    let probe = "probe_availability_without_seccomp";
    let out = under_filter(probe, libc::SYS_seccomp, libc::EPERM)
        .output()
        .unwrap();
    let said = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{said}{stderr}");
    let want: BTreeMap<String, String> = [("availability", NO_SECCOMP), ("launch", "refused")]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    assert_eq!(common::report(&out.stdout), want, "{said}{stderr}");
    assert!(stderr.contains(NO_SECCOMP), "{stderr}");
}

/// Run by the test below, under a seccomp filter that answers `close_range` with
/// `ENOSYS`, as a kernel before Linux 5.9 does: launch [`probe_refused_calls`] with a
/// socket left open, and pass on what it reports.
#[test]
#[ignore = "the host side of the close_range test, under a filter that hides the call"]
fn probe_left_open_without_close_range() {
    let fd = left_open(libc::AF_UNIX).unwrap();
    let env = BTreeMap::from([(LEFT_OPEN.to_string(), format!("unix={fd}"))]);
    let plan = common::plan("probe_refused_calls", Vec::new(), env);
    let (code, out, _) = dm_sandbox::linux::run_captured(&plan).unwrap();
    println!("{}", String::from_utf8_lossy(&out));
    assert_eq!(code, 0);
}

#[test]
fn without_close_range_a_socket_left_open_is_still_not_inherited() {
    if let Err(why) = dm_sandbox::linux::availability() {
        eprintln!("skipping: {why}");
        return;
    }
    let probe = "probe_left_open_without_close_range";
    let out = under_filter(probe, libc::SYS_close_range, libc::ENOSYS)
        .output()
        .unwrap();
    let said = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{said}{stderr}");
    let seen = common::report(&out.stdout);
    let not_open = libc::EBADF.to_string();
    assert_eq!(
        seen.get("left_open_unix"),
        Some(&not_open),
        "{said}{stderr}"
    );
}
