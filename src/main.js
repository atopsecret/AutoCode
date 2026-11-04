// ============================================================
// AutoCode 前端 — 主页 + 设置面板
// ============================================================

import { ConfigEditor, SourceStateStore, LatestCodeStore, errorText } from './ui-state.js';

const history = [];
const MAX_HISTORY = 20;
let currentConfig = null;  // 当前配置对象
const configEditor = new ConfigEditor();
const sourceStates = new SourceStateStore();
const latestCode = new LatestCodeStore();
let updateBusy = false;
let saveBusy = false;
let saveToastTimer;
let lastPermissions = null;
let startupEvents = 0;

const configFields = {
  'cfg-listen-imessage': 'listen_imessage',
  'cfg-listen-apple-mail': 'listen_apple_mail',
  'cfg-listen-outlook': 'listen_outlook',
  'cfg-listen-notification': 'listen_notification',
  'cfg-auto-enter': 'auto_enter',
  'cfg-launch-at-login': 'launch_at_login',
};

// ===================== 初始化 =====================
async function initApp() {
  setupTabs();
  setupDelaySlider();

  if (window.__TAURI__) {
    // 版本号
    window.__TAURI__.core.invoke('get_version').then(function (v) {
      document.getElementById('version').textContent = 'v' + v;
      // 同时更新设置页面的版本显示
      document.getElementById('current-version').textContent = '当前版本：v' + v;
    }).catch(showError);
    refreshStartupError();

    // 监听验证码事件
    await window.__TAURI__.event.listen('verification-code', function (ev) {
      if (latestCode.event(ev.payload)) showLatestCode();
    });
    await window.__TAURI__.event.listen('config-changed', function (ev) {
      receiveConfig(ev.payload);
    });
    await window.__TAURI__.event.listen('monitor-status', function (ev) {
      sourceStates.event(ev.payload);
      renderMonitorStatus();
    });
    await window.__TAURI__.event.listen('startup-error', function (ev) {
      startupEvents += 1;
      renderStartupError(ev.payload);
    });
    await window.__TAURI__.event.listen('app-error', function (ev) {
      showError(ev.payload);
    });

    // 监听后端发起的跳转到设置页
    await window.__TAURI__.event.listen('navigate', function (ev) {
      if (ev.payload === 'settings') {
        switchTab('settings');
      }
    });

    // 先注册监听，再取快照，避免慢的初始请求覆盖较新的托盘/验证码事件。
    window.__TAURI__.core.invoke('get_config').then(receiveConfig).catch(showError);
    refreshMonitorStatus();
    const codeToken = latestCode.token();
    window.__TAURI__.core.invoke('get_latest_code').then(function (value) {
      if (latestCode.snapshot(value, codeToken)) showLatestCode();
    }).catch(showError);
    checkPermissions();
  }

  // 按钮事件
  document.getElementById('btn-save').addEventListener('click', saveConfig);
  document.getElementById('btn-reset').addEventListener('click', resetConfig);
  document.getElementById('copy-btn').addEventListener('click', copyLatestCode);

  // 权限检测按钮
  document.getElementById('btn-open-fda').addEventListener('click', function () {
    if (window.__TAURI__) window.__TAURI__.core.invoke('open_fda_settings').catch(showError);
  });
  document.getElementById('btn-open-acc').addEventListener('click', function () {
    if (window.__TAURI__) window.__TAURI__.core.invoke('open_accessibility_settings').catch(showError);
  });
  document.getElementById('btn-recheck').addEventListener('click', checkPermissions);

  // 检查更新按钮
  document.getElementById('btn-check-update').addEventListener('click', function () {
    checkForUpdates(true);
  });

  for (const [id, field] of Object.entries(configFields)) {
    document.getElementById(id).addEventListener('change', function (event) {
      configEditor.edit(field, event.target.checked);
    });
  }
  document.querySelectorAll('input[name="paste_mode"]').forEach(function (radio) {
    radio.addEventListener('change', function () {
      if (radio.checked) configEditor.edit('paste_mode', radio.value);
    });
  });

  // 添加项目按钮
  document.getElementById('add-keyword').addEventListener('click', function () {
    showInlineInput('keyword-list', function (val) {
      editList('verification_keywords', function (items) { items.push(val); });
      renderKeywords();
    });
  });
  document.getElementById('add-sender').addEventListener('click', function () {
    showInlineInput('sender-list', function (val) {
      editList('known_2fa_senders', function (items) { items.push(val); });
      renderSenders();
    });
  });
  document.getElementById('add-pattern').addEventListener('click', function () {
    showInlineInput('pattern-list', function (val) {
      editList('verification_patterns', function (items) { items.push(val); });
      renderPatterns();
    });
  });
  document.getElementById('add-autofill-app').addEventListener('click', function () {
    showInlineInput('autofill-app-list', function (val) {
      editList('native_autofill_apps', function (items) { items.push(val); });
      renderAutofillApps();
    });
  });
}

