import {byId, copy, element, request} from './core.js';

function diffLine(parent, kind, text) {
  const line = document.createElement('pre');
  line.className = kind;
  line.textContent = text;
  parent.append(line);
  return line;
}

const visible = value => JSON.stringify(value).replace(/[\s\u200b-\u200f\u2060-\u206f\ufeff]/gu,
  char => `\\u${char.codePointAt(0).toString(16).padStart(4, '0')}`);

function byteText(bytes) {
  try {
    return visible(new TextDecoder('utf-8', {fatal: true, ignoreBOM: true}).decode(new Uint8Array(bytes)));
  } catch {
    return `HEX ${bytes.map(byte => byte.toString(16).padStart(2, '0')).join(' ')}`;
  }
}

function rawSnapshot(parent, title, snapshot, testid) {
  const details = document.createElement('details');
  details.dataset.testid = testid;
  const summary = document.createElement('summary');
  summary.textContent = `${title} · ${snapshot.body.length} 字节 · ${snapshot.complete ? '完整' : '不完整'}`;
  details.append(summary);
  diffLine(details, 'diff-raw', JSON.stringify({target: snapshot.target, status: snapshot.status, headers: snapshot.headers}, null, 2));
  diffLine(details, 'diff-raw', byteText(snapshot.body));
  parent.append(details);
}

function diffParts(left, right, deadline = performance.now() + 100, anchors = true) {
  let start = 0, end = 0;
  while (start < Math.min(left.length, right.length) && left[start] === right[start]) start++;
  while (end < Math.min(left.length, right.length) - start && left.at(-end - 1) === right.at(-end - 1)) end++;
  const a = left.slice(start, left.length - end), b = right.slice(start, right.length - end);
  let middle = [];
  if (a.length || b.length) {
    middle = performance.now() < deadline && ByteDiff.diffArrays(a, b,
      {timeout: Math.max(0, deadline - performance.now()), maxEditLength: 1000});
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
    line.append(element('span', 'diff-number', `${kind === 'removed' ? '−' : kind === 'added' ? '+' : ' '} ${kind === 'added' ? newLine : oldLine}`));
    const content = element('span', 'diff-code', text);
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
  const format = value => JSON.stringify(value, (_key, item) => item && typeof item === 'object' && !Array.isArray(item)
    ? Object.fromEntries(Object.keys(item).sort().map(key => [key, item[key]])) : item, 2);
  return {changes, before: format(left), after: format(right)};
}

function byteDifference(parent, before, after) {
  let changes;
  try {
    const decoder = new TextDecoder('utf-8', {fatal: true, ignoreBOM: true});
    const tokens = bytes => decoder.decode(new Uint8Array(bytes)).match(/"(?:\\[\s\S]|[^"\\])*"|\s+|[^\s"{}\[\],:]+|[\s\S]/gu) || [];
    changes = diffParts(tokens(before), tokens(after)).map(part => ({...part, value: Array.from(new TextEncoder().encode(part.value.join('')))}));
  } catch {
    changes = diffParts(before, after);
  }
  let oldOffset = 0, newOffset = 0, changed = false;
  for (let index = 0; index < changes.length; index++) {
    const change = changes[index];
    if (change.removed || change.added) {
      changed = true;
      const line = diffLine(parent, change.removed ? 'removed' : 'added',
        `${change.removed ? '−' : '+'} 字节 ${change.removed ? oldOffset : newOffset} · ${change.value.length} 字节: `);
      const next = changes[index + 1];
      if (change.removed && next?.added) {
        const added = diffLine(parent, 'added', `+ 字节 ${newOffset} · ${next.value.length} 字节: `);
        highlightPair(line, added, byteText(change.value), byteText(next.value));
        oldOffset += change.value.length;
        newOffset += next.value.length;
        index++;
        continue;
      }
      line.append(document.createTextNode(byteText(change.value)));
    }
    if (!change.added) oldOffset += change.value.length;
    if (!change.removed) newOffset += change.value.length;
  }
  if (!changed) diffLine(parent, 'diff-empty', '正文逐字节相同');
}

