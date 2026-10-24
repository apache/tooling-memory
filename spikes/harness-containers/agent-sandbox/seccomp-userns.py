#!/usr/bin/env python3
"""Write a Localhost seccomp profile: the container default, plus what our namespaces
backend needs.

Reads moby's default profile (Docker's format, Apache-2.0, vendored in moby/moby at
vendor/github.com/moby/profiles/seccomp/default.json) and flattens it for an x86_64
container that holds no capabilities: a rule gated on a capability is dropped, a rule
excluded by one is kept, a rule for another architecture is dropped. The result is in the
OCI form, which is what a kubelet Localhost profile is read as. It then allows unshare,
setns, namespace flags on clone, and mounting (mount(2) and the fsopen family), which a
process with no capabilities can only use inside a user namespace it created. AF_VSOCK stays refused,
because the default profile only allows socket() for the families it lists.

usage: seccomp-userns.py default.json > amem-userns.json
"""

import json
import sys

ARCHES = {"amd64", "x86_64", "x86", "x32", "386"}


def keep(rule):
    inc, exc = rule.get("includes") or {}, rule.get("excludes") or {}
    if inc.get("caps"):
        return False
    if inc.get("arches") and not ARCHES & set(inc["arches"]):
        return False
    if exc.get("arches") and ARCHES & set(exc["arches"]):
        return False
    return True


def main(path):
    with open(path) as f:
        default = json.load(f)
    rules = []
    for rule in default["syscalls"]:
        if not keep(rule):
            continue
        out = {"names": rule["names"], "action": rule["action"]}
        for key in ("args", "errnoRet"):
            if rule.get(key) is not None:
                out[key] = rule[key]
        rules.append(out)
    rules.append(
        {
            "names": [
                "unshare", "setns", "clone", "mount", "umount2", "pivot_root",
                # the new mount API, which util-linux's mount(8) tries first
                "fsopen", "fsconfig", "fsmount", "fspick", "move_mount", "open_tree",
                "mount_setattr",
            ],
            "action": "SCMP_ACT_ALLOW",
        }
    )
    profile = {
        "defaultAction": default["defaultAction"],
        "defaultErrnoRet": default.get("defaultErrnoRet", 1),
        "architectures": ["SCMP_ARCH_X86_64", "SCMP_ARCH_X86", "SCMP_ARCH_X32"],
        "syscalls": rules,
    }
    json.dump(profile, sys.stdout, indent=1)
    print()


main(sys.argv[1])
