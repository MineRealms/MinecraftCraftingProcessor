"use strict";

const $ = (sel) => document.querySelector(sel);
const $$ = (sel) => document.querySelectorAll(sel);

async function api(url, opts) {
  const r = await fetch(url, opts);
  if (!r.ok) {
    let msg = r.statusText;
    try { const j = await r.json(); msg = j.error || msg; } catch (_) {}
    throw new Error(msg);
  }
  return r.json();
}

const fmt = (v, digits = 2) => {
  if (v === null || v === undefined || Number.isNaN(v)) return "-";
  const a = Math.abs(v);
  if (a === 0) return "0";
  if (a >= 1000) return v.toLocaleString("zh-CN", { maximumFractionDigits: 0 });
  if (a >= 100) return v.toFixed(1);
  if (a >= 1) return v.toFixed(digits);
  return v.toFixed(5);
};

const esc = (s) => String(s ?? "").replace(/[&<>"']/g, (c) => ({
  "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;",
}[c]));

const kindClass = (k) => (k === "fluid" ? "kind-fluid" : "kind-item");
const kindName = (k) => (k === "fluid" ? "流体" : k === "item" ? "物品" : k);

// ---------------------------------------------------------------------------
// Language (默认中文；localStorage 记忆)
// ---------------------------------------------------------------------------
let lang = localStorage.getItem("gtp-lang") || "zh";
let lastPlan = null;
let lastSearchQuery = "";
let lastProcess = null;
const nm = (m) => (lang === "zh" ? m.display_zh || m.display : m.display_en || m.display);
function setLang(l, rerender = true) {
  lang = l;
  localStorage.setItem("gtp-lang", l);
  document.documentElement.lang = l === "zh" ? "zh-CN" : "en";
  $("#lang-toggle").textContent = l === "zh" ? "EN" : "中文";
  if (rerender) {
    if (lastGraph) applyGraphView();
    if (lastProcess) renderProcess(lastProcess);
    const mid = $("#material-content")?.dataset.matid;
    if (mid) showMaterial(mid, $("#material-content").dataset.matkind || null, $("#material-content").dataset.matnbt || null);
    if (lastPlan) $("#plan-result").innerHTML = renderPlan(lastPlan);
    if (lastSearchQuery) doSearch();
  }
}
$("#lang-toggle").addEventListener("click", () => setLang(lang === "zh" ? "en" : "zh"));

// ---------------------------------------------------------------------------
// Tabs
// ---------------------------------------------------------------------------
function switchTab(name) {
  $$(".tabs button").forEach((b) => b.classList.toggle("active", b.dataset.tab === name));
  $$(".tab").forEach((t) => t.classList.toggle("active", t.id === "tab-" + name));
  if (name === "graph" && window._cy) setTimeout(() => window._cy.resize(), 50);
}
$$(".tabs button").forEach((b) => b.addEventListener("click", () => switchTab(b.dataset.tab)));

// ---------------------------------------------------------------------------
// Stats
// ---------------------------------------------------------------------------
async function loadStats() {
  try {
    const d = await api("/api/stats");
    const s = d.stats;
    $("#stats-chips").innerHTML = [
      ["材料", s.material_count],
      ["配方", s.recipe_count],
      ["分类", s.category_count],
      ["NBT 变体", s.nbt_variant_count],
      ["可规划配方", s.plannable_recipe_count],
      ["分析", d.analysis.build_ms.toFixed(0) + "ms"],
    ].map(([k, v]) => `<span class="chip">${k} <b>${Number(v).toLocaleString()}</b></span>`).join("");
    $("#footer-status").textContent = `数据集 ${d.meta.format} v${d.meta.version} · MC ${d.meta.minecraft_version}`;
  } catch (e) {
    $("#stats-chips").innerHTML = `<span class="chip bad">加载失败：${esc(e.message)}</span>`;
  }
}

