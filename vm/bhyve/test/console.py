#!/usr/bin/env python3
"""console.py [--port N | --pty COMMAND] [--log FILE] STEP...: drive a guest's serial console.

--port N: connect to bhyve -o console=tcp=127.0.0.1:N (output from before the connection is lost).
--pty COMMAND: make a pseudo-terminal, run COMMAND (a shell command line, {tty} is the terminal's
name: bhyve -o console={tty} ...) and drive that; nothing is lost.

Each STEP is "EXPECT=>SEND" (wait for the text EXPECT, then type SEND and Enter), "EXPECT=>" (only
wait), "=>SEND" (type at once), "sleep:SECONDS", or "run:COMMAND" (start a host
command in the background); a SEND of "<enter>" is Enter alone, "<key>c" the key c without Enter. A step may start with "T:" to set its timeout in seconds (default 60),
e.g. "120:login:=>root". Prints what the guest wrote, without terminal escape sequences; exits 1 if
an EXPECT did not come.
"""
import re, socket, sys, time

args = sys.argv[1:]
port, logf, pty_cmd = 5555, None, None
while args and args[0].startswith("--"):
    if args[0] == "--port": port = int(args[1])
    elif args[0] == "--log": logf = open(args[1], "wb")
    elif args[0] == "--pty": pty_cmd = args[1]
    else: sys.exit(__doc__)
    args = args[2:]

if pty_cmd is not None:
    # The console on a pseudo-terminal that exists before bhyve starts: nothing the guest
    # writes is lost, however early. The command gets the terminal's name in place of {tty}.
    import os, pty, subprocess, tty
    master, slave = pty.openpty()
    tty.setraw(slave)
    proc = subprocess.Popen(pty_cmd.replace("{tty}", os.ttyname(slave)), shell=True)
    os.set_blocking(master, False)

    class PtySock:
        def recv(self, n):
            import select
            if not select.select([master], [], [], 0.2)[0]:
                if proc.poll() is not None:
                    return b""
                raise socket.timeout()
            try:
                return os.read(master, n)
            except BlockingIOError:
                raise socket.timeout()
        def sendall(self, d):
            os.write(master, d)
    s = PtySock()
else:
    deadline = time.time() + 15
    while True:
        try:
            s = socket.create_connection(("127.0.0.1", port), timeout=1)
            break
        except OSError:
            if time.time() > deadline: sys.exit("console.py: nothing listens on port %d" % port)
            time.sleep(0.2)
    s.settimeout(0.2)
ESC = re.compile(rb"\x1b\[[0-9;?]*[A-Za-z]|\x1b[78]|\r|\x00")
buf = b""          # cleaned output not yet matched against

def pump():
    global buf
    try:
        d = s.recv(65536)
    except socket.timeout:
        return
    if not d: sys.exit("console.py: the console closed (bhyve ended)")
    if logf: logf.write(d); logf.flush()
    d = ESC.sub(b"", d)
    sys.stdout.write(d.decode("utf-8", "replace")); sys.stdout.flush()
    buf += d

for step in args:
    if step.startswith("run:"):
        # a command on the host, in the background, while the next steps go on in the guest
        import subprocess
        subprocess.Popen(step[4:], shell=True)
        continue
    if step.startswith("sleep:"):
        end = time.time() + float(step[6:])
        while time.time() < end: pump()
        continue
    timeout = 60
    m = re.match(r"(\d+):", step)
    if m: timeout, step = int(m.group(1)), step[m.end():]
    expect, _, send = step.partition("=>")
    end = time.time() + timeout
    while expect.encode() not in buf:
        if time.time() > end:
            print("\nconsole.py: no %r within %d s" % (expect, timeout)); sys.exit(1)
        pump()
    if expect:
        buf = buf.split(expect.encode(), 1)[1]
    if send == "<enter>":
        s.sendall(b"\r")
    elif send.startswith("<key>"):
        s.sendall(send[5:].encode())
    elif send != "":
        s.sendall(send.encode() + b"\r")
end = time.time() + 1
while time.time() < end: pump()
