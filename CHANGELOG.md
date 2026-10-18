# 更新日志

## 0.1.0 (2026-10-18)

首个版本。仿照 [fadb](https://github.com/yeqing17/fadb) 架构的鸿蒙 / OpenHarmony
设备调试工具箱,通过 `hdc` 命令行工具检查与管理设备。

### 功能

- **设备与连接**:`hdc list targets` 自动发现(2 秒轮询,插拔即见)、`tconn`/`tdisconn`
  无线连接与历史端点记忆、`tmode port` 设备监听、重启 hdc 服务;连接代数保护,
  设备重连后拒绝陈旧会话的命令。
- **概览**:型号/品牌/系统版本/API/内核/CPU/内存/`/data` 分区,9 路探测并发。
- **终端**:交互式 `hdc shell`,增量 UTF-8 解码与流式 ANSI 清洗(跨块续接),
  输入历史,可持久化快捷指令栏(JSON 导入/导出)。
- **文件**:设备端定界符脚本列目录、`hdc file send/recv` 上传下载(覆盖需确认)、
  新建/重命名/递归删除(需确认)、拖拽上传。
- **应用**:`bm dump` 列表与详情、`aa start`/`force-stop`、清数据、卸载、
  `hdc install` 安装 HAP(拖拽即装)。
- **进程**:`ps -ef` 表格 3 秒自动刷新、排序搜索、结束进程(需确认)。
- **日志**:`hilog` 实时流,级别着色过滤、关键字、暂停、清缓冲、保存。
- **截图**:`snapshot_display` 截取,适应/原始显示、保存、复制图像。
- **自升级**:GitHub Releases 检查更新、下载(SHA256 校验、进度、可取消)、
  一键换名重启。
- 找不到 hdc 时自动退回内置假后端(`FHDC_FAKE=1` 强制),界面完整可体验。

### 质量

- 六轮独立代码扫描的全部功能级/数据级问题清零。
- 32 个单元测试;`clippy -D warnings` 零告警;CI 覆盖 fmt/clippy/test/release。
