import test from 'node:test';
import assert from 'node:assert/strict';
import { ConfigEditor, SourceStateStore, LatestCodeStore } from '../src/ui-state.js';

const config = (revision, values = {}) => ({ revision, version: 2, listen_imessage: true,
  listen_notification: true, auto_enter: false, verification_keywords: ['验证码'], ...values });

test('托盘关闭来源后，保存无关设置不会重新开启来源', () => {
  const editor = new ConfigEditor();
  editor.merge(config(0));
  editor.edit('auto_enter', true);
  editor.merge(config(1, { listen_imessage: false }));
  assert.equal(editor.payload().listen_imessage, false);
  assert.equal(editor.payload().auto_enter, true);
  assert.equal(editor.payload().revision, 1);
});

test('初始旧快照不能覆盖已经接收到的较新配置', () => {
  const editor = new ConfigEditor();
  editor.merge(config(3, { listen_imessage: false }));
  assert.equal(editor.merge(config(2)), false);
  assert.equal(editor.payload().listen_imessage, false);
});

test('revision 冲突刷新后保留用户编辑，但必须再次显式保存', () => {
  const editor = new ConfigEditor();
  editor.merge(config(0));
  editor.edit('verification_keywords', ['验证码', 'OTP']);
  const rejected = editor.payload();
  editor.merge(config(1, { listen_notification: false }));
  assert.equal(rejected.revision, 0);
  assert.equal(editor.payload().revision, 1);
  assert.deepEqual(editor.payload().verification_keywords, ['验证码', 'OTP']);
  assert.equal(editor.payload().listen_notification, false);
  assert.ok(editor.dirty.has('verification_keywords'));
});

test('保存过程中继续编辑同一字段，不被保存结果覆盖', () => {
  const editor = new ConfigEditor();
  editor.merge(config(0));
  editor.edit('verification_keywords', ['验证码', 'OTP']);
  const submitted = editor.payload();
  editor.edit('verification_keywords', ['验证码', 'OTP', 'security code']);
  editor.committed({ ...submitted, revision: 1 }, submitted);
  assert.deepEqual(editor.payload().verification_keywords, ['验证码', 'OTP', 'security code']);
  assert.ok(editor.dirty.has('verification_keywords'));
});

test('正常保存清空已提交 dirty，并采用后端递增 revision', () => {
  const editor = new ConfigEditor();
  editor.merge(config(0));
  editor.edit('auto_enter', true);
  const submitted = editor.payload();
  editor.committed({ ...submitted, revision: 1 }, submitted);
  assert.equal(editor.dirty.size, 0);
  assert.equal(editor.payload().revision, 1);
});

test('来源状态与配置开关无关，异常不会被认为正在运行', () => {
  const states = new SourceStateStore();
  states.event({ source: 'notification', state: 'error', detail: 'schema 不兼容' });
  assert.equal(states.states.get('notification').state, 'error');
  assert.equal(states.set({ source: 'notification', state: true }), false);
});

test('慢的状态快照不覆盖期间收到的异常事件', () => {
  const states = new SourceStateStore();
  const token = states.token();
  states.event({ source: 'notification', state: 'error', detail: '读取失败' });
  states.snapshot([{ source: 'notification', state: 'running' },
    { source: 'imessage', state: 'disabled' }], token);
  assert.equal(states.states.get('notification').state, 'error');
  assert.equal(states.states.get('imessage').state, 'disabled');
});

test('浮窗新验证码事件不会被旧初始化快照覆盖', () => {
  const codes = new LatestCodeStore();
  const token = codes.token();
  codes.event({ code: '222222', source: '合成通知' });
  assert.equal(codes.snapshot({ code: '111111', source: '合成邮件' }, token), false);
  assert.equal(codes.value.code, '222222');
});

test('有 sequence 的验证码拒绝重复或乱序事件', () => {
  const codes = new LatestCodeStore();
  assert.equal(codes.event({ code: '222222', sequence: 2 }), true);
  assert.equal(codes.event({ code: '111111', sequence: 1 }), false);
  assert.equal(codes.event({ code: '222222', sequence: 2 }), false);
  assert.equal(codes.value.code, '222222');
});

test('非法验证码不能进入前端最近验证码状态', () => {
  const codes = new LatestCodeStore();
  assert.equal(codes.event({ code: '<script>' }), false);
  assert.equal(codes.event({ code: '123456789' }), false);
  assert.equal(codes.value, null);
});
