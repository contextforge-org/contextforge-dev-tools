"""Build machine-readable and Slack-ready FYRE benchmark reports."""

from __future__ import annotations

import argparse
import csv
import json
from pathlib import Path


def comparison_markdown(config: dict, rows: list[dict]) -> str:
    infrastructure = config.get("infrastructure", {})
    openshift = infrastructure.get("kind") == "openshift"
    openshift_config = infrastructure.get("openshift", {})
    workload = config.get("workload", {})
    all_parallel = openshift and bool(workload.get("parallel_user_levels"))
    target = (config.get("scenarios") or [{}])[0]
    images = config.get("images", {})
    tools = workload.get("tools", [])
    duration = int(workload.get("measure_seconds", 0))
    helpers = config.get("active_helper", {})
    locust_cpu = (
        openshift_config.get("load_pod", {}).get("cpu_millicores", 0) / 1_000
        if openshift
        else helpers.get("locust_cpu", "n/a")
    )
    locust_memory = (
        openshift_config.get("load_pod", {}).get("memory_mib", 0) / 1_024
        if openshift
        else helpers.get("locust_memory_gb", "n/a")
    )
    fast_time_cpu = (
        openshift_config.get("backend_pod", {}).get("cpu_millicores", 0) / 1_000
        if openshift
        else helpers.get("fast_time_cpu", "n/a")
    )
    fast_time_memory = (
        openshift_config.get("backend_pod", {}).get("memory_mib", 0) / 1_024
        if openshift
        else helpers.get("fast_time_memory_gb", "n/a")
    )
    disk = (
        f"{infrastructure.get('openshift', {}).get('base_disk_gb', 'n/a')} GB"
        if openshift
        else "250 GB (standalone FYRE default)"
    )
    lines = [
        "# FYRE built-in dataplane and external dataplane comparison",
        "",
        "## Architecture",
        "",
        "```mermaid",
        "flowchart LR",
        "  subgraph BI[Built-in dataplane lane]",
        f"    LBI[Locust: {locust_cpu} vCPU / {locust_memory} GiB] --> TBI[Gateway + PostgreSQL + Redis: {target.get('cpu', 'n/a')} vCPU / {target.get('memory_gb', 'n/a')} GiB]",
        f"    TBI --> FBI[Fast Time: {fast_time_cpu} vCPU / {fast_time_memory} GiB]",
        "  end",
        "  subgraph EX[External dataplane lane]",
        f"    LEX[Locust: {locust_cpu} vCPU / {locust_memory} GiB] --> TEX[Rust + Redis + JWKS: {target.get('cpu', 'n/a')} vCPU / {target.get('memory_gb', 'n/a')} GiB]",
        f"    TEX --> FEX[Fast Time: {fast_time_cpu} vCPU / {fast_time_memory} GiB]",
        "  end",
        "```",
        "",
        (
            (
                "All eight lane/user measurements ran concurrently. Each measurement had "
                "its own reserved Locust, target, and Fast Time pods. Pods of the same role "
                "shared a dedicated worker pool, with requests and limits equal to the stated "
                "per-measurement allocation."
                if all_parallel
                else "The two lanes ran concurrently on six dedicated OpenShift worker nodes. "
                "Each lane had its own load generator, target, and backend, so the measured "
                "targets and helpers shared no worker node."
            )
            if openshift
            else "The two lanes ran sequentially on the same standalone target VM."
        ),
        "",
        "| Setting | Value |",
        "| --- | --- |",
        f"| Infrastructure | {'FYRE OpenShift ' + str(infrastructure.get('openshift', {}).get('version', '')) if openshift else 'FYRE standalone VMs'} |",
        f"| Worker root disk | {disk} |",
        f"| Target allocation per lane | {target.get('cpu', 'n/a')} vCPU / {target.get('memory_gb', 'n/a')} GiB |",
        f"| Locust allocation per measurement | {locust_cpu} vCPU / {locust_memory} GiB; "
        + ("one master and three workers |" if openshift else "distributed workers |"),
        f"| Fast Time allocation per measurement | {fast_time_cpu} vCPU / {fast_time_memory} GiB |",
        f"| MCP protocol | {workload.get('protocol_version', 'n/a')} |",
        f"| Timing per measurement | {workload.get('ramp_seconds', 0)} s ramp, {workload.get('warmup_seconds', 0)} s warmup, {duration // 60} min measured |",
        "| Client | Locust FastHttpUser, zero wait |",
        "| Failure policy | Stop all concurrent measurements on the first request or worker error |"
        if all_parallel
        else "| Failure policy | Stop the current pair on the first request or worker error; do not advance |",
        "",
        "## Requests",
        "",
        "Each virtual user calls `server/discover` once during startup. Statistics reset after warmup, so measured traffic consists only of repeated `tools/call` requests with uniform random selection across these six Fast Time tools:",
        "",
        ", ".join(f"`{tool}`" for tool in tools) + ".",
        "",
        "The backend has explicit zero delay. The workload makes no `resources/read` requests and no fan-out calls.",
        "",
        "## Results",
        "",
        "| Users | Built-in requests | Built-in errors | Built-in RPS | Built-in p50/p95/p99 | Built-in memory avg/peak | External requests | External errors | External RPS | External p50/p95/p99 | External memory avg/peak | External vs built-in |",
        "| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |",
    ]
    for row in rows:
        builtin_memory = memory_pair(
            row["built_in_dataplane_memory_average_mib"],
            row["built_in_dataplane_memory_peak_mib"],
        )
        external_memory = memory_pair(
            row["external_dataplane_memory_average_mib"],
            row["external_dataplane_memory_peak_mib"],
        )
        lines.append(
            f"| {row['users']:,} | {row['built_in_dataplane_requests']:,} | "
            f"{row['built_in_dataplane_errors']} | {row['built_in_dataplane_rps']:,.2f} | "
            f"{row['built_in_dataplane_p50_ms']:.0f}/{row['built_in_dataplane_p95_ms']:.0f}/{row['built_in_dataplane_p99_ms']:.0f} ms | "
            f"{builtin_memory} | "
            f"{row['external_dataplane_requests']:,} | {row['external_dataplane_errors']} | "
            f"{row['external_dataplane_rps']:,.2f} | "
            f"{row['external_dataplane_p50_ms']:.0f}/{row['external_dataplane_p95_ms']:.0f}/{row['external_dataplane_p99_ms']:.0f} ms | "
            f"{external_memory} | "
            f"{row['external_vs_built_in']:.2f}× |"
        )
    lines.extend(
        [
            "",
            "Gateway memory is sampled every 10 seconds during the measured window and includes all gateway-side containers: Python, PostgreSQL, and Redis for the built-in lane; Rust, Redis, and loopback JWKS for the external lane.",
            "",
            "## Software",
            "",
            "| Component | Pinned image |",
            "| --- | --- |",
            *[
                f"| {name.replace('_', ' ').title()} | `{image}` |"
                for name, image in sorted(images.items())
            ],
            "",
        ]
    )
    return "\n".join(lines)


