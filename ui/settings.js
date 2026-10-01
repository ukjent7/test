import {byId, notify, request, showError} from './core.js';

export function createSettings({getStatus, refresh, onSaved}) {
  const dialog = byId('settings-dialog');
  const form = byId('settings-form');
  let saving = false;

  function proxyModeChanged() {
    const custom = byId('proxy-mode').value === 'custom';
    byId('proxy-address-field').hidden = !custom;
    byId('proxy-address').required = custom;
  }

  function open(focus) {
    const status = getStatus();
    if (!status || saving) return;
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

  byId('edit-upstream').addEventListener('click', () => open('upstream-input'));
  byId('edit-opencode').addEventListener('click', () => open('opencode-input'));
  byId('edit-proxy').addEventListener('click', () => open('proxy-mode'));
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
  });
  form.addEventListener('submit', async event => {
    event.preventDefault();
    if (saving) return;
    const settings = {
      upstream_base_url: byId('upstream-input').value.trim(),
      opencode_base_url: byId('opencode-input').value.trim(),
      proxy: byId('proxy-mode').value === 'system' ? '' : byId('proxy-mode').value === 'direct' ? 'direct' : byId('proxy-address').value.trim(),
      stepfun_use_proxy: byId('stepfun-use-proxy').checked,
      opencode_use_proxy: byId('opencode-use-proxy').checked,
    };
    for (const [id, key] of [['stepfun-key', 'stepfun_api_key'], ['opencode-key', 'opencode_api_key']]) {
      if (byId(id).value.trim()) settings[key] = byId(id).value.trim();
    }
    saving = true;
    form.setAttribute('aria-busy', 'true');
    byId('save-progress').hidden = false;
    for (const field of form.querySelectorAll('fieldset, button')) field.disabled = true;
    showError('settings-error');
    try {
      await request('/ui/settings', settings);
      onSaved();
      dialog.close();
      notify('上游设置已保存');
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