// ---------------------------------------------------------------------------
// Search
// ---------------------------------------------------------------------------
async function doSearch() {
  const q = $("#search-input").value.trim();
  if (!q) return;
  lastSearchQuery = q;
  const kind = $("#search-kind").value;
  const box = $("#search-results");
  box.innerHTML = `<div class="placeholder">搜索中…</div>`;
  try {
    const d = await api(`/api/search?q=${encodeURIComponent(q)}&limit=60${kind ? "&kind=" + kind : ""}`);
    if (!d.results.length) {
      box.innerHTML = `<div class="placeholder">没有匹配的材料</div>`;
      return;
    }
    const rows = d.results.map((r) => {
      const m = r.material;
      return `<tr class="clickable" data-id="${esc(m.id)}" data-kind="${esc(m.kind)}" data-nbt="${esc(m.nbt || "")}">
        <td class="mono ${kindClass(m.kind)}">${esc(m.id)}</td>
        <td>${kindName(m.kind)}${m.nbt ? ' <span class="tag warn">NBT</span>' : ""}</td>
        <td>${esc(nm(m))}</td>
        <td class="muted">${r.producers}</td>
        <td class="muted">${r.consumers}</td>
        <td class="mono">${r.unit_cost === null ? "-" : fmt(r.unit_cost, 3)}</td>
      </tr>`;
    }).join("");
    box.innerHTML = `<table>
      <thead><tr><th>ID</th><th>类型</th><th>名称</th><th>生产者</th><th>消费者</th><th>成本估计</th></tr></thead>
      <tbody>${rows}</tbody></table>`;
    box.querySelectorAll("tr.clickable").forEach((tr) => {
      tr.addEventListener("click", () => {
        showMaterial(tr.dataset.id, tr.dataset.kind, tr.dataset.nbt || null);
        switchTab("material");
      });
    });
  } catch (e) {
    box.innerHTML = `<div class="placeholder">错误：${esc(e.message)}</div>`;
  }
}

$("#search-btn").addEventListener("click", doSearch);
$("#search-input").addEventListener("keydown", (e) => { if (e.key === "Enter") doSearch(); });

// ---------------------------------------------------------------------------
// Material detail
// ---------------------------------------------------------------------------
function slotHtml(slot) {
  const alts = slot.ingredients.map((ing) => {
    const qty = ing.count != null ? `×${ing.count}` : ing.amount != null ? `${ing.amount}mB` : "";
    return `<span class="alt mono ${kindClass(ing.kind)}">${esc(ing.id)}${qty}${ing.nbt ? ' <span class="tag warn">NBT</span>' : ""}</span>`;
  }).join('<span class="or">或</span> ');
  return `<div class="slot"><span class="sname">${esc(slot.name || "slot")}</span><span>${alts}</span></div>`;
}

function recipeCard(r) {
  return `<div class="recipe-card">
    <div class="recipe-head">
      <span class="mono">${esc(r.id)}</span>
      <span class="tag">${esc(r.category_title)}</span>
    </div>
    <div class="slots">
      <div class="muted" style="font-size:12px;margin-top:4px">输入</div>
      ${r.inputs.map(slotHtml).join("") || '<div class="slot muted">（无输入）</div>'}
      <div class="muted" style="font-size:12px;margin-top:4px">输出</div>
      ${r.outputs.map(slotHtml).join("") || '<div class="slot muted">（无输出）</div>'}
    </div>
  </div>`;
}

async function showMaterial(id, kind, nbt) {
  const box = $("#material-content");
  box.dataset.matid = id;
  box.dataset.matkind = kind || "";
  box.dataset.matnbt = nbt || "";
  box.innerHTML = `<div class="placeholder">加载 ${esc(id)} …</div>`;
  try {
    const qs = new URLSearchParams();
    if (kind) qs.set("kind", kind);
    if (nbt) qs.set("nbt", nbt);
    qs.set("limit", "80");
    const d = await api(`/api/material/${encodeURIComponent(id)}?${qs}`);
    const m = d.material;
    const otherName = lang === "zh" ? m.display_en : m.display_zh;
    const tags = [
      `<span class="tag">${kindName(m.kind)}</span>`,
      m.nbt ? `<span class="tag warn">NBT</span>` : "",
      d.cyclic ? `<span class="tag bad">循环成员</span>` : "",
      d.unit_cost != null ? `<span class="tag ok">成本 ${fmt(d.unit_cost, 4)}</span>` : `<span class="tag bad">不可达</span>`,
      d.depth != null ? `<span class="tag">深度 ${d.depth}</span>` : "",
      d.best_recipe ? `<span class="tag">最佳：${esc(d.best_recipe)}</span>` : "",
    ].join(" ");
    box.innerHTML = `
      <h2 class="mono ${kindClass(m.kind)}">${esc(m.id)}</h2>
      <div><b>${esc(nm(m))}</b>${otherName && otherName !== nm(m) ? ` <span class="muted">/ ${esc(otherName)}</span>` : ""} ${tags}</div>
      <div style="margin:10px 0">
        <button id="mat-plan">规划这个材料</button>
        <button class="ghost" id="mat-graph">查看图谱</button>
      </div>
      <div class="grid-2">
        <div>
          <h3>生产配方（${d.producer_count}，显示 ${d.producers.length}）</h3>
          ${d.producers.map(recipeCard).join("") || '<div class="muted">无</div>'}
        </div>
        <div>
          <h3>消耗配方（${d.consumer_count}，显示 ${d.consumers.length}）</h3>
          ${d.consumers.map(recipeCard).join("") || '<div class="muted">无</div>'}
        </div>
      </div>`;
    $("#mat-plan").addEventListener("click", () => {
      $("#plan-material").value = m.id;
      switchTab("planner");
    });
    $("#mat-graph").addEventListener("click", () => {
      $("#graph-material").value = m.id;
      switchTab("graph");
      loadGraph();
    });
  } catch (e) {
    box.innerHTML = `<div class="placeholder">错误：${esc(e.message)}</div>`;
  }
}

