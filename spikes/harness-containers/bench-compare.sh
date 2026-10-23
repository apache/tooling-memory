#!/bin/sh
# bench.sh under rootless podman (runc), gVisor as it comes, and gVisor with its mounts
# cached (`--file-access-mounts=exclusive`), on one rootfs, ROUNDS times (3 by default)
# with the boxes interleaved, then the median per task. ROOTFS and RUNSC as for
# run-gvisor.sh. Where /dev/kvm is open to the caller, gVisor on its kvm platform
# (`--platform=kvm`) runs as a fourth box. Run it on a quiet machine: anything else running
# lands in the numbers.
here=$(cd "$(dirname "$0")" && pwd)
log=$(mktemp "${TMPDIR:-/tmp}/bench.XXXXXX")
r=0
while [ "$r" -lt "${ROUNDS:-3}" ]; do
    r=$((r + 1))
    "$here/run-podman.sh" sh /work/bench.sh 2>/dev/null | sed -n "s/^\([a-z_0-9]* [0-9]*\)$/runc \1/p" >> "$log"
    "$here/run-gvisor.sh" sh /work/bench.sh 2>/dev/null | sed -n "s/^\([a-z_0-9]* [0-9]*\)$/gvisor \1/p" >> "$log"
    RUNSC_FLAGS=--file-access-mounts=exclusive "$here/run-gvisor.sh" sh /work/bench.sh 2>/dev/null |
        sed -n "s/^\([a-z_0-9]* [0-9]*\)$/gvisor-exclusive \1/p" >> "$log"
    if [ -r /dev/kvm ] && [ -w /dev/kvm ]; then
        RUNSC_FLAGS=--platform=kvm "$here/run-gvisor.sh" sh /work/bench.sh 2>/dev/null |
            sed -n "s/^\([a-z_0-9]* [0-9]*\)$/gvisor-kvm \1/p" >> "$log"
    fi
done
python3 -I - "$log" <<'EOF'
import collections, statistics, sys
runs = collections.defaultdict(list)
for line in open(sys.argv[1]):
    box, task, ms = line.split()
    runs[(task, box)].append(int(ms))
boxes = [b for b in ["runc", "gvisor", "gvisor-exclusive", "gvisor-kvm"]
         if any(k[1] == b for k in runs)]
print(f"| Task | {' | '.join(b + ' ms' for b in boxes)} | {' | '.join(b + ' / runc' for b in boxes[1:])} |")
print("|---|" + "---|" * (2 * len(boxes) - 1))
for task in dict.fromkeys(t for t, _ in runs):
    m = [statistics.median(runs[(task, b)]) for b in boxes]
    print(f"| {task} | {' | '.join(f'{x:.0f}' for x in m)} | {' | '.join(f'{x / m[0]:.1f}' for x in m[1:])} |")
EOF
rm -f "$log"
