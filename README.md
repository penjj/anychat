# AnyChat

多模型 AI 聊天工作台。左侧栏收录 30 个对话站点，右侧一个内容 WebView，各站点独立会话，数据全部留在本地。

用 **Tauri 2 + 系统 WebView** 构建，release 包体约 7 MB，内存占用远低于 Electron 同类应用。

**窗口不抢焦点、不置顶**——这是首要约束，不是可选项。

## 需求 → 实现 → 验证

| 需求 | 实现 | 门禁断言 |
|---|---|---|
| 侧边栏收录大量 AI 工具 | `src-tauri/src/tools.rs` 的 `TOOLS` 常量，31 条 / 3 组 | `tool-inventory`、`group-count`、`rail-rendered` |
| ⌥Space 切换窗口显隐 | `tauri-plugin-global-shortcut` 注册 `Alt+Space` → `toggle_window` | `shortcut-alt-space`、`window-hidden-by-default` |
| 不抢焦点 | 摘要走 `orderFrontRegardless`，**不** `makeKeyAndOrderFront`，也不 `set_focus` | `summon-does-not-activate` |
| 不置顶 | `always_on_top(false)`，每次摘要重申 | `summon-not-on-top`、`window-not-always-on-top` |
| 进程常驻 | `CloseRequested` 里 `prevent_close()` + `hide()` | `window-exists` |
| 开机自启 | `tauri-plugin-autostart`，托盘与设置面板各一处开关 | `autostart-round-trip` |
| ⌘N 基于当前 AI 新开会话 | 原生菜单「会话 → 新会话」`CmdOrCtrl+N` → `new_chat()` 导航到 `tool.new_chat` | `cmd-n-new-chat` |
| 各站点登录态隔离 | macOS 用 `data_store_identifier`（FNV-1a 稳定 16 字节），其他平台用 `data_directory` | `per-tool-partition-dirs` |
| 侧栏可拖动改宽 | `#resizer` 拖动 → `set_rail_width`，夹在 52–420px 并落盘 | `rail-width-clamped-and-saved`、`rail-width-min-enforced`、`rail-width-max-enforced` |
| 拖到窄处只剩图标 | 宽度 < 88px 时 `#app.icon-only`，隐去名称与分组标题 | `drag-narrow-enters-icon-mode`、`icon-strip-keeps-icons` |
| 无原生标题栏 | `decorations(false)`；顶栏最右端是置顶 / 隐藏按钮，关闭走 `hide()` | `window-hide-visible`、`hide-button-at-right-edge` |
| 不进 Dock | 启动设 `NSApplicationActivationPolicy::Accessory` | 手工核对（系统 API，无进程内断言） |
| 站点 favicon | 取站点根，解析 `<link rel="icon">`（含 `apple-touch-icon-precomposed`），回退 `/favicon.ico`；按根元素校验确为图片后转 data URL 缓存 | `favicons-rendered`、`favicons-cached` |
| 工具排序 | 按点击次数降序，次数相同按最近使用，再按名称 | `rail-has-31-tools` |
| 切换不重载 | 每工具一个 `Webview`，切换只 `hide/show`，复用同一实例 | `webview-reused-on-return` |

## 为什么不置顶也不抢焦点

`NSWindow::show()` 在 macOS 等价于 `makeKeyAndOrderFront:`，会夺走前台应用。所以 `show_window` 改成：

```rust
ns_window.orderFrontRegardless();   // 出现在当前 Space，但不激活 App
```

配合 `focused(false)` 建窗、全程不调 `set_focus()`。门禁里先 `hide()` + `NSApp.deactivate()` 把自家应用踢出前台，再摘要，断言 `activeAfter=false && focused=false`——避免「本来就前台的假通过」。

## 结构

```
src-tauri/
├── Cargo.toml              tauri 2.12.5 + wry 0.57.1（unstable = 多 webview 必需）
├── tauri.conf.json         windows: [] —— 窗口在 Rust 里手建，才能控制 focused/visible
├── capabilities/default.json   必须含 "webviews": ["rail"]，否则 rail 的 IPC 被 ACL 静默拦掉
├── icons/                  scripts/gen-icons.py 生成：png 全套 + icon.icns + icon.ico
└── src/
    ├── main.rs             窗口/托盘/菜单/快捷键/命令/自检
    └── tools.rs            30 个工具的唯一事实源
frontend/                   rail UI（原生 JS，零依赖）；设置面板是 rail 内的全屏视图
scripts/{start,smoke}.sh
```