// ===================== 权限检测 =====================
function checkPermissions() {
  if (!window.__TAURI__) return;

  window.__TAURI__.core.invoke('check_permissions').then(function (status) {
    lastPermissions = status;
    renderPermissions(status);
    refreshMonitorStatus();
  }).catch(showError);
}

function renderPermissions(status) {
    var banner = document.getElementById('perm-banner');
    var fdaEl = document.getElementById('perm-fda');
    var accEl = document.getElementById('perm-acc');
    var titleEl = banner.querySelector('.perm-title');

    // FDA 是来源访问的提示，不是以 chat.db 存在与否替代所有监控状态。
    var needsFDA = status.full_disk_access === false;
    var needsAcc = !status.accessibility && currentConfig &&
      ['smart', 'always'].includes(currentConfig.paste_mode);

    fdaEl.style.display = needsFDA ? 'flex' : 'none';
    accEl.style.display = needsAcc ? 'flex' : 'none';

    if (needsFDA || needsAcc) {
      banner.style.display = 'block';
      banner.classList.remove('all-granted');
      titleEl.innerHTML =
        '<svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M10.29 3.86L1.82 18a2 2 0 0 0 1.71 3h16.94a2 2 0 0 0 1.71-3L13.71 3.86a2 2 0 0 0-3.42 0z"/><line x1="12" y1="9" x2="12" y2="13"/><line x1="12" y1="17" x2="12.01" y2="17"/></svg> 需要权限';
    } else {
      // 没有待处理权限时直接隐藏，不用旧计时器隐藏后来的权限错误。
      banner.style.display = 'none';
      banner.classList.add('all-granted');
      titleEl.innerHTML =
        '<svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><polyline points="20 6 9 17 4 12"/></svg> 权限已就绪';
      document.getElementById('btn-recheck').style.display = '';
    }
}

function receiveConfig(cfg) {
  if (!configEditor.merge(cfg)) return;
  currentConfig = configEditor.draft;
  applyConfigToUI(currentConfig);
}

function renderStartupError(error) {
  const banner = document.getElementById('startup-error');
  banner.hidden = !error;
  banner.textContent = error ? '配置加载失败，已暂停全部监控：' + errorText(error) : '';
}

async function refreshStartupError() {
  if (!window.__TAURI__) return;
  const token = startupEvents;
  try {
    const error = await window.__TAURI__.core.invoke('get_startup_error');
    if (startupEvents === token) renderStartupError(error);
  } catch (error) { showError(error); }
}

async function refreshMonitorStatus() {
  if (!window.__TAURI__) return;
  const token = sourceStates.token();
  try {
    const statuses = await window.__TAURI__.core.invoke('get_monitor_status');
    sourceStates.snapshot(statuses, token);
    renderMonitorStatus();
  } catch (error) { showError(error); }
}

function renderMonitorStatus() {
  const ids = { imessage: 'status-imessage', apple_mail: 'status-applemail',
    outlook: 'status-outlook', notification: 'status-notification' };
  const labels = { disabled: '已关闭', starting: '正在启动', running: '运行中', error: '异常，等待重试' };
  for (const [source, id] of Object.entries(ids)) {
    const element = document.getElementById(id);
    const status = sourceStates.states.get(source);
    const state = status ? status.state : 'starting';
    element.classList.toggle('active', state === 'running');
    element.classList.toggle('error', state === 'error');
    element.dataset.state = state;
    element.title = status && status.detail ? status.detail : labels[state];
    element.querySelector('.status-detail').textContent =
      status && status.detail ? labels[state] + '：' + status.detail : labels[state];
  }
}

