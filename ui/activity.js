import {byId, count, element} from './core.js';
import {createDetails} from './details.js';

function callRow(call) {
  const row = byId('call-template').content.firstElementChild.cloneNode(true);
  row.dataset.callId = call.id;
  row.querySelector('.model').textContent = call.model;
  row.querySelector('.model').title = call.model;
  row.querySelector('.time').textContent = `#${call.id} · ${new Date(call.timestamp).toLocaleString('zh-CN', {hour12: false})}`;
  row.querySelector('.outcome').textContent = `${call.result} ${call.status}`;
  row.querySelector('.outcome').dataset.result = call.result === '失败' || call.status >= 400 ? 'failed' : call.result === '取消' ? 'cancelled' : 'success';
  row.querySelector('.duration').textContent = `${count(call.duration_ms)} ms`;
  row.querySelector('.repair').textContent = call.repairs ? `修补 ${call.repairs}` : '原生转发';
  const {input_tokens: input, cache_read_tokens: read, cache_write_tokens: write} = call.cache;
  const hit = read == null ? '缓存命中未报告' : input > 0
    ? `缓存命中 ${Math.round(read / input * 100)}%` : `缓存读取 ${count(read)}`;
  row.querySelector('.cache-usage').textContent = `${hit} · 输入 ${count(input)} · 读取 ${count(read)} · 写入 ${count(write)}`;
  row.querySelector('.routing-identity').textContent = call.routing ? `会话 ${call.routing.fingerprint} · ${call.routing.source}` : '路由身份未提供';
  const button = row.querySelector('.diff-button');
  button.dataset.callId = call.id;
  button.textContent = `查看详情与差异 · 修补 ${call.request_changes} / ${call.response_changes}`;
  button.setAttribute('aria-label', `查看差异 #${call.id}`);
  return row;
}

export function createActivity() {
  const details = createDetails();
  const list = byId('activity-list');
  const rows = new Map();
  let calls = [];
  let renderedKey = '';

  function render() {
    const filter = byId('call-filter').value.trim().toLowerCase();
    const errors = byId('errors-only').checked;
    const key = JSON.stringify([calls, filter, errors]);
    if (key === renderedKey) return;
    renderedKey = key;
    const selected = calls.filter(call => (!errors || call.result === '失败' || call.status >= 400)
      && (!filter || [String(call.id), call.model, String(call.status), call.result].some(value => value.toLowerCase().includes(filter))));
    byId('call-count').textContent = `${selected.length} / ${calls.length} 条`;
    byId('clear-filter').hidden = !filter && !errors;
    if (!selected.length) {
      const empty = element('div', 'empty');
      empty.append(element('span', 'empty-icon', '↔'), element('strong', '', calls.length ? '没有符合条件的请求' : '还没有请求'),
        element('p', '', calls.length ? '调整筛选条件，或清除筛选查看全部记录。' : '从客户端发送一条消息，即可查看请求详情。'));
      list.replaceChildren(empty);
    } else {
      for (const child of [...list.children]) if (!child.classList.contains('call')) child.remove();
      for (const [index, call] of selected.entries()) {
        let row = rows.get(call.id);
        if (!row) {
          row = callRow(call);
          rows.set(call.id, row);
        }
        if (list.children[index] !== row) list.insertBefore(row, list.children[index] || null);
      }
      const ids = new Set(selected.map(call => String(call.id)));
      for (const row of [...list.children]) if (!ids.has(row.dataset.callId)) row.remove();
    }
    const retained = new Set(calls.map(call => call.id));
    for (const id of rows.keys()) if (!retained.has(id)) rows.delete(id);
  }

  list.addEventListener('click', event => {
    const button = event.target.closest('button[data-call-id]');
    if (button) details.open(Number(button.dataset.callId));
  });
  byId('call-filter').addEventListener('input', render);
  byId('errors-only').addEventListener('change', render);
  byId('clear-filter').addEventListener('click', () => {
    byId('call-filter').value = '';
    byId('errors-only').checked = false;
    render();
    byId('call-filter').focus();
  });
  return {render(value) { calls = value; render(); }};
}
