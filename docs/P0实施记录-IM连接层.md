# P0 实施记录：IM 连接层（飞书首发）

> 分支：`feat/im-channels`（deep-student 仓库）｜实施日期：2026-11-03

## 已完成

### 后端（Rust，src-tauri）
| 文件 | 动作 | 内容 |
|---|---|---|
| `crates/dstu-agent/src/chat_v2/task_command.rs` | 修改 | 新增 `CHANNEL_FEISHU` 通道常量 |
| `src/plugins/im_core.rs` | **新增** | 统一消息模型：`ImChannel`/`AttachKind`/`InboundAttachment`/`UnifiedInbound`/`UnifiedOutbound` + Unicode 安全分片 `chunk_text` + 附件清单 `attachment_manifest`（含 5 条单测） |
| `src/plugins/im_media.rs` | **新增** | 附件管线：20MB 上限、扩展名/MIME 白名单、文件名清洗、`(channel, msg_id, index)` 幂等落盘到 `app_data/im_inbox/<channel>/<日期>/`、临时文件原子改名（含 5 条单测） |
| `src/plugins/feishu_bot/client.rs` | **新增** | 飞书 REST：tenant_access_token 缓存（提前 5min 刷新）、媒体下载、图片上传、text/image 发送、业务码校验（含单测） |
| `src/plugins/feishu_bot/bridge.rs` | **新增** | Node sidecar 管理：脚本落盘、SDK 自动 `npm install`、进程拉取、JSON Lines 事件读取；**容忍官方 SDK 打到 stdout 的非 JSON 日志行**（实测冒烟发现，否则会误判断线）（含单测） |
| `src/plugins/feishu_bot/bridge.mjs` | **新增** | 官方 `@larksuiteoapi/node-sdk` WSClient 长连接桥：注册 `im.message.receive_v1`，事件 JSON 输出 |
| `src/plugins/feishu_bot/mod.rs` | **新增** | `FeishuBotPlugin` ChannelPlugin：事件归一化（text/image/file/audio/media/post、p2p）、自动绑定首条消息发送者、限流、附件入库、任务路由（RemoteTaskManager → headless agent turn）、断线指数退避重连、飞书特有配置流（含 5 条单测） |
| `src/plugins/ilink_bot/mod.rs` | 修改 | `guard` 模块开放为 `pub(crate)`（绑定校验+限流器复用） |
| `src/plugins/mod.rs` | 修改 | 注册 3 个新模块 |
| `src/plugins/manager.rs` | 修改 | 注册 FeishuBotPlugin；`set_enabled`/`bootstrap_enabled` 的 settings key 从 ilinkbot 硬编码泛化为 `plugin.<id>.enabled` |

### 前端（React/TS）
| 文件 | 动作 | 内容 |
|---|---|---|
| `features/settings/api/pluginsApi.ts` | 修改 | `IlinkBotConfig` 增加飞书扩展字段（appId/hasSecret/configured/boundOpenId） |
| `features/settings/components/plugins/PluginsTab.tsx` | 修改 | 详情页飞书分支：跳过扫码流程、`FeishuCredsForm` 凭证表单（App ID + App Secret 密码框，已存时占位掩码）、「重新配置凭证」入口 |
| `locales/{zh-CN,en-US}/settings.json` | 修改 | 新增 5 个 i18n 键（凭证说明/字段/保存/编辑/绑定提示） |

## 关键设计决策（实施中修正）

1. **WS 长连接用官方 Node SDK sidecar 而非纯 Rust**：调研确认飞书长连接协议
   （握手/ticket/心跳）为 SDK 私有实现、无公开稳定文档，社区共识是复用官方 SDK。
   REST 四组 API 仍为纯 Rust。Node 依赖与项目 MCP stdio 生态一致，不新增门槛。
2. **绑定语义调整**：`is_bound` = 凭证就绪（否则插件永远无法启动收首条消息，
   死锁）；真正的发送者绑定在首条私聊消息时自动锁定（`plugin.feishubot.bound_open_id`）。
3. **SDK stdout 污染**：冒烟测试发现官方 SDK 直接往 stdout 打 `[info]:`/`[error]:`
   非空格 JSON 行，Rust 侧 `read_event` 已做容错（非 `{` 开头一律按日志跳过）。

## 验证状态

- ✅ `bridge.mjs` 通过 `node --check` 语法校验
- ✅ **端到端冒烟**：真实 npm install SDK（52 包）+ 运行桥脚本，成功连上飞书
  WS 网关（假凭证返回 `invalid appId`，证明协议链路全通）
- ✅ 两份 locale JSON 语法校验通过
- ✅ Rust 代码经逐文件人工复查（对照 ilink_bot 同构实现与既有 API 签名）
- ⚠️ **Rust 编译验证未做**：本机无 Rust 工具链与 MSVC（安装约需数 GB，待用户决定）。
  在有工具链的机器上执行：`cd src-tauri && cargo check` + `cargo test -p deep-student im_`
- ⏳ `tsc --noEmit`（前端类型检查）：依赖安装后执行中

## 用户使用路径（打包后）

1. 飞书开放平台 → 创建企业自建应用 → 记下 App ID / App Secret
2. 权限：`im:message` 读写、`im:resource`；事件订阅选「长连接」+ 添加
   `im.message.receive_v1`；发布版本
3. DeepStudent 设置 → 插件 → 飞书机器人 → 填凭证 → 启用
4. 在飞书给机器人发私聊消息 → 自动绑定 → 后续文字/图片/文件直接进任务系统

## 下一步（P1 预告）

- `ilink_bot` 接入统一层重构（normalize_inbound/send_unified）
- 微信 iLink 图片协议 spike
- `im_media` → dstu-vfs 资源库正式索引接入（P3，当前落盘路径已可被 agent 引用）