function editList(field, edit) {
  if (!currentConfig) return;
  const items = currentConfig[field].slice();
  edit(items);
  configEditor.edit(field, items);
}

// ===================== Tab 切换 =====================
function setupTabs() {
  var btns = document.querySelectorAll('.tab-btn');
  btns.forEach(function (btn) {
    btn.addEventListener('click', function () {
      switchTab(btn.getAttribute('data-tab'));
    });
  });
}

function switchTab(name) {
  document.querySelectorAll('.tab-btn').forEach(function (b) {
    b.classList.toggle('active', b.getAttribute('data-tab') === name);
  });
  document.querySelectorAll('.page').forEach(function (p) {
    p.classList.toggle('active', p.id === 'page-' + name);
  });
}

// ===================== 配置 → UI =====================
function applyConfigToUI(cfg) {
  // 设置开关
  document.getElementById('cfg-listen-imessage').checked = cfg.listen_imessage;
  document.getElementById('cfg-listen-apple-mail').checked = cfg.listen_apple_mail;
  document.getElementById('cfg-listen-outlook').checked = cfg.listen_outlook;
  document.getElementById('cfg-listen-notification').checked = cfg.listen_notification;
  document.getElementById('cfg-auto-enter').checked = cfg.auto_enter;
  document.getElementById('cfg-launch-at-login').checked = cfg.launch_at_login;

  // 粘贴模式
  var radios = document.querySelectorAll('input[name="paste_mode"]');
  radios.forEach(function (r) {
    r.checked = r.value === cfg.paste_mode;
  });

  // 延迟滑块
  var slider = document.getElementById('cfg-autofill-delay');
  slider.value = cfg.autofill_detect_delay_ms;
  document.getElementById('delay-value').textContent = cfg.autofill_detect_delay_ms + 'ms';

  // 列表
  renderKeywords();
  renderSenders();
  renderPatterns();
  renderAutofillApps();
  if (lastPermissions) renderPermissions(lastPermissions);
}

// ===================== UI → 配置 =====================
function collectConfigFromUI() {
  if (!currentConfig) return null;
  return configEditor.payload();
}

// ===================== 保存 / 重置 =====================
async function saveConfig() {
  var cfg = collectConfigFromUI();
  if (!cfg || !window.__TAURI__ || saveBusy) return;
  saveBusy = true;
  document.getElementById('btn-save').disabled = true;
  try {
    const saved = await window.__TAURI__.core.invoke('update_config', { newConfig: cfg });
    currentConfig = configEditor.committed(saved, cfg);
    applyConfigToUI(currentConfig);
    await refreshStartupError();
    showToast(configEditor.dirty.size ? '已保存提交的设置；还有未保存改动' : '设置已保存');
  } catch (error) {
    try { receiveConfig(await window.__TAURI__.core.invoke('get_config')); }
    catch (_) { /* 原始保存错误仍展示，本地改动不会清空。 */ }
    showError(errorText(error) + '。本地改动已保留，请检查后重新保存。');
  } finally {
    saveBusy = false;
    document.getElementById('btn-save').disabled = false;
  }
}

function resetConfig() {
  if (!window.__TAURI__) return;
  // 先保存一个空配置让后端产生默认值，再重新获取
  // 简单方式：直接请求默认值
  if (!confirm('确定要恢复所有设置为默认值吗？')) return;

  // 使用内建的默认值
  window.__TAURI__.core.invoke('get_default_config').then(function (cfg) {
    for (const [key, value] of Object.entries(cfg)) configEditor.edit(key, value);
    currentConfig = configEditor.draft;
    applyConfigToUI(currentConfig);
    saveConfig();
  }).catch(showError);
}

function showToast(message = '设置已保存', error = false) {
  var toast = document.getElementById('save-toast');
  toast.textContent = message;
  toast.classList.toggle('error', error);
  toast.classList.add('show');
  clearTimeout(saveToastTimer);
  saveToastTimer = setTimeout(function () {
    toast.classList.remove('show');
  }, error ? 8000 : 2500);
}

function showError(error) {
  showToast(errorText(error), true);
}

function showLatestCode() {
  const value = latestCode.value;
  showCode(value.code, value.source, value.strategy, value.confidence);
}

