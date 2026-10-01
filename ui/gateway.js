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
  const trigger = byId('model-trigger');
  const picker = byId('model-picker');
  const search = byId('model-filter');
  let highlighted = -1;
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

  function closePicker(restoreFocus = false) {
    picker.hidden = true;
    trigger.setAttribute('aria-expanded', 'false');
    search.removeAttribute('aria-activedescendant');
    if (restoreFocus) trigger.focus();
  }

  function choose(model) {
    selectedModel = model;
    remember('model', model);
    closePicker(true);
    renderModels();
  }

  function highlight(index) {
    const options = [...byId('model-list').querySelectorAll('[role="option"]')];
    highlighted = options.length ? (index + options.length) % options.length : -1;
    options.forEach((option, position) => option.classList.toggle('highlighted', position === highlighted));
    if (highlighted < 0) search.removeAttribute('aria-activedescendant');
    else {
      search.setAttribute('aria-activedescendant', options[highlighted].id);
      options[highlighted].scrollIntoView({block: 'nearest'});
    }
  }

  function openPicker() {
    if (trigger.disabled) return;
    search.value = '';
    picker.hidden = false;
    trigger.setAttribute('aria-expanded', 'true');
    const box = trigger.getBoundingClientRect();
    const width = Math.min(Math.max(box.width, 340), innerWidth - 24);
    const below = innerHeight - box.bottom - 40;
    const above = box.top - 12;
    const placeBelow = below >= 200 || below >= above;
    const height = Math.min(380, placeBelow ? below : above);
    picker.style.width = `${width}px`;
    picker.style.maxHeight = `${height}px`;
    picker.style.left = `${Math.max(12, Math.min(box.left, innerWidth - width - 12))}px`;
    picker.style.top = `${placeBelow ? box.bottom + 5 : Math.max(12, box.top - height - 5)}px`;
    renderModels();
    search.focus();
  }
  trigger.addEventListener('click', () => picker.hidden ? openPicker() : closePicker());
  trigger.addEventListener('keydown', event => {
    if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
      event.preventDefault();
      openPicker();
      highlight(event.key === 'ArrowDown' ? 0 : -1);
    }
  });
  search.addEventListener('keydown', event => {
    const options = [...byId('model-list').querySelectorAll('[role="option"]')];
    if (['ArrowDown', 'ArrowUp', 'Home', 'End', 'Enter', 'Escape'].includes(event.key)) event.preventDefault();
    if (event.key === 'ArrowDown') highlight(highlighted + 1);
    if (event.key === 'ArrowUp') highlight(highlighted - 1);
    if (event.key === 'Home') highlight(0);
    if (event.key === 'End') highlight(options.length - 1);
    if (event.key === 'Enter' && options[highlighted]) choose(options[highlighted].dataset.model);
    if (event.key === 'Escape') closePicker(true);
  });
  picker.addEventListener('keydown', event => {
    if (event.key === 'Escape') { event.preventDefault(); closePicker(true); }
  });
  document.addEventListener('pointerdown', event => {
    if (!picker.hidden && !picker.contains(event.target) && !trigger.contains(event.target)) closePicker();
  });
  document.addEventListener('focusin', event => {
    if (!picker.hidden && !picker.contains(event.target) && event.target !== trigger) closePicker();
  });
  window.addEventListener('resize', () => closePicker());
  document.querySelector('main').addEventListener('scroll', () => closePicker());

  function connectionConfig() {
    const status = getStatus();
    if (!status) return;
    const model = selectedModel || 'stepfun/step-5-preview';
    if (!protocolChosen) byId('client-protocol').value = model.startsWith('opencode/') ? 'chat_completions' : 'messages';
    byId('config-example').textContent = `[model.gateway]\nmodel = ${JSON.stringify(model)}\nbase_url = ${JSON.stringify(status.endpoint)}\napi_key = ""\napi_backend = ${JSON.stringify(byId('client-protocol').value)}`;
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
      const option = element('button', '', model.id);
      option.type = 'button';
      option.id = `model-option-${models.indexOf(model)}`;
      option.dataset.model = model.id;
      option.setAttribute('role', 'option');
      option.setAttribute('aria-selected', String(model.id === selectedModel));
      option.addEventListener('click', () => choose(model.id));
      list.append(option);
    }
    if (!visible.length) list.append(element('p', 'empty-note', '没有符合条件的模型'));
    highlighted = -1;
    search.removeAttribute('aria-activedescendant');
    trigger.textContent = selectedModel || (catalogLoaded ? '没有可用模型' : '拉取模型后选择');
    trigger.title = selectedModel;
    trigger.disabled = !models.length || !!modelRequest;
    byId('copy-model').disabled = !models.some(model => model.id === selectedModel) || !!modelRequest;
    byId('model-count').textContent = catalogLoaded ? `${visible.length} / ${models.length} 个模型` : '尚未拉取';
    connectionConfig();
  }

  function invalidateModels() {
    modelRequest?.abort();
    modelRequest = null;
    closePicker();
    models = [];
    catalogLoaded = false;
    byId('model-filter').value = '';
    byId('model-loading').hidden = true;
    byId('fetch-models').disabled = !connected;
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

  renderModels();
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