两个 webview 叠在一个 `Window` 上：`rail`（52–420px 侧栏 + 40px 顶栏）和 `tool_<id>`（内容区）。rail 量 `#slot` 的 rect 经 `layout` 命令回报，Rust 侧 `set_position` / `set_size` 对齐。

## ⌘N 的两个入口

1. **原生菜单**「会话 → 新会话」绑定 `CmdOrCtrl+N`。内容 WebView 占满右侧时键盘事件进的是站点页面，rail 的 `keydown` 收不到，所以必须有原生菜单。
2. rail 里的 `keydown` 也处理 `⌘N`，但只在焦点在侧栏时生效。

`new_chat` 导航到 `Tool.new_chat`，与首次打开用的 `Tool.url` 分开——Claude 两者都是 `/new`，但像 Gemini 这类站点两者可以不同。

## 图标

`scripts/gen-icons.py` 一次生成三平台容器，无外部依赖（纯 stdlib）：

| 文件 | 用途 |
|---|---|
| `icon.icns` | macOS 打包（`iconutil`，10 个尺寸含 @2x） |
| `icon.ico` | **Windows 打包必需**。缺它 `cargo tauri build --target *-pc-windows-*` 直接失败 |
| `icon.png` + `32x32/128x128/…png` | Linux / AppImage，以及托盘与窗口图标的回退 |
| `512x512`–`1024x1024` | 高 DPI 与商店素材 |

`.ico` 不是改名的 PNG：`ICONDIR` 头 + 6 条 `ICONDIRENTRY`（16/32/48/64/128/256），每条指向 PNG 载荷。校验过 `type=1`、32bpp。

```bash
python3 scripts/gen-icons.py     # 幂等，可随时重跑
```

## 验证：两套门禁，各管一段

```bash
./scripts/smoke.sh    # 进程内自检
./scripts/probe.sh    # 24 项，活体探针
```

**没有折叠按钮。** 侧栏宽度就是唯一状态：拖到 88px 以下自动隐去名称和分组标题，只剩 favicon；拖回去名称回来。模式由宽度推导，所以不会出现「图标模式但宽度很宽」这种自相矛盾的状态。

**为什么是两套。** `smoke.sh` 在主线程跑断言，主线程一 `sleep` 就冻结 Tauri 事件循环，页面轮询停摆——凡是需要「页面回话」的检查（设置面板真的画出来了吗）它都测不了。`probe.sh` 反过来：进程常驻不阻塞，脚本往 stdin 写命令、从日志读页面自报的 DOM，所以能验证真实渲染。

分工：

| 门禁 | 管什么 |
|---|---|
| `smoke.sh` | 窗口属性、焦点、快捷键、分区、⌘N、宽度的持久化——**主进程就能断言的事** |
| `probe.sh` | 侧栏 DOM、内容区宽度、**设置面板真的可见且 ≥300px 宽**、拖窄进图标模式、**隐藏按钮贴右边缘**、favicon 已缓存 |

`probe.sh` 当前输出：

```
PASS rail-reports-dom
PASS rail-has-31-tools
PASS content-area-has-width
PASS settings-panel-visible
PASS settings-card-width
PASS settings-closes
PASS rail-width-applied
PASS rail-width-persisted
PASS drag-narrow-enters-icon-mode
PASS icon-strip-is-narrow
PASS icon-strip-keeps-icons
PASS window-pin-visible
PASS window-hide-visible
PASS hide-after-pin
PASS hide-button-at-right-edge
PASS zoom-applied-and-persisted
PASS favicons-cached
RESULT ok
```

实测窗口：`System Events` 报 `1, AnyChat`，菜单栏 `Apple, AnyChat, 会话, 编辑`。release 二进制 7.2 MB。

`screencapture` 被系统权限挡住，没有截图；界面证据来自 rail 自报的 DOM（`{"tools":31,"groups":3,"active":"chatgpt","slot":{"w":876,"h":720}}`）与 System Events 的窗口查询。

## 平台坑（都踩过，写在这里免得重犯）

