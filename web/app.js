// rowdiet page logic: textarea → lint via the wasm module → per-table byte rulers.

import { initRowdiet } from "./loader.js";
import { initEditor } from "./editor.js";

const MAXALIGN = 8;
const TUPLE_HEADER_ALIGNED = 24;
const els = {
  sql: document.getElementById("sql"),
  rows: document.getElementById("rows"),
  results: document.getElementById("results"),
  status: document.getElementById("status"),
  tooltip: document.getElementById("tooltip"),
  hlcode: document.getElementById("hlcode"),
  gutter: document.getElementById("gutter"),
  dragline: document.getElementById("dragline"),
  download: document.getElementById("download"),
};

let api = null;
try {
  api = await initRowdiet(await wasmBytes());
  els.status.textContent = "";
  analyze();
} catch (e) {
  els.status.textContent = `failed to load rowdiet.wasm — run web/build.sh first (${e})`;
}

// The standalone single-file build embeds the module as base64 so the page works from file://
// (where fetch is blocked); the hosted multi-file page fetches it normally.
async function wasmBytes() {
  const embedded = globalThis.ROWDIET_WASM_B64;
  if (embedded) {
    const bin = atob(embedded);
    const bytes = new Uint8Array(bin.length);
    for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
    return bytes;
  }
  return await (await fetch("./rowdiet.wasm")).arrayBuffer();
}

let debounce = null;
function schedule() {
  clearTimeout(debounce);
  debounce = setTimeout(analyze, 350);
}
els.rows.addEventListener("input", schedule);
initEditor({
  textarea: els.sql,
  highlightCode: els.hlcode,
  gutter: els.gutter,
  dragline: els.dragline,
  onChange: schedule,
});
els.download.addEventListener("click", () => downloadSql("schema.sql", els.sql.value));

function downloadSql(name, text) {
  const url = URL.createObjectURL(new Blob([text], { type: "application/sql" }));
  const a = document.createElement("a");
  a.href = url;
  a.download = name;
  a.click();
  setTimeout(() => URL.revokeObjectURL(url), 2000);
}

function analyze() {
  if (!api) return;
  const out = api.lint({ sources: [{ name: "pasted.sql", sql: els.sql.value }], fail_over: 0 });
  render(out);
}

function pad(off, align) {
  return (align - (off % align)) % align;
}

function alignBytes(a) {
  return { char: 1, short: 2, int: 4, double: 8 }[a];
}

// Mirrors layout::walk for the suggested order (the JSON carries the current-order walk only).
function walk(kinds) {
  let off = 0;
  const segs = [];
  let padding = 0;
  for (const kind of kinds) {
    let p, size;
    if (kind.fixed) {
      p = pad(off, alignBytes(kind.fixed.align));
      size = kind.fixed.len;
    } else if (kind.varlena.proven_short) {
      p = 0;
      size = 1;
    } else {
      p = pad(off, alignBytes(kind.varlena.align));
      size = 4;
    }
    off += p;
    segs.push({ pad: p, offset: off, size });
    off += size;
    padding += p;
  }
  return { segs, end: off, padding };
}

function render(out) {
  els.results.replaceChildren();
  if (out.error) {
    els.results.append(el("p", "err", out.error));
    return;
  }
  const rows = Number(els.rows.value) || 0;
  for (const t of out.analysis.tables) {
    els.results.append(tableCard(t, rows));
  }
  // Wide tables squeeze segments below legibility — drop those labels (tooltips keep the name).
  requestAnimationFrame(() => {
    for (const seg of els.results.querySelectorAll(".seg")) {
      if (seg.textContent && seg.clientWidth < 26) seg.textContent = "";
    }
  });
  for (const n of out.analysis.notes) {
    els.results.append(el("p", "note", `${n.origin.source}:${n.origin.line} [${n.kind}] ${n.detail}`));
  }
  if (!out.analysis.tables.length && !out.analysis.notes.length) {
    els.results.append(el("p", "note", "no CREATE TABLE statements found"));
  }
}

