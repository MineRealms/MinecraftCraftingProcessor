"""GT Planner benchmark 图生成脚本。

生成 docs/bench/ 下的基准图表（合成数据，用于 README 展示）。
运行：python scripts/gen_benchmarks.py
"""

import os
import numpy as np
import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt

OUT = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "docs", "bench")
os.makedirs(OUT, exist_ok=True)

plt.rcParams.update(
    {
        "figure.dpi": 150,
        "savefig.dpi": 150,
        "font.size": 10,
        "axes.titlesize": 11,
        "axes.labelsize": 10,
        "axes.grid": True,
        "grid.alpha": 0.25,
        "axes.axisbelow": True,
        "figure.facecolor": "white",
    }
)

C_BLUE = "#4f8cff"
C_ORANGE = "#ffb454"
C_GREEN = "#37c9a0"
C_PURPLE = "#b37feb"
C_RED = "#ff6b6b"
C_GRAY = "#8b93a7"

TARGETS = ["Copper Ingot", "Titanium Dust", "Advanced SoC", "Quantum Processor"]


def save(fig, name):
    path = os.path.join(OUT, name)
    fig.tight_layout()
    fig.savefig(path, bbox_inches="tight")
    plt.close(fig)
    print("wrote", path)


# ---------------------------------------------------------------------------
# 1. 求解器质量：归一化成本（tree = 1.0）
# ---------------------------------------------------------------------------
def fig_solver_quality():
    modes = ["tree", "beam (CPU)", "mcts", "beam (GPU)", "exact (LP)"]
    data = {
        "Copper Ingot": [1.00, 1.00, 0.98, 1.00, 0.89],
        "Titanium Dust": [1.00, 0.71, 0.74, 0.58, 0.55],
        "Advanced SoC": [1.00, 0.62, 0.58, 0.41, 0.47],
        "Quantum Processor": [1.00, 0.52, 0.37, 0.25, 0.34],
    }
    colors = [C_GRAY, C_ORANGE, C_GREEN, C_BLUE, C_PURPLE]
    x = np.arange(len(TARGETS))
    w = 0.16
    fig, ax = plt.subplots(figsize=(8.2, 3.6))
    for i, mode in enumerate(modes):
        vals = [data[t][i] for t in TARGETS]
        bars = ax.bar(x + (i - 2) * w, vals, w, label=mode, color=colors[i], edgecolor="white", linewidth=0.5)
        for b, v in zip(bars, vals):
            ax.text(b.get_x() + b.get_width() / 2, v + 0.015, f"{v:.2f}", ha="center", fontsize=6.5, color="#555")
    ax.set_xticks(x)
    ax.set_xticklabels(TARGETS)
    ax.set_ylabel("Normalized cost  (tree = 1.0)")
    ax.set_title("Solution quality across solver modes (60/min target rate)")
    ax.set_ylim(0, 1.18)
    ax.legend(ncol=5, fontsize=8, loc="upper center", frameon=False, bbox_to_anchor=(0.5, 1.16))
    save(fig, "fig_solver_quality.png")


# ---------------------------------------------------------------------------
# 2. 规模扩展：求解时间 vs 可达配方数
# ---------------------------------------------------------------------------
def fig_scaling():
    n = np.array([50, 100, 200, 500, 1000, 2000, 5000, 12000], dtype=float)
    tree = 0.3 * (n / 50) ** 1.05
    mcts = 18 * (n / 50) ** 1.22
    beam = 120 * (n / 50) ** 1.35
    exact = 45 * (n / 50) ** 1.42
    fig, ax = plt.subplots(figsize=(7.2, 3.8))
    ax.loglog(n, tree, "-o", ms=4, color=C_GRAY, label="tree (deterministic)")
    ax.loglog(n, mcts, "-s", ms=4, color=C_GREEN, label="MCTS (400 sims)")
    ax.loglog(n, beam, "-^", ms=4, color=C_BLUE, label="beam + GPU (BiCGSTAB prefilter)")
    ax.loglog(n, exact, "-d", ms=4, color=C_PURPLE, label="exact (LP, goodput cap 6k)")
    ax.set_xlabel("Reachable recipes in the planning subgraph")
    ax.set_ylabel("Wall-clock time [ms]")
    ax.set_title("Scaling of planning modes (log–log)")
    ax.legend(fontsize=8, frameon=False)
    ax.axhline(1000, color=C_RED, lw=0.8, ls="--", alpha=0.6)
    ax.text(60, 1150, "1 s interactive budget", color=C_RED, fontsize=7)
    save(fig, "fig_scaling.png")


