import {byId, copy, element, laneName, request} from './core.js';

function diffLine(parent, kind, text, tinted = false) {
  const line = document.createElement('pre');
  line.className = kind;
  tinted ? tintJSON(line, text) : line.textContent = text;
  parent.append(line);
  return line;
}

const visible = value => JSON.stringify(value).replace(/[\s\u200b-\u200f\u2060-\u206f\ufeff]/gu,
  char => `\\u${char.codePointAt(0).toString(16).padStart(4, '0')}`);

/* 旧记录的事件标签可能带悬挂分隔符（如 "SSE #49 · "）。 */
const eventLabel = event => typeof event === 'string' ? event.replace(/\s*·\s*$/, '').trim() : null;

function byteText(bytes) {
  try {
    return visible(new TextDecoder('utf-8', {fatal: true, ignoreBOM: true}).decode(new Uint8Array(bytes)));
  } catch {
    return `HEX ${bytes.map(byte => byte.toString(16).padStart(2, '0')).join(' ')}`;
  }
}

/* 给 JSON 行上语法色:键、字符串、数字、布尔与 null。全部 textContent 构建。 */
const JSON_TOKEN = /("(?:\\.|[^"\\])*")(\s*:)?|\b(true|false|null)\b|(-?\d+(?:\.\d+)?(?:[eE][+-]?\d+)?)/g;
function tintJSON(parent, text) {
  let last = 0, match;
  JSON_TOKEN.lastIndex = 0;
  while ((match = JSON_TOKEN.exec(text))) {
    if (match.index > last) parent.append(document.createTextNode(text.slice(last, match.index)));
    if (match[1] !== undefined) {
      if (match[2]) {
        parent.append(element('span', 'tk-k', match[1]));
        parent.append(document.createTextNode(match[2]));
      } else parent.append(element('span', 'tk-s', match[1]));
    } else if (match[3] !== undefined) parent.append(element('span', 'tk-f', match[3]));
    else parent.append(element('span', 'tk-n', match[4]));
    last = JSON_TOKEN.lastIndex;
  }
  if (last < text.length) parent.append(document.createTextNode(text.slice(last)));
}

function rawSnapshot(parent, title, short, snapshot, testid) {
  const details = document.createElement('details');
  details.dataset.testid = testid;
  details.dataset.anchorLabel = short;
  const summary = document.createElement('summary');
  summary.textContent = `${title} · ${snapshot.body.length} 字节 · ${snapshot.complete ? '完整' : '不完整'}`;
  details.append(summary);
  diffLine(details, 'diff-raw', JSON.stringify({target: snapshot.target, status: snapshot.status, headers: snapshot.headers}, null, 2), true);
  diffLine(details, 'diff-raw', byteText(snapshot.body));
  parent.append(details);
}

function diffParts(left, right, deadline = performance.now() + 100, anchors = true, maxEditLength = 1000) {
  let start = 0, end = 0;
  while (start < Math.min(left.length, right.length) && left[start] === right[start]) start++;
  while (end < Math.min(left.length, right.length) - start && left.at(-end - 1) === right.at(-end - 1)) end++;
  const a = left.slice(start, left.length - end), b = right.slice(start, right.length - end);
  let middle = [];
  if (a.length || b.length) {
    middle = performance.now() < deadline && ByteDiff.diffArrays(a, b,
      {timeout: Math.max(0, deadline - performance.now()), maxEditLength});
    if (!middle && anchors) {
      const unique = values => {
        const positions = new Map();
        values.forEach((value, index) => positions.set(value, positions.has(value) ? -1 : index));
        return positions;
      };
      const old = unique(a), next = unique(b), pairs = [];
      for (const [value, index] of old) {
        if (index >= 0 && next.get(value) >= 0) pairs.push([index, next.get(value)]);
      }
      const tails = [], previous = [];
      pairs.forEach((pair, index) => {
        let lo = 0, hi = tails.length;
        while (lo < hi) {
          const mid = (lo + hi) >> 1;
          if (pairs[tails[mid]][1] < pair[1]) lo = mid + 1;
          else hi = mid;
        }
        previous[index] = tails[lo - 1] ?? -1;
        tails[lo] = index;
      });
      const matches = [];
      for (let index = tails.at(-1) ?? -1; index >= 0; index = previous[index]) matches.unshift(pairs[index]);
      if (matches.length) {
        middle = [];
        let oldIndex = 0, newIndex = 0;
        for (const [oldMatch, newMatch] of [...matches, [a.length, b.length]]) {
          middle.push(...diffParts(a.slice(oldIndex, oldMatch), b.slice(newIndex, newMatch), deadline, false));
          if (oldMatch < a.length) middle.push({value: [a[oldMatch]]});
          oldIndex = oldMatch + 1;
          newIndex = newMatch + 1;
        }
      }
    }
    // A bounded comparison still returns a real replacement, retaining every token.
    if (!middle) middle = [...(a.length ? [{removed: true, value: a}] : []), ...(b.length ? [{added: true, value: b}] : [])];
  }
  return [...(start ? [{value: left.slice(0, start)}] : []), ...middle,
    ...(end ? [{value: left.slice(left.length - end)}] : [])];
}

