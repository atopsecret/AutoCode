// 前端纯状态逻辑；测试不依赖 WebView、用户配置或系统剪贴板。
function clone(value) {
  return value === undefined ? undefined : JSON.parse(JSON.stringify(value));
}

function equal(a, b) {
  return JSON.stringify(a) === JSON.stringify(b);
}

export class ConfigEditor {
  constructor() {
    this.base = null;
    this.draft = null;
    this.dirty = new Set();
  }

  merge(snapshot) {
    if (!snapshot || !Number.isSafeInteger(snapshot.revision) || snapshot.revision < 0) {
      throw new Error('后端返回了无效配置版本');
    }
    if (this.base && snapshot.revision < this.base.revision) return false;
    if (!this.draft) this.draft = clone(snapshot);
    for (const [key, value] of Object.entries(snapshot)) {
      if (!this.dirty.has(key) || key === 'revision' || key === 'version') {
        this.draft[key] = clone(value);
      }
    }
    this.base = clone(snapshot);
    return true;
  }

  edit(key, value) {
    if (!this.base || key === 'revision' || key === 'version') return;
    this.draft[key] = clone(value);
    if (equal(value, this.base[key])) this.dirty.delete(key);
    else this.dirty.add(key);
  }

  payload() {
    if (!this.base) throw new Error('配置尚未加载');
    return { ...clone(this.draft), revision: this.base.revision, version: this.base.version };
  }

  // 保存期间继续编辑的字段不能被异步保存结果清空。
  committed(snapshot, submitted) {
    this.merge(snapshot);
    for (const key of Array.from(this.dirty)) {
      if (equal(this.draft[key], submitted[key])) {
        this.dirty.delete(key);
        this.draft[key] = clone(this.base[key]);
      }
    }
    return this.draft;
  }
}

export class SourceStateStore {
  constructor() {
    this.states = new Map();
    this.events = new Map();
    this.counter = 0;
  }

  token() {
    return this.counter;
  }

  event(status) {
    this.counter += 1;
    this.events.set(status.source, this.counter);
    return this.set(status);
  }

  snapshot(statuses, token) {
    for (const status of statuses || []) {
      if ((this.events.get(status.source) || 0) <= token) this.set(status);
    }
    return Array.from(this.states.values());
  }

  set(status) {
    const sources = ['imessage', 'apple_mail', 'outlook', 'notification'];
    const states = ['disabled', 'starting', 'running', 'error'];
    if (!status || !sources.includes(status.source) || !states.includes(status.state)) return false;
    this.states.set(status.source, clone(status));
    return true;
  }
}

export class LatestCodeStore {
  constructor() {
    this.value = null;
    this.events = 0;
  }

  token() {
    return this.events;
  }

  event(value) {
    this.events += 1;
    return this.set(value);
  }

  snapshot(value, token) {
    if (this.events !== token) return false;
    return this.set(value);
  }

  set(value) {
    if (!value || typeof value.code !== 'string' || !/^[A-Za-z0-9]{4,8}$/.test(value.code)) return false;
    if (this.value && Number.isSafeInteger(value.sequence) && Number.isSafeInteger(this.value.sequence)) {
      if (value.sequence <= this.value.sequence) return false;
    }
    this.value = clone(value);
    return true;
  }
}

export function errorText(error) {
  if (typeof error === 'string') return error;
  if (error && typeof error.message === 'string') return error.message;
  return String(error || '未知错误');
}
