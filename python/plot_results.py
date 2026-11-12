#!/usr/bin/env python3
"""Plot benchmark results from results/*.json (pinned schema, ARCHITECTURE §11).

Usage: python python/plot_results.py results/ results/plots
Reads every *.json run record; produces one summary chart set (PNG):
  - hit rate vs qps (baseline vs flatcake)
  - read amplification vs qps
  - latency p50/p99 vs qps
  - per-medium hit distribution (stacked bars at peak qps)
Writes summary.csv with one row per run for the experiment log.
"""

import csv
import json
import os
import re
import sys

QPS_RE = re.compile(r"qps([\d.]+)")


def load_runs(results_dir):
    runs = []
    for fn in sorted(os.listdir(results_dir)):
        if not fn.endswith(".json") or fn.startswith("summary"):
            continue
        path = os.path.join(results_dir, fn)
        try:
            with open(path, encoding="utf-8") as f:
                r = json.load(f)
        except (json.JSONDecodeError, OSError) as e:
            print(f"skip {fn}: {e}", file=sys.stderr)
            continue
        if "metrics" not in r or "system" not in r:
            continue  # non-run JSON (e.g. io_probe.json)
        m = QPS_RE.search(fn)
        r["_qps"] = float(m.group(1)) if m else 0.0
        r["_file"] = fn
        runs.append(r)
    return runs


def write_summary_csv(runs, out_dir):
    path = os.path.join(out_dir, "summary.csv")
    with open(path, "w", newline="", encoding="utf-8") as f:
        w = csv.writer(f)
        w.writerow([
            "file", "system", "seed", "qps", "delivered_hit_rate", "read_amp",
            "write_amp", "physical_read_ops", "logical_block_gets",
            "lat_p50_us", "lat_p99_us", "throughput_blocks_per_s",
            "modeled_recompute_us", "integrity_failures",
        ])
        for r in runs:
            m = r["metrics"]
            lat = m.get("latency_us", {})
            w.writerow([
                r["_file"], r["system"], r["seed"], r["_qps"],
                f"{m['delivered_hit_rate']:.4f}", f"{m['read_amp']:.4f}",
                f"{m['write_amp']:.4f}", m["physical_read_ops"],
                m["logical_block_gets"], lat.get("p50", 0), lat.get("p99", 0),
                f"{m['throughput_blocks_per_s']:.1f}",
                m["modeled_recompute_us"], m.get("integrity_failures", -1),
            ])
    return path


def plot(runs, out_dir):
    try:
        import matplotlib

        matplotlib.use("Agg")
        import matplotlib.pyplot as plt
    except ImportError:
        print("matplotlib not installed — skipping charts (summary.csv still written)", file=sys.stderr)
        return None

    def series(system, key, agg="mean"):
        pts = {}
        for r in runs:
            if r["system"] != system:
                continue
            m = r["metrics"]
            if key == "lat_p50":
                v = m.get("latency_us", {}).get("p50", 0)
            elif key == "lat_p99":
                v = m.get("latency_us", {}).get("p99", 0)
            else:
                v = m[key]
            pts.setdefault(r["_qps"], []).append(v)
        xs = sorted(pts)
        ys = [sum(pts[x]) / len(pts[x]) for x in xs]
        return xs, ys

    charts = [
        ("delivered_hit_rate", "Delivered hit rate vs load", "hit rate", (0, 1.05)),
        ("read_amp", "SSD read amplification vs load", "physical/logical bytes", None),
        ("write_amp", "SSD write amplification vs load", "physical/logical bytes", None),
        ("lat_p50", "P50 request latency vs load", "us", None),
        ("lat_p99", "P99 request latency vs load", "us", None),
    ]
    paths = []
    for key, title, ylabel, ylim in charts:
        fig, ax = plt.subplots(figsize=(6, 4))
        for system, label, color in [("baseline", "Tiered LRU baseline", "#c26d6d"),
                                     ("flatcake", "FlatCake (FC-GVA+MAS+CIO)", "#5a8f5a")]:
            xs, ys = series(system, key)
            if xs:
                ax.plot(xs, ys, marker="o", label=label, color=color)
        ax.set_xlabel("offered load (qps)")
        ax.set_ylabel(ylabel)
        ax.set_title(title)
        if ylim:
            ax.set_ylim(*ylim)
        ax.legend(frameon=False)
        ax.spines["top"].set_visible(False)
        ax.spines["right"].set_visible(False)
        fig.tight_layout()
        p = os.path.join(out_dir, f"{key}.png")
        fig.savefig(p, dpi=150)
        plt.close(fig)
        paths.append(p)

    # Per-medium stacked bars at peak qps per system.
    fig, ax = plt.subplots(figsize=(6, 4))
    labels, hbm, dram, ssd = [], [], [], []
    for system in ("baseline", "flatcake"):
        peak = max((r for r in runs if r["system"] == system),
                   key=lambda r: r["_qps"], default=None)
        if peak:
            m = peak["metrics"]
            tot = max(m["hit_hbm"] + m["hit_dram"] + m["hit_ssd"], 1)
            labels.append(f"{system}@{peak['_qps']}qps")
            hbm.append(100 * m["hit_hbm"] / tot)
            dram.append(100 * m["hit_dram"] / tot)
            ssd.append(100 * m["hit_ssd"] / tot)
    if labels:
        bottom = [0] * len(labels)
        for vals, name, color in [(hbm, "hbm", "#7da7d8"), (dram, "dram", "#e2b96f"), (ssd, "ssd", "#8fbf8f")]:
            ax.bar(labels, vals, bottom=bottom, label=name, color=color)
            bottom = [b + v for b, v in zip(bottom, vals)]
        ax.set_ylabel("% of delivered hits")
        ax.set_title("Per-medium hit attribution (peak load)")
        ax.legend(frameon=False)
        fig.tight_layout()
        p = os.path.join(out_dir, "per_medium.png")
        fig.savefig(p, dpi=150)
        plt.close(fig)
        paths.append(p)
    return paths


def main():
    results_dir = sys.argv[1] if len(sys.argv) > 1 else "results"
    out_dir = sys.argv[2] if len(sys.argv) > 2 else os.path.join(results_dir, "plots")
    os.makedirs(out_dir, exist_ok=True)
    runs = load_runs(results_dir)
    if not runs:
        print("no run records found", file=sys.stderr)
        return 1
    csv_path = write_summary_csv(runs, out_dir)
    print(f"summary: {csv_path} ({len(runs)} runs)")
    plots = plot(runs, out_dir)
    if plots:
        for p in plots:
            print(f"plot: {p}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
