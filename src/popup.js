import { LatestCodeStore, errorText } from './ui-state.js';

const store = new LatestCodeStore();
const copyButton = document.getElementById('popup-copy');
const message = document.getElementById('popup-message');

function render() {
  const value = store.value;
  if (!value) return;
  document.getElementById('popup-code').textContent = value.code;
  document.getElementById('popup-source').textContent = value.source || '';
  copyButton.disabled = false;
  copyButton.textContent = '复制验证码';
  message.textContent = '点击复制，不会自动输入或回车。';
}

async function initPopup() {
  const api = window.__TAURI__;
  if (!api) {
    message.textContent = '无法连接 AutoCode 后台';
    return;
  }
  await api.event.listen('verification-code', function (event) {
    if (store.event(event.payload)) render();
  });
  // 监听注册之后才取快照，期间收到新事件则不使用旧快照。
  const token = store.token();
  const value = await api.core.invoke('get_latest_code');
  if (store.snapshot(value, token)) render();
}

copyButton.addEventListener('click', async function () {
  if (!store.value || !window.__TAURI__) return;
  const code = store.value.code;
  copyButton.disabled = true;
  try {
    // 后端只允许复制仍有效的、经过提取校验的验证码。
    await window.__TAURI__.core.invoke('copy_verified_code', { code });
    if (store.value.code === code) {
      copyButton.textContent = '已复制';
      message.textContent = '已复制，请在目标输入框手动粘贴。';
    }
  } catch (error) {
    message.textContent = errorText(error);
  } finally {
    copyButton.disabled = !store.value;
  }
});

function dismiss() {
  if (window.__TAURI__) window.__TAURI__.core.invoke('dismiss_popup').catch(function (error) {
    message.textContent = errorText(error);
  });
}

document.getElementById('dismiss').addEventListener('click', dismiss);
document.addEventListener('keydown', function (event) {
  if (event.key === 'Escape') dismiss();
});

initPopup().catch(function (error) { message.textContent = errorText(error); });
