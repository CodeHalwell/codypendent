# /// script
# requires-python = ">=3.11"
# dependencies = ["pyte>=0.8.2,<0.9"]
# ///
"""Exercise the built CLI through a real PTY, saving screen and byte evidence.

    uv run crates/cli/tests/pty_smoke.py --binary target/debug/codypendent

Uses disposable local data by default and never submits a model prompt. Pass
--data-dir, --socket and --repository to attach to a prepared local fixture.
"""

import argparse
import codecs
import fcntl
import json
import os
from pathlib import Path
import pty
import select
import struct
import subprocess
import tempfile
import termios
import time

import pyte


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--data-dir", type=Path)
    parser.add_argument("--socket", type=Path)
    parser.add_argument("--repository", type=Path)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--inspect-only", action="store_true", help="View the existing transcript and workspace without editing or submitting")
    parser.add_argument("--review-patch", metavar="FILE_QUERY", help="Find a patch by its changed-file search text and inspect file/hunk shortcuts; never submits")
    parser.add_argument("--open-reviewed-source", metavar="FILE", help="With --review-patch, open this preview file in the real IDE and preserve an unsent draft")
    args = parser.parse_args()
    if args.open_reviewed_source and not args.review_patch:
        parser.error("--open-reviewed-source requires --review-patch")
    binary = args.binary.resolve()
    root = Path(tempfile.mkdtemp(prefix="cody-pty-"))
    data = (args.data_dir or root / "data").resolve()
    repository = (args.repository or root / "workspace").resolve()
    output = (args.output or root / "evidence").resolve()
    repository.mkdir(parents=True, exist_ok=True)
    output.mkdir(parents=True, exist_ok=True)
    if not args.repository:
        subprocess.run(["git", "init", "-q", str(repository)], check=True)
    env = dict(os.environ, TERM="xterm-256color", COLORTERM="truecolor")
    env["CODYPENDENT_DATA_DIR"] = str(data)
    env["CODYPENDENT_CONFIG_DIR"] = str(data / "config")
    env["CODYPENDENT_SOCKET"] = str(args.socket or data / "run/daemon.sock")
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 32, 110, 0, 0))

    def claim_terminal():
        os.setsid()
        fcntl.ioctl(0, termios.TIOCSCTTY, 0)

    process = subprocess.Popen(
        [str(binary), "-C", str(repository), "--theme", "dark"],
        stdin=slave, stdout=slave, stderr=slave, env=env, preexec_fn=claim_terminal,
    )
    os.close(slave)
    screen = pyte.Screen(110, 32)
    stream = pyte.Stream(screen)
    decoder = codecs.getincrementaldecoder("utf-8")("replace")
    transcript = bytearray()
    checks = []

    def read_for(seconds=0.25):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            remaining = max(0, min(0.05, deadline - time.monotonic()))
            if select.select([master], [], [], remaining)[0]:
                try:
                    chunk = os.read(master, 65536)
                except OSError:
                    break
                if not chunk:
                    break
                transcript.extend(chunk)
                stream.feed(decoder.decode(chunk))

    def visible():
        return "\n".join(screen.display)

    def wait_for(predicate, label, timeout=10):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            read_for()
            if predicate(visible()):
                checks.append(label)
                print(f"PASS {label}", flush=True)
                return
            if process.poll() is not None:
                break
        raise AssertionError(f"{label}\n{visible()}")

    def send(keys):
        os.write(master, keys.encode())
        read_for()

    def capture(name):
        (output / f"{name}.txt").write_text(visible() + "\n")

    def wait_exit(timeout=15):
        deadline = time.monotonic() + timeout
        while process.poll() is None and time.monotonic() < deadline:
            read_for(0.1)
        if process.poll() is None:
            raise subprocess.TimeoutExpired(process.args, timeout)

    try:
        wait_for(lambda text: "ENTER" in text, "ready splash", timeout=180)
        capture("01-splash")
        send("\r")
        wait_for(lambda text: "MESSAGE" in text or "Connect a model" in text,
                 "onboarding or composer visible")
        # First launch without a model opens onboarding. Skip only in the
        # disposable fixture so keyboard checks need no provider credentials.
        if "Connect a model" in visible():
            send("\x1b")
            wait_for(lambda text: "Skip model setup?" in text, "skip setup confirmation")
            send("\r")
        wait_for(lambda text: "MESSAGE" in text and "Skip model setup?" not in text
                 and "Connect a model" not in text, "composer visible")
        if args.inspect_only or args.review_patch:
            capture("02-existing-transcript")
            if args.review_patch:
                if args.open_reviewed_source:
                    send("handoff draft - do not send")
                    wait_for(lambda text: "handoff draft - do not send" in text,
                             "unsent handoff draft visible")
                # Search selects a stable patch directly even in long runs.
                send("\x06")
                wait_for(lambda text: "Find in conversation" in text, "conversation search visible")
                send(args.review_patch)
                wait_for(lambda text: "1 match(es)" in text, "exact changed-file search result")
                send("\r")
                wait_for(lambda text: "Find in conversation" not in text, "patch selected from search")
                send("\x1bN")
                wait_for(lambda text: "File 1/" in text, "changed-file review visible")
                capture("03-patch-first-file")
                send("\x1bN")
                wait_for(lambda text: "File 2/" in text, "next changed file")
                capture("04-patch-next-file")
                send("\x1bP")
                wait_for(lambda text: "File 1/" in text, "previous changed file")
                send("\x1bn")
                wait_for(lambda text: "Hunk " in text, "next hunk with context")
                capture("05-patch-hunk")
                send("\x1bp")
                wait_for(lambda text: "Hunk 1/" in text, "previous hunk saturates safely")
                if args.open_reviewed_source:
                    def source_selected(text):
                        return any(("File " in row or "Hunk " in row)
                                   and f"b/{args.open_reviewed_source}" in row
                                   for row in text.splitlines())

                    for _ in range(32):
                        if source_selected(visible()):
                            break
                        send("\x1bN")
                    wait_for(source_selected, "requested source file selected")
                    if "Hunk " not in visible():
                        send("\x1bn")
                    capture("06-source-hunk")
                    send("\x1bo")
                    wait_for(lambda text: f"Opened {args.open_reviewed_source}:" in text and "same session" in text,
                             "reviewed source handed to real IDE", timeout=30)
                    assert "handoff draft - do not send" in visible(), visible()
                    checks.append("exact source handoff preserves the unsent draft")
                    print(f"PASS {checks[-1]}", flush=True)
                    capture("06-editor-handoff")
                send("\x1b\r")
                wait_for(lambda text: "full diff" in text and "Hunk " not in text,
                         "return to complete preview")
                capture("06-patch-full-preview")
            send("\x1bOQ")
            wait_for(lambda text: "Runs (" in text, "existing run workspace visible")
            capture("07-existing-workspace" if args.review_patch else "03-existing-workspace")
            send("\x03")
            wait_exit()
            read_for()
            assert process.returncode == 0, process.returncode
            checks.append("view-only clean detach without a submitted objective")
            print(f"PASS {checks[-1]}", flush=True)
            return
        send("\x1b[200~fix café errors\x1b[201~")
        wait_for(lambda text: "fix café errors" in text, "Unicode bracketed paste")
        send("\x1bb")
        send("\x1b\x7f")
        wait_for(lambda text: "fix errors" in text and "café" not in text,
                 "Option word movement and deletion")
        send("\x1bf")
        send("\x1b[13;2u")
        send("second line")
        wait_for(lambda text: "fix errors" in text and "second line" in text,
                 "Shift-Enter creates a multiline draft")
        capture("02-multiline-composer")
        send("\x1bOP")
        send("word")
        wait_for(lambda text: "Filter: word" in text, "searchable keyboard help")
        capture("03-help")
        send("\x1b")
        wait_for(lambda text: "Filter: word" not in text and "second line" in text,
                 "help dismissed with draft intact")
        send("\x1b")
        wait_for(lambda text: "second line" not in text, "Escape clears draft")
        send("\x1a")
        wait_for(lambda text: "second line" in text, "Escape draft restored with Ctrl-Z")
        send("\x1bOQ")
        capture("04-workspace")
        fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack("HHHH", 12, 40, 0, 0))
        screen.resize(12, 40)
        read_for(0.5)
        capture("05-compact")
        fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack("HHHH", 32, 110, 0, 0))
        screen.resize(32, 110)
        read_for(0.5)
        send("\x1bOQ")
        wait_for(lambda text: "second line" in text, "draft survives layout and terminal resize")
        capture("06-restored")
        send("\x03")
        wait_exit()
        read_for()
        assert process.returncode == 0, process.returncode
        assert transcript.count(b"\x1b[22;0t") == 1
        assert transcript.count(b"\x1b]0;codypendent") == 1
        assert b"\x1b[23;0t" in transcript
        assert b"\x1b[<1u" in transcript
        checks.append("clean detach; one title update; title and keyboard state restored")
        print(f"PASS {checks[-1]}", flush=True)
    finally:
        if process.poll() is None:
            os.write(master, b"\x03")
            try:
                wait_exit()
            except subprocess.TimeoutExpired:
                process.terminate()
                try:
                    wait_exit(5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=5)
        (output / "terminal.raw").write_bytes(transcript)
        (output / "result.json").write_text(json.dumps({"checks": checks, "exit_code": process.returncode}, indent=2))
        os.close(master)
        if not args.data_dir:
            subprocess.run([str(binary), "daemon", "stop"], env=env, check=False, capture_output=True)
        print(f"Evidence: {output}", flush=True)


if __name__ == "__main__":
    main()