function highlightPair(removed, added, before, after) {
  const words = text => text.match(/[\p{L}\p{N}_]+|\s+|[^\p{L}\p{N}_\s]/gu) || [];
  for (const part of diffParts(words(before), words(after))) {
    const text = part.value.join('');
    if (!part.added) removed.append(part.removed ? element('mark', '', text) : document.createTextNode(text));
    if (!part.removed) added.append(part.added ? element('mark', '', text) : document.createTextNode(text));
  }
}

function lineDifference(parent, before, after) {
  const parts = diffParts(before.split('\n'), after.split('\n'));
  const patch = element('div', 'diff-patch');
  let oldLine = 1, newLine = 1;
  function row(kind, text) {
    const line = element('pre', `diff-row ${kind}`);
    if (kind !== 'added') line.dataset.oldLine = oldLine;
    if (kind !== 'removed') line.dataset.newLine = newLine;
    line.append(element('span', 'diff-num', kind === 'added' ? '' : oldLine),
      element('span', 'diff-num', kind === 'removed' ? '' : newLine));
    const content = element('span', 'diff-code', '');
    if (kind === 'diff-context') tintJSON(content, text);
    else content.textContent = text;
    line.append(content);
    patch.append(line);
    return content;
  }
  for (let index = 0; index < parts.length; index++) {
    const part = parts[index], next = parts[index + 1];
    if (part.removed && part.value.length === 1 && next?.added && next.value.length === 1) {
      const removed = row('removed');
      oldLine++;
      const added = row('added');
      newLine++;
      highlightPair(removed, added, part.value[0], next.value[0]);
      index++;
    } else if (part.removed || part.added) {
      for (const text of part.value) {
        row(part.removed ? 'removed' : 'added', text);
        if (part.removed) oldLine++;
        else newLine++;
      }
    } else {
      const leading = index > 0, trailing = index < parts.length - 1;
      let skipped = 0;
      for (const [position, text] of part.value.entries()) {
        if ((leading && position < 4) || (trailing && position >= part.value.length - 4)) {
          if (skipped) patch.append(element('div', 'diff-gap', `··· ${skipped} 行未变`));
          skipped = 0;
          row('diff-context', text);
        } else skipped++;
        oldLine++;
        newLine++;
      }
      if (skipped) patch.append(element('div', 'diff-gap', `··· ${skipped} 行未变`));
    }
  }
  parent.append(patch);
}

/* 按值格式化:对象键排序、两空格缩进,忽略字段顺序与空白。 */
const formatValue = value => JSON.stringify(value, (_key, item) => item && typeof item === 'object' && !Array.isArray(item)
  ? Object.fromEntries(Object.keys(item).sort().map(key => [key, item[key]])) : item, 2);

const formatPatchValue = value => {
  if (value === undefined) return '（字段不存在）';
  if (value === null || typeof value !== 'object') return JSON.stringify(value);
  return formatValue(value);
};

