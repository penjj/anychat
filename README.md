# AnyChat

多模型 AI 聊天工作台。左侧栏收录 30 个对话站点，右侧一个内容区，各站点独立会话，数据全部留在本地。

用 **Tauri 2 + 系统 WebView** 构建，release 包 7 MB 左右。

窗口**不抢焦点、不置顶**——这是首要约束，不是可选项。

## 功能

- **30 个 AI 站点**，分 3 组，按点击频次排序
- **共享登录态**：所有站点共用一个 cookie 存储，Google 系（Gemini / NotebookLM / AI Studio）登录一次即可通用
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

## 自动更新

应用用 `tauri-plugin-updater` 检查新版本，更新源是 GitHub Release 里的 `latest.json`：

```
https://github.com/penjj/anychat/releases/latest/download/latest.json
```

启动后 8 秒自动检查一次，发现新版本会在后台把包下载好，并在工具栏和页面之间出现一条提示：点「重启更新」即装好并重启，点 `×` 则本次不再显示。整个过程中**不会在用户没有点确认的情况下替换应用**。

后台先下好再提示，是为了让那个按钮真的是「重启」而不是「开始下载」——点了就立即生效，不用再等 4 MB。

设置面板里有开关可以关掉启动检查，也保留「检查更新」按钮手动触发。

### 国内网络

GitHub 的直连在国内经常超时。应用按顺序尝试下列端点，前一个失败才换下一个：

| 顺序 | 端点 |
|---|---|
| 1 | `https://ghproxy.net/` + Release URL |
| 2 | `https://ghfast.top/` + Release URL |
| 3 | Release URL（直连） |

注意是**成对**的：第 N 个代理取 `latest-mirrorN.json`，而那份 manifest 里的下载地址也已改写成同一个代理。只代理 manifest 不管用——包还在 github.com 上，只会把卡顿从查询挪到下载。

这些反代是第三方服务，可能随时失效，所以直连永远留在列表最后兜底。签名覆盖的是包本身而不是 URL，改主机名不影响校验。

要加/换代理，改 `src-tauri/src/main.rs` 里的 `GITHUB_PROXIES`，索引要和 `latest-mirrorN.json` 对齐；移除一个就把它置空字符串，不要删数组项，否则后面的编号全错位。

### 首次配置签名密钥

更新包必须签名，否则已安装的应用会拒绝安装。公钥编译进应用，私钥只存在 CI secrets 里。

生成密钥对（**私钥只显示一次**，务必立刻备份）：

```bash
npx tauri signer generate -w ~/.tauri/anychat.key
```

命令会输出公钥，把它填进 `src-tauri/tauri.conf.json` 的 `plugins.updater.pubkey`，然后把私钥和密码存进仓库 secrets：

| Secret | 内容 |
|---|---|
| `TAURI_SIGNING_PRIVATE_KEY` | 私钥文件全文（含 `untrusted comment:` 首行） |
| `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` | 生成时设的密码 |

```bash
gh secret set TAURI_SIGNING_PRIVATE_KEY < ~/.tauri/anychat.key
gh secret set TAURI_SIGNING_PRIVATE_KEY_PASSWORD
```

**私钥丢失的后果**：已安装的应用再也无法验证新版本，用户只能手动重新下载。请离线备份。

### 发布流程

推一个 tag 就会构建四个平台、合并各自的 `latest.json`、签名并发布：

```bash
git tag -a v0.2.0 -m "0.2.0"
git push origin v0.2.0
```

Tauri 只签名更新包（产出 `.sig`），**不生成** `latest.json`——这一点很容易误判，曾经导致每次发布到最后一步才发现没有 manifest。所以由 `scripts/build-updater-manifest.py` 从实际产物组装：

```bash
python3 scripts/build-updater-manifest.py <artifacts-dir> <out-dir> <version> <tag>
```

三个注意点：

- **文件名会冲突**。两个 macOS 平台都产出 `AnyChat.app.tar.gz`，两个 Windows 平台都产出同名的 NSIS 包。直接平铺上传会互相覆盖，所以 `release.yml` 先按目标改名为 `AnyChat_<version>_<target>.<格式>`，manifest 里的 URL 也指向改名后的文件。
- **版本号取自 `tauri.conf.json`，不是 tag**。tag 只是给 Release 起名，更新器比较的是配置里的版本。
- **Windows 的签名在裸安装包旁边**，不在 zip 里。`createUpdaterArtifacts: true` 时 bundler 产出 `-setup.exe.sig` 和 `_en-US.msi.sig`；只有切到 `v1Compatible` 模式才会额外包一层 `.exe.zip` 并签它。上传路径必须显式列出这些 `.sig`，漏掉的话 Windows 端永远收不到更新，而 Release 看起来是完整的。

`scripts/mirror-manifest.py` 再据此产出 `latest-mirror1.json`、`latest-mirror2.json`，每份的平台下载地址指向对应反代。两个脚本都由 `release.yml` 自动执行。

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