function tableCard(t, rows) {
  const card = el("section", "card");
  const head = el("div", "card-head");
  head.append(el("h2", "", t.name), el("span", "chip", tierLabel(t.tier)));
  card.append(head);
  const hero = el("p", "hero");
  if (t.ignored) {
    hero.append(el("span", "muted", "ignored (rowdiet:ignore)"));
  } else if (t.natts === 0) {
    hero.append(el("span", "muted", "no modeled columns — not analyzable (partition child / LIKE / typed table)"));
    card.append(hero);
    return card;
  } else if (t.avoidable_bytes_per_row > 0) {
    hero.append(el("strong", "bad", `▲ ${t.avoidable_bytes_per_row} B/row avoidable`));
    if (rows > 0) hero.append(el("span", "muted", `  × ${rows.toLocaleString("en")} rows ≈ ${humanBytes(t.avoidable_bytes_per_row * rows)}`));
  } else {
    hero.append(el("strong", "good", "✓ optimal"), el("span", "muted", ` — ${t.current.padding} B scenario padding, none avoidable`));
  }
  card.append(hero);
  const current = { segs: t.columns.map((c) => ({ pad: c.pad_before, size: sizeOf(c.kind) })), end: null, cols: t.columns };
  const byName = new Map(t.columns.map((c) => [c.name, c]));
  const orderedCols = t.suggested_order.map((n) => byName.get(n)).filter(Boolean);
  const suggested = walk(orderedCols.map((c) => c.kind));
  const curEnd = endOf(t.columns);
  card.append(
    ruler("current", t, t.columns, current.segs, curEnd, curEnd, t.current),
    ruler("suggested", t, orderedCols, suggested.segs, suggested.end, curEnd, t.suggested),
  );
  card.append(legend(t.tier));
  if (t.avoidable_bytes_per_row > 0) {
    const btn = el("button", "copy", "copy reordered CREATE TABLE");
    btn.addEventListener("click", () => {
      navigator.clipboard.writeText(reorderedDdl(t));
      btn.textContent = "copied ✓";
      setTimeout(() => (btn.textContent = "copy reordered CREATE TABLE"), 1500);
    });
    const dl = el("button", "copy", "download .sql");
    dl.addEventListener("click", () =>
      downloadSql(`${t.name.replace(/[^A-Za-z0-9_-]/g, "_")}-suggested.sql`, reorderedDdl(t)),
    );
    card.append(btn, dl);
  }
  card.append(columnTable(t));
  return card;
}

function sizeOf(kind) {
  if (kind.fixed) return kind.fixed.len;
  return kind.varlena.proven_short ? 1 : 4;
}

function endOf(cols) {
  let end = 0;
  for (const c of cols) end += c.pad_before + sizeOf(c.kind);
  return end;
}

function ruler(label, t, cols, segs, end, scaleEnd, stats) {
  const wrap = el("div", "ruler");
  const cap = el("div", "ruler-label");
  cap.append(el("span", "", label), el("span", "muted", statsLine(stats)));
  wrap.append(cap);
  const bar = el("div", "bar");
  const exact = t.tier === "exact";
  const total = exact ? TUPLE_HEADER_ALIGNED + scaleEnd + MAXALIGN : scaleEnd;
  if (exact) bar.append(seg("hdr", TUPLE_HEADER_ALIGNED, total, "t_hoff — tuple header (order-invariant)", "hdr"));
  cols.forEach((c, i) => {
    const s = segs[i];
    if (s.pad > 0) bar.append(seg("", s.pad, total, `${s.pad} B alignment padding before ${c.name}`, "padseg"));
    const kindText = c.kind.fixed
      ? `fixed ${c.kind.fixed.len} B, ${c.kind.fixed.align}-aligned`
      : c.kind.varlena.proven_short
        ? "varlena, proven short (1 B header, unaligned)"
        : `varlena long-form header (4 B, ${c.kind.varlena.align}-aligned) — payload excluded`;
    const cls = c.kind.fixed ? "data" : "vhdr";
    bar.append(seg(c.name, sizeOf(c.kind), total, `${c.name} ${c.type_display}${c.not_null ? " NOT NULL" : ""} — ${kindText}`, cls));
  });
  if (exact) {
    const tail = footprintOf(stats) - TUPLE_HEADER_ALIGNED - end;
    if (tail > 0) bar.append(seg("", tail, total, `${tail} B MAXALIGN rounding at row placement`, "padseg"));
  } else {
    bar.append(el("span", "ghost", "+ payloads…"));
  }
  const shrink = scaleEnd > 0 ? Math.max(end / scaleEnd, 0.05) : 1;
  bar.style.width = `${Math.min(shrink, 1) * 100}%`;
  wrap.append(bar);
  return wrap;
}