function jsonDifference(before, after) {
  let left, right;
  try {
    const decoder = new TextDecoder('utf-8', {fatal: true, ignoreBOM: true});
    left = JSON.parse(decoder.decode(new Uint8Array(before)));
    right = JSON.parse(decoder.decode(new Uint8Array(after)));
  } catch {
    return null;
  }
  const changes = [];
  function walk(a, b, path) {
    if (a === b) return;
    if (a && b && typeof a === 'object' && typeof b === 'object' && Array.isArray(a) === Array.isArray(b)) {
      for (const key of new Set([...Object.keys(a), ...Object.keys(b)])) {
        walk(Object.hasOwn(a, key) ? a[key] : undefined, Object.hasOwn(b, key) ? b[key] : undefined,
          `${path}/${key.replace(/~/g, '~0').replace(/\//g, '~1')}`);
      }
    } else changes.push({path: path || '/', before: a, after: b});
  }
  walk(left, right, '');
  return {changes, before: formatValue(left), after: formatValue(right)};
}

function byteDifference(parent, before, after) {
  let text = null;
  try {
    const decoder = new TextDecoder('utf-8', {fatal: true, ignoreBOM: true});
    text = {before: decoder.decode(new Uint8Array(before)), after: decoder.decode(new Uint8Array(after))};
  } catch {}
  // 多行正文（SSE、换行分隔）按行比较：每行带换行符，逐字节可重建；单行 JSON 仍按词元比较。
  const changes = text && (text.before.includes('\n') || text.after.includes('\n'))
    ? diffParts(text.before.match(/[^\n]*\n|[^\n]+/g) || [], text.after.match(/[^\n]*\n|[^\n]+/g) || [],
        performance.now() + 500, true, 1e6)
        .map(part => ({removed: part.removed, added: part.added, lines: part.value}))
    : text
      ? diffParts(text.before.match(/"(?:\\[\s\S]|[^"\\])*"|\s+|[^\s"{}\[\],:]+|[\s\S]/gu) || [],
          text.after.match(/"(?:\\[\s\S]|[^"\\])*"|\s+|[^\s"{}\[\],:]+|[\s\S]/gu) || [])
        .map(part => ({removed: part.removed, added: part.added, tokens: part.value}))
      : diffParts(before, after)
        .map(part => ({removed: part.removed, added: part.added, bytes: part.value}));
  const encoder = new TextEncoder(), byteSize = value => encoder.encode(value).length;
  const items = change => change.lines ? change.lines.map(line => ({text: line, size: byteSize(line)}))
    : change.tokens ? [{text: change.tokens.join(''), size: byteSize(change.tokens.join(''))}]
    : [{bytes: change.bytes, size: change.bytes.length}];
  const payload = item => item.bytes ? byteText(item.bytes) : visible(item.text);
  let oldOffset = 0, newOffset = 0, changed = false;
  const row = (kind, item, offset) => diffLine(parent, kind,
    `${kind === 'removed' ? '−' : '+'} 字节 ${offset} · ${item.size} 字节: `);
  for (let index = 0; index < changes.length; index++) {
    const change = changes[index];
    if (!change.removed && !change.added) {
      for (const item of items(change)) { oldOffset += item.size; newOffset += item.size; }
      if (change.lines) diffLine(parent, 'diff-gap', `··· ${change.lines.length} 行未变`);
      continue;
    }
    changed = true;
    const next = changes[index + 1], left = items(change), right = next?.added ? items(next) : null;
    if (change.removed && right) {
      /* 删除与新增必须一一配对（多余侧补零长行），差异对话框的重建校验依赖配对完整。 */
      const paired = Math.min(left.length, right.length);
      for (let i = 0; i < paired; i++) {
        const removed = row('removed', left[i], oldOffset);
        oldOffset += left[i].size;
        const added = row('added', right[i], newOffset);
        newOffset += right[i].size;
        highlightPair(removed, added, payload(left[i]), payload(right[i]));
      }
      for (const item of left.slice(paired)) {
        row('removed', item, oldOffset).append(document.createTextNode(payload(item)));
        oldOffset += item.size;
        row('added', {size: 0}, newOffset).append(document.createTextNode('""'));
      }
      for (const item of right.slice(paired)) {
        row('removed', {size: 0}, oldOffset).append(document.createTextNode('""'));
        row('added', item, newOffset).append(document.createTextNode(payload(item)));
        newOffset += item.size;
      }
      index++;
      continue;
    }
    for (const item of left) {
      const kind = change.removed ? 'removed' : 'added';
      row(kind, item, change.removed ? oldOffset : newOffset).append(document.createTextNode(payload(item)));
      if (change.removed) { oldOffset += item.size; row('added', {size: 0}, newOffset).append(document.createTextNode('""')); }
      else { newOffset += item.size; row('removed', {size: 0}, oldOffset).append(document.createTextNode('""')); }
    }
  }
  if (!changed) diffLine(parent, 'diff-empty', '正文逐字节相同');
}