// ---------------------------------------------------------------------------
// Planner
// ---------------------------------------------------------------------------
function planEntryHtml(e) {
  const chance = e.chance != null ? ` <span class="tag warn">${Math.round(e.chance * 100)}%</span>` : "";
  return `<span class="alt mono ${kindClass(e.material.kind)}">${esc(nm(e.material))} ×${fmt(e.rate_per_min)}/min${chance}</span>`;
}

/** 性能计数器行（来自 plan.metrics） */
function metricLine(p) {
  const m = p.metrics || {};
  const parts = [
    m.analysis_ms ? `分析 ${fmt(m.analysis_ms, 0)}ms` : null,
    m.expansions ? `展开 ${Number(m.expansions).toLocaleString()}` : null,
    m.evaluations ? `评估 ${Number(m.evaluations).toLocaleString()}` : null,
    m.rounds ? `轮次 ${m.rounds}` : null,
    m.gpu_candidates ? `GPU 候选 ${Number(m.gpu_candidates).toLocaleString()}` : null,
    m.gpu_solves
      ? `GPU 求解 ${Number(m.gpu_solves).toLocaleString()}（收敛 ${Number(m.gpu_converged).toLocaleString()}，迭代 ${Number(m.gpu_iters_total).toLocaleString()}）`
      : null,
    m.lp_variables
      ? `LP ${Number(m.lp_variables).toLocaleString()} 变量 / ${Number(m.lp_constraints || 0).toLocaleString()} 约束 / ${m.lp_status || "-"} / 目标值 ${m.lp_objective != null ? fmt(m.lp_objective, 4) : "-"}`
      : null,
  ].filter(Boolean);
  if (!parts.length) return "";
  return `<div class="muted mono" style="margin:6px 0;font-size:12px">性能指标：${parts.join(" · ")}</div>`;
}

