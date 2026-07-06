#!/usr/bin/env python3
"""K3 U-Boot serial console helper.

Drives the board's main UART (U-Boot prompt) and the RUART (firmware log)
via pyserial. Designed to be called by k3-flash.sh but also usable standalone
for debugging.

Subcommands:
    ensure-uboot            reset the board, catch the autoboot window, land at =>
    run "<cmd>"             send one U-Boot command, wait for => to return
    send-stage <itb>        host-side `fastboot stage <itb>`, then Ctrl-C the
                            board out of fastboot mode back to =>
    reset                   send `reset` over the console
    tail-log                attach the RUART and stream firmware log output

Configuration is read from environment variables (see cfg() below); a sensible
default is used if unset. The main UART is the one with the U-Boot prompt;
the RUART is the rt-async firmware log UART.

Exit codes: 0 on success, non-zero on timeout / port error / missing tool.
"""

import os
import sys
import time
import threading
import subprocess
import argparse


# ── configuration ────────────────────────────────────────────────────────────

def cfg(key, default):
    return os.environ.get(key, default)


MAIN_UART   = lambda: cfg("K3_MAIN_UART",   "/dev/tty.usbmodem62B68F06E7BF1")
RUART       = lambda: cfg("K3_RUART",       "/dev/tty.usbserial-114120")
BAUD        = lambda: int(cfg("K3_BAUD",    "115200"))
PROMPT      = lambda: cfg("K3_UBOOT_PROMPT", "=>")
AUTOBOOT_RE = lambda: cfg("K3_AUTOBOOT_RE", "stop autoboot|Hit any key|autoboot")
# how long to keep spamming 's' after reset to catch a very short autoboot window
ENSURE_TIMEOUT = lambda: float(cfg("K3_ENSURE_TIMEOUT", "15"))
# per-command timeout for `run`
RUN_TIMEOUT    = lambda: float(cfg("K3_RUN_TIMEOUT", "30"))


# ── serial helpers ───────────────────────────────────────────────────────────

def open_port(dev, baud, label):
    """Open a serial port with a clear error if it's busy/missing."""
    try:
        import serial
    except ImportError:
        sys.exit("ERROR: pyserial not installed.  pip3 install pyserial")
    if not os.path.exists(dev):
        sys.exit(f"ERROR [{label}]: serial device not found: {dev}\n"
                 f"  check the cable / K3_MAIN_UART / K3_RUART in flash.conf")
    try:
        return serial.Serial(dev, baud, timeout=0.1)
    except serial.SerialException as e:
        if "Resource busy" in str(e) or "could not open" in str(e).lower():
            sys.exit(f"ERROR [{label}]: {dev} is busy.\n"
                     f"  Close any picocom/screen/minicom holding it first:\n"
                     f"    pkill -f 'picocom {dev}' ; pkill -f 'screen {dev}'")
        raise


class Console:
    """A buffered reader/writer around a pyserial port.

    Reading is line/byte oriented; we accumulate everything received into
    self.buf (bytes) and expose helpers to wait for a substring or regex.
    """

    def __init__(self, port):
        self.port = port
        self.buf = bytearray()
        self.lock = threading.Lock()
        self._stop = False
        self._reader = threading.Thread(target=self._read_loop, daemon=True)
        self._reader.start()

    def _read_loop(self):
        try:
            while not self._stop:
                chunk = self.port.read(256)
                if chunk:
                    with self.lock:
                        self.buf.extend(chunk)
                        # echo to stdout so the user sees live board output
                        sys.stdout.write(chunk.decode("utf-8", "replace"))
                        sys.stdout.flush()
        except Exception:
            pass

    def write(self, data):
        if isinstance(data, str):
            data = data.encode()
        self.port.write(data)
        self.port.flush()

    def send(self, line):
        """Send a command followed by CR."""
        self.write(line + "\r")

    def snapshot(self):
        with self.lock:
            return bytes(self.buf)

    def tail(self, n=4096):
        """Return the last n bytes of the buffer."""
        with self.lock:
            return bytes(self.buf[-n:])

    def wait_for(self, needle, timeout, regex=False):
        """Block until `needle` (bytes) appears in buf, or timeout.

        needle may be a plain bytes substring or, if regex=True, a regex
        pattern matched against the tail of the buffer.
        Returns the matched segment or None on timeout.
        """
        if isinstance(needle, str):
            needle = needle.encode()
        deadline = time.time() + timeout
        if regex:
            import re
            pat = re.compile(needle)
        while time.time() < deadline:
            tail = self.tail(8192)
            if regex:
                m = pat.search(tail.decode("utf-8", "replace"))
                if m:
                    return m.group(0)
            else:
                if needle in tail:
                    return needle
            time.sleep(0.05)
        return None

    def close(self):
        self._stop = True
        time.sleep(0.1)
        try:
            self.port.close()
        except Exception:
            pass


