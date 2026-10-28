# 同席安卓端开发方案

> 状态：方案，未开工。来源：`tasks/insight and future.md` 第 3 条——手机通过 Tailscale 连桌面端，远程控制与协作。

## 定位

手机端是**遥控器**，不是第二个同席。成员（Claude、Codex、DSH、Hermes、OpenCode）、会话库、项目目录、工作树都还在桌面上，手机只做三件事：

1. 看：会话列表、消息流（含思考和工具活动）、主脑派活进度、协作问答。
2. 说：给私聊、群聊、项目群发消息，插话、停止，带图片或文件（附件已支持，见 `src/attachments.ts`）。
3. 拍板：回答协作问答、放行花费上限、确认合并 diff。

不做：在手机上跑成员、改成员与服务、接入新的 harness、打包发版。

## 架构

```
手机（Tauri 2 Android，复用 src/ 前端）
   │  HTTPS / WebSocket，只走 Tailscale 内网（100.x / *.ts.net）
   ▼
桌面同席（现有 Tauri 进程）新增「远程网关」
   ├─ POST /rpc/<command>   → 调用与 invoke 相同的处理函数
   ├─ GET  /events (WS)     → 转发现在 emit 给前端的事件
   └─ GET  /attachments/... → 附件上传下载
```

### 为什么是 Tauri 2 Android

- 前端是纯 TS + Vite，没有框架；Tauri 2 原生支持 Android，同一套 `src/` 能直接打 APK，不用重写界面。
- 前端所有后端调用都走 `@tauri-apps/api` 的 `invoke` / `listen`（约 79 个命令，10 来个事件）。手机端只需把这两个函数换成网络版：
  - 新增 `src/transport.ts`：桌面构建原样导出 Tauri 的 `invoke`/`listen`；手机构建（`import.meta.env.VITE_REMOTE`）改成 `fetch('/rpc/…')` 和 WebSocket 订阅。
  - 各模块的 `import { invoke } from '@tauri-apps/api/core'` 改成从 `./transport` 导入，一次机械替换。
- 布局：现有窄屏规则（≤980px 起成员面板浮出）是起点，再加一档 ≤600px：侧栏改成首页列表、会话页全屏、成员面板改成底部抽屉。

### 桌面端远程网关

- 在 `src-tauri` 里起一个小 HTTP/WS 服务（`axum` 或 `tiny_http` 二选一，选依赖少的），默认关闭，「成员与服务」里开关。
- **只监听 Tailscale 网卡地址**（100.64.0.0/10 或 `tailscale ip -4` 的结果），不绑 0.0.0.0。
- RPC 不走反射：在 `main.rs` 里给允许远程调用的命令列一张白名单 match 表（读会话、发消息、停止、回答问答、附件……），表外一律 403。服务更新、成员接入、签名私钥相关命令永不进表。
- 事件：现在 `app.emit(...)` 的地方加一个广播通道，网关把同一份 payload 推给已连接的手机。
- 鉴权：首次配对时桌面显示一次性二维码（网关地址 + 随机 token），手机扫码保存；token 存桌面 `metadata`，可在桌面吊销。Tailscale 本身已是加密隧道，token 防同一 tailnet 里的其他设备。

### 手机端特有的

- 推送：手机后台时 WS 会断。第一版不接 FCM，回到前台时按 `revision` 补拉；第二版考虑 Tailscale 内自建 ntfy 推「主脑问你了」「活交回了」。
- 附件：相机/相册直接选图，走现有附件上传接口。
- 断线：顶部一条「桌面离线」提示，消息框保留草稿（现有 `draftKey` 逻辑可复用）。

## 分期

| 期 | 内容 | 验收 |
|---|---|---|
| 0 | `transport.ts` 抽象 + 桌面网关（白名单只含只读命令）+ 配对 | 浏览器经 Tailscale 打开网关地址能看会话和消息流 |
| 1 | 发消息、插话、停止、附件上传；事件推送 | 手机发消息，桌面成员回复实时流到手机 |
| 2 | Tauri Android 打包、≤600px 布局、扫码配对 | 真机装 APK，项目群里回答一条协作问答 |
| 3 | 后台通知（ntfy）、断线补拉 | 手机锁屏时主脑提问能收到通知 |

第 0 期做完其实就能用手机浏览器遥控，APK 是锦上添花，可以看第 0、1 期的使用感受再决定要不要做第 2 期。

## 风险

- **安全**：网关等于把桌面上能改代码、能推送的成员交给手机。白名单 + 只听 Tailscale + token 三层都要有；权限档（全权/只改工作树/只读）照常生效。
- **Android 构建环境**：需要 Android SDK、NDK、JDK 17，Windows 上首次搭建较慢；Rust 依赖里只在桌面用的（`single-instance`、`global-shortcut`）要用 `cfg(desktop)` 隔开。
- **流量与耗电**：消息流里思考和工具事件很密，手机端默认折叠思考、按需拉取。