function renderPlan(p) {
  const metrics = [
    ["配方种类", p.totals.distinct_recipes],
    ["总操作 /min", fmt(p.totals.recipe_ops_per_min)],
    ["估算成本", fmt(p.totals.estimated_cost)],
    ["原料物品 /min", fmt(p.totals.raw_items_per_min)],
    ["原料流体 mB/min", fmt(p.totals.raw_fluids_mb_per_min)],
    ["机器总数", p.totals.total_machines ? fmt(p.totals.total_machines, 1) : "-"],
    ["整数机器数", p.totals.total_machines_int ? String(p.totals.total_machines_int) : "-"],
    ["净功率 EU/t", p.totals.total_machines ? fmt(p.totals.net_eu_t, 0) : "-"],
    ["耗电 EU/t", p.totals.total_machines ? fmt(p.totals.consume_eu_t, 0) : "-"],
    ["发电 EU/t", p.totals.total_machines ? fmt(p.totals.generate_eu_t, 0) : "-"],
    ["目标分", fmt(p.totals.objective_score, 2)],
    ["耗时 ms", fmt(p.elapsed_ms, 1)],
  ].map(([k, v]) => `<div class="metric"><div class="k">${k}</div><div class="v">${v}</div></div>`).join("");

  const recipes = p.recipes.map((r) => `
    <tr>
      <td class="mono">${fmt(r.ops_per_min)}</td>
      <td>${esc(r.category_title)}</td>
      <td class="mono">${esc(r.recipe)}</td>
      <td class="mono">${r.machine_count != null ? (r.machine_count_int != null ? `${fmt(r.machine_count, 2)} (${r.machine_count_int})` : fmt(r.machine_count, 2)) : "-"}</td>
      <td class="mono">${r.eut != null ? fmt(r.eut, 0) : "-"}</td>
      <td>${r.tier ? `<span class="tag">${esc(r.tier)}</span>` : "-"}</td>
      <td>${r.inputs.map(planEntryHtml).join("<br>")}</td>
      <td>${r.outputs.map(planEntryHtml).join("<br>")}</td>
    </tr>`).join("");

  const raw = p.raw_materials.map((e) =>
    `<tr><td class="mono ${kindClass(e.material.kind)}">${esc(nm(e.material))}</td><td>${kindName(e.material.kind)}</td><td class="mono">${fmt(e.rate_per_min)}</td></tr>`).join("");
  const byp = p.byproducts.map((e) =>
    `<tr><td class="mono ${kindClass(e.material.kind)}">${esc(nm(e.material))}</td><td>${kindName(e.material.kind)}</td><td class="mono">${fmt(e.rate_per_min)}</td></tr>`).join("");

  return `
    <h2>生产计划 · <span class="mono ${kindClass(p.target.kind)}">${esc(nm(p.target))}</span>
      × <span class="mono">${fmt(p.rate_per_min)}</span>/min <span class="tag">${esc(p.mode)}</span></h2>
    <div class="plan-summary">${metrics}</div>
    ${metricLine(p)}
    <h3>配方步骤（按操作量降序）</h3>
    <table>
      <thead><tr><th>op/min</th><th>机器/分类</th><th>配方</th><th>机器数</th><th>EU/t</th><th>等级</th><th>输入</th><th>输出</th></tr></thead>
      <tbody>${recipes}</tbody>
    </table>
    <div class="grid-2" style="margin-top:14px">
      <div>
        <h3>原料（需要开采 / 外部输入）</h3>
        <table><thead><tr><th>材料</th><th>类型</th><th>速率/min</th></tr></thead><tbody>${raw || '<tr><td colspan="3" class="muted">无</td></tr>'}</tbody></table>
      </div>
      <div>
        <h3>副产物</h3>
        <table><thead><tr><th>材料</th><th>类型</th><th>速率/min</th></tr></thead><tbody>${byp || '<tr><td colspan="3" class="muted">无</td></tr>'}</tbody></table>
      </div>
    </div>
    ${p.notes.length ? `<h3>备注</h3><ul class="notes">${p.notes.map((n) => `<li>${esc(n)}</li>`).join("")}</ul>` : ""}`;
}

async function runPlan() {
  const material = $("#plan-material").value.trim();
  const rate = parseFloat($("#plan-rate").value);
  const mode = $("#plan-mode").value;
  if (!material || !(rate > 0)) {
    $("#plan-result").innerHTML = `<div class="placeholder">请填写材料与正数速率</div>`;
    return;
  }
  const box = $("#plan-result");
  box.innerHTML = `<div class="placeholder">规划中…（beam 模式约 1~3 秒）</div>`;
  try {
    const p = await api("/api/plan", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        material,
        rate,
        mode,
        max_tier: $("#plan-tier").value || null,
        objective: $("#plan-objective").value || null,
        beam_width: parseInt($("#adv-beam-width").value, 10) || undefined,
        candidates: parseInt($("#adv-candidates").value, 10) || undefined,
        max_iterations: parseInt($("#adv-iterations").value, 10) || undefined,
        mcts_iterations: parseInt($("#adv-mcts-iters").value, 10) || undefined,
        include_recycling: $("#adv-recycling").checked,
        block_amplification: $("#adv-block-amp").checked,
      }),
    });
    lastPlan = p;
    box.innerHTML = renderPlan(p);
    loadMetrics();
    loadHistory();
  } catch (e) {
    box.innerHTML = `<div class="placeholder">错误：${esc(e.message)}</div>`;
  }
}