| 现象 | 原因 |
|---|---|
| rail 收不到事件、`eval` 无回调、`eval_with_callback` 静默失败 | 子 webview 在宿主窗口未聚焦时事件与 eval 通道会断；**不要**让 UI 依赖事件，用命令返回值 + 轮询 |
| 隐藏窗口里页面定时器只跑几次就停 | WKWebView 对隐藏/非激活窗口节流 |
| 改 `frontend/*` 后界面不更新 | `cargo` 不追踪 web 资源；`build.rs` 已加 `rerun-if-changed`，仍不生效时 `cargo clean` |
| 侧栏宽 208 但 `#main` 宽 0 | rail webview 必须铺满整窗，侧栏宽度只是 CSS 变量；把 webview 缩成 208 会让 flex 塌掉 |
| “登录状态没保存” | 曾同时设 `data_directory` 与 `data_store_identifier`。macOS 只认后者，混用会看起来有隔离实际共用 |

## 已知限制

- **开机自启仅 macOS/Windows**（`LaunchAgent`）；Linux 未接 `.desktop`。
- **⌥Space 可能被其他应用占用**。注册失败时托盘菜单项禁用，设置面板显示黄色警告。
- **外部打开用实时地址**。以前前端缓存 `url`，只在切工具和工具栏导航时刷新，站内点击后点「在浏览器打开」拿到的是过期值甚至根域名。现在点击时向后端要 `current_url`，后端也接受空值并自行兜底，两道都指向同一个真相：webview 自己的 URL。
- **窗口圆角 20px**。只给 HTML 加 `border-radius` 没用——内容区是叠在 rail 之上的原生子视图，直角会透出来。所以窗口设 `transparent(true)`，再对 `contentView` 的 layer 设 `cornerRadius` + `masksToBounds`，这样主页面和子 webview 被同一层遮罩裁切。
- **侧边栏可拖拽、可收成图标条**。默认 208px；拖到 88px 以下自动切换图标模式，标签和分组标题收起，只剩 favicon。没有折叠按钮——模式由宽度推导，不会和实际宽度打架。
- **无原生标题栏**。窗口 `decorations(false)`，红绿灯不再压在 DOM 上；顶栏最右端是置顶和隐藏两个按钮。关闭走 `hide()` 而不是退出，⌥Space 才能再叫回来。
- **Dock 图标已移除**。启动时设 `NSApplicationActivationPolicy::Accessory`，只留菜单栏托盘，符合工具定位。
- **图标是内联 SVG**（Lucide，MIT），不是 iconfont。桌面外壳不该为了自己的按钮去连 CDN，而且之前用的 `‹ › ⟳ ⚙` 这类字形在每个系统上粗细都不一样。
- **favicon 覆盖率 30/31**。抓取链路：站点根解析 `<link rel="icon">`（含 `apple-touch-icon-precomposed`）→ 回退 `/favicon.ico`、`/favicon.svg`、`/apple-touch-icon.png` → 最后走 Google/DuckDuckGo 的图标服务。只有 `phind.com` 拿不到：站点 403，且图标 CDN 只返回占位图，被字节校验挡下，因此用配色字母块兜底。
- **图标抓取必须走 HTTP 代理**。本机 `ALL_PROXY` 是 socks5，而 ureq 未编译 socks 支持——它又优先读 `ALL_PROXY`，导致所有国际站点静默超时。代码里显式挑 `HTTPS_PROXY`/`HTTP_PROXY` 并跳过 socks 值。
- **缓存会校验**。早期版本把 HTML 404 页面当 ico 存了进去（deepseek、zai、tongyi、wenxin 都中招）且永不重试；现在读取时解码并检查字节头，坏条目视为未命中会重抓。

- **切换工具会丢掉页面里未发送的草稿吗**——不会，因为 webview 被复用而非重建；但切到从未打开过的工具才首次真正加载，第一次会等网络。
- **Gemini / NotebookLM 的 Google 登录**在嵌入式 WebView 里可能被 Google 判为不安全客户端（`disallowed_useragent`）。这是系统 WebView 方案的固有代价；换自带 Chromium 的方案可以规避，但包体会从 7 MB 涨到 100 MB 以上。
- **未验证**：打包成 `.app`/`.dmg`/`.msi` 需 `cargo tauri build`，本机未装 tauri-cli。`.ico` 已就位，Windows 打包的前置条件满足，但**没有实际跑过 Windows 构建**。
- **体积**：`cargo tauri build` 产出 `.app` 7.27 MB、`.dmg` 3.20 MB。

