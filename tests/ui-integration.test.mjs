import test from 'node:test';
import assert from 'node:assert/strict';

// 合成 DOM/IPC；不创建 WebView，不调用真实剪贴板、权限或监控。
class Element {
  constructor(id = '') {
    this.id = id;
    this.style = {};
    this.dataset = {};
    this.listeners = new Map();
    this.children = [];
    this.textContent = '';
    this.innerHTML = '';
    this.value = id === 'cfg-autofill-delay' ? '1500' : '';
    this.checked = false;
    this.disabled = false;
    this.classes = new Set();
    this.classList = {
      add: (name) => this.classes.add(name),
      remove: (name) => this.classes.delete(name),
      toggle: (name, yes) => yes ? this.classes.add(name) : this.classes.delete(name),
      contains: (name) => this.classes.has(name),
    };
    this.parts = new Map();
  }
  addEventListener(name, handler) { this.listeners.set(name, handler); }
  querySelector(selector) {
    if (selector === '.inline-input') return null;
    if (!this.parts.has(selector)) this.parts.set(selector, new Element(selector));
    return this.parts.get(selector);
  }
  querySelectorAll() { return []; }
  appendChild(child) { this.children.push(child); return child; }
  getAttribute(name) { return this.dataset[name.replace(/^data-/, '')]; }
  emit(name) {
    const handler = this.listeners.get(name);
    return handler && handler({ target: this });
  }
}

const realTimeout = globalThis.setTimeout;
globalThis.setTimeout = (...args) => {
  const timer = realTimeout(...args);
  timer.unref();
  return timer;
};

function fixture(overrides = {}) {
  const elements = new Map();
  const get = (id) => {
    if (!elements.has(id)) elements.set(id, new Element(id));
    return elements.get(id);
  };
  const radios = ['smart', 'always', 'floating_only', 'clipboard_only'].map((value) => {
    const element = new Element(value); element.value = value; return element;
  });
  globalThis.document = {
    readyState: 'loading',
    getElementById: get,
    querySelectorAll: (selector) => selector === 'input[name="paste_mode"]' ? radios : [],
    addEventListener: () => {},
    createElement: (tag) => new Element(tag),
    createTextNode: (text) => ({ textContent: text }),
  };
  const callbacks = new Map();
  const calls = [];
  let backend = {
    revision: 0, version: 2, listen_imessage: true, listen_apple_mail: true,
    listen_outlook: true, listen_notification: true, auto_enter: false,
    launch_at_login: false, paste_mode: 'smart', autofill_detect_delay_ms: 1500,
    verification_keywords: ['验证码'], known_2fa_senders: [], verification_patterns: [], native_autofill_apps: [],
  };
  globalThis.window = {
    confirm: () => false,
    __TAURI__: {
      event: { listen: async (name, callback) => { callbacks.set(name, callback); return () => {}; } },
      core: { invoke: async (name, args) => {
        calls.push({ name, args });
        if (Object.hasOwn(overrides, name)) return overrides[name](args);
        if (name === 'get_config') return structuredClone(backend);
        if (name === 'get_version') return '0.6.0';
        if (name === 'get_startup_error') return null;
        if (name === 'get_latest_code') return null;
        if (name === 'get_monitor_status') return [{ source: 'imessage', state: 'error', detail: '合成读取失败' }];
        if (name === 'check_permissions') return { full_disk_access: true, accessibility: true };
        if (name === 'update_config') { backend = { ...args.newConfig, revision: backend.revision + 1 }; return structuredClone(backend); }
        if (name === 'copy_verified_code' || name === 'dismiss_popup') return null;
        throw new Error('未定义合成 IPC: ' + name);
      } },
    },
  };
  return {
    get, calls, callbacks,
    event: (name, payload) => callbacks.get(name)({ payload }),
    tray: (values) => {
      backend = { ...backend, ...values, revision: backend.revision + 1 };
      callbacks.get('config-changed')({ payload: structuredClone(backend) });
    },
  };
}

