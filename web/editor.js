// Overlay SQL editor: a syntax-highlight layer behind a transparent-text textarea, a drag
// gutter that reorders column lines inside CREATE TABLE bodies (commas renormalized), and
// Alt+ArrowUp/Down as the keyboard equivalent. No dependencies — the standalone build inlines it.

const KEYWORDS = new Set(
  ("create table alter add column not null primary key default references constraint unique check " +
    "if exists partition of by range list hash values from to type as enum domain drop rename " +
    "unlogged temporary temp using with without time zone varying generated always identity " +
    "index on insert into select do begin end for cascade restrict inherits like including").split(" "),
);
const TYPES = new Set(
  ("bigint bigserial bit bool boolean box bytea char character cidr circle citext date decimal " +
    "double precision float4 float8 halfvec hstore inet int int2 int4 int8 integer interval json " +
    "jsonb line lseg macaddr macaddr8 money numeric oid path pg_lsn point polygon real serial " +
    "smallint smallserial sparsevec text timestamp timestamptz timetz tsquery tsvector uuid varbit " +
    "varchar vector xml").split(" "),
);

export function initEditor({ textarea, highlightCode, gutter, dragline, onChange }) {
  const lineHeight = parseFloat(getComputedStyle(textarea).lineHeight);
  const padTop = parseFloat(getComputedStyle(textarea).paddingTop);
  let drag = null;
  const handles = document.createElement("div");
  gutter.append(handles);
  function refresh() {
    highlightCode.innerHTML = highlight(textarea.value) + "\n";
    renderHandles();
    syncScroll();
  }
  function syncScroll() {
    highlightCode.parentElement.scrollTop = textarea.scrollTop;
    highlightCode.parentElement.scrollLeft = textarea.scrollLeft;
    handles.style.transform = `translateY(${-textarea.scrollTop}px)`;
  }
  function renderHandles() {
    const model = bodyModel(textarea.value);
    handles.replaceChildren();
    for (const [line, block] of model.draggable) {
      const h = document.createElement("div");
      h.className = "handle";
      h.textContent = "⋮⋮";
      h.title = "drag to reorder (or Alt+↑/↓ in the text)";
      h.style.top = `${padTop + line * lineHeight}px`;
      h.style.height = `${lineHeight}px`;
      h.addEventListener("pointerdown", (e) => startDrag(e, line, block, model));
      handles.append(h);
    }
  }
  function startDrag(e, line, block, model) {
    e.preventDefault();
    drag = { line, block, model, to: line };
    document.body.classList.add("dragging");
    window.addEventListener("pointermove", onDragMove);
    window.addEventListener("pointerup", endDrag, { once: true });
    onDragMove(e);
  }
  function onDragMove(e) {
    const rect = textarea.getBoundingClientRect();
    const y = e.clientY - rect.top + textarea.scrollTop - padTop;
    const lines = drag.model.blockLines.get(drag.block);
    const slot = Math.max(0, Math.min(lines.length, Math.round(y / lineHeight - lines[0])));
    drag.to = lines[Math.max(0, Math.min(lines.length - 1, slot))];
    const boundary = Math.max(lines[0], Math.min(lines[lines.length - 1] + 1, lines[0] + slot));
    dragline.hidden = false;
    dragline.style.top = `${padTop + boundary * lineHeight - textarea.scrollTop - 1}px`;
    drag.boundary = boundary;
  }
  function endDrag() {
    window.removeEventListener("pointermove", onDragMove);
    document.body.classList.remove("dragging");
    dragline.hidden = true;
    if (drag) {
      const insertAt = drag.boundary > drag.line ? drag.boundary - 1 : drag.boundary;
      if (insertAt !== drag.line) moveLine(drag.line, insertAt);
      drag = null;
    }
  }
  function moveLine(from, to) {
    const lines = textarea.value.split("\n");
    const model = bodyModel(textarea.value);
    const block = model.draggable.get(from);
    if (block == null || model.draggable.get(to) == null || model.draggable.get(to) !== block) return;
    const [moved] = lines.splice(from, 1);
    lines.splice(to, 0, moved);
    renormalizeCommas(lines, model.blockLines.get(block));
    textarea.value = lines.join("\n");
    const caret = lines.slice(0, to).join("\n").length + (to > 0 ? 1 : 0);
    textarea.setSelectionRange(caret, caret + lines[to].length);
    refresh();
    onChange();
  }
  // Among a body's draggable lines, every line but the last carries a trailing comma.
  function renormalizeCommas(lines, blockLineIdxs) {
    for (let i = 0; i < blockLineIdxs.length; i++) {
      const idx = blockLineIdxs[i];
      let line = lines[idx].replace(/,\s*$/, "");
      if (i < blockLineIdxs.length - 1) line += ",";
      lines[idx] = line;
    }
  }
  textarea.addEventListener("input", () => {
    refresh();
    onChange();
  });
  textarea.addEventListener("scroll", syncScroll);
  textarea.addEventListener("keydown", (e) => {
    if (!e.altKey || (e.key !== "ArrowUp" && e.key !== "ArrowDown")) return;
    const line = textarea.value.slice(0, textarea.selectionStart).split("\n").length - 1;
    const model = bodyModel(textarea.value);
    const block = model.draggable.get(line);
    const target = line + (e.key === "ArrowUp" ? -1 : 1);
    if (block != null && model.draggable.get(target) === block) {
      e.preventDefault();
      moveLine(line, target);
    }
  });
  new ResizeObserver(renderHandles).observe(textarea);
  refresh();
}