# ── subcommands ──────────────────────────────────────────────────────────────

def cmd_reset(args):
    """Send `reset` and return (board reboots; caller decides what's next)."""
    con = Console(open_port(MAIN_UART(), BAUD(), "main"))
    try:
        con.send("reset")
        time.sleep(0.3)
    finally:
        con.close()
    return 0


def cmd_ensure_uboot(args):
    """Reset the board and land at the U-Boot prompt.

    Strategy for the very-short autoboot window:
      1. Send `reset`.
      2. Start spamming 's' every ~50ms immediately (don't wait to see the
         autoboot line — the window may be too short to read it).
      3. Also watch for the autoboot marker; if seen, send 's'.
      4. Stop as soon as the U-Boot prompt `=>` appears.
    """
    con = Console(open_port(MAIN_UART(), BAUD(), "main"))
    try:
        sys.stderr.write("▶ resetting board, catching autoboot window...\n")
        con.send("reset")
        deadline = time.time() + ENSURE_TIMEOUT()
        sent_stop = False
        import re
        autoboot = re.compile(AUTOBOOT_RE().encode())
        prompt = PROMPT().encode()
        while time.time() < deadline:
            tail = con.tail(4096)
            # did we reach the prompt?
            if prompt in tail:
                # the spam of 's' chars left junk on the command line; flush it
                # with a CR so the next command isn't corrupted (sssssfastboot...)
                con.write("\r")
                time.sleep(0.3)
                sys.stderr.write("✓ at U-Boot prompt\n")
                return 0
            # did the autoboot line show up? send 's' once
            if not sent_stop and autoboot.search(tail):
                con.write("s")
                sent_stop = True
            # always also spam 's' to cover an invisible short window
            con.write("s")
            time.sleep(0.05)
        sys.stderr.write("✗ timed out waiting for U-Boot prompt\n")
        return 1
    finally:
        con.close()


def cmd_run(args):
    """Send one U-Boot command and wait for the prompt to come back."""
    con = Console(open_port(MAIN_UART(), BAUD(), "main"))
    try:
        # flush any leftover chars on the command line first, then wait for a
        # clean prompt so we don't capture a stale one
        con.write("\r")
        con.wait_for(PROMPT(), 2)
        # CRITICAL: clear the buffer right before sending, so the wait_for below
        # only matches a NEW prompt produced by THIS command — not the stale `=>`
        # that the \r above just drew (which would make long commands like
        # `mtd write` falsely appear to finish instantly).
        with con.lock:
            con.buf.clear()
        con.send(args.cmd)
        ok = con.wait_for(PROMPT(), RUN_TIMEOUT())
        if ok is None:
            sys.stderr.write(f"✗ timeout waiting for prompt after: {args.cmd}\n")
            return 2
        return 0
    finally:
        con.close()


def cmd_send_stage(args):
    """Run host-side `fastboot stage <itb>`, then send Ctrl-C over the console
    to bring the board out of fastboot gadget mode back to the U-Boot prompt.

    Verified manual flow: stage transfers → Ctrl-C on the serial console →
    U-Boot reprints `=>` → then mtd erase/write can run.
    """
    itb = args.itb
    if not os.path.exists(itb):
        sys.exit(f"ERROR: itb not found: {itb}")
    # 1. host-side stage
    sys.stderr.write(f"▶ fastboot stage {itb}\n")
    rc = subprocess.call(["fastboot", "stage", itb])
    if rc != 0:
        sys.exit(f"ERROR: fastboot stage failed (rc={rc})")
    # 2. Ctrl-C the board out of fastboot mode -> back to =>
    sys.stderr.write("▶ sending Ctrl-C to leave fastboot mode...\n")
    con = Console(open_port(MAIN_UART(), BAUD(), "main"))
    try:
        for _ in range(3):
            con.write("\x03")  # Ctrl-C
            time.sleep(0.3)
        con.write("\r")
        ok = con.wait_for(PROMPT(), 10)
        if ok is None:
            sys.stderr.write("⚠ did not see prompt after Ctrl-C; continuing\n")
        return 0
    finally:
        con.close()