## 内存回收

每个站点一个常驻 Webview，切换只 hide/show，所以打开过的站点全都留在内存里。实测 `chatgpt.com` 在长对话下会到 1.5 GB——这是页面自己的 DOM 和 JS 堆，不是壳的问题。

两条回收路径：

| 方式 | 触发 | 行为 |
|---|---|---|
| 空闲回收 | 非活动站点 10 分钟未显示 | 后台线程每分钟检查一次，销毁超时的 webview |
| 手动回收 | 右键侧栏任一图标 | 「释放此站内存」或「释放所有空闲站点」 |

**当前选中的站点永不销毁**，三个入口都有保护：`reclaim_idle` 过滤掉 active、`reclaim_tool` 对 active 返回错误、右键菜单里该项 disabled。

销毁是彻底的：`Webview::close()` 连同它的渲染进程一起回收。下次切回该站点会重新加载——这是用时间换内存的明确取舍。

`ANYCHAT_PROBE=1` 的探针日志会打印 `reclaimed N`，`resident` 命令可查当前常驻列表。

## 主题

三档：深色、浅色、跟随系统（默认）。设置面板里切换，存到 `layout.json` 的 `theme` 字段。

`auto` 由 CSS 的 `prefers-color-scheme` 解析，不靠 JS 监听系统事件——那样会有一帧延迟。首帧防闪靠 `index.html` 里的内联脚本：它同步读 localStorage 并立刻设置 `data-theme`，而 Rust 侧的真实值在后续 `state` 里覆盖它。

侧栏图标按需加底盘：站点 favicon 是为白底设计的，GitHub、OpenAI 这类近黑图标在深色侧栏上会消失。前端在 16×16 canvas 上测平均亮度，只有过暗或过亮时才加反差底盘，彩色图标不受影响。

## 启动动画字体

`--font-display` 是标题栏和启动动画共用的字族，`SplashAnimation.jsx` 里 canvas 的字体必须与它保持一致，否则点的形状和布局预留的空间对不上。

`font-preview.html` 是选字体用的预览页，用同样的 mote 点阵采样渲染 16 款候选。直接用浏览器打开即可，不参与构建。

## 窗口行为

- **失焦自动隐藏**：窗口失去焦点 400ms 后隐藏，不用按 ⌘W。三种情况抑制：右键菜单打开、设置面板打开、有登录弹窗在屏幕上。
- **开机自启**：开发版（`target/debug` 或 `target/release` 路径）拒绝写入 LaunchAgent，否则会把开发版路径注册进去，覆盖已安装应用的同名条目，下次重建失效——这正是开关「反复勾选却保持不住」的原因。
- **四角缩放**：`decorations(false)` 让 macOS 剥掉了标准缩放手柄，注入的圆角又裁掉剩下的部分。rail 页面因此放了 8 个热区（四角 18px、四边 6px），由 `resize_window` 命令驱动窗口几何。

## 启动动画

全屏透明窗口，约 5.8 秒：

1. **0–0.5s** 深色遮罩淡入，光点从屏幕边缘游移
2. **0.5–2.8s** 约 3.1 万个光点螺旋汇聚
3. **2.8s** 光标自行组成 `AnyChat` 字形
4. **3.0–4.4s** 副标题「多模型聊天工作台」缓慢浮出
5. **4.4–5.4s** 整体上浮淡出，主窗口接手

文字形状不是手绘的：先把 `AnyChat` 渲染到离屏 canvas，采样不透明像素作为目标点，每个光点飞向其中一个。因此任何窗口尺寸下字形比例都正确，密度自适应（目标 28000 点，实测 31281）。

字形只由光点构成，没有叠清晰文字层，所以点密度必须足够——这是不能用更少粒子的原因。

## 运行

```bash
cd src-tauri && cargo build
../scripts/smoke.sh     # 门禁
../scripts/start.sh     # 启动后是隐藏的，用 ⌥Space 或托盘召唤
```