// Which lines are draggable column lines, and which CREATE TABLE body they belong to. A line
// qualifies when it starts inside the parens of a CREATE TABLE, is non-empty, and is neither
// the closing paren nor a comment line. String/comment/dollar-quote aware across lines.
function bodyModel(text) {
  const draggable = new Map();
  const blockLines = new Map();
  const lines = text.split("\n");
  let depth = 0;
  let mode = "code";
  let tag = "";
  let inCreate = false;
  let block = -1;
  let recent = "";
  for (let li = 0; li < lines.length; li++) {
    const startDepth = depth;
    const startedInCreate = inCreate;
    const line = lines[li];
    for (let i = 0; i < line.length; i++) {
      const c = line[i];
      if (mode === "line-comment") break;
      if (mode === "block-comment") {
        if (c === "*" && line[i + 1] === "/") { mode = "code"; i++; }
        continue;
      }
      if (mode === "string") {
        if (c === "'") { if (line[i + 1] === "'") i++; else mode = "code"; }
        continue;
      }
      if (mode === "dollar") {
        if (line.startsWith(tag, i)) { i += tag.length - 1; mode = "code"; }
        continue;
      }
      if (c === "-" && line[i + 1] === "-") { mode = "line-comment"; break; }
      if (c === "/" && line[i + 1] === "*") { mode = "block-comment"; i++; continue; }
      if (c === "'") { mode = "string"; continue; }
      if (c === "$") {
        const m = /^\$[A-Za-z_]*\$/.exec(line.slice(i));
        if (m) { tag = m[0]; mode = "dollar"; i += tag.length - 1; continue; }
      }
      if (/[A-Za-z_]/.test(c)) {
        let j = i;
        while (j < line.length && /[A-Za-z0-9_]/.test(line[j])) j++;
        recent = `${recent} ${line.slice(i, j).toLowerCase()}`.split(" ").slice(-3).join(" ").trim();
        i = j - 1;
        continue;
      }
      if (c === "(") {
        if (depth === 0 && /(^|\s)create\s+table$|create\s+table\s+\S+$/.test(recent)) {
          inCreate = true;
          block++;
          blockLines.set(block, []);
        }
        depth++;
      } else if (c === ")") {
        depth = Math.max(0, depth - 1);
        if (depth === 0) inCreate = false;
      } else if (c === ";") {
        recent = "";
        inCreate = false;
      }
    }
    if (mode === "line-comment") mode = "code";
    const trimmed = line.trim();
    if (
      startedInCreate && startDepth >= 1 && trimmed !== "" &&
      !trimmed.startsWith(")") && !trimmed.startsWith("--")
    ) {
      draggable.set(li, block);
      blockLines.get(block)?.push(li);
    }
  }
  return { draggable, blockLines };
}

function esc(s) {
  return s.replaceAll("&", "&amp;").replaceAll("<", "&lt;").replaceAll(">", "&gt;");
}

function highlight(sql) {
  let out = "";
  let i = 0;
  while (i < sql.length) {
    const rest = sql.slice(i);
    let m;
    if ((m = /^--[^\n]*/.exec(rest))) {
      out += `<span class="tk-com">${esc(m[0])}</span>`;
    } else if ((m = /^\/\*[\s\S]*?(\*\/|$)/.exec(rest))) {
      out += `<span class="tk-com">${esc(m[0])}</span>`;
    } else if ((m = /^'(?:[^']|'')*('|$)/.exec(rest))) {
      out += `<span class="tk-str">${esc(m[0])}</span>`;
    } else if ((m = /^\$([A-Za-z_]*)\$[\s\S]*?(\$\1\$|$)/.exec(rest))) {
      out += `<span class="tk-str">${esc(m[0])}</span>`;
    } else if ((m = /^[A-Za-z_][A-Za-z0-9_]*/.exec(rest))) {
      const word = m[0].toLowerCase();
      const cls = KEYWORDS.has(word) ? "tk-kw" : TYPES.has(word) ? "tk-ty" : "";
      out += cls ? `<span class="${cls}">${esc(m[0])}</span>` : esc(m[0]);
    } else if ((m = /^\d[\d._]*/.exec(rest))) {
      out += `<span class="tk-num">${esc(m[0])}</span>`;
    } else {
      m = [rest[0]];
      out += esc(m[0]);
    }
    i += m[0].length;
  }
  return out;
}