def memory_pair(average: float | None, peak: float | None) -> str:
    if average is None or peak is None:
        return "n/a"
    return f"{average:.0f}/{peak:.0f} MiB"


def comparison_report(config: dict, results_root: Path, *, render: bool = True) -> None:
    result_path = results_root / "comparison" / "result.json"
    if not result_path.is_file():
        raise RuntimeError("comparison result is required")
    result = json.loads(result_path.read_text(encoding="utf-8"))
    if result.get("status") != "confirmed":
        raise RuntimeError("all eight comparison benchmarks must pass before reporting")
    lanes = {
        lane: {item["users"]: item for item in result["runs"][lane]}
        for lane in ("builtin", "rust")
    }
    rows = []
    for users in config["workload"]["user_levels"]:
        builtin = lanes["builtin"][users]
        rust = lanes["rust"][users]
        rows.append(
            {
                "users": users,
                "built_in_dataplane_requests": builtin["requests"],
                "built_in_dataplane_errors": builtin["failures"],
                "built_in_dataplane_rps": builtin["rps"],
                "built_in_dataplane_p50_ms": builtin["p50_ms"],
                "built_in_dataplane_p95_ms": builtin["p95_ms"],
                "built_in_dataplane_p99_ms": builtin["p99_ms"],
                "built_in_dataplane_memory_average_mib": builtin.get("memory", {}).get("average_mib"),
                "built_in_dataplane_memory_peak_mib": builtin.get("memory", {}).get("peak_mib"),
                "external_dataplane_requests": rust["requests"],
                "external_dataplane_errors": rust["failures"],
                "external_dataplane_rps": rust["rps"],
                "external_dataplane_p50_ms": rust["p50_ms"],
                "external_dataplane_p95_ms": rust["p95_ms"],
                "external_dataplane_p99_ms": rust["p99_ms"],
                "external_dataplane_memory_average_mib": rust.get("memory", {}).get("average_mib"),
                "external_dataplane_memory_peak_mib": rust.get("memory", {}).get("peak_mib"),
                "external_vs_built_in": rust["rps"] / builtin["rps"],
            }
        )
    (results_root / "summary.json").write_text(
        json.dumps({"rows": rows}, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    with (results_root / "summary.csv").open("w", newline="", encoding="utf-8") as stream:
        writer = csv.DictWriter(stream, fieldnames=rows[0].keys())
        writer.writeheader()
        writer.writerows(rows)
    (results_root / "report.md").write_text(
        comparison_markdown(config, rows), encoding="utf-8"
    )

    if not render:
        return

    import matplotlib.pyplot as plt
    from matplotlib.patches import FancyBboxPatch

    openshift = config.get("infrastructure", {}).get("kind") == "openshift"
    openshift_config = config.get("infrastructure", {}).get("openshift", {})
    helpers = config.get("active_helper", {})
    workload = config["workload"]
    all_parallel = openshift and bool(workload.get("parallel_user_levels"))
    target = config["scenarios"][0]
    load_cpu = openshift_config.get("load_pod", {}).get("cpu_millicores", 0) / 1_000
    load_memory = openshift_config.get("load_pod", {}).get("memory_mib", 0) / 1_024
    backend_cpu = openshift_config.get("backend_pod", {}).get("cpu_millicores", 0) / 1_000
    backend_memory = openshift_config.get("backend_pod", {}).get("memory_mib", 0) / 1_024
    figure = plt.figure(figsize=(18, 10), dpi=160, facecolor="#0b1020")
    axis = figure.add_axes([0, 0, 1, 1])
    axis.set_axis_off()
    figure.text(
        0.035,
        0.95,
        "FYRE OpenShift built-in vs external dataplane — eight parallel one-hour measurements"
        if all_parallel
        else "FYRE OpenShift built-in vs external dataplane — parallel one-hour load comparison"
        if openshift
        else "FYRE built-in dataplane vs external dataplane — one-hour load comparison",
        color="white",
        fontsize=25,
        fontweight="bold",
    )
    figure.text(
        0.035,
        0.91,
        "Eight benchmarks run together • reserved 2 vCPU / 2 GB targets • 40 GB OpenShift nodes"
        if all_parallel
        else "Eight zero-error benchmarks • isolated equal-size lane allocations • 40 GB OpenShift nodes"
        if openshift
        else "Eight zero-error benchmarks • same modern client and same target VM allocation • private FYRE network",
        color="#a7b0c0",
        fontsize=12,
    )

    cards = [
        (
            0.035,
            "LOAD GENERATOR",
            f"Per measurement: {load_cpu:g} vCPU / {load_memory:g} GB pod\n"
            "3 distributed workers • zero wait\nShared dedicated-role OpenShift worker"
            if all_parallel
            else "Per lane: 4 vCPU / 16 GB pod\n3 distributed workers • zero wait\nDedicated OpenShift worker"
            if openshift
            else f"Locust VM • {helpers['locust_cpu']} vCPU / {helpers['locust_memory_gb']} GB\n"
            f"{max(2, int(helpers['locust_cpu']) - 1)} distributed workers • zero wait",
        ),
        (
            0.355,
            "TARGETS — 8 RESERVED PODS"
            if all_parallel
            else "TARGETS — ISOLATED, PARALLEL"
            if openshift
            else "TARGET — SAME VM, SEQUENTIAL",
            f"Per measurement: {target['cpu']} vCPU / {target['memory_gb']} GB pod allocation\n"
            "Built-in: Python + Postgres + Redis\nExternal: Rust + Redis + loopback JWKS\n"
            "Shared dedicated target worker"
            if openshift
            else f"{target['cpu']} vCPU / {target['memory_gb']} GB\n"
            "Built-in dataplane: Python gateway + Postgres + Redis\n"
            "External dataplane: Rust + Redis + loopback JWKS",
        ),
        (
            0.71,
            "BACKEND",
            f"Per measurement: {backend_cpu:g} vCPU / {backend_memory:g} GB pod\n"
            "6 tools • explicit zero delay\nShared dedicated-role OpenShift worker"
            if all_parallel
            else "Per lane: 8 vCPU / 32 GB pod\n6 tools • explicit zero delay\nDedicated OpenShift worker"
            if openshift
            else f"Fast Time VM • {helpers['fast_time_cpu']} vCPU / {helpers['fast_time_memory_gb']} GB\n6 tools • explicit zero delay",
        ),
    ]
    widths = [0.27, 0.31, 0.255]
    for (x, title, body), width in zip(cards, widths):
        axis.add_patch(
            FancyBboxPatch(
                (x, 0.74),
                width,
                0.12,
                boxstyle="round,pad=0.008,rounding_size=0.008",
                linewidth=1.4,
                edgecolor="#3a4765",
                facecolor="#172039",
            )
        )
        figure.text(x + 0.018, 0.825, title, color="#49a7ff", fontsize=11, fontweight="bold")
        figure.text(x + 0.018, 0.78, body, color="white", fontsize=10, va="center", linespacing=1.4)
    for start, end in ((0.305, 0.355), (0.665, 0.71)):
        axis.annotate(
            "",
            xy=(end, 0.8),
            xytext=(start, 0.8),
            arrowprops={"arrowstyle": "-|>", "color": "#45d6a0", "lw": 2.5},
        )

    table_axis = figure.add_axes([0.025, 0.28, 0.95, 0.38])
    table_axis.axis("off")
    headers = [
        "Users",
        "Built-in DP\nrequests",
        "Built-in DP\nerrors",
        "Built-in DP\nRPS",
        "Built-in DP p50 /\np95 / p99",
        "External DP\nrequests",
        "External DP\nerrors",
        "External DP\nRPS",
        "External DP p50 /\np95 / p99",
        "Built-in memory\navg / peak MiB",
        "External memory\navg / peak MiB",
        "External vs\nbuilt-in",
    ]
    cells = [
        [
            f"{row['users']:,}",
            f"{row['built_in_dataplane_requests']:,}",
            str(row["built_in_dataplane_errors"]),
            f"{row['built_in_dataplane_rps']:,.2f}",
            f"{row['built_in_dataplane_p50_ms']:.0f} / {row['built_in_dataplane_p95_ms']:.0f} / {row['built_in_dataplane_p99_ms']:.0f} ms",
            f"{row['external_dataplane_requests']:,}",
            str(row["external_dataplane_errors"]),
            f"{row['external_dataplane_rps']:,.2f}",
            f"{row['external_dataplane_p50_ms']:.0f} / {row['external_dataplane_p95_ms']:.0f} / {row['external_dataplane_p99_ms']:.0f} ms",
            memory_pair(
                row["built_in_dataplane_memory_average_mib"],
                row["built_in_dataplane_memory_peak_mib"],
            ).removesuffix(" MiB").replace("/", " / "),
            memory_pair(
                row["external_dataplane_memory_average_mib"],
                row["external_dataplane_memory_peak_mib"],
            ).removesuffix(" MiB").replace("/", " / "),
            f"{row['external_vs_built_in']:.2f}×",
        ]
        for row in rows
    ]
    table = table_axis.table(
        cellText=cells,
        colLabels=headers,
        cellLoc="center",
        loc="center",
        colWidths=[0.05, 0.09, 0.055, 0.075, 0.115, 0.09, 0.055, 0.075, 0.115, 0.09, 0.09, 0.08],
    )
    table.auto_set_font_size(False)
    table.set_fontsize(8.0)
    table.scale(1, 2.3)
    for (row, _column), cell in table.get_celld().items():
        cell.set_edgecolor("#34415f")
        cell.set_facecolor("#172039" if row else "#253250")
        cell.get_text().set_color("white")
        if row == 0:
            cell.get_text().set_fontweight("bold")

    figure.text(
        0.035,
        0.20,
        f"Method: MCP {workload['protocol_version']} for both clients • FastHttpUser • "
        f"{workload['ramp_seconds']} s ramp • {workload['warmup_seconds']} s warmup • "
        f"{workload['measure_seconds'] // 60} min measured • statistics reset after warmup",
        color="#a7b0c0",
        fontsize=11,
    )
    figure.text(
        0.035,
        0.155,
        "Traffic: the same six Fast Time tools through each public MCP route • first request or worker error stops the campaign",
        color="#a7b0c0",
        fontsize=11,
    )
    figure.text(
        0.035,
        0.095,
        "External vs built-in = external dataplane RPS ÷ built-in dataplane RPS at the same user count.",
        color="#45d6a0",
        fontsize=13,
        fontweight="bold",
    )
    for name in ("slack-comparison.png", "slack-scaling.png"):
        figure.savefig(
            results_root / name,
            bbox_inches="tight",
            facecolor=figure.get_facecolor(),
        )


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--config", required=True)
    parser.add_argument("--results", required=True)
    args = parser.parse_args()
    config = json.loads(Path(args.config).read_text(encoding="utf-8"))
    results_root = Path(args.results)
    if config.get("benchmark_kind", "scaling") == "comparison":
        comparison_report(config, results_root)
        return
    import matplotlib.pyplot as plt
    from matplotlib.patches import FancyBboxPatch

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

    helpers = config["active_helper"]
    workload = config["workload"]
    architecture = (
        f"Architecture: Locust {helpers['locust_cpu']}c/{helpers['locust_memory_gb']}G VM"
        "  →  Rust dataplane VM(s)  →  "
        f"Fast Time {helpers['fast_time_cpu']}c/{helpers['fast_time_memory_gb']}G VM"
        "  •  private FYRE traffic"
    )
    method = (
        f"MCP {workload['protocol_version']}  •  {len(workload['tools'])} zero-delay tools"
        f"  •  {workload['ramp_seconds']}s ramp  •  {workload['warmup_seconds']}s warmup"
        f"  •  {workload['measure_seconds']}s measured  •  "
        f"{workload['repetitions']} confirmations  •  fail-fast on first error"
    )
    deployment = (
        "Each dataplane VM: one Rust instance + local Redis + loopback JWKS  •  "
        f"{config['infrastructure']['os']}  •  direct balanced replica traffic"
    )

    figure = plt.figure(figsize=(16, 10), dpi=160, facecolor="#0b1020")
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
        architecture,
        color="#a7b0c0",
        fontsize=10,
        ha="left",
    )
    figure.text(0.065, 0.91, method, color="#a7b0c0", fontsize=10, ha="left")
    figure.text(
        0.065,
        0.885,
        deployment,
        color="#a7b0c0",
        fontsize=10,
        ha="left",
    )
    figure.subplots_adjust(top=0.83)

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
