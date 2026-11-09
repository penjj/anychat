# AnyChat

多模型 AI 聊天工作台。左侧栏收录 30 个对话站点，右侧一个内容区，各站点独立会话，数据全部留在本地。

用 **Tauri 2 + 系统 WebView** 构建，release 包 7 MB 左右。

窗口**不抢焦点、不置顶**——这是首要约束，不是可选项。

## 功能

- **30 个 AI 站点**，分 3 组，按点击频次排序
- **独立会话**：每个站点单独的 WebView 与存储分区，登录态互不影响
- **切换不重载**：站点实例复用，切回来还是原页面
- **侧栏拖动改宽**：拖到 88px 以下自动收成图标条
- **主题**：深色、浅色、跟随系统
- **内存回收**：空闲站点自动释放，也可手动释放
- **⌥Space 显隐**，不进 Dock，关闭即隐藏
- **⌘N** 基于当前站点新开会话

## 安装

从 [Releases](https://github.com/penjj/anychat/releases) 下载对应平台：

| 平台 | 文件 |
|---|---|
| macOS Apple Silicon | `AnyChat_*_aarch64.dmg` |
| macOS Intel | `AnyChat_*_x64.dmg` |
| Windows x64 | `AnyChat_*_x64-setup.exe` 或 `.msi` |
| Windows ARM64 | `AnyChat_*_arm64-setup.exe` 或 `.msi` |

首次启动后是隐藏的，按 **⌥Space** 或点菜单栏图标召唤。

## 从源码构建

```bash
npm install
npm run tauri build          # 产物在 src-tauri/target/release/bundle/
```

开发：

```bash
npm run tauri dev
```

依赖 Rust stable、Node 20+。macOS 需要 Xcode 命令行工具，Windows 需要 MSVC 生成工具与 WebView2（Win11 自带）。

## 结构

```
src-tauri/
├── src/main.rs         窗口、托盘、菜单、快捷键、命令、门禁
├── src/tools.rs        30 个站点的唯一事实源
├── capabilities/       IPC 权限
└── icons/              三平台图标容器，scripts/gen-icons.py 生成
ui/                     React 侧栏界面
├── src/App.jsx         主界面
├── src/SplashAnimation.jsx  启动动画
├── src/styles.css      样式与主题变量
└── vite.config.js
frontend/               构建产物（不提交）
scripts/
├── gen-icons.py        生成图标
├── smoke.sh            门禁：进程内断言
├── probe.sh            门禁：活体探针
└── start.sh            开发启动
```

两个 WebView 叠在一个窗口上：`rail` 负责侧栏与顶栏，`tool_<id>` 是内容区。侧栏量出 `#slot` 的矩形后回报给 Rust，由 Rust 调整内容 WebView 的位置和尺寸。

## 配置

设置面板里改，存在 `~/Library/Application Support/app.anychat.desktop/layout.json`（Windows 在 `%APPDATA%`）：

| 字段 | 说明 |
|---|---|
| `rail_width` | 侧栏宽度，52–420 |
| `zoom` | 内容区缩放，默认 0.8 |
| `win_w` / `win_h` | 窗口尺寸 |
| `always_on_top` | 置顶 |
| `theme` | `auto` / `light` / `dark` |

开机自启存于系统（macOS 是 LaunchAgent，Windows 是注册表）。

## 主题

三档，默认跟随系统。

`auto` 由 CSS 的 `prefers-color-scheme` 解析，不靠 JS 监听系统事件。首帧防闪靠 `ui/index.html` 里的内联脚本：它同步读 localStorage 并立即设置 `data-theme`，Rust 侧的真实值随后覆盖。

侧栏图标按需加底盘：站点 favicon 是为白底设计的，GitHub、OpenAI 这类近黑图标在深色侧栏上会消失。前端在 16×16 canvas 上测平均亮度，只有过暗或过亮时才加反差底盘，彩色图标不受影响。

## 启动动画

全屏透明窗口，约 5.8 秒：光点从边缘游移，螺旋汇聚成 `AnyChat` 字形，副标题浮出，整体上浮淡出。

字形不是手绘的——先把文字渲染到离屏 canvas，采样不透明像素作为目标点，每个光点飞向其中一个。因此任何窗口尺寸下比例都正确。

字体由 `--font-display` 控制，`SplashAnimation.jsx` 与样式表共用这一个变量。`font-preview.html` 是选字体用的预览页，直接用浏览器打开，不参与构建。

## 门禁

```bash
./scripts/smoke.sh    # 18 项，进程内断言
./scripts/probe.sh    # 25 项，活体探针
```

`smoke.sh` 在主线程跑断言，管窗口属性、焦点、快捷键、分区、⌘N、持久化——主进程自己能断言的事。

`probe.sh` 进程常驻不阻塞，脚本往 stdin 写命令、从日志读页面自报的 DOM，管侧栏渲染、设置面板可见性、图标模式、favicon 缓存——需要页面回话的事。

两套分工是因为主线程一 `sleep` 就冻结 Tauri 事件循环，凡是需要页面回应的检查在 `smoke.sh` 里都测不了。

## 已知限制

- **开机自启仅 macOS / Windows**，Linux 未接入。
- **⌥Space 可能被其他应用占用**。注册失败时托盘菜单项禁用，设置面板显示警告。
- **Gemini / NotebookLM 的 Google 登录**可能被判定为不安全客户端（`disallowed_useragent`）。这是系统 WebView 方案的固有代价；换自带 Chromium 会让包体从 7 MB 涨到 100 MB 以上。
- **favicon 覆盖率 30/30**，抓不到的站点用配色字母块兜底。
- **窗口圆角 20px** 由 macOS 的 `contentView` 图层遮罩实现，Windows 使用系统窗口装饰。

## 许可

MIT
