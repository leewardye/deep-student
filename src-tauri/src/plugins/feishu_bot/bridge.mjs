//! 飞书长连接桥（Node sidecar）。
//!
//! 使用官方 SDK 建立 WebSocket 长连接，把 im.message.receive_v1 等事件以
//! JSON Lines 打到 stdout：{"name": "<event>", "data": {...}}。
//! 诊断信息走 stderr 或 {"type":"log"|"error","message":"..."} 行。
//!
//! 由 Rust 侧 bridge.rs 负责落盘本文件、npm install 依赖、spawn 进程。

import * as Lark from "@larksuiteoapi/node-sdk";

const [, , appId, appSecret] = process.argv;
if (!appId || !appSecret) {
  console.error("usage: node bridge.mjs <app_id> <app_secret>");
  process.exit(2);
}

function emitLine(obj) {
  process.stdout.write(JSON.stringify(obj) + "\n");
}

const eventDispatcher = new Lark.EventDispatcher({}).register({
  "im.message.receive_v1": async (data) => {
    emitLine({ name: "im.message.receive_v1", data });
    return {};
  },
});

const wsClient = new Lark.WSClient({ appId, appSecret, loggerLevel: Lark.LoggerLevel.warn });

try {
  wsClient.start({ eventDispatcher });
  emitLine({ type: "log", message: "feishu bridge started" });
} catch (err) {
  emitLine({ type: "error", message: String(err && err.message ? err.message : err) });
  process.exit(1);
}

// 未捕获异常：报告后退出，由 Rust 侧重连逻辑接管
process.on("uncaughtException", (err) => {
  emitLine({ type: "error", message: String(err && err.message ? err.message : err) });
  process.exit(1);
});
process.on("unhandledRejection", (err) => {
  emitLine({ type: "error", message: String(err && err.message ? err.message : err) });
});
