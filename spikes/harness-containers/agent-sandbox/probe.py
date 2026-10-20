#!/usr/bin/env python3
"""What can this container reach? One line per `name=host:port` argument, each
tried as a TCP connect (3 s timeout), and an HTTP GET where the name ends in `-http`.
Standard library only."""

import socket
import sys

for arg in sys.argv[1:]:
    name, target = arg.split("=", 1)
    host, port = target.rsplit(":", 1)
    try:
        with socket.create_connection((host, int(port)), timeout=3) as s:
            if name.endswith("-http"):
                s.sendall(b"GET / HTTP/1.0\r\nHost: egress\r\n\r\n")
                line = s.recv(256).split(b"\r\n", 1)[0].decode(errors="replace")
                print(f"REACH    {name} {target}  ({line})")
            else:
                print(f"REACH    {name} {target}")
    except OSError as e:
        print(f"BLOCKED  {name} {target}  ({e.strerror or e})")