// ===================== 验证码展示 =====================
function showCode(code, source, strategy, confidence) {
  var display = document.getElementById('code-display');
  display.style.display = 'block';
  document.getElementById('code-value').textContent = code;
  document.getElementById('code-source').textContent = source + ' · ' + strategy;

  var now = new Date();
  var timeStr = pad(now.getHours()) + ':' + pad(now.getMinutes()) + ':' + pad(now.getSeconds());

  history.unshift({ code: code, source: source, time: timeStr });
  if (history.length > MAX_HISTORY) history.pop();
  renderHistory();
}

function copyLatestCode() {
  if (history.length === 0) return;

  // 检查 clipboard API 是否可用
  if (!navigator.clipboard || !navigator.clipboard.writeText) {
    // 降级方案：使用传统方法
    fallbackCopyToClipboard(history[0].code);
    return;
  }

  navigator.clipboard.writeText(history[0].code).then(function () {
    var btn = document.getElementById('copy-btn');
    btn.classList.add('copied');
    btn.innerHTML =
      '<svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><polyline points="20 6 9 17 4 12"/></svg> 已复制';
    setTimeout(function () {
      btn.classList.remove('copied');
      btn.innerHTML =
        '<svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><rect x="9" y="9" width="13" height="13" rx="2" ry="2"/><path d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1"/></svg> 复制';
    }, 1500);
  }).catch(function (err) {
    console.error('复制失败:', err);
    fallbackCopyToClipboard(history[0].code);
  });
}

// 降级复制方案
function fallbackCopyToClipboard(text) {
  var textArea = document.createElement('textarea');
  textArea.value = text;
  textArea.style.position = 'fixed';
  textArea.style.left = '-999999px';
  document.body.appendChild(textArea);
  textArea.select();
  try {
    document.execCommand('copy');
    var btn = document.getElementById('copy-btn');
    btn.classList.add('copied');
    btn.textContent = '已复制';
    setTimeout(function () {
      btn.classList.remove('copied');
      btn.textContent = '复制';
    }, 1500);
  } catch (err) {
    console.error('降级复制也失败:', err);
  }
  document.body.removeChild(textArea);
}

function renderHistory() {
  var el = document.getElementById('history-list');
  if (history.length === 0) {
    el.innerHTML = '<p class="empty-hint">等待验证码…</p>';
    return;
  }
  var html = '';
  for (var i = 0; i < history.length; i++) {
    var item = history[i];
    html += '<div class="history-item" data-code="' + esc(item.code) + '">' +
            '<span class="code">' + esc(item.code) + '</span>' +
            '<span class="meta">' + esc(item.source) + '<br>' + item.time + '</span>' +
            '</div>';
  }
  el.innerHTML = html;

  // 点击复制
  el.querySelectorAll('.history-item').forEach(function (item) {
    item.addEventListener('click', function () {
      var code = item.getAttribute('data-code');
      if (navigator.clipboard && navigator.clipboard.writeText) {
        navigator.clipboard.writeText(code).catch(function (err) {
          console.error('复制失败:', err);
          fallbackCopyToClipboard(code);
        });
      } else {
        fallbackCopyToClipboard(code);
      }
    });
  });
}

// ===================== 列表渲染 =====================
function renderKeywords() {
  renderTagList('keyword-list', currentConfig ? currentConfig.verification_keywords : [], function (idx) {
    editList('verification_keywords', function (items) { items.splice(idx, 1); });
    renderKeywords();
  });
}

function renderSenders() {
  renderTagList('sender-list', currentConfig ? currentConfig.known_2fa_senders : [], function (idx) {
    editList('known_2fa_senders', function (items) { items.splice(idx, 1); });
    renderSenders();
  });
}

function renderAutofillApps() {
  renderTagList('autofill-app-list', currentConfig ? currentConfig.native_autofill_apps : [], function (idx) {
    editList('native_autofill_apps', function (items) { items.splice(idx, 1); });
    renderAutofillApps();
  });
}

