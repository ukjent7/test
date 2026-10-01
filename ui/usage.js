import {byId, count, element, providerName, request, showError} from './core.js';

function fields(row) {
  const reported = (value, reports) => `${count(value)}${reports > 0 && reports < row.requests ? `（${reports}/${row.requests} 条报告）` : ''}`;
  return [
    ['请求', count(row.requests)],
    ['总 Token', row.input_tokens == null && row.output_tokens == null ? '未报告'
      : `${count((row.input_tokens || 0) + (row.output_tokens || 0))}${row.input_reported_requests < row.requests || row.output_reported_requests < row.requests ? '（部分报告）' : ''}`],
    ['输入', reported(row.input_tokens, row.input_reported_requests)],
    ['输出', reported(row.output_tokens, row.output_reported_requests)],
    ['缓存读取', reported(row.cache_read_tokens, row.cache_reported_requests)],
    ['缓存写入', reported(row.cache_write_tokens, row.cache_write_reported_requests)],
    ['缓存命中率', row.cache_hit_rate == null ? (row.cache_reported_requests ? '—（输入为零或未报告）' : '未报告')
      : `${row.cache_hit_rate.toFixed(1)}%${row.cache_reported_requests < row.requests ? `（${row.cache_reported_requests}/${row.requests} 条报告）` : ''}`],
  ];
}

function render(data) {
  byId('usage-total').replaceChildren(...fields(data.total).map(([label, value]) => {
    const item = element('div', 'usage-metric');
    item.append(element('span', '', label), element('strong', '', value));
    return item;
  }));
  for (const [id, rows, models] of [['usage-providers', data.providers, false], ['usage-models', data.models, true]]) {
    const list = byId(id);
    if (!rows.length) {
      list.replaceChildren(element('p', 'empty-note', '此时间范围内没有请求'));
      continue;
    }
    const table = element('table');
    const header = table.createTHead().insertRow();
    for (const label of [models ? '供应商 / 模型' : '供应商', ...fields(data.total).map(([label]) => label)]) {
      const cell = element('th', '', label);
      cell.scope = 'col';
      header.append(cell);
    }
    const body = table.createTBody();
    for (const row of rows) {
      const line = body.insertRow();
      for (const value of [models ? `${providerName(row.provider)} / ${row.model}` : providerName(row.provider), ...fields(row).map(([, value]) => value)]) {
        const cell = line.insertCell();
        const note = cell.cellIndex ? value.indexOf('（') : -1;
        cell.textContent = note < 0 ? value : value.slice(0, note);
        if (note >= 0) cell.append(element('small', '', value.slice(note)));
      }
    }
    list.replaceChildren(table);
  }
}

export function createUsage() {
  let period = '30d';
  let active = false;
  let pending = false;
  let sequence = 0;
  let renderedKey = '';
  const periods = [...document.querySelectorAll('[data-period]')];

  async function refresh(force = false) {
    if (!active || (pending && !force)) return;
    const revision = ++sequence;
    const selectedPeriod = period;
    const since = new Date();
    since.setHours(0, 0, 0, 0);
    since.setDate(since.getDate() - ({today: 0, '7d': 6, '30d': 29}[selectedPeriod] || 0));
    pending = true;
    byId('usage').setAttribute('aria-busy', 'true');
    byId('usage-loading').hidden = false;
    showError('usage-error');
    try {
      const data = await request(`/ui/usage?since=${selectedPeriod === 'all' ? 0 : since.getTime()}`);
      if (revision !== sequence || !active) return;
      const key = JSON.stringify([selectedPeriod, data]);
      if (key !== renderedKey) {
        render(data);
        renderedKey = key;
      }
      byId('usage-updated').textContent = `更新于 ${new Date().toLocaleTimeString('zh-CN', {hour12: false})}`;
    } catch (error) {
      if (revision === sequence && active) showError('usage-error', error.message);
    } finally {
      if (revision === sequence) {
        pending = false;
        byId('usage').setAttribute('aria-busy', 'false');
        byId('usage-loading').hidden = true;
      }
    }
  }

  for (const button of periods) button.addEventListener('click', () => {
    period = button.dataset.period;
    for (const other of periods) other.setAttribute('aria-pressed', String(other === button));
    refresh(true);
  });
  byId('retry-usage').addEventListener('click', () => refresh(true));
  return {
    refresh,
    activate(value) {
      active = value;
      if (active) refresh(true);
      else {
        ++sequence;
        pending = false;
        byId('usage').setAttribute('aria-busy', 'false');
        byId('usage-loading').hidden = true;
      }
    },
  };
}
