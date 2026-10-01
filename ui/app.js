'use strict';

const byId = id => document.getElementById(id);
const dialog = byId('settings-dialog');
const toggle = byId('enabled');
let current = null;
let busy = false;
let toastTimer;
let callsKey = '';
let diffRequest = 0;
let diffDetail = null;

async function request(path, body) {
  const response = await fetch(path, body === undefined ? {} : {
    method: 'POST', headers: {'Content-Type': 'application/json'}, body: JSON.stringify(body),
  });
  if (!response.ok) {
    const error = await response.json().catch(() => null);
    throw new Error(error?.error?.message || `请求失败（${response.status}）`);
  }
  return response.status === 204 ? null : response.json();
}

function notify(text) {
  clearTimeout(toastTimer);
  byId('toast').textContent = text;
  byId('toast').hidden = false;
  toastTimer = setTimeout(() => byId('toast').hidden = true, 2800);
}

async function copy(text) {
  try {
    await navigator.clipboard.writeText(text);
    notify('已复制');
  } catch {
    notify('复制失败，请手动选择并复制内容');
  }
}

function renderCalls(calls) {
  const filter = byId('call-filter').value.trim().toLowerCase();
  const errors = byId('errors-only').checked;
  const key = JSON.stringify([calls, filter, errors]);
  if (callsKey === key) return;
  callsKey = key;
  const list = byId('activity-list');
  list.replaceChildren();
  const selected = calls.filter(call => (!errors || call.result === '失败' || call.status >= 400)
    && (!filter || [String(call.id), call.model, String(call.status), call.result].some(value => value.toLowerCase().includes(filter))));
  if (!selected.length) diffLine(list, 'diff-empty', '没有符合条件的请求');
  for (const call of selected) {
    const row = document.createElement('div');
    row.className = 'call';
    row.innerHTML = '<div><div class="model"></div><div class="time"></div></div><span class="outcome"></span><span class="duration"></span><span class="repair"></span>';
    row.querySelector('.model').textContent = call.model;
    row.querySelector('.time').textContent = `#${call.id} · ${new Date(call.timestamp).toLocaleString('zh-CN', {hour12:false})}`;
    row.querySelector('.outcome').textContent = `${call.result} ${call.status}`;
    row.querySelector('.outcome').classList.toggle('failed', call.result === '失败');
    row.querySelector('.duration').textContent = `${call.duration_ms} ms`;
    row.querySelector('.repair').textContent = call.repairs ? `修补 ${call.repairs}` : '—';
    const usage = document.createElement('div');
    usage.className = 'cache-usage';
    usage.dataset.testid = 'cache-usage';
    const {input_tokens: input, cache_read_tokens: read, cache_write_tokens: write} = call.cache;
    const tokens = value => value === null ? '未报告' : value.toLocaleString('zh-CN');
    const hit = read === null ? '缓存命中未报告' : input > 0
      ? `缓存命中 ${Math.round(read / input * 100)}%` : `缓存读取 ${tokens(read)}`;
    usage.textContent = `${hit} · 输入 ${tokens(input)} · 读取 ${tokens(read)} · 写入 ${tokens(write)}`;
    const routing = document.createElement('div');
    routing.className = 'routing-identity';
    routing.dataset.testid = 'routing-identity';
    routing.textContent = call.routing ? `会话 ${call.routing.fingerprint} · ${call.routing.source}` : '路由身份未提供';
    row.firstElementChild.append(usage, routing);
    const button = document.createElement('button');
    button.className = 'text-button diff-button';
    button.textContent = `查看详情与差异 · 修补 ${call.request_changes} / ${call.response_changes}`;
    button.setAttribute('aria-label', `查看差异 #${call.id}`);
    button.addEventListener('click', () => showDiff(call.id));
    row.firstElementChild.append(button);
    list.append(row);
  }
}

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

function wireDifference(parent, title, before, after, testid) {
  const section = document.createElement('section');
  section.dataset.testid = testid;
  const heading = document.createElement('h3');
  heading.textContent = title;
  section.append(heading);
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
  const changes = ByteDiff.diffArrays(before.body, after.body, {timeout: 1000});
  if (!changes) {
    diffLine(section, 'diff-empty', '精确差异计算超时；以下显示完整前后正文，未截断。');
    diffLine(section, 'removed', `− ${byteText(before.body)}`);
    diffLine(section, 'added', `+ ${byteText(after.body)}`);
  } else {
    let oldOffset = 0, newOffset = 0, changed = false;
    for (const change of changes) {
      if (change.removed || change.added) {
        changed = true;
        diffLine(section, change.removed ? 'removed' : 'added',
          `${change.removed ? '−' : '+'} 字节 ${change.removed ? oldOffset : newOffset} · ${change.count} 字节: ${byteText(change.value)}`);
      }
      if (!change.added) oldOffset += change.count;
      if (!change.removed) newOffset += change.count;
    }
    if (!changed) diffLine(section, 'diff-empty', '正文逐字节相同');
  }
  parent.append(section);
}