function wireDifference(parent, title, short, before, after, testid) {
  const section = document.createElement('section');
  section.dataset.testid = testid;
  section.dataset.anchorLabel = short;
  const heading = document.createElement('h3');
  heading.textContent = title;
  section.append(heading);
  const identical = before.body.length === after.body.length && before.body.every((value, index) => value === after.body[index]);
  const body = element('div', 'json-body-diff');
  let focuses = null;
  const json = identical ? null : jsonDifference(before.body, after.body);
  if (identical) diffLine(body, 'diff-empty', '正文逐字节相同');
  else if (json === null) diffLine(body, 'diff-empty', '正文不是完整 JSON，请展开原始字节 DIFF 查看改动。');
  else if (!json.changes.length) diffLine(body, 'diff-empty', 'JSON 内容相同；字段顺序、序列化与空白变化见原始字节 DIFF。');
  else {
    body.append(element('p', 'diff-body-summary', `JSON 正文 DIFF · ${json.changes.length} 处字段变化 · 格式化后比较，忽略字段顺序和空白`));
    const paths = element('div', 'diff-paths');
    focuses = element('div', 'diff-focuses');
    for (const change of json.changes) {
      const kind = change.before === undefined ? '新增' : change.after === undefined ? '删除' : '修改';
      const chip = element('button', 'diff-path', `${kind} ${change.path}`);
      chip.type = 'button';
      chip.dataset.jsonPath = change.path;
      const box = element('div', 'diff-focus');
      box.hidden = true;
      if (change.before !== undefined) diffLine(box, 'removed', `− ${formatValue(change.before)}`, true);
      if (change.after !== undefined) diffLine(box, 'added', `+ ${formatValue(change.after)}`, true);
      chip.addEventListener('click', () => {
        box.hidden = !box.hidden;
        chip.classList.toggle('open', !box.hidden);
        if (!box.hidden) box.scrollIntoView({block: 'nearest'});
      });
      paths.append(chip);
      focuses.append(box);
    }
    body.append(paths);
    lineDifference(body, json.before, json.after);
  }
  section.append(body);
  if (focuses) section.append(focuses);
  const metadata = snapshot => {
    const values = {target: snapshot.target, status: snapshot.status, complete: snapshot.complete};
    for (const [name, value] of snapshot.headers) (values[`header/${name}`] ||= []).push(value);
    return values;
  };
  const left = metadata(before), right = metadata(after);
  for (const key of new Set([...Object.keys(left), ...Object.keys(right)])) {
    if (JSON.stringify(left[key]) === JSON.stringify(right[key])) continue;
    diffLine(section, 'removed', `− ${key}: ${Object.hasOwn(left, key) ? visible(left[key]) : '（不存在）'}`);
    diffLine(section, 'added', `+ ${key}: ${Object.hasOwn(right, key) ? visible(right[key]) : '（不存在）'}`);
  }
  const bytes = element('details', 'byte-diff');
  bytes.append(element('summary', '', `原始字节 DIFF（含字段顺序与空白） · ${before.body.length} → ${after.body.length} 字节`));
  let rendered = false;
  bytes.addEventListener('toggle', () => {
    if (!bytes.open || rendered) return;
    rendered = true;
    byteDifference(bytes, before.body, after.body);
  });
  section.append(bytes);
  parent.append(section);
}

function renderExchange(content, exchange) {
  if (exchange.error) diffLine(content, 'removed', `错误：${exchange.error}`);
  rawSnapshot(content, '原始请求', '原始请求', exchange.request, 'wire-original-request');
  for (const [index, attempt] of exchange.attempts.entries()) {
    const n = index + 1;
    wireDifference(content, `上游尝试 ${n} · 请求完整差异`, `请求差异 ${n}`, exchange.request, attempt.request, `wire-request-${n}`);
    rawSnapshot(content, `上游尝试 ${n} · 实际请求`, `上游请求 ${n}`, attempt.request, `wire-upstream-request-${n}`);
    if (attempt.response) rawSnapshot(content, `上游尝试 ${n} · 实际响应`, `上游响应 ${n}`, attempt.response, `wire-upstream-response-${n}`);
    if (index === exchange.attempts.length - 1 && attempt.response && exchange.response)
      wireDifference(content, `上游尝试 ${n} · 响应完整差异`, `响应差异 ${n}`, attempt.response, exchange.response, `wire-response-${n}`);
  }
  if (exchange.response) rawSnapshot(content, '客户端响应', '客户端响应', exchange.response, 'wire-downstream-response');
}

