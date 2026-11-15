"""Recompute descriptive resource slopes from the preserved raw samples."""
import csv
import json
from pathlib import Path
import statistics as st
import sys
import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt

root = Path(sys.argv[1]).resolve()
for artifact in ["metrics.json", "resource-curve.png"]:
    if (root/artifact).exists():
        raise FileExistsError(f"refusing to overwrite {root/artifact}")
metrics = {}
for name in ["pilot-5", "main-200"]:
    records = list(csv.DictReader((root/name/"samples.csv").open(encoding="utf-8")))
    rows = [{k: (v if k == "phase" else float(v)) for k, v in r.items()} for r in records]
    closed = [r for r in rows if r["phase"] == "closed"]
    baseline = next(r for r in rows if r["phase"] == "baseline")
    idle = [r for r in rows if r["phase"] == "final_idle"]
    tail = closed[-100:]
    result = json.loads((root/name/"result.json").read_text(encoding="utf-8"))
    stats = {}
    for key in ["private_bytes", "working_set", "handles", "threads", "children"]:
        slope = st.linear_regression([r["round"] for r in tail], [r[key] for r in tail]).slope
        stats[key] = dict(baseline=baseline[key], final_idle=idle[-1][key],
                          closed_min=min(r[key] for r in closed), closed_max=max(r[key] for r in closed),
                          all_sample_peak=max(r[key] for r in rows),
                          tail_samples=len(tail), tail_slope_per_round=slope,
                          tail_first=tail[0][key], tail_last=tail[-1][key],
                          tail_first20_median=st.median(r[key] for r in tail[:20]),
                          tail_last20_median=st.median(r[key] for r in tail[-20:]))
    metrics[name] = dict(result=result, resources=stats, samples=len(rows),
                         final_idle_cpu_s=idle[-1]["cpu_s"]-idle[0]["cpu_s"],
                         final_idle_interval_s=idle[-1]["elapsed_s"]-idle[0]["elapsed_s"],
                         measured_loop_cpu_s=closed[-1]["cpu_s"]-baseline["cpu_s"],
                         measured_loop_wall_s=closed[-1]["elapsed_s"]-baseline["elapsed_s"],
                         visible_samples=sum(r["visible"] for r in rows),
                         max_registry=max(r["registry_total"] for r in rows),
                         closed_registry_values=sorted(set((r["registry_total"],r["registry_closing"]) for r in closed)),
                         minimum_system_available=min(r["system_available"] for r in rows))
    if name == "main-200":
        fig, axes = plt.subplots(2, 2, figsize=(11, 7), constrained_layout=True)
        x = [r["round"] for r in closed]
        for key, label in [("private_bytes", "Private Bytes"), ("working_set", "Working Set")]:
            axes[0, 0].plot(x, [r[key]/1024**2 for r in closed], label=label)
        axes[0, 0].set_ylabel("MiB after each close")
        axes[0, 0].legend()
        axes[0, 1].plot(x, [r["handles"] for r in closed], label="Handles")
        axes[0, 1].set_ylabel("Mica process handles")
        axes[1, 0].plot(x, [r["threads"] for r in closed], label="Threads")
        axes[1, 0].plot(x, [r["children"] for r in closed], label="Owned descendants")
        axes[1, 0].set_ylabel("Count after each close")
        axes[1, 0].legend()
        axes[1, 1].plot([r["elapsed_s"] for r in rows], [r["cpu_s"] for r in rows])
        axes[1, 1].set_ylabel("Cumulative Mica CPU seconds")
        axes[1, 1].set_xlabel("Elapsed seconds (includes idle)")
        for ax in [axes[0, 0], axes[0, 1], axes[1, 0]]:
            ax.set_xlabel("Completed tab open/close rounds")
        for ax in axes.flat:
            ax.grid(alpha=0.25)
        fig.suptitle("Mica release: 200 isolated tab lifecycle rounds, 2026-11-11")
        fig.savefig(root/"resource-curve.png", dpi=160)
        plt.close(fig)
(root/"metrics.json").write_text(json.dumps(metrics, indent=2), encoding="utf-8")
print(json.dumps(metrics, indent=2))