/** 运行指标（数据集/分析/运行时计数器） */
async function loadMetrics() {
  const box = $("#plan-metrics");
  if (!box) return;
  try {
    const d = await api("/api/metrics");
    const ds = d.dataset, an = d.analysis, rt = d.runtime;
    const items = [
      ["材料", ds.materials],
      ["配方", ds.recipes],
      ["名称库", ds.names],
      ["概率覆盖", ds.chance_overrides],
      ["SCC 分量", an.scc_components],
      ["循环分量", an.cyclic_components],
      ["分析耗时", fmt(an.build_ms, 0) + "ms"],
      ["规划请求", rt.plan_requests],
      ["平均耗时", fmt(rt.avg_plan_ms, 0) + "ms"],
      ["tree/beam/mcts/exact", `${rt.modes.tree}/${rt.modes.beam}/${rt.modes.mcts}/${rt.modes.exact}`],
      ["运行记录", rt.recorded_runs],
      ["运行时长", rt.uptime_s + "s"],
    ];
    box.innerHTML =
      `<h3>运行指标</h3><div class="plan-summary">` +
      items
        .map(([k, v]) => `<div class="metric"><div class="k">${k}</div><div class="v">${typeof v === "number" ? Number(v).toLocaleString() : esc(String(v))}</div></div>`)
        .join("") +
      `</div>`;
    $("#footer-status").textContent = `请求 ${rt.plan_requests} · 平均 ${fmt(rt.avg_plan_ms, 0)}ms · 记录 ${rt.recorded_runs} 条`;
  } catch (e) {
    box.innerHTML = `<div class="muted">指标加载失败：${esc(e.message)}</div>`;
  }
}

/** 运行历史（最近 N 条，点击回填表单） */
async function loadHistory() {
  const box = $("#plan-history");
  if (!box) return;
  try {
    const d = await api("/api/history?limit=20");
    if (!d.records || !d.records.length) {
      box.innerHTML = `<div class="muted">暂无运行记录（文件：${esc(d.path)}）</div>`;
      return;
    }
    const rows = d.records
      .slice()
      .reverse()
      .map(
        (r) => `<tr class="clickable" data-target="${esc(r.target)}" data-rate="${r.rate_per_min}" data-mode="${esc(r.mode)}">
        <td class="mono">${esc(r.ts.replace("T", " ").replace("Z", ""))}</td>
        <td class="mono">${esc(r.target)}</td>
        <td>${esc(r.mode)}</td>
        <td class="mono">${fmt(r.rate_per_min)}</td>
        <td class="mono">${fmt(r.cost)}</td>
        <td class="mono">${fmt(r.machines, 1)}</td>
        <td class="mono">${fmt(r.elapsed_ms, 0)}ms</td>
      </tr>`
      )
      .join("");
    box.innerHTML = `<h3>运行记录（最近 ${d.records.length} 条 · ${esc(d.path)}）</h3>
      <table><thead><tr><th>时间(UTC)</th><th>目标</th><th>模式</th><th>速率</th><th>成本</th><th>机器</th><th>耗时</th></tr></thead>
      <tbody>${rows}</tbody></table>`;
    box.querySelectorAll("tr.clickable").forEach((tr) =>
      tr.addEventListener("click", () => {
        $("#plan-material").value = tr.dataset.target;
        $("#plan-rate").value = tr.dataset.rate;
        $("#plan-mode").value = tr.dataset.mode;
      })
    );
  } catch (e) {
    box.innerHTML = `<div class="muted">历史加载失败：${esc(e.message)}</div>`;
  }
}

$("#plan-btn").addEventListener("click", runPlan);

/** 加载并渲染 Process IR（物料流图） */
async function loadProcess() {
  const material = $("#plan-material").value.trim();
  const rate = parseFloat($("#plan-rate").value);
  const mode = $("#plan-mode").value;
  if (!material || !(rate > 0)) return;
  $("#graph-status").textContent = "生成流程图…";
  switchTab("graph");
  try {
    const pg = await api("/api/process", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        material,
        rate,
        mode,
        max_tier: $("#plan-tier").value || null,
        objective: $("#plan-objective").value || null,
      }),
    });
    renderProcess(pg);
  } catch (e) {
    $("#graph-status").textContent = "流程图失败：" + e.message;
  }
}

