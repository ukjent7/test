import {byId, request, showError} from './core.js';
import {createGateway} from './gateway.js';
import {createActivity} from './activity.js';
import {createUsage} from './usage.js';

let status = null;
let pending = null;
const usage = createUsage();
const activity = createActivity();
const gateway = createGateway({getStatus: () => status, refresh});
const refreshButton = byId('refresh-status');

async function refresh() {
  pending?.abort();
  const controller = new AbortController();
  pending = controller;
  refreshButton.disabled = true;
  refreshButton.setAttribute('aria-busy', 'true');
  try {
    const next = await request('/ui/status', undefined, {signal: controller.signal});
    if (controller.signal.aborted) return;
    status = next;
    gateway.render(status, true);
    activity.render(status.calls);
    byId('version').textContent = `v${status.version}`;
    byId('connection').textContent = status.enabled ? '网关已连接' : '服务已连接 · 转发已停止';
    byId('footer-dot').classList.add('connected');
    byId('last-updated').textContent = new Date().toLocaleTimeString('zh-CN', {hour12: false});
    showError('connection-error');
    showError('history-error', status.history_error || '');
    usage.refresh();
  } catch (error) {
    if (controller.signal.aborted) return;
    gateway.render(status, false);
    byId('connection').textContent = '无法连接网关';
    byId('footer-dot').classList.remove('connected');
    showError('connection-error', '无法连接本地网关。请确认程序仍在运行，然后刷新状态。');
  } finally {
    if (pending === controller) {
      pending = null;
      refreshButton.disabled = false;
      refreshButton.setAttribute('aria-busy', 'false');
    }
  }
}

const tabs = [...document.querySelectorAll('[role="tab"]')];
const scroller = document.querySelector('main');
const scrollPositions = new Map();
function activate(tab) {
  const previous = tabs.find(other => other.getAttribute('aria-selected') === 'true');
  if (previous) scrollPositions.set(previous.id, scroller.scrollTop);
  for (const other of tabs) {
    const selected = other === tab;
    other.setAttribute('aria-selected', String(selected));
    other.tabIndex = selected ? 0 : -1;
    byId(other.getAttribute('aria-controls')).hidden = !selected;
  }
  usage.activate(tab.getAttribute('aria-controls') === 'usage');
  scroller.scrollTop = scrollPositions.get(tab.id) || 0;
}
for (const [index, tab] of tabs.entries()) {
  tab.addEventListener('click', () => activate(tab));
  tab.addEventListener('keydown', event => {
    const next = {ArrowLeft: (index + tabs.length - 1) % tabs.length, ArrowRight: (index + 1) % tabs.length, Home: 0, End: tabs.length - 1}[event.key];
    if (next === undefined) return;
    event.preventDefault();
    activate(tabs[next]);
    tabs[next].focus();
  });
}
byId('theme').addEventListener('click', () => {
  const theme = document.documentElement.dataset.theme === 'dark' ? 'light' : 'dark';
  document.documentElement.dataset.theme = theme;
  byId('theme').setAttribute('aria-pressed', String(theme === 'dark'));
  try { localStorage.setItem('theme', theme); } catch { /* The theme remains usable without storage. */ }
});
byId('theme').setAttribute('aria-pressed', String(document.documentElement.dataset.theme === 'dark'));
refreshButton.addEventListener('click', refresh);
document.addEventListener('visibilitychange', () => { if (!document.hidden && !pending) refresh(); });

async function poll() {
  if (!document.hidden && !pending) await refresh();
  setTimeout(poll, 1000);
}
poll();
