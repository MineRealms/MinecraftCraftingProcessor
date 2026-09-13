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
        <td>${esc(m.display)}</td>
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
  box.innerHTML = `<div class="placeholder">加载 ${esc(id)} …</div>`;
  try {
    const qs = new URLSearchParams();
    if (kind) qs.set("kind", kind);
    if (nbt) qs.set("nbt", nbt);
    qs.set("limit", "80");
    const d = await api(`/api/material/${encodeURIComponent(id)}?${qs}`);
    const m = d.material;
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
      <div>${esc(m.display)} ${tags}</div>
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
  return `<span class="alt mono ${kindClass(e.material.kind)}">${esc(e.material.id)} ×${fmt(e.rate_per_min)}/min</span>`;
}

function renderPlan(p) {
  const metrics = [
    ["配方种类", p.totals.distinct_recipes],
    ["总操作 /min", fmt(p.totals.recipe_ops_per_min)],
    ["估算成本", fmt(p.totals.estimated_cost)],
    ["原料物品 /min", fmt(p.totals.raw_items_per_min)],
    ["原料流体 mB/min", fmt(p.totals.raw_fluids_mb_per_min)],
    ["耗时 ms", fmt(p.elapsed_ms, 1)],
  ].map(([k, v]) => `<div class="metric"><div class="k">${k}</div><div class="v">${v}</div></div>`).join("");

  const recipes = p.recipes.map((r) => `
    <tr>
      <td class="mono">${fmt(r.ops_per_min)}</td>
      <td>${esc(r.category_title)}</td>
      <td class="mono">${esc(r.recipe)}</td>
      <td>${r.inputs.map(planEntryHtml).join("<br>")}</td>
      <td>${r.outputs.map(planEntryHtml).join("<br>")}</td>
    </tr>`).join("");

  const raw = p.raw_materials.map((e) =>
    `<tr><td class="mono ${kindClass(e.material.kind)}">${esc(e.material.id)}</td><td>${kindName(e.material.kind)}</td><td class="mono">${fmt(e.rate_per_min)}</td></tr>`).join("");
  const byp = p.byproducts.map((e) =>
    `<tr><td class="mono ${kindClass(e.material.kind)}">${esc(e.material.id)}</td><td>${kindName(e.material.kind)}</td><td class="mono">${fmt(e.rate_per_min)}</td></tr>`).join("");

  return `
    <h2>生产计划 · <span class="mono ${kindClass(p.target.kind)}">${esc(p.target.id)}</span>
      × <span class="mono">${fmt(p.rate_per_min)}</span>/min <span class="tag">${esc(p.mode)}</span></h2>
    <div class="plan-summary">${metrics}</div>
    <h3>配方步骤（按操作量降序）</h3>
    <table>
      <thead><tr><th>op/min</th><th>机器/分类</th><th>配方</th><th>输入</th><th>输出</th></tr></thead>
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
      body: JSON.stringify({ material, rate, mode }),
    });
    box.innerHTML = renderPlan(p);
  } catch (e) {
    box.innerHTML = `<div class="placeholder">错误：${esc(e.message)}</div>`;
  }
}

$("#plan-btn").addEventListener("click", runPlan);
$("#plan-material").addEventListener("keydown", (e) => { if (e.key === "Enter") runPlan(); });

// ---------------------------------------------------------------------------
// Graph (cytoscape)
// ---------------------------------------------------------------------------
let cy = null;

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
          "width": 22,
          "height": 22,
          "text-max-width": "110px",
          "text-wrap": "ellipsis",
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
          "width": 14,
          "height": 14,
          "text-max-width": "90px",
          "text-wrap": "ellipsis",
        },
      },
      {
        selector: "edge",
        style: {
          "width": 1,
          "line-color": "#3a4152",
          "target-arrow-color": "#3a4152",
          "target-arrow-shape": "triangle",
          "arrow-scale": 0.7,
          "curve-style": "bezier",
        },
      },
    ],
    layout: { name: "breadthfirst", directed: true, padding: 20, spacingFactor: 1.2 },
    wheelSensitivity: 0.2,
  });
  window._cy = cy;
  return cy;
}

async function loadGraph() {
  const material = $("#graph-material").value.trim();
  if (!material) return;
  const depth = $("#graph-depth").value;
  const box = $("#cy");
  try {
    const d = await api(`/api/graph?material=${encodeURIComponent(material)}&depth=${depth}&limit=500`);
    const c = ensureCy();
    c.elements().remove();
    c.add([...d.nodes, ...d.edges]);
    c.layout({ name: "breadthfirst", directed: true, padding: 24, spacingFactor: 1.15 }).run();
    $("#footer-status").textContent =
      `图谱：${d.nodes.length} 节点 / ${d.edges.length} 边${d.truncated ? "（已截断）" : ""}`;
  } catch (e) {
    box.innerHTML = "";
    $("#footer-status").textContent = "图谱加载失败：" + e.message;
  }
}

$("#graph-btn").addEventListener("click", loadGraph);
$("#graph-material").addEventListener("keydown", (e) => { if (e.key === "Enter") loadGraph(); });
$("#graph-depth").addEventListener("input", (e) => { $("#graph-depth-val").textContent = e.target.value; });

// ---------------------------------------------------------------------------
// Init
// ---------------------------------------------------------------------------
loadStats();