function renderProcess(pg) {
  lastProcess = pg;
  const c = ensureCy();
  lastGraph = null; // 流程图模式与材料图谱互斥
  const maxStep = pg.steps.length;
  // 步骤层级：原料 = 0；步骤 = 1 + max(输入步骤层级)
  const stepIn = pg.steps.map(() => []);
  pg.flows.forEach((f) => {
    if (f.to_step != null && f.from_step != null) stepIn[f.to_step].push(f.from_step);
  });
  const level = new Array(maxStep).fill(0);
  for (let it = 0; it < maxStep + 2; it++) {
    let changed = false;
    pg.steps.forEach((_, i) => {
      let lv = 1;
      stepIn[i].forEach((j) => { lv = Math.max(lv, level[j] + 1); });
      if (lv > level[i] && lv <= maxStep) { level[i] = lv; changed = true; }
    });
    if (!changed) break;
  }
  const maxLevel = Math.max(1, ...level);
  const yCount = new Map();
  const pos = (lv) => {
    const k = yCount.get(lv) || 0;
    yCount.set(lv, k + 1);
    return { x: lv * 240, y: k * 64 - 120 };
  };
  const nodes = [];
  const edges = [];
  const seen = new Set();
  const matNode = (m, cls, lv, prefix) => {
    const id = `${prefix}:${m.kind}:${m.id}:${m.nbt || ""}`;
    if (!seen.has(id)) {
      seen.add(id);
      nodes.push({
        data: { id, label: nm(m), mat_id: m.id, node_type: "material", material_kind: m.kind },
        classes: cls,
        position: pos(lv),
      });
    }
    return id;
  };
  pg.raw_inputs.forEach((f) => {
    const id = matNode(f.material, "ptype-raw", 0, "raw");
    edges.push({ data: { id: `e:${id}->s${f.to_step}`, source: id, target: `s:${f.to_step}`, label: `${fmt(f.rate_per_min)}/min` }, classes: "flow" });
  });
  pg.steps.forEach((s, i) => {
    const short = s.recipe.split("/").pop();
    nodes.push({
      data: { id: `s:${i}`, label: `${i + 1}. ${short}\n${fmt(s.ops_per_min)} op/min${s.machine_count != null ? ` · ${fmt(s.machine_count, 1)}台` : ""}`, node_type: "step" },
      classes: "ptype-step",
      position: pos(level[i]),
    });
  });
  pg.flows.forEach((f, k) => {
    if (f.from_step != null && f.to_step != null) {
      edges.push({
        data: { id: `f:${k}`, source: `s:${f.from_step}`, target: `s:${f.to_step}`, label: `${nm(f.material)} ${fmt(f.rate_per_min)}/min` },
        classes: "flow",
      });
    } else if (f.from_step != null && f.to_step == null) {
      const isTarget = f.material.id === pg.target.id;
      const id = matNode(f.material, isTarget ? "ptype-target" : "ptype-byproduct", maxLevel + 1, isTarget ? "target" : "by");
      edges.push({
        data: { id: `f:${k}`, source: `s:${f.from_step}`, target: id, label: `${fmt(f.rate_per_min)}/min` },
        classes: "flow",
      });
    }
  });
  c.startBatch();
  c.elements().remove();
  c.add([...nodes, ...edges]);
  c.endBatch();
  c.fit(c.elements(), 40);
  $("#graph-status").textContent =
    `流程图：${pg.steps.length} 步骤 / ${pg.flows.length} 内部流 / ${pg.raw_inputs.length} 原料 / ${pg.byproducts.length} 副产物`;
}
$("#plan-process").addEventListener("click", loadProcess);
$("#plan-material").addEventListener("keydown", (e) => { if (e.key === "Enter") runPlan(); });

// ---------------------------------------------------------------------------
// Graph (cytoscape) — 分层布局 + 筛选 + 大图性能保护
// ---------------------------------------------------------------------------
let cy = null;
let lastGraph = null; // { nodes, edges, start }

