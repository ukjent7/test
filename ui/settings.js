import {byId, notify, request, showError} from './core.js';

export function createSettings({getStatus, refresh, onSaved}) {
  const dialog = byId('settings-dialog');
  const form = byId('settings-form');
  let saving = false;
  let scope = 'stepfun';
  let opener;

  function proxyModeChanged() {
    const custom = byId('proxy-mode').value === 'custom';
    byId('proxy-address-field').hidden = !custom;
    byId('proxy-address').required = scope === 'network' && custom;
  }

  function open(kind, focus, trigger) {
    const status = getStatus();
    if (!status || saving) return;
    scope = kind;
    opener = byId(trigger);
    for (const name of ['stepfun', 'opencode', 'network']) {
      byId(`${name}-fields`).hidden = name !== scope;
    }
    byId('upstream-input').required = scope === 'stepfun';
    byId('opencode-input').required = scope === 'opencode';
    byId('settings-title').textContent = {stepfun: 'StepFun', opencode: 'OpenCode Zen', network: '网络代理'}[scope];
    byId('settings-hint').textContent = scope === 'network'
      ? '系统模式读取系统代理或 HTTP_PROXY / HTTPS_PROXY 等环境设置。'
      : `${scope === 'opencode' ? 'Zen 匿名访问可填 public；' : ''}配置上游密钥后，客户端无需提供密钥。`;
    byId('settings-description').textContent = scope === 'network'
      ? '设置连接方式，并选择需要通过代理的上游。'
      : '保存后立即用于新请求。密钥留空保留已有值。';
    form.reset();
    byId('upstream-input').value = status.upstream_base_url;
    byId('opencode-input').value = status.opencode_base_url;
    byId('proxy-mode').value = status.proxy === 'direct' ? 'direct' : status.proxy ? 'custom' : 'system';
    byId('proxy-address').value = ['', 'direct'].includes(status.proxy) ? '' : status.proxy;
    byId('stepfun-use-proxy').checked = status.stepfun_use_proxy;
    byId('opencode-use-proxy').checked = status.opencode_use_proxy;
    for (const [id, configured] of [['stepfun-key', status.stepfun_key_configured], ['opencode-key', status.opencode_key_configured]]) {
      byId(id).placeholder = configured ? '已设置，留空保留' : '未设置';
    }
    proxyModeChanged();
    showError('settings-error');
    dialog.showModal();
    byId(focus).focus();
  }

  byId('edit-upstream').addEventListener('click', () => open('stepfun', 'upstream-input', 'edit-upstream'));
  byId('edit-opencode').addEventListener('click', () => open('opencode', 'opencode-input', 'edit-opencode'));
  byId('edit-proxy').addEventListener('click', () => open('network', 'proxy-mode', 'edit-proxy'));
  byId('proxy-mode').addEventListener('change', proxyModeChanged);
  for (const id of ['close-settings', 'cancel-settings']) {
    byId(id).addEventListener('click', () => { if (!saving) dialog.close(); });
  }
  dialog.addEventListener('cancel', event => { if (saving) event.preventDefault(); });
  dialog.addEventListener('close', () => {
    form.reset();
    byId('stepfun-key').value = '';
    byId('opencode-key').value = '';
    showError('settings-error');
    opener?.focus();
  });
  form.addEventListener('submit', async event => {
    event.preventDefault();
    if (saving) return;
    const settings = {
      upstream_base_url: scope === 'stepfun' ? byId('upstream-input').value.trim() : getStatus().upstream_base_url,
    };
    if (scope === 'network') {
      settings.proxy = byId('proxy-mode').value === 'system' ? '' : byId('proxy-mode').value === 'direct' ? 'direct' : byId('proxy-address').value.trim();
      for (const provider of ['stepfun', 'opencode']) settings[`${provider}_use_proxy`] = byId(`${provider}-use-proxy`).checked;
    } else {
      if (scope === 'opencode') settings.opencode_base_url = byId('opencode-input').value.trim();
      if (byId(`${scope}-key`).value.trim()) settings[`${scope}_api_key`] = byId(`${scope}-key`).value.trim();
    }
    saving = true;
    form.setAttribute('aria-busy', 'true');
    byId('save-progress').hidden = false;
    for (const field of form.querySelectorAll('fieldset, button')) field.disabled = true;
    showError('settings-error');
    try {
      await request('/ui/settings', settings);
      if (scope !== 'network') onSaved();
      dialog.close();
      notify('设置已保存');
      await refresh();
    } catch (error) {
      showError('settings-error', error.message);
    } finally {
      saving = false;
      form.setAttribute('aria-busy', 'false');
      byId('save-progress').hidden = true;
      for (const field of form.querySelectorAll('fieldset, button')) field.disabled = false;
    }
  });
}
