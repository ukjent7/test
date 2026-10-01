import {byId, copy, notify, request, showError} from './core.js';
import {createSettings} from './settings.js';

export function createGateway({getStatus, refresh}) {
  let connected = false;
  let toggling = false;
  let models = [];
  let selectedModel = '';
  let catalogLoaded = false;
  let modelRequest = null;
  let protocolChosen = false;

  function connectionConfig() {
    const status = getStatus();
    if (!status) return;
    const model = byId('model-select').value || selectedModel || 'stepfun/step-5-preview';
    if (!protocolChosen) byId('client-protocol').value = model.startsWith('opencode/') ? 'chat_completions' : 'messages';
    byId('config-example').textContent = `[model.gateway]\nmodel = ${JSON.stringify(model)}\nbase_url = ${JSON.stringify(status.endpoint)}\napi_key = ""\napi_backend = ${JSON.stringify(byId('client-protocol').value)}`;
  }

  function renderModels() {
    const query = byId('model-filter').value.trim().toLowerCase();
    const visible = models.filter(model => model.id.toLowerCase().includes(query));
    const select = byId('model-select');
    select.replaceChildren(...visible.map(model => new Option(model.id, model.id)));
    if (visible.some(model => model.id === selectedModel)) select.value = selectedModel;
    if (!visible.length) select.add(new Option(catalogLoaded ? '没有符合条件的模型' : '拉取模型后选择', ''));
    select.disabled = !visible.length || !!modelRequest;
    byId('copy-model').disabled = !visible.length || !!modelRequest;
    byId('model-count').textContent = catalogLoaded ? `${visible.length} / ${models.length} 个模型` : '尚未拉取';
    connectionConfig();
  }

  function invalidateModels() {
    modelRequest?.abort();
    modelRequest = null;
    models = [];
    selectedModel = '';
    catalogLoaded = false;
    byId('model-filter').value = '';
    byId('model-loading').hidden = true;
    byId('fetch-models').disabled = !connected;
    showError('model-error');
    renderModels();
  }

  createSettings({getStatus, refresh, onSaved: invalidateModels});
  byId('model-filter').addEventListener('input', renderModels);
  byId('model-select').addEventListener('change', () => {
    selectedModel = byId('model-select').value;
    connectionConfig();
  });
  byId('client-protocol').addEventListener('change', () => {
    protocolChosen = true;
    connectionConfig();
  });
  byId('copy-endpoint').addEventListener('click', () => { if (getStatus()) copy(getStatus().endpoint); });
  byId('copy-config').addEventListener('click', () => copy(byId('config-example').textContent));
  byId('copy-model').addEventListener('click', () => copy(byId('model-select').value));
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
        byId('fetch-models').disabled = !connected;
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

  return {
    render(status, online) {
      connected = online;
      byId('state').textContent = online ? (status.enabled ? '运行中' : '已停止') : '连接已断开';
      byId('state').classList.toggle('running', online && status.enabled);
      byId('service-card').dataset.state = online ? (status.enabled ? 'running' : 'stopped') : 'offline';
      byId('service-description').textContent = online
        ? (status.enabled ? '新请求将按模型前缀转发至对应上游。' : '已暂停新请求，正在进行的请求继续完成。')
        : status ? '显示上次同步的数据，恢复连接后可继续操作。' : '服务尚未连接，请确认程序状态后刷新。';
      byId('copy-endpoint').disabled = !status;
      byId('copy-config').disabled = !status;
      byId('enabled').disabled = !online || toggling;
      for (const id of ['edit-upstream', 'edit-opencode', 'edit-proxy']) byId(id).disabled = !online;
      byId('fetch-models').disabled = !online || !!modelRequest;
      if (!status) return;
      byId('enabled').setAttribute('aria-checked', String(status.enabled));
      byId('local-address').textContent = status.endpoint.replace(/\/v1$/, '');
      byId('endpoint').textContent = status.endpoint;
      for (const [id, value] of [['upstream', status.upstream_base_url], ['opencode-upstream', status.opencode_base_url]]) {
        byId(id).textContent = value;
        byId(id).title = value;
      }
      for (const provider of ['stepfun', 'opencode']) {
        byId(`${provider}-key-state`).textContent = status[`${provider}_key_configured`] ? '密钥已设置' : '使用客户端密钥';
        byId(`${provider}-proxy-state`).textContent = status[`${provider}_use_proxy`] ? '跟随代理设置' : '直连';
      }
      const proxy = status.proxy === 'direct' ? '直连' : status.proxy ? '自定义代理' : '系统代理';
      byId('proxy-status').textContent = `${proxy} · StepFun ${status.stepfun_use_proxy ? '跟随设置' : '直连'} · Zen ${status.opencode_use_proxy ? '跟随设置' : '直连'}`;
      byId('proxy-status').title = byId('proxy-status').textContent;
      for (const field of ['requests', 'active', 'repairs', 'errors']) byId(field).textContent = status.stats[field].toLocaleString('zh-CN');
      connectionConfig();
    },
  };
}