function renderDiff(content, diff) {
  content.replaceChildren();
  for (const [direction, title, short] of [['request', '请求 · Grok Build → 上游', '请求修补'],
    ['response', '响应 · 上游 → Grok Build', '响应修补']]) {
    const section = document.createElement('section');
    section.dataset.testid = `diff-${direction}`;
    section.dataset.anchorLabel = short;
    const heading = document.createElement('h3');
    heading.textContent = title;
    section.append(heading);
    if (!diff[direction].length) diffLine(section, 'diff-empty', '未记录主动修补；完整字节差异见下方');
    /* 只记路径的修补（如思维链逐帧改名）按原因与路径聚合成一张可展开的卡片。 */
    const patchless = change => !Object.hasOwn(change, 'before') && !Object.hasOwn(change, 'after');
    const groupKey = change => `${change.reason}\n${change.path}\n${Object.hasOwn(change, 'after_path') ? change.after_path : ''}`;
    const changes = diff[direction];
    for (let index = 0; index < changes.length; index++) {
      const change = changes[index];
      let count = 1;
      if (patchless(change)) {
        while (index + count < changes.length && patchless(changes[index + count])
          && groupKey(changes[index + count]) === groupKey(change)) count++;
      }
      const item = document.createElement('div');
      item.className = 'diff-item';
      const label = document.createElement('strong');
      label.textContent = change.reason;
      const path = document.createElement('code');
      path.textContent = change.path === change.after_path || !Object.hasOwn(change, 'after')
        ? change.path : `${change.path} → ${change.after_path}`;
      item.append(label, path);
      if (count > 1) item.append(element('span', 'diff-count', `共 ${count} 处 · 逐帧见下`));
      const events = changes.slice(index, index + count).map(patched => eventLabel(patched.event));
      if (count > 1 && events.some(Boolean)) {
        const frames = document.createElement('details');
        frames.className = 'diff-events';
        frames.append(element('summary', '', `${events.find(Boolean)} – ${events.findLast(Boolean)}`));
        for (const event of events) if (event) frames.append(element('code', '', event));
        item.append(frames);
      } else if (events[0]) {
        const event = document.createElement('span');
        event.className = 'diff-event';
        event.textContent = events[0];
        item.append(event);
      }
      if (count === 1 && Object.hasOwn(change, 'before') && Object.hasOwn(change, 'after')) {
        const isPlainObj = val => val !== null && typeof val === 'object' && !Array.isArray(val);
        if (isPlainObj(change.before) && isPlainObj(change.after)) {
          for (const key of new Set([...Object.keys(change.before), ...Object.keys(change.after)])) {
            const hasBefore = Object.hasOwn(change.before, key);
            const hasAfter = Object.hasOwn(change.after, key);
            if (hasBefore && hasAfter && JSON.stringify(change.before[key]) === JSON.stringify(change.after[key])) {
              continue;
            }
            diffLine(item, 'removed', `− ${key}: ${hasBefore ? formatPatchValue(change.before[key]) : '（字段不存在）'}`, true);
            diffLine(item, 'added', `+ ${key}: ${hasAfter ? formatPatchValue(change.after[key]) : '（已删除）'}`, true);
          }
        } else if (JSON.stringify(change.before) !== JSON.stringify(change.after)) {
          if (change.before !== null && change.before !== undefined) diffLine(item, 'removed', `− ${formatPatchValue(change.before)}`, true);
          if (change.after !== null && change.after !== undefined) diffLine(item, 'added', `+ ${formatPatchValue(change.after)}`, true);
        }
      } else if (count === 1 && (Object.hasOwn(change, 'before') || Object.hasOwn(change, 'after'))) {
        if (Object.hasOwn(change, 'before') && change.before !== null && change.before !== undefined) {
          diffLine(item, 'removed', `− ${formatPatchValue(change.before)}`, true);
        }
        if (Object.hasOwn(change, 'after') && change.after !== null && change.after !== undefined) {
          diffLine(item, 'added', `+ ${formatPatchValue(change.after)}`, true);
        }
      }
      section.append(item);
      index += count - 1;
    }
    content.append(section);
  }
}

