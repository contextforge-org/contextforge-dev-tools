"""Build machine-readable and Slack-ready FYRE scaling reports."""

from __future__ import annotations

import argparse
import csv
import json
from pathlib import Path

import matplotlib.pyplot as plt


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--config", required=True)
    parser.add_argument("--results", required=True)
    args = parser.parse_args()
    config = json.loads(Path(args.config).read_text(encoding="utf-8"))
    results_root = Path(args.results)
    results = {}
    for scenario in config["scenarios"]:
        path = results_root / scenario["id"] / "result.json"
        if path.exists():
            results[scenario["id"]] = json.loads(path.read_text(encoding="utf-8"))
    if "baseline" not in results:
        raise RuntimeError("baseline result is required to calculate scaling")

    baseline = results["baseline"]["rps"]
    rows = []
    for scenario in config["scenarios"]:
        result = results.get(scenario["id"])
        if not result or result.get("status") != "confirmed":
            continue
        total_cpu = scenario["replicas"] * scenario["cpu"]
        speedup = result["rps"] / baseline
        row = {
            "scenario": scenario["label"],
            "scenario_id": scenario["id"],
            "replicas": scenario["replicas"],
            "cpu_per_vm": scenario["cpu"],
            "memory_gb_per_vm": scenario["memory_gb"],
            "total_cpu": total_cpu,
            "total_memory_gb": scenario["replicas"] * scenario["memory_gb"],
            "users": result["users"],
            "lower_bound": result.get("lower_bound", False),
            "rps": result["rps"],
            "p50_ms": result["p50_ms"],
            "p95_ms": result["p95_ms"],
            "p99_ms": result["p99_ms"],
            "speedup": speedup,
            "efficiency": speedup / scenario["multiplier"],
            "rps_per_vcpu": result["rps"] / total_cpu,
            "rps_cv_percent": result["rps_cv_percent"],
            "horizontal_advantage": None,
            "replica_imbalance_percent": result["replica_imbalance_percent"],
        }
        if scenario["id"].startswith("horizontal-"):
            vertical_id = scenario["id"].replace("horizontal", "vertical")
            vertical = results.get(vertical_id)
            if vertical and vertical.get("status") == "confirmed":
                row["horizontal_advantage"] = result["rps"] / vertical["rps"]
        rows.append(row)

    (results_root / "summary.json").write_text(
        json.dumps({"rows": rows}, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    with (results_root / "summary.csv").open(
        "w", newline="", encoding="utf-8"
    ) as stream:
        writer = csv.DictWriter(stream, fieldnames=rows[0].keys())
        writer.writeheader()
        writer.writerows(rows)

    figure = plt.figure(figsize=(16, 9), dpi=160, facecolor="#0b1020")
    grid = figure.add_gridspec(2, 1, height_ratios=[2.1, 1.5], hspace=0.2)
    axis = figure.add_subplot(grid[0])
    axis.set_facecolor("#0b1020")
    colors = [
        "#a7b0c0"
        if row["scenario_id"] == "baseline"
        else "#49a7ff"
        if "vertical" in row["scenario_id"]
        else "#45d6a0"
        for row in rows
    ]
    bars = axis.bar(
        [row["scenario"] for row in rows], [row["rps"] for row in rows], color=colors
    )
    axis.set_ylim(0, max(row["rps"] for row in rows) * 1.18)
    axis.set_ylabel("Confirmed zero-error requests/second", color="white", fontsize=12)
    axis.tick_params(axis="x", colors="white", rotation=12)
    axis.tick_params(axis="y", colors="white")
    for spine in axis.spines.values():
        spine.set_color("#56617a")
    axis.grid(axis="y", color="#28324a", alpha=0.7)
    for bar, row in zip(bars, rows):
        comparison = f"{row['speedup']:.2f}x"
        if row["horizontal_advantage"] is not None:
            comparison += f"\n{row['horizontal_advantage']:.2f}x vs vertical"
        axis.text(
            bar.get_x() + bar.get_width() / 2,
            bar.get_height(),
            f"{row['rps']:,.0f} RPS\n{comparison}",
            ha="center",
            va="bottom",
            color="white",
            fontsize=10,
            fontweight="bold",
        )
    figure.suptitle(
        "ContextForge Rust dataplane scaling on FYRE",
        color="white",
        fontsize=20,
        fontweight="bold",
        x=0.065,
        y=0.985,
        ha="left",
    )
    figure.text(
        0.065,
        0.935,
        "Same total dataplane CPU/RAM for matched vertical and horizontal comparisons",
        color="#a7b0c0",
        fontsize=10,
        ha="left",
    )
    figure.subplots_adjust(top=0.88)

    table_axis = figure.add_subplot(grid[1])
    table_axis.axis("off")
    headers = [
        "Scenario",
        "VMs × size",
        "Users",
        "RPS",
        "p50 / p95 / p99 ms",
        "Speedup",
        "Efficiency",
        "RPS/vCPU",
        "CV / imbalance",
    ]
    cells = []
    for row in rows:
        users = f"≥{row['users']:,}" if row["lower_bound"] else f"{row['users']:,}"
        cells.append(
            [
                row["scenario"],
                f"{row['replicas']} × {row['cpu_per_vm']}c/{row['memory_gb_per_vm']}G",
                users,
                f"{row['rps']:,.0f}",
                f"{row['p50_ms']:.1f} / {row['p95_ms']:.1f} / {row['p99_ms']:.1f}",
                f"{row['speedup']:.2f}x",
                f"{100 * row['efficiency']:.1f}%",
                f"{row['rps_per_vcpu']:,.0f}",
                f"{row['rps_cv_percent']:.1f}% / {row['replica_imbalance_percent']:.1f}%",
            ]
        )
    table = table_axis.table(
        cellText=cells,
        colLabels=headers,
        cellLoc="center",
        loc="center",
        colWidths=[0.16, 0.12, 0.08, 0.09, 0.18, 0.09, 0.09, 0.10, 0.07],
    )
    table.auto_set_font_size(False)
    table.set_fontsize(9)
    table.scale(1, 1.8)
    for (row, _column), cell in table.get_celld().items():
        cell.set_edgecolor("#28324a")
        cell.set_facecolor("#172039" if row else "#253250")
        cell.get_text().set_color("white")
        if row == 0:
            cell.get_text().set_fontweight("bold")
    figure.savefig(
        results_root / "slack-scaling.png",
        bbox_inches="tight",
        facecolor=figure.get_facecolor(),
    )


if __name__ == "__main__":
    main()
