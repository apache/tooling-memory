#!/usr/bin/env python3
"""Run inside a candidate box: does it hold contract points 1 and 2?

Point 1, the network: no interface but loopback, no route to the internet, no name
resolution, and the one unix socket bind-mounted in answers.
Point 2, the filesystem: the private home and the working tree are writable, the memory
binary and the projection are read-only, the system is read-only, and a marker file in
the host's home is not visible at its host path.

CHECK_SOCKET may name a vsock address, `vsock:CID:PORT`, for a microVM, whose egress
crosses as a vsock port rather than a bind-mounted socket. Paths default to the Linux layout (crates/sandbox, `Layout::namespaces`); a backend that
keeps host paths sets CHECK_* to say where it put each thing. Prints one line per check
and exits 1 if any failed. Standard library only, so any python3 inside the box will do.
"""

import os
import socket
import sys

LAYOUT = {
    "home": os.environ.get("CHECK_HOME", "/home/user"),
    "work": os.environ.get("CHECK_WORK", "/work"),
    "memory_bin": os.environ.get("CHECK_MEMORY_BIN", "/opt/memory/memory"),
    "projection": os.environ.get("CHECK_PROJECTION", "/memory"),
    "socket": os.environ.get("CHECK_SOCKET", "/run/memory/egress.sock"),
    "system": os.environ.get("CHECK_SYSTEM", "/usr"),
}
MARKER = os.environ.get("CHECK_HOST_MARKER", "")

results = []


def check(point, name, ok, detail=""):
    results.append(ok)
    status = "PASS" if ok else "FAIL"
    print(f"{status}  {point}  {name}{'  (' + detail + ')' if detail else ''}")


def writable(directory):
    probe = os.path.join(directory, ".contract-check")
    try:
        with open(probe, "w") as f:
            f.write("x")
        os.remove(probe)
        return True, ""
    except OSError as e:
        return False, e.strerror or str(e)


def interfaces():
    try:
        return sorted(os.listdir("/sys/class/net"))
    except OSError:
        with open("/proc/net/dev") as f:
            return sorted(line.split(":")[0].strip() for line in f.readlines()[2:])


def tcp_out(host, port):
    try:
        with socket.create_connection((host, port), timeout=3):
            return True, "connected"
    except OSError as e:
        return False, e.strerror or str(e)


def resolves(name):
    try:
        socket.getaddrinfo(name, 443)
        return True, "resolved"
    except OSError as e:
        return False, str(e)


def vsock_address(spec):
    cid, port = spec.removeprefix("vsock:").split(":")
    return int(cid), int(port)


def egress_answers(where):
    family, address = (
        (socket.AF_VSOCK, vsock_address(where)) if where.startswith("vsock:") else (socket.AF_UNIX, where)
    )
    try:
        with socket.socket(family, socket.SOCK_STREAM) as s:
            s.settimeout(3)
            s.connect(address)
            s.sendall(b"GET /contract HTTP/1.0\r\nHost: egress\r\n\r\n")
            reply = s.recv(4096)
        # any HTTP answer will do: the stand-in says 200, the real egress 404 to this path
        return reply.startswith(b"HTTP/"), reply.split(b"\r\n", 1)[0].decode(errors="replace")
    except OSError as e:
        return False, e.strerror or str(e)


ifaces = interfaces()
check(1, "only loopback", ifaces == ["lo"], ",".join(ifaces))
ok, why = tcp_out("1.1.1.1", 443)
check(1, "no TCP to the internet", not ok, why)
ok, why = resolves("example.com")
check(1, "no name resolution", not ok, why)
if LAYOUT["socket"].startswith("vsock:"):
    # A microVM's egress is a vsock port the host maps to its socket; any other port on
    # the host must find nothing.
    cid, port = vsock_address(LAYOUT["socket"])
    reached = []
    for other in (port + 1, 22, 1024, 49152):
        try:
            with socket.socket(socket.AF_VSOCK, socket.SOCK_STREAM) as s:
                s.settimeout(3)
                s.connect((cid, other))
            reached.append(str(other))
        except OSError:
            pass
    check(1, f"vsock reaches only port {port} on cid {cid}", not reached, ",".join(reached))
else:
    # A network namespace does not cover vsock: under a VM (WSL2, a cloud guest) it
    # reaches the hypervisor's host whatever the namespace says, so the box must refuse
    # the family.
    try:
        socket.socket(socket.AF_VSOCK, socket.SOCK_STREAM).close()
        check(1, "no vsock socket", False, "created")
    except OSError as e:
        check(1, "no vsock socket", True, e.strerror or str(e))
ok, why = egress_answers(LAYOUT["socket"])
check(1, "the egress socket answers", ok, why)

for key in ("home", "work"):
    ok, why = writable(LAYOUT[key])
    check(2, f"{key} writable at {LAYOUT[key]}", ok, why)
for key in ("projection", "system"):
    ok, why = writable(LAYOUT[key])
    readable = os.access(LAYOUT[key], os.R_OK)
    check(2, f"{key} readable, not writable, at {LAYOUT[key]}", readable and not ok, why)
mb = LAYOUT["memory_bin"]
try:
    open(mb, "ab").close()
    mb_writable = True
except OSError:
    mb_writable = False
check(2, f"memory binary present, read-only, at {mb}", os.access(mb, os.X_OK) and not mb_writable)
# A read-only bind that is not recursive leaves the host's submounts under it writable.
system = LAYOUT["system"].rstrip("/") + "/"
with open("/proc/self/mountinfo") as f:
    rw_below = [
        fields[4]
        for fields in (line.split() for line in f)
        if fields[4].startswith(system) and "rw" in fields[5].split(",")
    ]
check(2, f"no read-write mount under {LAYOUT['system']}", not rw_below, ",".join(rw_below))
if MARKER:
    check(2, "host home marker not visible", not os.path.exists(MARKER), MARKER)

print(f"info  pid 1 is {open('/proc/1/comm').read().strip()}; {len([p for p in os.listdir('/proc') if p.isdigit()])} processes visible")
print(f"info  uid {os.getuid()}, hostname {socket.gethostname()}")
sys.exit(0 if all(results) else 1)