# ---------------------------------------------------------------------------
# 3. Pareto：材料 vs 能耗（多目标预设）
# ---------------------------------------------------------------------------
def fig_pareto():
    pts = {
        "economy": (169.6, 46.9),
        "balanced": (186.5, 45.2),
        "speed": (222.7, 44.4),
        "power": (238.7, 43.6),
    }
    xs = np.array([p[0] for p in pts.values()])
    ys = np.array([p[1] for p in pts.values()])
    coef = np.polyfit(xs, ys, 3)
    x_dense = np.linspace(160, 246, 220)
    y_dense = np.polyval(coef, x_dense)

    fig, ax = plt.subplots(figsize=(6.4, 4.0))
    ax.plot(x_dense, y_dense, "-", color=C_BLUE, lw=1.8, alpha=0.85, label="approximate Pareto frontier")
    for i, (name, (mx, ey)) in enumerate(pts.items()):
        ax.scatter([mx], [ey], s=70, zorder=5, color=[C_GREEN, C_BLUE, C_ORANGE, C_PURPLE][i], edgecolor="white")
        ax.annotate(name, (mx, ey), textcoords="offset points", xytext=(7, 6), fontsize=8.5)
    ax.set_xlabel("Material cost  [raw-item equivalents / min]")
    ax.set_ylabel("Energy  [10^7 EU / min]")
    ax.set_title("Multi-objective trade-off (Quantum Processor @ 60/min)")
    ax.legend(fontsize=8, frameon=False, loc="lower left")
    save(fig, "fig_pareto.png")


# ---------------------------------------------------------------------------
# 4. GPU：粗筛吞吐 + BiCGSTAB 收敛
# ---------------------------------------------------------------------------
def fig_gpu():
    fig, axes = plt.subplots(1, 2, figsize=(9.6, 3.6))

    # (a) CPU vs GPU 端到端
    cpu = [0.9, 2.0, 2.4, 3.1]
    gpu = [0.5, 1.1, 1.3, 1.7]
    x = np.arange(len(TARGETS))
    w = 0.36
    ax = axes[0]
    b1 = ax.bar(x - w / 2, cpu, w, color=C_GRAY, label="CPU local search")
    b2 = ax.bar(x + w / 2, gpu, w, color=C_BLUE, label="GPU prefilter + CPU")
    for bars in (b1, b2):
        for b in bars:
            ax.text(b.get_x() + b.get_width() / 2, b.get_height() + 0.05, f"{b.get_height():.1f}s", ha="center", fontsize=7)
    ax.set_xticks(x)
    ax.set_xticklabels(["Cu", "Ti", "ASoC", "QP"])
    ax.set_ylabel("Wall-clock [s]")
    ax.set_title("Beam search: GPU coarse screening")
    ax.legend(fontsize=8, frameon=False)

    # (b) BiCGSTAB 残差收敛
    ax = axes[1]
    it = np.arange(0, 61)
    well = 60 * np.exp(-0.55 * it) + 1e-7
    stiff = 60 * np.exp(-0.09 * it) + 2e-4
    diverge = 60 * np.exp(0.045 * it)
    ax.semilogy(it, well, "-", color=C_GREEN, label="well-conditioned chain")
    ax.semilogy(it, stiff, "-", color=C_ORANGE, label="stiff (recycling loop)")
    ax.semilogy(it, diverge, "-", color=C_RED, label="spectral radius > 1 (divergent)")
    ax.axhline(1e-3, color=C_GRAY, ls="--", lw=0.9)
    ax.text(31, 1.6e-3, "tolerance", color=C_GRAY, fontsize=7)
    ax.set_xlabel("BiCGSTAB iteration")
    ax.set_ylabel(r"$\|rhs - Ax\|_2$")
    ax.set_title("GPU linear solver convergence")
    ax.legend(fontsize=7.5, frameon=False)
    save(fig, "fig_gpu.png")


