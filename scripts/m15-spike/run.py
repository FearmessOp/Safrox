#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
#
# M15 spike runner. Throwaway; deleted with probe.S once the question
# is answered.
#
# The claim under test: QMP `send-key` delivers a scan code to port
# 0x60 of the guest, under the exact QEMU flags the project uses
# (`-M q35 -display none`).
#
# The claim is proved only if the serial log shows the *expected hex
# bytes*. A clean exit proves nothing; an empty log proves the
# opposite of what we want. So the script names the bytes it expects
# and fails loudly when they do not appear.

import json
import os
import socket
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
IMAGE = os.path.join(HERE, "probe.img")
SERIAL = os.path.join(HERE, "serial.log")
PORT = 55556


def rpc(f, obj):
    f.write((json.dumps(obj) + "\n").encode())
    f.flush()
    while True:
        line = f.readline()
        if not line:
            return None
        msg = json.loads(line)
        if "return" in msg or "error" in msg:
            return msg


def main() -> int:
    if os.path.exists(SERIAL):
        os.remove(SERIAL)

    cmd = [
        "qemu-system-x86_64",
        "-M", "q35",
        "-m", "256M",
        "-cpu", "qemu64,+smep,+smap",
        "-no-reboot",
        "-no-shutdown",
        "-display", "none",
        "-drive", f"file={IMAGE},format=raw,if=floppy",
        "-serial", f"file:{SERIAL}",
        "-qmp", f"tcp:127.0.0.1:{PORT},server=on,wait=off",
    ]
    print("[spike] " + " ".join(cmd), flush=True)
    proc = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)

    # Wait for the guest to announce itself rather than sleeping a
    # fixed amount (ADR 023, K8). If the banner never appears the
    # spike is broken and nothing it says about the keyboard counts.
    deadline = time.monotonic() + 15.0
    ready = False
    while time.monotonic() < deadline:
        if os.path.exists(SERIAL):
            with open(SERIAL, "rb") as handle:
                if b"SPIKE-READY" in handle.read():
                    ready = True
                    break
        time.sleep(0.05)

    if not ready:
        print("[spike] FAIL: the boot sector never printed its banner.")
        print("[spike] The spike itself is broken; it says nothing about QEMU.")
        proc.kill()
        return 1

    print("[spike] guest is up and polling 0x60")

    sock = socket.create_connection(("127.0.0.1", PORT), timeout=5)
    f = sock.makefile("rwb")
    f.readline()                       # greeting
    rpc(f, {"execute": "qmp_capabilities"})

    # 'a' alone, then shift+a, then an extended key and the three
    # modifiers the brief names.
    #
    # **Which set arrives is itself part of the answer** and the
    # reason the log is printed in full rather than merely searched:
    # the brief assumes set 2, and if 'a' comes back as 0x1e/0x9e then
    # translation is on and the keyboard component must be written
    # against set 1.
    #
    # `up` is here because the 0xE0 prefix is an acceptance criterion
    # and translation handles prefixed codes differently from plain
    # ones -- the prefix passes through untranslated while the byte
    # after it does not. Guessing that would be guessing.
    for keys in (["a"], ["shift", "a"], ["up"], ["ctrl", "c"], ["alt", "x"]):
        result = rpc(f, {"execute": "send-key", "arguments": {
            "keys": [{"type": "qcode", "data": k} for k in keys]
        }})
        print(f"[spike] send-key {keys}: {result}")
        time.sleep(0.3)

    time.sleep(0.5)
    rpc(f, {"execute": "quit"})
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        proc.kill()

    with open(SERIAL, "rb") as handle:
        output = handle.read().decode("ascii", errors="replace")

    print("--- serial " + "-" * 50)
    print(output.strip())
    print("-" * 61)

    codes = output.replace("SPIKE-READY", "").split()
    if not codes:
        print("[spike] RESULT: NO. send-key returned success but no byte")
        print("[spike]         ever reached port 0x60. Key injection via")
        print("[spike]         QMP is not available; M15 needs a fallback.")
        return 1

    print(f"[spike] RESULT: YES. {len(codes)} byte(s) at 0x60: {' '.join(codes)}")
    if "1e" in codes:
        print("[spike]         0x1e present -> scan code set 1 (BIOS translation on)")
    if "1c" in codes:
        print("[spike]         0x1c present -> scan code set 2 (no translation)")
    return 0


if __name__ == "__main__":
    sys.exit(main())