let serial = 0;
async function main(overrides) {
  const fake = fixture(overrides);
  const app = await import('../src/main.js?fixture=' + serial++);
  await app.initApp();
  await new Promise(setImmediate);
  return { fake, app };
}

test('真实保存函数保留托盘关闭与本地无关编辑', async () => {
  const { fake, app } = await main();
  fake.get('cfg-auto-enter').checked = true;
  fake.get('cfg-auto-enter').emit('change');
  fake.tray({ listen_imessage: false });
  await app.saveConfig();
  const saved = fake.calls.find((call) => call.name === 'update_config').args.newConfig;
  assert.equal(saved.listen_imessage, false);
  assert.equal(saved.auto_enter, true);
  assert.equal(saved.revision, 1);
  assert.equal(fake.get('save-toast').textContent, '设置已保存');
});

test('真实 UI 不会因为来源开关为 true 把异常来源点亮', async () => {
  const { fake } = await main();
  assert.equal(fake.get('cfg-listen-imessage').checked, true);
  assert.equal(fake.get('status-imessage').classList.contains('active'), false);
  assert.equal(fake.get('status-imessage').dataset.state, 'error');
  assert.match(fake.get('status-imessage').querySelector('.status-detail').textContent, /合成读取失败/);
});

test('配置加载错误持续显示，不把默认配置当作已正常启动', async () => {
  const { fake } = await main({ get_startup_error: () => '合成 TOML 格式错误' });
  assert.equal(fake.get('startup-error').hidden, false);
  assert.match(fake.get('startup-error').textContent, /已暂停全部监控.*合成 TOML/);
});

test('托盘保存失败事件在前端可见，已修复的启动错误可以清除', async () => {
  const { fake } = await main({ get_startup_error: () => '合成配置错误' });
  fake.event('app-error', '合成托盘保存失败');
  assert.match(fake.get('save-toast').textContent, /托盘保存失败/);
  fake.event('startup-error', null);
  assert.equal(fake.get('startup-error').hidden, true);
});

test('更新后端报错在页面可见，无需不存在的 dialog/process 插件', async () => {
  const { fake, app } = await main({ check_update: () => Promise.reject('尚未配置签名更新') });
  await app.checkForUpdates(true);
  assert.match(fake.get('save-toast').textContent, /尚未配置签名更新/);
  assert.equal(fake.get('save-toast').classList.contains('error'), true);
  assert.equal(fake.get('btn-check-update').disabled, false);
});

test('revision 冲突只刷新，不偷偷重试覆盖后端', async () => {
  const { fake, app } = await main({ update_config: () => Promise.reject('配置版本冲突') });
  fake.get('cfg-auto-enter').checked = true;
  fake.get('cfg-auto-enter').emit('change');
  fake.tray({ listen_notification: false });
  await app.saveConfig();
  assert.equal(fake.calls.filter((call) => call.name === 'update_config').length, 1);
  assert.equal(fake.get('cfg-auto-enter').checked, true);
  assert.equal(fake.get('cfg-listen-notification').checked, false);
  assert.match(fake.get('save-toast').textContent, /本地改动已保留/);
});

test('真实 popup 初始化旧快照不会覆盖先到的新事件，显示不触发复制', async () => {
  let resolveSnapshot;
  const fake = fixture({ get_latest_code: () => new Promise((resolve) => { resolveSnapshot = resolve; }) });
  await import('../src/popup.js?fixture=' + serial++);
  await new Promise(setImmediate);
  fake.event('verification-code', { code: '222222', source: '合成新通知' });
  resolveSnapshot({ code: '111111', source: '合成旧邮件' });
  await new Promise(setImmediate);
  assert.equal(fake.get('popup-code').textContent, '222222');
  assert.equal(fake.calls.some((call) => call.name === 'copy_verified_code'), false);
  await fake.get('popup-copy').emit('click');
  assert.deepEqual(fake.calls.find((call) => call.name === 'copy_verified_code').args, { code: '222222' });
  assert.equal(fake.get('popup-copy').textContent, '已复制');
});