function buildAnchors(content) {
  const nav = byId('diff-nav');
  nav.replaceChildren();
  for (const section of content.querySelectorAll('[data-anchor-label]')) {
    const chip = element('button', 'diff-anchor', section.dataset.anchorLabel);
    chip.type = 'button';
    chip.addEventListener('click', () => section.scrollIntoView({
      block: 'start', behavior: matchMedia('(prefers-reduced-motion: reduce)').matches ? 'auto' : 'smooth'}));
    nav.append(chip);
  }
  nav.hidden = !nav.children.length;
}

export function createDetails(neighbors = () => ({prev: null, next: null})) {
  const dialog = byId('diff-dialog');
  const content = byId('diff-content');
  let pending = null;
  let detail = null;
  let currentId = null;

  function refreshSteps() {
    const around = currentId === null ? {prev: null, next: null} : neighbors(currentId);
    byId('diff-prev').disabled = around.prev == null;
    byId('diff-next').disabled = around.next == null;
  }

  function step(delta) {
    if (currentId === null || !dialog.open) return;
    const around = neighbors(currentId);
    const target = delta < 0 ? around.prev : around.next;
    if (target != null) open(target);
  }

  async function open(id) {
    pending?.abort();
    const controller = new AbortController();
    pending = controller;
    currentId = id;
    detail = null;
    refreshSteps();
    byId('copy-diff').disabled = true;
    byId('retry-diff').hidden = true;
    byId('diff-nav').replaceChildren();
    byId('diff-nav').hidden = true;
    byId('diff-meta').textContent = `请求 #${id} · 正在读取`;
    content.replaceChildren(...[0, 1, 2, 3].map(width => {
      const bar = element('div', 'diff-skeleton');
      bar.style.width = `${width % 2 ? 58 : 82 - width * 6}%`;
      return bar;
    }));
    content.setAttribute('aria-busy', 'true');
    if (!dialog.open) dialog.showModal();
    try {
      const result = await request(`/ui/calls/${id}`, undefined, {signal: controller.signal});
      if (controller.signal.aborted || !dialog.open) return;
      detail = result;
      const around = neighbors(id);
      const at = Object.hasOwn(around, 'at') ? ` · ${around.at}/${around.total}` : '';
      const lane = detail.call.bypass ? ` · 旁路·${laneName(detail.call.bypass)}` : '';
      byId('diff-meta').textContent = `#${id} · ${detail.call.model} · ${detail.call.result} ${detail.call.status} · ${detail.call.duration_ms} ms${at}${lane}`;
      renderDiff(content, detail.diff);
      renderExchange(content, detail.exchange);
      buildAnchors(content);
      refreshSteps();
      byId('copy-diff').disabled = false;
      content.scrollTop = 0;
    } catch (error) {
      if (!controller.signal.aborted && dialog.open) {
        content.replaceChildren(element('p', 'field-error', error.message));
        byId('retry-diff').hidden = false;
      }
    } finally {
      if (pending === controller) {
        pending = null;
        content.setAttribute('aria-busy', 'false');
      }
    }
  }

  byId('close-diff').addEventListener('click', () => dialog.close());
  byId('diff-prev').addEventListener('click', () => step(-1));
  byId('diff-next').addEventListener('click', () => step(1));
  dialog.addEventListener('keydown', event => {
    if (event.key === 'ArrowLeft') { event.preventDefault(); step(-1); }
    if (event.key === 'ArrowRight') { event.preventDefault(); step(1); }
  });
  dialog.addEventListener('close', () => {
    pending?.abort();
    pending = null;
    detail = null;
    currentId = null;
    content.replaceChildren();
    content.setAttribute('aria-busy', 'false');
    byId('copy-diff').disabled = true;
    refreshSteps();
  });
  byId('copy-diff').addEventListener('click', event => { if (detail) copy(JSON.stringify(detail, null, 2), event.currentTarget); });
  byId('retry-diff').addEventListener('click', () => { if (currentId !== null) open(currentId); });
  return {open};
}