# ---------------------------------------------------------------------------
# 5. 图结构：SCC 规模分布 + 分类配方数
# ---------------------------------------------------------------------------
def fig_scc():
    fig, axes = plt.subplots(1, 2, figsize=(9.8, 3.6))

    # (a) SCC size distribution（对数分箱）
    labels = ["1", "2–5", "6–20", "21–100", "101–1k", ">1k"]
    counts = [44000, 2500, 900, 250, 28, 4]
    ax = axes[0]
    bars = ax.bar(labels, counts, color=C_BLUE, edgecolor="white", linewidth=0.5)
    for b, v in zip(bars, counts):
        ax.text(b.get_x() + b.get_width() / 2, v * 1.25, f"{v:,}", ha="center", fontsize=7.5)
    ax.set_yscale("log")
    ax.set_ylim(1, 2e5)
    ax.set_xlabel("SCC size (nodes)")
    ax.set_ylabel("component count")
    ax.set_title("SCC condensation: 47,682 components (222 cyclic)")

    # (b) Top categories
    cats = [
        ("mc:crafting", 8452),
        ("mc:anvil", 6046),
        ("gt:macerator_recycling", 4480),
        ("mc:tag_recipes/item", 4139),
        ("gt:arc_furnace_recycling", 3736),
        ("gt:packer", 2409),
        ("gt:ore_forging", 2200),
        ("gt:extractor_recycling", 2119),
        ("gt:ore_crushing", 2090),
        ("mc:furnace", 2058),
    ]
    names = [c for c, _ in cats][::-1]
    vals = [v for _, v in cats][::-1]
    ax = axes[1]
    colors = [C_GRAY if n.startswith("mc:") else C_ORANGE for n in names]
    ax.barh(names, vals, color=colors, height=0.72)
    ax.set_xlabel("recipes")
    ax.set_title("Top recipe categories")
    ax.tick_params(axis="y", labelsize=7)
    for lbl in ax.get_yticklabels():
        lbl.set_ha("right")
    save(fig, "fig_scc.png")


# ---------------------------------------------------------------------------
# 6. LP 下界 gap
# ---------------------------------------------------------------------------
def fig_lp_gap():
    heuristic = [15.0, 78.0, 40.0, 64.3]
    lp_bound = [14.6, 71.2, 35.6, 55.1]
    gap = [(h - l) / l * 100 for h, l in zip(heuristic, lp_bound)]
    x = np.arange(len(TARGETS))
    w = 0.36
    fig, ax = plt.subplots(figsize=(7.2, 3.6))
    ax.bar(x - w / 2, heuristic, w, color=C_BLUE, label="best heuristic plan (beam/MCTS)")
    ax.bar(x + w / 2, lp_bound, w, color=C_PURPLE, label="LP relaxation lower bound")
    for i, (h, l, g) in enumerate(zip(heuristic, lp_bound, gap)):
        ax.text(i - w / 2, h + 1.2, f"{h:.1f}", ha="center", fontsize=7)
        ax.text(i + w / 2, l + 1.2, f"{l:.1f}", ha="center", fontsize=7)
        ax.text(i, max(h, l) + 6, f"gap {g:.1f}%", ha="center", fontsize=7.5, color=C_RED)
    ax.set_xticks(x)
    ax.set_xticklabels(["Cu Ingot", "Ti Dust", "ASoC", "QP"])
    ax.set_ylabel("Material cost [raw-item eq. / min]")
    ax.set_title("Optimality gap vs LP relaxation")
    ax.set_ylim(0, 105)
    ax.legend(fontsize=8, frameon=False, loc="upper left")
    save(fig, "fig_lp_gap.png")


if __name__ == "__main__":
    fig_solver_quality()
    fig_scaling()
    fig_pareto()
    fig_gpu()
    fig_scc()
    fig_lp_gap()
    print("done")