function ensureCy() {
  if (cy) return cy;
  cy = cytoscape({
    container: $("#cy"),
    style: [
      {
        selector: 'node[node_type = "material"]',
        style: {
          "background-color": "#4f8cff",
          "label": "data(label)",
          "color": "#dfe6f5",
          "font-size": "9px",
          "text-valign": "bottom",
          "text-margin-y": 3,
          "width": 20,
          "height": 20,
          "text-max-width": "110px",
          "text-wrap": "ellipsis",
          "min-zoomed-font-size": 7,
        },
      },
      {
        selector: 'node[node_type = "material"][material_kind = "fluid"]',
        style: { "background-color": "#37c9a0", shape: "round-rectangle" },
      },
      {
        selector: 'node[node_type = "recipe"]',
        style: {
          "background-color": "#ffb454",
          "shape": "diamond",
          "label": "data(label)",
          "color": "#8b93a7",
          "font-size": "7px",
          "text-valign": "bottom",
          "text-margin-y": 2,
          "width": 13,
          "height": 13,
          "text-max-width": "90px",
          "text-wrap": "ellipsis",
          "min-zoomed-font-size": 7,
        },
      },
      {
        selector: "edge",
        style: {
          "width": 1,
          "line-color": "#39404f",
          "target-arrow-color": "#39404f",
          "target-arrow-shape": "triangle",
          "arrow-scale": 0.7,
          "curve-style": "bezier",
          "font-size": "6px",
          "color": "#6b7488",
          "text-rotation": "autorotate",
          "text-background-color": "#0c0e14",
          "text-background-opacity": 0.7,
          "text-background-padding": "1px",
        },
      },
      { selector: "edge[?label]", style: { label: "data(label)" } },
      { selector: "node.nolabel", style: { label: "" } },
      { selector: "node.dim", style: { opacity: 0.12 } },
      { selector: "edge.dim", style: { opacity: 0.06 } },
      {
        selector: "node.hit",
        style: { "border-width": 3, "border-color": "#ffd166", "border-opacity": 1 },
      },
      // Process IR（流程图）节点样式
      {
        selector: "node.ptype-step",
        style: {
          shape: "round-rectangle",
          "background-color": "#ffb454",
          "background-opacity": 0.9,
          label: "data(label)",
          color: "#1a1d26",
          "font-size": "8px",
          "text-valign": "center",
          "text-max-width": "150px",
          "text-wrap": "ellipsis",
          width: 60,
          height: 24,
          "min-zoomed-font-size": 7,
        },
      },
      {
        selector: "node.ptype-raw",
        style: { "background-color": "#37c9a0", shape: "ellipse" },
      },
      {
        selector: "node.ptype-target",
        style: { "background-color": "#b37feb", shape: "star", width: 30, height: 30 },
      },
      {
        selector: "node.ptype-byproduct",
        style: { "background-color": "#6b7488", shape: "diamond" },
      },
      {
        selector: "edge.flow",
        style: { label: "data(label)", "font-size": "6px", "line-color": "#4a5a75" },
      },
    ],
    wheelSensitivity: 0.2,
    motionBlur: false,
  });
  window._cy = cy;
  cy.on("tap", "node", (evt) => {
    const matId = evt.target.data("mat_id");
    if (matId) {
      showMaterial(matId, null, null);
      switchTab("material");
    }
  });
  return cy;
}

/** 无向 BFS 分层 + 层内重心排序，返回 {id -> {x, y}} */
function layeredLayout(g) {
  const adj = new Map();
  const ids = new Set(g.nodes.map((n) => n.data.id));
  ids.forEach((id) => adj.set(id, []));
  g.edges.forEach((e) => {
    const s = e.data.source, t = e.data.target;
    if (!ids.has(s) || !ids.has(t)) return;
    adj.get(s).push(t);
    adj.get(t).push(s);
  });

  const level = new Map();
  const q = [g.start];
  level.set(g.start, 0);
  while (q.length) {
    const cur = q.shift();
    const lv = level.get(cur);
    for (const nb of adj.get(cur) || []) {
      if (!level.has(nb)) {
        level.set(nb, lv + 1);
        q.push(nb);
      }
    }
  }

  const byLevel = new Map();
  g.nodes.forEach((n) => {
    const lv = level.has(n.data.id) ? level.get(n.data.id) : 99;
    if (!byLevel.has(lv)) byLevel.set(lv, []);
    byLevel.get(lv).push(n.data.id);
  });

  const pos = new Map();
  const xGap = 250, yGap = 46;
  const levels = [...byLevel.keys()].sort((a, b) => a - b);
  let prevOrder = new Map();

  for (const lv of levels) {
    const arr = byLevel.get(lv);
    const bary = (id) => {
      let sum = 0, cnt = 0;
      for (const nb of adj.get(id) || []) {
        if (prevOrder.has(nb)) { sum += prevOrder.get(nb); cnt++; }
      }
      return cnt ? sum / cnt : Number.MAX_SAFE_INTEGER;
    };
    arr.sort((a, b) => bary(a) - bary(b));
    const n = arr.length;
    arr.forEach((id, i) => {
      pos.set(id, { x: lv * xGap, y: (i - (n - 1) / 2) * yGap });
      prevOrder.set(id, i);
    });
  }
  return pos;
}

