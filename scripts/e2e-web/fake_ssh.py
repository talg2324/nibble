#!/usr/bin/env python3
"""A stand-in for `ssh` in scripts/e2e.sh's "forward" leg, where no ssh server is at hand: the
"other machine" is this one. It does what nib asks of ssh and nothing else:

  ssh … HOST COMMAND             runs COMMAND with sh (nibd over stdio)
  ssh -N -L 127.0.0.1:L:localhost:R … HOST
                                 carries this machine's port L to port R until it is killed
  ssh -O forward|cancel -L … HOST
                                 the same on a "shared connection": a relay started apart, and
                                 stopped ($FAKE_SSH_DIR keeps their pids). Refused for hosts
                                 named in $FAKE_SSH_NO_MUX, as ssh does without a master
  ssh -O stop … | ssh -G …       nothing

Every invocation is appended to $FAKE_SSH_DIR/log.
"""
import os, signal, socket, subprocess, sys, threading

WITH_VALUE = {"-o", "-l", "-p", "-i", "-J", "-L", "-O", "-F", "-E"}


def relay(spec):
    _, lport, _, rport = spec.rsplit(":", 3)
    srv = socket.socket()
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    try:
        srv.bind(("127.0.0.1", int(lport)))
    except OSError as e:
        sys.exit(f"bind [127.0.0.1]:{lport}: {e}")
    srv.listen(16)

    def pump(a, b):
        try:
            while data := a.recv(65536):
                b.sendall(data)
        except OSError:
            pass
        for s in (a, b):
            try:
                s.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass

    while True:
        conn, _ = srv.accept()
        if lport == rport:
            # The other machine is this one: its port, were anything there, would not be this
            # relay. Nothing listens there.
            conn.close()
            continue
        try:
            out = socket.create_connection(("localhost", int(rport)), 5)
        except OSError:
            conn.close()
            continue
        threading.Thread(target=pump, args=(conn, out), daemon=True).start()
        threading.Thread(target=pump, args=(out, conn), daemon=True).start()


def main():
    args = sys.argv[1:]
    state = os.environ.get("FAKE_SSH_DIR", "/tmp")
    with open(os.path.join(state, "log"), "a") as f:
        f.write(" ".join(args) + "\n")
    if args[:1] == ["--relay"]:
        relay(args[1])
    opts, flags, rest = {}, set(), []
    i = 0
    while i < len(args):
        a = args[i]
        if a in WITH_VALUE:
            opts.setdefault(a, []).append(args[i + 1])
            i += 2
        elif a.startswith("-") and not rest:
            flags.add(a)
            i += 1
        else:
            rest.append(a)
            i += 1
    if "-G" in flags or not rest:
        return 0
    host = rest[0]
    assert "BatchMode=yes" in opts.get("-o", []) or "-O" not in opts and "-N" not in flags, "a forward that could ask"
    if "-O" in opts:
        what, spec = opts["-O"][0], (opts.get("-L") or [""])[0]
        pid_file = os.path.join(state, "mux-" + spec.replace(":", "_"))
        if what == "forward":
            if host in os.environ.get("FAKE_SSH_NO_MUX", "").split():
                sys.exit("Control socket connect(/nowhere): No such file or directory")
            p = subprocess.Popen([sys.executable, __file__, "--relay", spec], stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True)
            open(pid_file, "w").write(str(p.pid))
        elif what == "stop":
            # The shared connection ends, and what it carried with it.
            for name in os.listdir(state):
                if name.startswith("mux-"):
                    try:
                        os.kill(int(open(os.path.join(state, name)).read()), signal.SIGTERM)
                    except (OSError, ValueError):
                        pass
                    os.remove(os.path.join(state, name))
        elif what == "cancel":
            try:
                os.kill(int(open(pid_file).read()), signal.SIGTERM)
                os.remove(pid_file)
            except (OSError, ValueError):
                sys.exit("cancel: no such forward")
        return 0
    if "-N" in flags:
        relay(opts["-L"][0])
    os.execvp("sh", ["sh", "-c", rest[1]])


if __name__ == "__main__":
    sys.exit(main())
