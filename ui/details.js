import {byId, copy, element, request} from './core.js';

function diffLine(parent, kind, text) {
  const line = document.createElement('pre');
  line.className = kind;
  line.textContent = text;
  parent.append(line);
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
  return changes;
}

function byteDifference(parent, before, after) {
  const changes = ByteDiff.diffArrays(before, after, {timeout: 1000});
  if (!changes) {
    diffLine(parent, 'diff-empty', '字节差异计算超时。完整数据可在原始/实际正文中展开，或使用“复制差异”导出；上方 JSON 字段差异仍有效。');
    return;
  }
  let oldOffset = 0, newOffset = 0, changed = false;
  for (const change of changes) {
    if (change.removed || change.added) {
      changed = true;
      diffLine(parent, change.removed ? 'removed' : 'added',
        `${change.removed ? '−' : '+'} 字节 ${change.removed ? oldOffset : newOffset} · ${change.count} 字节: ${byteText(change.value)}`);
    }
    if (!change.added) oldOffset += change.count;
    if (!change.removed) newOffset += change.count;
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
  const changes = identical ? [] : jsonDifference(before.body, after.body);
  if (identical) diffLine(body, 'diff-empty', '正文逐字节相同');
  else if (changes === null) diffLine(body, 'diff-empty', '正文不是完整 JSON，请展开字节差异查看改动。');
  else if (!changes.length) diffLine(body, 'diff-empty', 'JSON 内容相同；字段顺序、序列化与空白变化见字节差异。');
  else {
    body.append(element('p', 'diff-body-summary', `JSON 字段变化 · ${changes.length} 处`));
    for (const change of changes) {
      const item = element('div', 'diff-item');
      item.dataset.jsonPath = change.path;
      item.append(element('code', '', change.path));
      const format = value => value === undefined ? '（字段不存在）' : JSON.stringify(value, null, 2);
      diffLine(item, 'removed', `− ${format(change.before)}`);
      diffLine(item, 'added', `+ ${format(change.after)}`);
      body.append(item);
    }
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
  bytes.append(element('summary', '', `字节差异（含字段顺序与空白） · ${before.body.length} → ${after.body.length} 字节`));
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