function wireDifference(parent, title, before, after, testid) {
  const section = document.createElement('section');
  section.dataset.testid = testid;
  const heading = document.createElement('h3');
  heading.textContent = title;
  section.append(heading);
  const identical = before.body.length === after.body.length && before.body.every((value, index) => value === after.body[index]);
  const body = element('div', 'json-body-diff');
  const json = identical ? null : jsonDifference(before.body, after.body);
  if (identical) diffLine(body, 'diff-empty', '正文逐字节相同');
  else if (json === null) diffLine(body, 'diff-empty', '正文不是完整 JSON，请展开原始字节 DIFF 查看改动。');
  else if (!json.changes.length) diffLine(body, 'diff-empty', 'JSON 内容相同；字段顺序、序列化与空白变化见原始字节 DIFF。');
  else {
    body.append(element('p', 'diff-body-summary', `JSON 正文 DIFF · ${json.changes.length} 处字段变化 · 格式化后比较，忽略字段顺序和空白`));
    const paths = element('div', 'diff-paths');
    for (const change of json.changes) {
      const item = element('code', '', `${change.before === undefined ? '新增' : change.after === undefined ? '删除' : '修改'} ${change.path}`);
      item.dataset.jsonPath = change.path;
      paths.append(item);
    }
    body.append(paths);
    lineDifference(body, json.before, json.after);
  }
  section.append(body);
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
  rawSnapshot(content, '原始请求', exchange.request, 'wire-original-request');
  for (const [index, attempt] of exchange.attempts.entries()) {
    wireDifference(content, `上游尝试 ${index + 1} · 请求完整差异`, exchange.request, attempt.request, `wire-request-${index + 1}`);
    rawSnapshot(content, `上游尝试 ${index + 1} · 实际请求`, attempt.request, `wire-upstream-request-${index + 1}`);
    if (attempt.response) rawSnapshot(content, `上游尝试 ${index + 1} · 实际响应`, attempt.response, `wire-upstream-response-${index + 1}`);
    if (index === exchange.attempts.length - 1 && attempt.response && exchange.response)
      wireDifference(content, `上游尝试 ${index + 1} · 响应完整差异`, attempt.response, exchange.response, `wire-response-${index + 1}`);
  }
  if (exchange.response) rawSnapshot(content, '客户端响应', exchange.response, 'wire-downstream-response');
}

function renderDiff(content, diff) {
  content.replaceChildren();
  for (const [direction, title] of [['request', '请求 · Grok Build → 上游'], ['response', '响应 · 上游 → Grok Build']]) {
    const section = document.createElement('section');
    section.dataset.testid = `diff-${direction}`;
    const heading = document.createElement('h3');
    heading.textContent = title;
    section.append(heading);
    if (!diff[direction].length) diffLine(section, 'diff-empty', '未记录主动修补；完整字节差异见下方');
    for (const change of diff[direction]) {
      const item = document.createElement('div');
      item.className = 'diff-item';
      const label = document.createElement('strong');
      label.textContent = change.reason;
      const path = document.createElement('code');
      path.textContent = change.path === change.after_path || !Object.hasOwn(change, 'after')
        ? change.path : `${change.path} → ${change.after_path}`;
      item.append(label, path);
      if (change.event) {
        const event = document.createElement('span');
        event.className = 'diff-event';
        event.textContent = change.event;
        item.append(event);
      }
      if (Object.hasOwn(change, 'before') && Object.hasOwn(change, 'after')) {
        for (const key of new Set([...Object.keys(change.before), ...Object.keys(change.after)])) {
          const format = value => Object.hasOwn(value, key) ? visible(value[key]) : '（字段不存在）';
          diffLine(item, 'removed', `− ${key}: ${format(change.before)}`);
          diffLine(item, 'added', `+ ${key}: ${format(change.after)}`);
        }
      } else {
        diffLine(item, 'removed', `− ${visible(change.before)}`);
        diffLine(item, 'added', '+ （已删除）');
      }
      section.append(item);
    }
    content.append(section);
  }
}

export function createDetails() {
  const dialog = byId('diff-dialog');
  const content = byId('diff-content');
  let pending = null;
  let detail = null;
  let currentId = null;

  async function open(id) {
    pending?.abort();
    const controller = new AbortController();
    pending = controller;
    currentId = id;
    detail = null;
    byId('copy-diff').disabled = true;
    byId('retry-diff').hidden = true;
    byId('diff-meta').textContent = `请求 #${id} · 正在读取`;
    content.replaceChildren(element('p', 'empty-note', '正在读取请求与响应…'));
    content.setAttribute('aria-busy', 'true');
    if (!dialog.open) dialog.showModal();
    try {
      const result = await request(`/ui/calls/${id}`, undefined, {signal: controller.signal});
      if (controller.signal.aborted || !dialog.open) return;
      detail = result;
      byId('diff-meta').textContent = `#${id} · ${detail.call.model} · ${detail.call.result} ${detail.call.status} · ${detail.call.duration_ms} ms`;
      renderDiff(content, detail.diff);
      renderExchange(content, detail.exchange);
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
  dialog.addEventListener('close', () => {
    pending?.abort();
    pending = null;
    detail = null;
    currentId = null;
    content.replaceChildren();
    content.setAttribute('aria-busy', 'false');
    byId('copy-diff').disabled = true;
  });
  byId('copy-diff').addEventListener('click', event => { if (detail) copy(JSON.stringify(detail, null, 2), event.currentTarget); });
  byId('retry-diff').addEventListener('click', () => { if (currentId !== null) open(currentId); });
  return {open};
}
