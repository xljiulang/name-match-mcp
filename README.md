# name-match-mcp

一个基于 Rust 的 stdio MCP（Model Context Protocol）服务：在**同一个 xlsx 工作簿内**，
用一张表的某列作为标准名称，匹配另一张表的某列，结果以**新增两列**的形式写回原工作簿。

典型用途是把上游工序的标准商品名对齐到下游工序的单据上，例如用 `9月压面!商品名称`
匹配 `9月贴面!组装商品1`，在贴面表里补出 `匹配名称` 与 `匹配度`。

## 功能

暴露单个 tool `match_workbook_column`：

| 参数 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `xlsx_path` | `string` | 是 | 待处理工作簿，**原地修改** |
| `reference_sheet` | `string` | 是 | 标准名称所在工作表，如 `9月压面` |
| `reference_column` | `string` | 是 | 标准名称列的表头文字，如 `商品名称` |
| `target_sheet` | `string` | 是 | 待匹配工作表，如 `9月贴面` |
| `target_column` | `string` | 是 | 待匹配列的表头文字，如 `组装商品1` |
| `header_row` | `number` | 否 | 表头行号，数据从其下一行开始，默认 `1` |
| `match_column_name` | `string` | 否 | 匹配名称列表头，默认 `匹配名称` |
| `score_column_name` | `string` | 否 | 匹配度列表头，默认 `匹配度` |
| `threshold` | `number` | 否 | 模糊匹配阈值 `0.0..=1.0`，默认 `0.6` |

返回摘要（明细都在工作簿里）：

```json
{
  "xlsx_path": "C:\\work\\统计.xlsx",
  "backup_path": "C:\\work\\统计.bak-20261009-014224.xlsx",
  "reference_count": 2804,
  "rows_scanned": 2487,
  "matched_count": 2485,
  "unmatched_count": 2,
  "exact_count": 2478,
  "fuzzy_count": 7,
  "match_column": "Q",
  "score_column": "R",
  "reused_columns": false,
  "elapsed_ms": 12
}
```

- `rows_scanned` 是目标列里非空的数据行数；`exact_count + fuzzy_count + unmatched_count` 恒等于它。
- `match_column` / `score_column` 是实际写入的列字母，便于直接去表里找。

## 行为

- **先精确后模糊**：目标值与标准值在「全角半角折叠 + 大小写折叠 + 去掉空白与标点括号」后完全相等，
  直接判 `匹配度 = 1`；否则用 Jaro-Winkler（0.7）+ 字符 bigram Jaccard（0.3）打分取最优，
  低于 `threshold` 时 `匹配名称` 留空、`匹配度` 照写，方便筛选排查。
- **新增列，保留原列**：结果写在整张表最后一列的右侧，原有列（含 `组装商品1`）一字不改。
- **幂等**：若表头行已存在同名的结果列，则复用该列覆盖写，不会越跑列越多。
- **强制备份**：每次修改前先在旁边生成 `<名>.bak-<yyyyMMdd-HHmmss>.xlsx`，内容与改前完全一致，
  可随时回滚；备份不会被自动删除。
- **只动目标表**：其余工作表、透视表、图片、打印设置等 zip 条目按原始字节复制，不做整体重存。
- 空单元格跳过；只接受 `.xlsx`，`.xls`/`.xlsm` 会报 `invalid_params`。
- 找不到工作表时错误信息会列出全部工作表名；找不到列时列出该表头行的所有列名。

## 匹配算法

1. **归一化**：全角转半角、Unicode 大小写折叠、去掉所有空白与标点/括号类字符
   （`京东 科技（北京）有限公司` 与 `京东科技北京有限公司` 归一为同一键）。
2. **精确查表**：归一化后的标准值建哈希表，命中即 1.0。实测真实数据 2487 行里 2478 行走这条路。
3. **候选召回**：为未命中的值建立字符 bigram 倒排索引，取共享 bigram 最多的前 200 个候选，
   单字名称等无 bigram 的情况回退全量。
4. **打分**：候选上取 Jaro-Winkler 与 bigram Jaccard 的加权均值，同分取标准列中靠前者。

## 构建

### Windows x64（一键脚本）

```bat
build-x64.bat
```

脚本会：切到自身所在目录 → 检查并自动补装 `x86_64-pc-windows-msvc` target →
执行 `cargo build --release --target x86_64-pc-windows-msvc` → 把产物复制到 `dist\` 并打印路径与大小。
任一步失败都会打印可读错误并返回非零退出码。

产物路径：

- 默认构建：`target\release\name-match-mcp.exe`
- 指定 target：`target\x86_64-pc-windows-msvc\release\name-match-mcp.exe`
- `build-x64.bat` 拷贝后：`dist\name-match-mcp.exe`

## 测试

```powershell
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
```

覆盖内容：归一化与相似度数值、精确优先与模糊兜底、计数分区不变式；xlsx 解析（共享字符串、
行内字符串、空单元格、自闭合行、dimension / autoFilter 扩展、列字母换算）；写入行为
（新增列、幂等复用、数值型分数、空匹配留空但保留样式）；错误场景（工作表/列不存在、
非 xlsx、表头行越界、阈值越界）；stdio 端到端 `initialize → tools/list → tools/call`。

## 在 MCP 客户端中注册

```json
{
  "mcpServers": {
    "name-match": {
      "command": "D:\\mcps\\name-match\\name-match-mcp.exe",
      "args": [],
      "cwd": "C:\\work"
    }
  }
}
```

服务通过 stdin/stdout 使用 JSON-RPC，stdout 只承载协议消息；诊断信息只写 stderr。

## 直接调用示例

```powershell
$server = ".\dist\name-match-mcp.exe"
$init   = '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"probe","version":"1.0"}}}'
$ready  = '{"jsonrpc":"2.0","method":"notifications/initialized"}'
$call   = @{ jsonrpc='2.0'; id=2; method='tools/call'; params=@{
    name='match_workbook_column'
    arguments=@{
        xlsx_path        = '统计.xlsx'
        reference_sheet  = '9月压面'
        reference_column = '商品名称'
        target_sheet     = '9月贴面'
        target_column    = '组装商品1'
    }
} } | ConvertTo-Json -Compress -Depth 8

@($init, $ready, $call) | & $server | Select-String '"id":2'
```

在 Codex 之类的 MCP 客户端里直接说需求即可，例如：
「用 name-match 把 `统计.xlsx` 里 9月压面的商品名称匹配到 9月贴面的组装商品1」。

## 项目结构

- `src/lib.rs`：归一化、相似度算法、候选召回、备份路径与时间戳。
- `src/xlsx.rs`：xlsx 外科手术——按 zip 条目读写、sheet XML 定位与改列、dimension/autoFilter 扩展。
- `src/replacement.rs`：纯逻辑编排——精确查表、模糊兜底、写回值生成。
- `src/server.rs`：MCP tool 定义、参数校验与 `ServerHandler`。
- `src/test_support.rs`：测试用最小 xlsx 构造器。
- `src/main.rs`：tokio 启动 stdio 服务。
- `tests/stdio.rs`：子进程 stdio 集成测试。
- `build-x64.bat`：Windows x64 构建与产物拷贝脚本。

## 已知边界

- 新列写在整张表最后一列的右侧；若该位置已被占用，会占用其右侧紧邻的空列。
- 不刷新透视表缓存，Excel 打开后按需自行刷新。
- 工作簿被 Excel 占用时写入会失败并提示先关闭文件（返回 `internal_error`，原文件保持不变）。
