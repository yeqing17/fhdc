<div align="center">

# fhdc

**鸿蒙 / OpenHarmony 设备调试工具箱**(纯 Rust,仿照 [fadb](https://github.com/yeqing17/fadb) 的架构)

Rust 1.90 · egui / eframe · Windows

</div>

---

fhdc 把日常的 `hdc` 命令行操作装进一个图形界面:设备选择、概览、交互式终端、文件管理、应用管理、hilog 日志、进程表、截图、无线连接,一屏搞定。比背 `hdc` 子命令舒服一点。

## ✨ 功能

### 📱 设备与连接
- **设备发现:** 自动列出 `hdc list targets` 的 USB / 网络设备,显式选择后才操作。
- **连接管理:** `hdc tconn IP:端口` 直连、`tdisconn` 断开、历史端点记忆(最多 8 个)、一键让设备监听 TCP 端口(`tmode port`)、重启 hdc 服务。
- **连接代数保护:** 设备断开重连后,旧会话的命令会被自动拒绝,不会误伤新连接。

### 🛠 调试面板
- **📊 概览:** 型号、品牌、系统版本、API 版本、内核、CPU 核心数、内存、/data 分区,点击即复制。
- **💻 终端:** 伪控制台承载的持久 `hdc shell`(Windows ConPTY),VT100 网格渲染:逐键输入、Ctrl+C、↑↓ 历史、拖拽划选复制、粘贴;快捷指令栏(14 条内置预设,含按 PID 抓日志;本地持久化,JSON 导入/导出)。
- **📁 文件:** 远端目录浏览(面包屑 + 排序)、上传/下载(覆盖需确认)、新建文件夹、重命名、递归删除(需确认);文件直接拖进窗口即上传,HAP 拖进窗口即安装。
- **📦 应用:** 桌面式图标网格(大图标 + 本地化名称,从 HAP 提取并缓存),网格/列表两种视图与四档图标缩放(偏好持久化);单击选中、双击启动、右键菜单(复制包名/启动/强制停止/清除数据/卸载);详情(包名/版本/入口 Ability/AppID/安装时间)点击即复制;`hdc install` 安装 HAP。
- **📋 进程:** `ps -ef` 表格自动刷新(3 秒),按 PID/CPU/命令排序,搜索,结束进程(需确认)。
- **📜 日志:** `hilog` 实时流,按级别(D/I/W/E/F)着色与过滤,关键字过滤,暂停/恢复,清除设备缓冲,保存到文件。
- **📷 截图:** `snapshot_display` 截取屏幕,适应/原始大小显示,保存 JPEG,一键复制图像。

## 🚀 使用

```bash
cargo run --release -p fhdc
```

或直接使用已构建的 `target/release/fhdc.exe`。

**hdc 查找顺序:** `FHDC_HDC` 环境变量 → 设置中的手动路径 → `HDC_HOME` / `OHOS_SDK_HOME` / `DEVECO_SDK_HOME` → DevEco Studio 默认安装位置 → `PATH`。找不到时自动退回内置假后端(界面完整可用,数据为演示数据),并在顶栏提示。

**无设备体验界面:**

```bash
# Git Bash
FHDC_FAKE=1 cargo run -p fhdc

# PowerShell
$env:FHDC_FAKE = "1"; cargo run -p fhdc
```

## 🏗 架构(与 fadb 同构)

```
crates/
  fhdc-domain   领域词汇:校验型 newtype(ConnectKey/RemotePath/BundleName…)、
                错误、BackendCommand / BackendEvent
  fhdc-hdc      hdc 封装:HdcTransport trait + 进程后端(有界输出、超时、取消、
                Windows 无窗口)、交互式 shell 通道、hilog 流、文件脚本、截图流水线
  fhdc-fake     假后端:同一 trait,内存态,供无设备开发与演示
  fhdc-device   DeviceRegistry:连接代数分配,陈旧目标拒绝
apps/
  fhdc-desktop  egui 界面:RuntimeBridge(专用线程 + 双向通道,事件驱动重绘)、
                主题(鸿蒙蓝,深/浅双槽)、七个面板
```

安全设计沿用 fadb:破坏性操作(删除/卸载/清除数据/覆盖/结束进程)必须经过确认弹窗;`RemotePath` / `BundleName` 在构造时拒绝路径穿越与注入字符;每个操作绑定显式连接键与代数。

## 🧪 质量

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --release -p fhdc
```

## ❓ 常见问题

**首次运行提示「Windows 已保护你的电脑 / 发布者未知」？**

这是 Windows SmartScreen 对**未签名且没有下载信誉**的程序的标准提示,与应用本身无关。
fhdc 的发布产物未做代码签名,从浏览器下载后会带「来自互联网」标记,首次运行即会触发:

- 本机放行:点「更多信息 → 仍要运行」;或用 `Unblock-File .\fhdc-x.y.z.exe` 去掉标记;
- 本地编译的 exe(`cargo build --release`)不带该标记,通常不会提示;
- 想对**所有用户**彻底消除,只能做代码签名(EV 证书立即获得信誉,OV 证书需累积下载量);
  自签名证书无效。

**为什么 fadb 不弹?**

SmartScreen 按「文件哈希 + 发布者」记信誉。fadb 发布得早、下载量足够大,已经积累了信誉;
或者本机点过一次「仍要运行」(Windows 会记住该文件)。这不是 fadb 有特殊处理——同样未签名。

## 📄 许可

MIT OR Apache-2.0
