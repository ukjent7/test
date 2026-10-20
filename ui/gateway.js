import {byId, copy, element, notify, providerName, request, showError} from './core.js';
import {createSettings} from './settings.js';

export function createGateway({getStatus, refresh}) {
  let connected = false;
  let toggling = false;
  let models = [];
  let selectedModel = '';
  let catalogLoaded = false;
  let modelRequest = null;
  let protocolChosen = false;
  const search = byId('model-filter');
  try {
    selectedModel = localStorage.getItem('gateway.model') || '';
    const protocol = localStorage.getItem('gateway.protocol');
    if (['messages', 'chat_completions', 'responses'].includes(protocol)) {
      byId('client-protocol').value = protocol;
      protocolChosen = true;
    }
  } catch { /* Preferences are optional. */ }

  function remember(key, value) {
    try { localStorage.setItem(`gateway.${key}`, value); } catch { /* Keep the live selection. */ }
  }

  function choose(model) {
    selectedModel = model;
    remember('model', model);
    renderModels();
  }

  let lastConfig = '';
  function connectionConfig() {
    const status = getStatus();
    if (!status) return;
    const model = selectedModel || 'stepfun/step-5-preview';
    const select = byId('client-protocol');
    if (!protocolChosen) {
      const derived = model.startsWith('lunaroute/')
        ? 'responses'
        : model.startsWith('opencode/')
        ? 'chat_completions'
        : 'messages';
      // 用户正操作下拉框时不抢值。
      if (document.activeElement !== select && select.value !== derived) select.value = derived;
    }
    const config = `[model.gateway]\nmodel = ${JSON.stringify(model)}\nbase_url = ${JSON.stringify(status.endpoint)}\napi_key = ""\napi_backend = ${JSON.stringify(select.value)}`;
    if (config !== lastConfig) {
      lastConfig = config;
      byId('config-example').textContent = config;
    }
  }

  function renderModels() {
    const query = search.value.trim().toLowerCase();
    const visible = models.filter(model => model.id.toLowerCase().includes(query));
    const list = byId('model-list');
    list.replaceChildren();
    let group = '';
    for (const model of visible) {
      const provider = model.id.split('/')[0];
      if (provider !== group) {
        list.append(element('div', 'model-group', providerName(provider)));
        group = provider;
      }
      const option = element('div', 'model-item');
      option.setAttribute('role', 'option');
      option.setAttribute('aria-label', model.id);
      option.setAttribute('aria-selected', String(model.id === selectedModel));
      option.tabIndex = 0;
      option.dataset.model = model.id;
      option.append(element('span', 'model-name', model.id));
      const copyButton = element('button', 'model-copy');
      copyButton.type = 'button';
      copyButton.setAttribute('aria-label', `复制 ${model.id}`);
      copyButton.title = '复制模型 ID';
      copyButton.addEventListener('click', event => {
        event.stopPropagation();
        copy(model.id, event.currentTarget);
      });
      option.append(copyButton);
      option.addEventListener('click', () => choose(model.id));
      option.addEventListener('keydown', event => {
        if (event.key === 'Enter' || event.key === ' ') {
          event.preventDefault();
          choose(model.id);
        }
      });
      list.append(option);
    }
    if (!visible.length) {
      list.append(element('p', 'empty-note', !catalogLoaded
        ? '尚未拉取模型，点击上方「拉取模型」获取上游可用列表。'
        : models.length ? '没有符合条件的模型。' : '上游未返回可用模型。'));
    }
    search.hidden = !catalogLoaded;
    byId('selected-model').textContent = selectedModel || '尚未选择';
    byId('selected-model').title = selectedModel;
    byId('copy-model').disabled = !models.some(model => model.id === selectedModel) || !!modelRequest;
    byId('fetch-models').disabled = !connected || !!modelRequest;
    byId('model-count').textContent = catalogLoaded ? `${visible.length} / ${models.length} 个模型` : '尚未拉取';
    connectionConfig();
  }

  function invalidateModels() {
    modelRequest?.abort();
    modelRequest = null;
    models = [];
    catalogLoaded = false;
    search.value = '';
    byId('model-loading').hidden = true;
    showError('model-error');
    renderModels();
  }

  createSettings({getStatus, refresh, onSaved: invalidateModels});
  byId('model-filter').addEventListener('input', renderModels);
  byId('client-protocol').addEventListener('change', () => {
    protocolChosen = true;
    remember('protocol', byId('client-protocol').value);
    connectionConfig();
  });
  byId('copy-endpoint').addEventListener('click', event => { if (getStatus()) copy(getStatus().endpoint, event.currentTarget); });
  byId('copy-config').addEventListener('click', event => copy(byId('config-example').textContent, event.currentTarget));
  byId('copy-model').addEventListener('click', event => copy(selectedModel, event.currentTarget));
  byId('fetch-models').addEventListener('click', async () => {
    if (!connected || modelRequest) return;
    const controller = new AbortController();
    modelRequest = controller;
    byId('fetch-models').disabled = true;
    byId('model-loading').hidden = false;
    showError('model-error');
    renderModels();
    try {
      const result = await request('/v1/models', undefined, {signal: controller.signal, timeout: 65000});
      if (controller.signal.aborted) return;
      models = result.data;
      if (models.length && !models.some(model => model.id === selectedModel)) {
        selectedModel = models[0].id;
        remember('model', selectedModel);
      }
      catalogLoaded = true;
      const warnings = result.upstream_errors.join('；');
      if (warnings) showError('model-error', warnings);
      else notify(models.length ? `已拉取 ${models.length} 个模型` : '上游未返回可用模型');
    } catch (error) {
      if (!controller.signal.aborted) showError('model-error', error.message);
    } finally {
      if (modelRequest === controller) {
        modelRequest = null;
        byId('model-loading').hidden = true;
        renderModels();
      }
    }
  });

  byId('enabled').addEventListener('click', async () => {
    const status = getStatus();
    if (!connected || !status || toggling) return;
    toggling = true;
    byId('enabled').disabled = true;
    byId('enabled').setAttribute('aria-busy', 'true');
    try {
      await request('/ui/enabled', {enabled: !status.enabled});
    } catch (error) {
      notify(error.message);
    } finally {
      toggling = false;
      byId('enabled').setAttribute('aria-busy', 'false');
      await refresh();
    }
  });

  renderModels();
  // 每秒轮询都会调 render:值没变的文本不重写,免得浏览器白做排版。
  const painted = new Map();
  function paint(id, value) {
    if (painted.get(id) !== value) byId(id).textContent = value;
  }
  return {
    render(status, online) {
      connected = online;
      paint('state', online ? (status?.enabled ? '运行中' : '已停止') : '连接已断开');
      byId('state').classList.toggle('running', online && status?.enabled);
      byId('service-card').dataset.state = online ? (status?.enabled ? 'running' : 'stopped') : 'offline';
      paint('service-description', online
        ? (status?.enabled ? '新请求将按模型前缀转发至对应上游。' : '已暂停新请求，正在进行的请求继续完成。')
        : status ? '显示上次同步的数据，恢复连接后可继续操作。' : '服务尚未连接，请确认程序状态后刷新。');
      byId('copy-endpoint').disabled = !status;
      byId('copy-config').disabled = !status;
      byId('enabled').disabled = !online || toggling;
      for (const id of ['edit-upstream', 'edit-opencode', 'edit-lunaroute', 'edit-proxy']) byId(id).disabled = !online;
      byId('fetch-models').disabled = !online || !!modelRequest;
      if (!status) return;
      byId('enabled').setAttribute('aria-checked', String(status.enabled));
      paint('endpoint', status.endpoint);
      for (const [id, value] of [
        ['upstream', status.upstream_base_url],
        ['opencode-upstream', status.opencode_base_url],
        ['lunaroute-upstream', status.lunaroute_base_url],
      ]) {
        paint(id, value);
        byId(id).title = value;
      }
      for (const provider of ['stepfun', 'opencode', 'lunaroute']) {
        paint(`${provider}-key-state`, status[`${provider}_key_configured`] ? '密钥已设置' : '使用客户端密钥');
        paint(`${provider}-proxy-state`, status[`${provider}_use_proxy`] ? '跟随代理设置' : '直连');
      }
      const proxy = status.proxy === 'direct' ? '直连' : status.proxy ? '自定义代理' : '系统代理';
      paint('proxy-status', `${proxy} · StepFun ${status.stepfun_use_proxy ? '跟随设置' : '直连'} · Zen ${status.opencode_use_proxy ? '跟随设置' : '直连'} · LunaRoute ${status.lunaroute_use_proxy ? '跟随设置' : '直连'}`);
      byId('proxy-status').title = byId('proxy-status').textContent;
      for (const field of ['requests', 'active', 'repairs', 'errors']) paint(field, status.stats[field].toLocaleString('zh-CN'));
      connectionConfig();
    },
  };
}