function footprintOf(stats) {
  return stats.footprint ?? 0;
}

function seg(text, bytes, total, tip, cls) {
  const s = el("span", `seg ${cls}`, text);
  s.style.flexGrow = String(bytes);
  // A byte-proportional basis keeps proportions honest when a wide table overflows into the
  // bar's own horizontal scroll (grow-only would collapse every segment to its min width).
  s.style.flexBasis = `${bytes * 6}px`;
  s.dataset.tip = `${tip} · ${bytes} B`;
  s.addEventListener("pointerenter", showTip);
  s.addEventListener("pointermove", moveTip);
  s.addEventListener("pointerleave", hideTip);
  return s;
}

function statsLine(s) {
  return s.footprint != null
    ? `${s.padding} B padding · ${s.footprint} B/row footprint · ${s.rows_per_page} rows/8kB page`
    : `${s.padding} B padding/row (long-form scenario)`;
}

function tierLabel(tier) {
  return tier === "exact" ? "exact — fixed-width only" : "estimate — long-form varlena scenario";
}

function legend(tier) {
  const wrap = el("div", "legend");
  for (const [cls, label] of [
    ["data", "fixed column"],
    ["vhdr", "varlena header"],
    ["padseg", "padding (dead bytes)"],
    ...(tier === "exact" ? [["hdr", "tuple header"]] : []),
  ]) {
    const item = el("span", "legend-item");
    item.append(el("i", `swatch ${cls}`), document.createTextNode(label));
    wrap.append(item);
  }
  return wrap;
}

function columnTable(t) {
  const det = el("details", "coltable");
  det.append(el("summary", "", "column table"));
  const table = el("table");
  const head = el("tr");
  for (const h of ["column", "type", "not null", "offset", "pad before"]) head.append(el("th", "", h));
  table.append(head);
  for (const c of t.columns) {
    const tr = el("tr");
    tr.append(
      el("td", "", c.name),
      el("td", "", c.type_display),
      el("td", "", c.not_null ? "yes" : ""),
      el("td", "num", String(c.offset)),
      el("td", "num", c.pad_before ? `${c.pad_before} B` : ""),
    );
    table.append(tr);
  }
  det.append(table);
  return det;
}

function reorderedDdl(t) {
  const byName = new Map(t.columns.map((c) => [c.name, c]));
  const lines = t.suggested_order
    .map((n) => byName.get(n))
    .filter(Boolean)
    .map((c) => `    ${quoteIdent(c.name)} ${c.type_display}${c.not_null ? " NOT NULL" : ""}`);
  return `-- rowdiet suggestion (column order only — re-attach defaults/constraints/options)\nCREATE TABLE ${t.name} (\n${lines.join(",\n")}\n);\n`;
}

function quoteIdent(name) {
  return /^[a-z_][a-z0-9_]*$/.test(name) ? name : `"${name.replaceAll('"', '""')}"`;
}

function humanBytes(b) {
  if (b >= 1e9) return `${(b / 1e9).toFixed(1)} GB`;
  if (b >= 1e6) return `${(b / 1e6).toFixed(1)} MB`;
  if (b >= 1e3) return `${(b / 1e3).toFixed(1)} kB`;
  return `${b} B`;
}

function el(tag, cls = "", text = "") {
  const node = document.createElement(tag);
  if (cls) node.className = cls;
  if (text) node.textContent = text;
  return node;
}

function showTip(e) {
  els.tooltip.textContent = e.currentTarget.dataset.tip;
  els.tooltip.hidden = false;
  moveTip(e);
}

function moveTip(e) {
  const pad = 14;
  const w = els.tooltip.offsetWidth;
  const x = Math.min(e.clientX + pad, window.innerWidth - w - pad);
  els.tooltip.style.left = `${x}px`;
  els.tooltip.style.top = `${e.clientY + pad}px`;
}

function hideTip() {
  els.tooltip.hidden = true;
}