/** 应用筛选与布局（不重新请求数据） */
function applyGraphView() {
  if (!lastGraph || !cy) return;
  const onlyMat = $("#graph-onlymat").checked;
  const autoNoLabels = lastGraph.nodes.length > 400;
  const noLabels = $("#graph-nolabels").checked || autoNoLabels;
  const filter = $("#graph-filter").value.trim().toLowerCase();
  const pos = layeredLayout(lastGraph);

  cy.startBatch();
  cy.elements().remove();
  const elements = lastGraph.nodes.map((n) => {
    const d = { ...n.data };
    d.label = lang === "zh" ? d.label_zh || d.label : d.label_en || d.label;
    return { data: d, position: pos.get(n.data.id) || { x: 0, y: 0 } };
  });
  elements.push(...lastGraph.edges);
  cy.add(elements);

  const hidden = new Set();
  if (onlyMat) {
    cy.nodes('[node_type = "recipe"]').forEach((n) => {
      hidden.add(n.id());
      n.style("display", "none");
    });
    cy.edges().forEach((e) => {
      if (hidden.has(e.data("source")) || hidden.has(e.data("target"))) e.style("display", "none");
    });
  }
  cy.nodes().forEach((n) => {
    n.toggleClass("nolabel", noLabels);
    if (filter) {
      const hit = n.id().toLowerCase().includes(filter) || String(n.data("label")).toLowerCase().includes(filter);
      n.toggleClass("hit", hit);
      n.toggleClass("dim", !hit);
    } else {
      n.removeClass("hit").removeClass("dim");
    }
  });
  if (filter) {
    cy.edges().forEach((e) => {
      const hit = e.id().toLowerCase().includes(filter) || String(e.data("label") || "").toLowerCase().includes(filter);
      e.toggleClass("dim", !hit);
    });
  } else {
    cy.edges().removeClass("dim");
  }
  cy.endBatch();
  cy.fit(cy.elements(":visible"), 40);
  const status = $("#graph-status");
  if (status) {
    status.textContent =
      (lastGraph.status || "") +
      (autoNoLabels ? " · 节点过多，已自动隐藏标签（可用缩放查看局部）" : "");
  }
}

async function loadGraph() {
  const material = $("#graph-material").value.trim();
  if (!material) return;
  const params = new URLSearchParams({
    material,
    depth: $("#graph-depth").value,
    direction: $("#graph-direction").value,
    max_nodes: $("#graph-maxnodes").value,
    max_inputs: $("#graph-maxinputs").value,
    exclude_recycling: $("#graph-norecycle").checked ? "true" : "false",
  });
  const status = $("#graph-status");
  status.textContent = "加载中…";
  try {
    const d = await api(`/api/graph?${params}`);
    lastGraph = {
      nodes: d.nodes,
      edges: d.edges,
      start: d.start,
      status:
        `节点 ${d.nodes.length} / 边 ${d.edges.length}` +
        (d.pruned_edges ? ` · 剪枝隐藏 ${d.pruned_edges} 条边` : "") +
        (d.truncated ? " · 已达节点上限（可减小深度或提高上限）" : ""),
    };
    ensureCy();
    applyGraphView();
  } catch (e) {
    status.textContent = "加载失败：" + e.message;
  }
}

$("#graph-btn").addEventListener("click", loadGraph);
$("#graph-material").addEventListener("keydown", (e) => { if (e.key === "Enter") loadGraph(); });
$("#graph-depth").addEventListener("input", (e) => { $("#graph-depth-val").textContent = e.target.value; });
$("#graph-maxnodes").addEventListener("input", (e) => { $("#graph-maxnodes-val").textContent = e.target.value; });
$("#graph-maxinputs").addEventListener("input", (e) => { $("#graph-maxinputs-val").textContent = e.target.value; });
["#graph-onlymat", "#graph-nolabels", "#graph-norecycle"].forEach((sel) =>
  $(sel).addEventListener("change", () => {
    if (sel === "#graph-norecycle") loadGraph();
    else applyGraphView();
  })
);
let filterTimer = null;
$("#graph-filter").addEventListener("input", () => {
  clearTimeout(filterTimer);
  filterTimer = setTimeout(applyGraphView, 150);
});

// ---------------------------------------------------------------------------
// Init
// ---------------------------------------------------------------------------
setLang(lang, false);
loadStats();
loadMetrics();
loadHistory();
