#!/usr/bin/env python3
"""I/O calibration probe (A8 assignment, absorbed by orchestrator).

Measures raw sequential and random-position read throughput/latency for 4 KiB and
1 MiB operations on this machine's NVMe, both buffered and (on Windows)
FILE_FLAG_NO_BUFFERING via ctypes. Purpose: calibrate the ceiling that `measured:`
numbers in results/*.json should respect, and sanity-check that NO_BUFFERING is
really bypassing the page cache (unbuffered 4 KiB random should be far slower than
buffered 4 KiB random once the file exceeds RAM cache relevance; here the file is
smaller than RAM, so the buffered numbers measure the cache, which is the point).

Usage: python scripts/audit_io_probe.py [file_mib=256]
Writes: results/io_probe.json
"""

import ctypes
import json
import os
import random
import statistics
import sys
import time
from ctypes import wintypes

KERNEL32 = ctypes.windll.kernel32
GENERIC_READ = 0x80000000
FILE_SHARE_READ = 1
OPEN_EXISTING = 3
FILE_FLAG_NO_BUFFERING = 0x20000000
FILE_BEGIN = 0


def open_nobuffer(path):
    h = KERNEL32.CreateFileW(path, GENERIC_READ, FILE_SHARE_READ, None,
                             OPEN_EXISTING, FILE_FLAG_NO_BUFFERING, None)
    if h == -1 or h == 0xFFFFFFFFFFFFFFFF:
        raise OSError(f"CreateFileW failed: {ctypes.GetLastError()}")
    return h


def read_at(h, offset, size):
    buf = ctypes.create_string_buffer(size)
    read = wintypes.DWORD(0)
    ok = KERNEL32.SetFilePointerEx(h, ctypes.c_int64(offset), None, FILE_BEGIN)
    if not ok:
        raise OSError("seek failed")
    overlapped = ctypes.c_void_p()
    ok = KERNEL32.ReadFile(h, buf, size, ctypes.byref(read), overlapped)
    if not ok or read.value != size:
        raise OSError(f"read failed ok={ok} got {read.value}")
    return buf.raw[:size]


def bench(path, mode, op_size, n_ops, seq=True, seed=1):
    """mode: 'buffered'|'unbuffered'. Returns (bytes_per_s, p50_us, p99_us)."""
    if mode == "unbuffered":
        h = open_nobuffer(path)
        readf = lambda off: read_at(h, off, op_size)
    else:
        f = open(path, "rb")
        def readf(off):
            f.seek(off)
            return f.read(op_size)
    file_len = os.path.getsize(path)
    max_off = max(0, file_len - op_size)
    rng = random.Random(seed)
    offsets = ([ (i * op_size * 64) % max_off for i in range(n_ops) ]  # stride to defeat prefetch
               if not seq else [ (i * op_size) % max_off for i in range(n_ops) ])
    # random mode: truly random offsets
    if not seq and mode == "buffered":
        offsets = [ rng.randrange(0, max_off + 1, 4096) for _ in range(n_ops) ]
    if not seq and mode == "unbuffered":
        offsets = [ rng.randrange(0, max_off + 1, 4096) for _ in range(n_ops) ]
    lat = []
    t0 = time.perf_counter()
    for off in offsets:
        s = time.perf_counter()
        readf(off)
        lat.append((time.perf_counter() - s) * 1e6)
    dt = time.perf_counter() - t0
    total = op_size * n_ops
    lat.sort()
    if mode == "unbuffered":
        KERNEL32.CloseHandle(h)
    else:
        f.close()
    return total / dt, statistics.median(lat), lat[max(0, int(0.99 * len(lat)) - 1)]


def main():
    mib = int(sys.argv[1]) if len(sys.argv) > 1 else 256
    os.makedirs("data/probe", exist_ok=True)
    os.makedirs("results", exist_ok=True)
    path = os.path.abspath(f"data/probe/probe_{mib}mib.bin")
    if not os.path.exists(path) or os.path.getsize(path) != mib * 1024 * 1024:
        with open(path, "wb") as f:
            blk = bytes(range(256)) * 16 * 1024  # 4 MiB pattern
            for _ in range(mib // 4):
                f.write(blk)
    out = {"file_mib": mib, "ops": {}}
    for mode in ("buffered", "unbuffered"):
        for size_name, size in (("4k", 4096), ("1m", 1024 * 1024)):
            for pat in ("seq", "random"):
                n = 4096 if size == 4096 else 256
                try:
                    bps, p50, p99 = bench(path, mode, size, n, seq=(pat == "seq"))
                    out["ops"][f"{mode}_{pat}_{size_name}"] = {
                        "MB_per_s": round(bps / 1e6, 1),
                        "p50_us": round(p50, 1),
                        "p99_us": round(p99, 1),
                        "n_ops": n,
                    }
                except OSError as e:
                    out["ops"][f"{mode}_{pat}_{size_name}"] = {"error": str(e)}
    with open("results/io_probe.json", "w", encoding="utf-8") as f:
        json.dump(out, f, indent=1)
    print(json.dumps(out, indent=1))


if __name__ == "__main__":
    main()