function renderExchange(exchange) {
  const content = byId('diff-content');
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

function renderDiff(diff) {
  const content = byId('diff-content');
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

async function showDiff(id) {
  const sequence = ++diffRequest;
  diffDetail = null;
  byId('copy-diff').disabled = true;
  byId('diff-meta').textContent = `请求 #${id}`;
  byId('diff-content').textContent = '正在读取差异…';
  byId('diff-dialog').showModal();
  try {
    const detail = await request(`/ui/calls/${id}`);
    if (sequence !== diffRequest || !byId('diff-dialog').open) return;
    diffDetail = detail;
    byId('diff-meta').textContent = `#${id} · ${detail.call.model} · ${detail.call.result} ${detail.call.status}`;
    renderDiff(detail.diff);
    renderExchange(detail.exchange);
    byId('copy-diff').disabled = false;
  } catch (error) {
    if (sequence === diffRequest && byId('diff-dialog').open) byId('diff-content').textContent = error.message;
  }
}

byId('close-diff').addEventListener('click', () => byId('diff-dialog').close());
byId('diff-dialog').addEventListener('close', () => ++diffRequest);
byId('copy-diff').addEventListener('click', () => diffDetail && copy(JSON.stringify(diffDetail, null, 2)));

async function refresh() {
  try {
    current = await request('/ui/status');
    byId('state').textContent = current.enabled ? '运行中' : '已停止';
    byId('state').classList.toggle('running', current.enabled);
    toggle.setAttribute('aria-checked', String(current.enabled));
    toggle.disabled = busy;
    byId('local-address').textContent = current.endpoint.replace(/\/v1$/, '');
    byId('endpoint').textContent = current.endpoint;
    byId('upstream').textContent = current.upstream_base_url;
    byId('upstream').title = current.upstream_base_url;
    byId('opencode-upstream').textContent = current.opencode_base_url;
    byId('opencode-upstream').title = current.opencode_base_url;
    byId('config-example').textContent = `base_url = "${current.endpoint}"\napi_backend = "messages"`;
    for (const field of ['requests', 'active', 'repairs', 'errors']) byId(field).textContent = current.stats[field];
    byId('version').textContent = `v${current.version}`;
    byId('connection').textContent = current.enabled ? '网关已连接' : '服务已连接 · 转发已停止';
    byId('footer-dot').classList.add('connected');
    renderCalls(current.calls);
    byId('history-error').hidden = !current.history_error;
    byId('history-error').textContent = current.history_error || '';
  } catch {
    byId('state').textContent = '连接已断开';
    byId('state').classList.remove('running');
    byId('connection').textContent = '无法连接网关';
    byId('footer-dot').classList.remove('connected');
    toggle.disabled = true;
  }
}

byId('call-filter').addEventListener('input', () => current && renderCalls(current.calls));
byId('errors-only').addEventListener('change', () => current && renderCalls(current.calls));

for (const tab of document.querySelectorAll('[role="tab"]')) {
  tab.addEventListener('click', () => {
    for (const other of document.querySelectorAll('[role="tab"]')) {
      other.setAttribute('aria-selected', String(other === tab));
      byId(other.getAttribute('aria-controls')).hidden = other !== tab;
    }
  });
}

byId('theme').addEventListener('click', () => {
  const theme = document.documentElement.dataset.theme === 'dark' ? 'light' : 'dark';
  document.documentElement.dataset.theme = theme;
  localStorage.setItem('theme', theme);
});
byId('copy-endpoint').addEventListener('click', () => current && copy(current.endpoint));
byId('copy-config').addEventListener('click', () => copy(byId('config-example').textContent));

byId('edit-upstream').addEventListener('click', () => {
  if (!current) return;
  byId('upstream-input').value = current.upstream_base_url;
  byId('opencode-input').value = current.opencode_base_url;
  for (const [id, configured] of [['stepfun-key', current.stepfun_key_configured], ['opencode-key', current.opencode_key_configured]]) {
    byId(id).value = '';
    byId(id).placeholder = configured ? '已设置，留空保留' : '未设置';
  }
  byId('settings-error').hidden = true;
  dialog.showModal();
});
byId('close-settings').addEventListener('click', () => dialog.close());
byId('cancel-settings').addEventListener('click', () => dialog.close());
byId('settings-form').addEventListener('submit', async event => {
  event.preventDefault();
  byId('save-settings').disabled = true;
  byId('settings-error').hidden = true;
  try {
    const settings = {upstream_base_url: byId('upstream-input').value, opencode_base_url: byId('opencode-input').value};
    for (const [id, key] of [['stepfun-key', 'stepfun_api_key'], ['opencode-key', 'opencode_api_key']]) {
      if (byId(id).value.trim()) settings[key] = byId(id).value.trim();
    }
    await request('/ui/settings', settings);
    await refresh();
    dialog.close();
    notify('上游设置已保存');
  } catch (error) {
    byId('settings-error').textContent = error.message;
    byId('settings-error').hidden = false;
  } finally {
    byId('save-settings').disabled = false;
  }
});

byId('fetch-models').addEventListener('click', async () => {
  byId('fetch-models').disabled = true;
  byId('model-error').hidden = true;
  byId('model-select').disabled = true;
  byId('copy-model').disabled = true;
  byId('model-select').replaceChildren();
  try {
    const result = await request('/v1/models');
    for (const model of result.data) byId('model-select').add(new Option(model.id, model.id));
    byId('model-select').disabled = !result.data.length;
    byId('copy-model').disabled = !result.data.length;
    if (result.upstream_errors.length) throw new Error(result.upstream_errors.join('；'));
    notify(`已拉取 ${result.data.length} 个模型`);
  } catch (error) {
    byId('model-error').textContent = error.message;
    byId('model-error').hidden = false;
  } finally {
    byId('fetch-models').disabled = false;
  }
});
byId('copy-model').addEventListener('click', () => copy(byId('model-select').value));

toggle.addEventListener('click', async () => {
  if (!current || busy) return;
  busy = true;
  toggle.disabled = true;
  try {
    await request('/ui/enabled', {enabled: !current.enabled});
  } catch (error) {
    notify(error.message);
  } finally {
    busy = false;
    await refresh();
  }
});

// Schedule after completion: slow connections never accumulate overlapping polls.
async function poll() {
  await refresh();
  setTimeout(poll, 1000);
}
poll();
