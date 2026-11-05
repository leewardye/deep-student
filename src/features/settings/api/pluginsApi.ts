import { invoke } from '@tauri-apps/api/core';

export type PluginState =
  | 'stopped'
  | 'starting'
  | 'waiting_login'
  | 'running'
  | 'stopping'
  | 'error';

export interface PluginInfo {
  id: string;
  label: string;
  blurb: string;
  kind: 'channel';
  state: PluginState;
  enabled: boolean;
  configured: boolean;
  bound: boolean;
  error?: string | null;
}

export interface PluginStatusSnapshot {
  state: PluginState;
  enabled: boolean;
  configured: boolean;
  bound: boolean;
  loginStatus?: string | null;
  accountId?: string | null;
  userId?: string | null;
  lastError?: string | null;
  lastActivity?: string | null;
  qrcodePngBase64?: string | null;
  qrcodeStatus?: string | null;
}

export interface IlinkBotConfig {
  enabled: boolean;
  rateLimitPerMin: number;
  modelConfigId: string;
  systemPrompt: string;
  /** 附件白名单补充扩展名（逗号分隔；两个 IM 通道共用，get_public_config 返回） */
  extraExt?: string;
  bound: boolean;
  hasToken: boolean;
  accountId: string;
  userId: string;
  baseUrl: string;
  /** feishubot 扩展字段（get_public_config 返回） */
  appId?: string;
  hasSecret?: boolean;
  configured?: boolean;
  boundOpenId?: string;
}

/** feishu_diagnose 返回的连接诊断结果（键名与后端 serde_json 输出一致，snake_case） */
export interface FeishuDiagnosis {
  all_ok: boolean;
  items: Array<{ key: string; ok: boolean; detail: string }>;
  bound_open_id: string;
  bound_chat_id: string;
  events_received: number;
  bridge_errors: number;
  recent_lines: string[];
  platform_checklist: string[];
}

export const PLUGIN_EVENTS = {
  stateChanged: 'plugin-state-changed',
  qrcode: 'plugin-qrcode',
  activity: 'plugin-activity',
} as const;

export const pluginsApi = {
  list: () => invoke<PluginInfo[]>('plugin_list'),
  start: (id: string) => invoke<PluginState>('plugin_start', { id }),
  stop: (id: string) => invoke<PluginState>('plugin_stop', { id }),
  getStatus: (id: string) => invoke<PluginStatusSnapshot>('plugin_get_status', { id }),
  getConfig: (id: string) => invoke<IlinkBotConfig>('plugin_get_config', { id }),
  /** 飞书连接诊断（凭证/token/桥进程/事件流体检） */
  diagnoseFeishu: () => invoke<FeishuDiagnosis>('feishu_diagnose'),
  /** 向绑定的飞书私聊推送测试文本 */
  pushFeishuMessage: (text: string) =>
    invoke<string>('feishu_push_message', { text, imagePaths: null, filePaths: null }),
  setConfig: (id: string, patch: Partial<IlinkBotConfig> & Record<string, unknown>) =>
    invoke<void>('plugin_set_config', { id, patch }),
  setEnabled: (id: string, enabled: boolean) =>
    invoke<void>('plugin_set_enabled', { id, enabled }),
  beginLogin: (id: string) => invoke<void>('plugin_begin_login', { id }),
  cancelLogin: (id: string) => invoke<void>('plugin_cancel_login', { id }),
  logout: (id: string) => invoke<void>('plugin_logout', { id }),
  unbind: (id: string) => invoke<void>('plugin_unbind', { id }),
};