function renderTagList(containerId, items, onRemove) {
  var container = document.getElementById(containerId);
  // 保留可能已存在的 inline-input
  var existingInput = container.querySelector('.inline-input');
  var html = '';
  for (var i = 0; i < items.length; i++) {
    html += '<span class="tag">' +
            esc(items[i]) +
            '<span class="remove" data-idx="' + i + '">&times;</span>' +
            '</span>';
  }
  container.innerHTML = html;
  if (existingInput) container.appendChild(existingInput);

  container.querySelectorAll('.remove').forEach(function (btn) {
    btn.addEventListener('click', function (e) {
      e.stopPropagation();
      onRemove(parseInt(btn.getAttribute('data-idx'), 10));
    });
  });
}

function renderPatterns() {
  var container = document.getElementById('pattern-list');
  var items = currentConfig ? currentConfig.verification_patterns : [];
  var existingInput = container.querySelector('.inline-input');
  var html = '';
  for (var i = 0; i < items.length; i++) {
    html += '<div class="pattern-item">' +
            '<span class="pattern-text">' + esc(items[i]) + '</span>' +
            '<span class="remove" data-idx="' + i + '">&times;</span>' +
            '</div>';
  }
  container.innerHTML = html;
  if (existingInput) container.appendChild(existingInput);

  container.querySelectorAll('.remove').forEach(function (btn) {
    btn.addEventListener('click', function (e) {
      e.stopPropagation();
      var idx = parseInt(btn.getAttribute('data-idx'), 10);
      editList('verification_patterns', function (items) { items.splice(idx, 1); });
      renderPatterns();
    });
  });
}

// ===================== Inline Input =====================
function showInlineInput(containerId, onAdd) {
  var container = document.getElementById(containerId);
  // 避免重复
  if (container.querySelector('.inline-input')) return;

  var wrapper = document.createElement('div');
  wrapper.className = 'inline-input';
  var input = document.createElement('input');
  input.type = 'text';
  input.placeholder = '输入内容后按回车…';
  var addBtn = document.createElement('button');
  addBtn.textContent = '添加';

  wrapper.appendChild(input);
  wrapper.appendChild(addBtn);
  container.appendChild(wrapper);

  input.focus();

  function commit() {
    var val = input.value.trim();
    if (val) {
      onAdd(val);
      wrapper.remove();
    }
  }

  addBtn.addEventListener('click', commit);
  input.addEventListener('keydown', function (e) {
    if (e.key === 'Enter') commit();
    if (e.key === 'Escape') wrapper.remove();
  });
  input.addEventListener('blur', function () {
    setTimeout(function () { wrapper.remove(); }, 200);
  });
}

// ===================== 延迟滑块 =====================
function setupDelaySlider() {
  var slider = document.getElementById('cfg-autofill-delay');
  var label = document.getElementById('delay-value');
  slider.addEventListener('input', function () {
    label.textContent = slider.value + 'ms';
    configEditor.edit('autofill_detect_delay_ms', Number(slider.value));
  });
}

// ===================== 自动更新 =====================
async function checkForUpdates(manual = false) {
  if (!window.__TAURI__ || updateBusy) return;
  updateBusy = true;
  const button = document.getElementById('btn-check-update');
  button.disabled = true;
  button.textContent = '正在检查…';

  try {
    const update = await window.__TAURI__.core.invoke('check_update');
    if (update) {
      const yes = window.confirm(
        `发现新版本 ${update.version}\n当前版本：${update.current_version}\n\n${update.notes || ''}\n\n下载并安装已签名更新？`
      );
      if (yes) {
        button.textContent = '正在安装…';
        await window.__TAURI__.core.invoke('install_update');
        showToast('更新已安装，请重新打开 AutoCode');
      }
    } else if (manual) {
      showToast('当前已是最新版本');
    }
  } catch (error) {
    if (manual) showError('检查更新失败：' + errorText(error));
  } finally {
    updateBusy = false;
    button.disabled = false;
    button.textContent = '检查更新';
  }
}

// ===================== 工具函数 =====================
function pad(n) {
  return n < 10 ? '0' + n : '' + n;
}

function esc(text) {
  var d = document.createElement('div');
  d.appendChild(document.createTextNode(text));
  return d.innerHTML;
}

// 导出真实控制函数供合成 DOM 回归测试使用，不启动应用或访问个人数据。
export { initApp, saveConfig, checkForUpdates };

// ===================== 启动 =====================
if (document.readyState === 'loading') {
  document.addEventListener('DOMContentLoaded', function () { initApp().catch(showError); });
} else {
  initApp().catch(showError);
}
