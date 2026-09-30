'use strict';

const byId = id => document.getElementById(id);
const dialog = byId('settings-dialog');
const toggle = byId('enabled');
let current = null;
let busy = false;
let toastTimer;
let callsKey = '';

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
    notify('复制失败，请手动选择并复制地址');
  }
}

function renderCalls(calls) {
  const key = JSON.stringify(calls);
  if (callsKey === key) return;
  callsKey = key;
  if (!calls.length) return;
  const list = byId('activity-list');
  list.replaceChildren();
  for (const call of calls) {
    const row = document.createElement('div');
    row.className = 'call';
    row.innerHTML = '<div><div class="model"></div><div class="time"></div></div><span class="outcome"></span><span class="duration"></span><span class="repair"></span>';
    row.querySelector('.model').textContent = call.model;
    row.querySelector('.time').textContent = new Date(call.timestamp).toLocaleTimeString('zh-CN', {hour12:false});
    row.querySelector('.outcome').textContent = `${call.result} ${call.status}`;
    row.querySelector('.outcome').classList.toggle('failed', call.result === '失败');
    row.querySelector('.duration').textContent = `${call.duration_ms} ms`;
    row.querySelector('.repair').textContent = call.repairs ? `修补 ${call.repairs}` : '—';
    list.append(row);
  }
}

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
    byId('config-example').textContent = `base_url = "${current.endpoint}"\napi_backend = "messages"`;
    for (const field of ['requests', 'active', 'repairs', 'errors']) byId(field).textContent = current.stats[field];
    byId('version').textContent = `v${current.version}`;
    byId('connection').textContent = current.enabled ? '网关已连接' : '服务已连接 · 转发已停止';
    byId('footer-dot').classList.add('connected');
    renderCalls(current.calls);
  } catch {
    byId('state').textContent = '连接已断开';
    byId('state').classList.remove('running');
    byId('connection').textContent = '无法连接网关';
    byId('footer-dot').classList.remove('connected');
    toggle.disabled = true;
  }
}

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
    await request('/ui/settings', {upstream_base_url: byId('upstream-input').value});
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
