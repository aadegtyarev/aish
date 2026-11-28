#!/usr/bin/env python3
"""PTY latency bench: startup to first prompt and key-echo latency, a binary vs `bash --norc -i`.
Usage: build/bench.py target/release/aish   (run by ./check.sh bench)"""
import os, pty, sys, time, select, tempfile, statistics as st

def read_until(fd, pred, timeout=5):
    buf = b''
    end = time.perf_counter() + timeout
    while time.perf_counter() < end:
        if select.select([fd], [], [], 0.05)[0]:
            try:
                d = os.read(fd, 65536)
            except OSError:
                break
            if not d:
                break
            buf += d
            if b'\x1b[6n' in d:  # aish asks for the cursor position
                os.write(fd, b'\x1b[1;47R')
            if pred(buf):
                return buf, time.perf_counter()
    return buf, None

def run(argv, env, ready):
    t0 = time.perf_counter()
    pid, fd = pty.fork()
    if pid == 0:
        os.execvpe(argv[0], argv, env)
    _, t = read_until(fd, ready)
    start = (t - t0) * 1000 if t else None
    time.sleep(0.3)
    read_until(fd, lambda b: False, 0.2)
    lat = []
    for i in range(300):
        c = b'xyzqwxyzqw'[i % 10:i % 10 + 1]
        t1 = time.perf_counter()
        os.write(fd, c)
        _, t2 = read_until(fd, lambda b, c=c: c in b, 2)
        if t2:
            lat.append((t2 - t1) * 1000)
        if i % 10 == 9:
            os.write(fd, b'\x15')  # kill line
            read_until(fd, lambda b: False, 0.03)
    os.write(fd, b'\x15exit\r')
    try:
        os.waitpid(pid, 0)
    except OSError:
        pass
    os.close(fd)
    return start, lat

def bench(name, argv, ready, env):
    starts, lats = [], []
    for _ in range(10):
        s, l = run(argv, env, ready)
        if s:
            starts.append(s)
        lats += l
    if not starts or not lats:
        sys.exit(f"{name}: no prompt or no echo")
    lats.sort()
    print(f"{name:5} startup ms: median {st.median(starts):.1f} max {max(starts):.1f} | "
          f"echo ms: median {st.median(lats):.3f} p99 {lats[int(len(lats) * .99)]:.3f} n={len(lats)}")
    return st.median(starts)

aish = os.path.abspath(sys.argv[1])
env = {'HOME': tempfile.mkdtemp(), 'PATH': '/usr/bin:/bin', 'TERM': 'xterm-256color',
       'LANG': 'C.UTF-8', 'PS1': '$ '}
s = bench('aish', [aish], lambda b: b'?\x1b[0m' in b, env)
bench('bash', ['bash', '--norc', '-i'], lambda b: b.rstrip().endswith(b'$'), env)
if s > 200:  # very generous: only catches a real regression
    sys.exit(f"aish startup {s:.0f} ms > 200 ms")
