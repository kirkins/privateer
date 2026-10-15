#!/usr/bin/env python3
"""Exercise the real demo TUI in an isolated PTY; no Herdr/fleet access."""
import argparse
import errno
import fcntl
import os
from pathlib import Path
import pty
import select
import signal
import struct
import termios
import time


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", default="target/debug/privateer-dashboard")
    args = parser.parse_args()
    binary = Path(args.binary).resolve()
    demo(str(binary))
    from dashboard_live_smoke import run, onboarding
    run(binary, binary.with_name("privateer"))
    onboarding(binary, binary.with_name("privateer"))


def demo(binary):
    gate_read, gate_write = os.pipe()
    pid, terminal = pty.fork()
    if pid == 0:
        os.close(gate_write)
        os.read(gate_read, 1)
        os.close(gate_read)
        os.environ["TERM"] = "xterm-256color"
        os.execv(binary, [binary, "--demo"])
    os.close(gate_read)
    before = termios.tcgetattr(terminal)
    output = bytearray()
    reaped = False

    def resize(width, height):
        fcntl.ioctl(terminal, termios.TIOCSWINSZ, struct.pack("HHHH", height, width, 0, 0))

    def drain(seconds):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            if select.select([terminal], [], [], min(0.05, max(0, deadline - time.monotonic())))[0]:
                try:
                    data = os.read(terminal, 8192)
                except OSError as error:
                    if error.errno == errno.EIO:
                        break
                    raise
                if not data:
                    break
                output.extend(data)
                if len(output) > 262144:
                    raise AssertionError("TUI produced excessive output during smoke test")

    try:
        resize(120, 34)
        os.write(gate_write, b"1")
        os.close(gate_write)
        drain(1.4)
        assert all(word in output for word in [b"PRIVATEER", b"DEMO", b"sample", b"data"]), "missing initial dashboard view"
        assert b"\x1b[?1006h" in output, "mouse capture was not enabled"
        def click(column, row):
            start = len(output)
            os.write(terminal, f"\x1b[<0;{column};{row}M\x1b[<0;{column};{row}m".encode())
            drain(0.3)
            return output[start:]
        assert b"Bridge" in click(5,10), "ship click did not open its workspace"
        assert b"awaiting" in click(55,5), "Assignments click did not show handoff details"
        assert b"codex" in click(71,5), "Terminals click did not show this ship's agent"
        assert b"Custody" in click(85,5), "Logbook click did not show ship history"
        os.write(terminal, b"i")
        drain(0.2)
        os.write(terminal, b"l")
        drain(0.2)
        os.write(terminal, b"m")
        drain(0.2)
        # Ratatui emits cursor moves between unchanged spaces, so phrases need
        # not appear contiguously in a terminal's incremental byte stream.
        assert all(word in output for word in [b"Tab:", b"preview", b"cancel"]), "demo composer did not open"
        os.write(terminal, b"status please\t\r")
        drain(0.3)
        assert b"Recorded" in output, "status preview did not render"
        click(5,4)
        for key in [b"2", b"3", b"f", b"4", b"1", b"c", b"0"]:
            os.write(terminal, key)
            drain(0.15)
        resize(20, 8)
        drain(0.25)
        resize(120, 34)
        drain(0.25)
        os.write(terminal, b"q")
        deadline = time.monotonic() + 3
        while time.monotonic() < deadline:
            drain(0.05)
            finished, status = os.waitpid(pid, os.WNOHANG)
            if finished:
                reaped = True
                assert os.waitstatus_to_exitcode(status) == 0, status
                break
        assert reaped, "TUI did not respond to quit"
        after = termios.tcgetattr(terminal)
        for flag in [termios.ECHO, termios.ICANON, termios.ISIG]:
            assert (before[3] & flag) == (after[3] & flag), "terminal mode was not restored"
        assert b"\x1b[?1049l" in output, "alternate screen was not restored"
        assert b"\x1b[?1006l" in output, "mouse capture was not restored"
        print("Dashboard PTY smoke passed: ship/inbox/liaison navigation, demo composer, resize, quit and terminal/mouse restoration.")
    finally:
        if not reaped:
            os.kill(pid, signal.SIGKILL)
            os.waitpid(pid, 0)
        os.close(terminal)


if __name__ == "__main__":
    main()

