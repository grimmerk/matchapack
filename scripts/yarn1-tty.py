#!/usr/bin/env python3
"""Run a command in a pseudo-terminal and answer `yarn upgrade-interactive`'s
prompt as a person would who takes every update: <a> (toggle all), then
Enter. For scripts/verify-yarn1.sh --command-tty (NOTES "Yarn 1 commands").

  yarn1-tty.py [--timeout SECONDS] -- <command> [args...]

The command's stdin, stdout and stderr are the terminal (120 columns); what
it prints is copied to stdout. When it prints Yarn's checkbox hint ("<a> to
toggle all"), the answer is typed once. A command that never asks (nothing
to upgrade) just runs to its end. Exit status: the command's (128 + the
signal when killed by one); 124 after the timeout (default 300 s), when the
command is killed.
"""
import fcntl, os, select, signal, struct, subprocess, sys, termios, time

PROMPT = b"to toggle all"


def main(argv):
    timeout = 300.0
    if len(argv) >= 2 and argv[0] == "--timeout":
        timeout = float(argv[1])
        argv = argv[2:]
    if not argv or argv[0] != "--" or len(argv) < 2:
        sys.exit("usage: yarn1-tty.py [--timeout SECONDS] -- <command> [args...]")
    cmd = argv[1:]
    master, slave = os.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))
    p = subprocess.Popen(cmd, stdin=slave, stdout=slave, stderr=slave, start_new_session=True, close_fds=True)
    os.close(slave)
    seen = b""
    answered = False
    deadline = time.monotonic() + timeout
    out = sys.stdout.buffer
    while True:
        left = deadline - time.monotonic()
        if left <= 0:
            os.killpg(p.pid, signal.SIGKILL)
            p.wait()
            out.write(b"\nyarn1-tty.py: timed out\n")
            out.flush()
            return 124
        r, _, _ = select.select([master], [], [], min(left, 0.5))
        if r:
            try:
                chunk = os.read(master, 65536)
            except OSError:
                chunk = b""
            if not chunk:
                break
            out.write(chunk)
            out.flush()
            # Looked for before the tail is cut: a read may hold the hint and
            # more than the tail after it.
            buf = seen + chunk
            hit = not answered and PROMPT in buf
            seen = buf[-4096:]
            if hit:
                answered = True
                # Inquirer reads keypresses; a short pause lets it draw first.
                time.sleep(0.3)
                os.write(master, b"a")
                time.sleep(0.3)
                os.write(master, b"\r")
        elif p.poll() is not None:
            # Exited, and nothing more to read for a while.
            break
    os.close(master)
    # A command that closed the terminal but runs on is held to the same
    # deadline.
    try:
        rc = p.wait(timeout=max(0.1, deadline - time.monotonic()))
    except subprocess.TimeoutExpired:
        os.killpg(p.pid, signal.SIGKILL)
        p.wait()
        out.write(b"\nyarn1-tty.py: timed out\n")
        out.flush()
        return 124
    return 128 - rc if rc < 0 else rc


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