def cmd_enter_fastboot(args):
    """Send `fastboot -l $loadaddr -s <size> usb 0` (literal `$loadaddr` string,
    expanded by U-Boot itself) and wait for the board to enter fastboot gadget
    mode by polling `fastboot devices` on the host.

    `$loadaddr` is sent verbatim — U-Boot expands it from its environment, so we
    don't need to know the numeric value on the host. `fastboot usb 0` is a
    BLOCKING command on the board and does not return to `=>` until exited, so
    we confirm via the host-side `fastboot devices` that the gadget is up.
    """
    size = args.size
    con = Console(open_port(MAIN_UART(), BAUD(), "main"))
    try:
        # flush any leftover chars, get a clean prompt
        con.write("\r")
        con.wait_for(PROMPT(), 2)
        # send the blocking fastboot command; $loadaddr is expanded by U-Boot
        con.send(f"fastboot -l $loadaddr -s {size} usb 0")
        time.sleep(0.5)  # let the command be consumed
    finally:
        con.close()
    # poll host-side fastboot devices (USB enumeration takes ~5-20s on K3)
    sys.stderr.write("▶ waiting for fastboot device...\n")
    deadline = time.time() + 40
    while time.time() < deadline:
        try:
            out = subprocess.check_output(["fastboot", "devices"], timeout=5)
        except Exception:
            out = b""
        if out.strip():
            sys.stderr.write(f"✓ fastboot device up: {out.decode().strip()}\n")
            return 0
        time.sleep(1)
    sys.stderr.write("✗ no fastboot device appeared after 40s\n")
    return 1


def cmd_tail_log(args):
    """Stream the RUART (firmware log) until Ctrl-C."""
    sys.stderr.write(f"▶ tailing RUART {RUART()} (Ctrl-C to exit)\n")
    con = Console(open_port(RUART(), BAUD(), "ruart"))
    try:
        while True:
            time.sleep(0.5)
    except KeyboardInterrupt:
        sys.stderr.write("\n■ stopped\n")
        return 0
    finally:
        con.close()


# ── entrypoint ───────────────────────────────────────────────────────────────

def main():
    p = argparse.ArgumentParser(
        description="K3 U-Boot serial console helper")
    sub = p.add_subparsers(dest="subcommand", required=True)

    sub.add_parser("ensure-uboot", help="reset + catch autoboot + land at =>")
    sub.add_parser("reset",        help="send `reset` over the console")
    sp = sub.add_parser("run",     help="send one U-Boot command, wait for =>")
    sp.add_argument("cmd",         help="the U-Boot command string")

    sp = sub.add_parser("send-stage", help="fastboot stage <itb> then Ctrl-C board out")
    sp.add_argument("itb")

    sp = sub.add_parser("enter-fastboot",
                        help="send `fastboot -l $loadaddr -s <size> usb 0` and "
                             "wait for the host to see the fastboot device")
    sp.add_argument("size", nargs="?", default="0x100000")

    sub.add_parser("tail-log",     help="stream the RUART firmware log")

    args = p.parse_args()

    dispatch = {
        "ensure-uboot":   cmd_ensure_uboot,
        "reset":          cmd_reset,
        "run":            cmd_run,
        "send-stage":     cmd_send_stage,
        "enter-fastboot": cmd_enter_fastboot,
        "tail-log":       cmd_tail_log,
    }
    try:
        return dispatch[args.subcommand](args)
    except SystemExit:
        raise
    except Exception as e:
        sys.stderr.write(f"ERROR: {type(e).__name__}: {e}\n")
        return 1


if __name__ == "__main__":
    sys.exit(main())
