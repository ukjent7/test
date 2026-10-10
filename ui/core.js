export const byId = id => document.getElementById(id);
export const count = value => value == null ? '未报告' : value.toLocaleString('zh-CN');
export const providerName = provider => ({stepfun: 'StepFun', opencode: 'OpenCode Zen'}[provider] || provider);

// 旁路请求种类 → 界面文案;未知种类显示客户端原始标签。
export const laneName = lane => ({'memory-capture': '记忆采集', dream: '记忆整理', 'dream-v2': '记忆整理 v2',
  flush: '记忆落盘', 'turn-summary': '轮次摘要', recap: '回顾', btw: '顺带回顾', 'title-refresh': '标题刷新',
  compact: '历史压缩', promptsuggest: '提示建议', 'perm-classifier': '权限分类', 'trace-classifier': '轨迹分类',
  'goal-eval': '目标评估'}[lane] || lane);

export function element(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}

export function showError(id, message = '') {
  const node = byId(id);
  node.textContent = message;
  node.hidden = !message;
}

export async function request(path, body, {signal, timeout = 15000} = {}) {
  const controller = new AbortController();
  const cancel = () => controller.abort();
  if (signal?.aborted) cancel();
  signal?.addEventListener('abort', cancel, {once: true});
  const timer = setTimeout(cancel, timeout);
  try {
    const response = await fetch(path, {
      signal: controller.signal,
      cache: 'no-store',
      ...(body === undefined ? {} : {
        method: 'POST', headers: {'Content-Type': 'application/json'}, body: JSON.stringify(body),
      }),
    });
    if (!response.ok) {
      const error = await response.json().catch(() => null);
      throw new Error(error?.error?.message || `请求失败（HTTP ${response.status}）`);
    }
    return response.status === 204 ? null : await response.json();
  } catch (error) {
    if (controller.signal.aborted && !signal?.aborted) throw new Error('请求超时，请重试');
    throw error;
  } finally {
    clearTimeout(timer);
    signal?.removeEventListener('abort', cancel);
  }
}

let toastTimer;
export function notify(text) {
  clearTimeout(toastTimer);
  byId('toast').textContent = text;
  byId('toast').hidden = false;
  toastTimer = setTimeout(() => { byId('toast').hidden = true; }, 2800);
}

const copyTimers = new WeakMap();
export async function copy(text, button) {
  try {
    await navigator.clipboard.writeText(text);
    if (button) {
      clearTimeout(copyTimers.get(button));
      button.dataset.copied = 'true';
      copyTimers.set(button, setTimeout(() => { delete button.dataset.copied; }, 1400));
    }
    notify('已复制');
  } catch {
    notify('复制失败，请手动选择并复制内容');
  }
}
