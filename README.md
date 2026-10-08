# name-match-mcp

一个基于 Rust 的 stdio MCP（Model Context Protocol）服务：读取两个 txt 名称清单（每行一个名称），
把待匹配集合 B 的每一行与参考集合 A 做名称匹配，结果写成 CSV 文件。

输入输出都走文件路径，名称清单不经过 JSON-RPC 报文，因此支持任意规模、也不会占用调用方的上下文。

## 功能

暴露单个 tool `match_names`：

| 参数 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `reference_path` | `string` | 是 | 参考名称集合 A 的 txt 路径，每行一个名称 |
| `target_path` | `string` | 是 | 待匹配名称集合 B 的 txt 路径，每行一个名称 |
| `output_path` | `string` | 是 | 结果 CSV 的写出路径；相对路径按服务进程工作目录解析 |
| `threshold` | `number` | 否 | 匹配阈值，取值 `0.0..=1.0`，默认 `0.6` |

返回的是摘要而不是明细（明细都在 CSV 里），这样可以做到「结果条数再多也不回到报文里」：

```json
{
  "csv_path": "C:\\work\\匹配结果.csv",
  "reference_count": 1559,
  "target_count": 1440,
  "result_count": 1440,
  "matched_count": 1440,
  "unmatched_count": 0,
  "exact_matches": 883,
  "elapsed_ms": 262
}
```

- `result_count` 恒等于 `target_count`，即 B 的每一行都有且只有一条结果。
- `matched_count + unmatched_count == result_count`。
- `exact_matches` 是归一化后完全相等的行数（`匹配度` 为 1）。

## 输出 CSV

三列、UTF-8 BOM、逗号分隔、CRLF 行尾，行顺序严格对应 B 的顺序：

```csv
待匹配名称,匹配名称,匹配度
12厘W2510云灰ENF-长沙万华,12厘W2510云灰ENF-长沙万华,1
京东世纪贸易,北京京东世纪贸易有限公司,0.7197
完全不相关的名字,,
```

- `匹配名称` 在低于 `threshold` 时写空字段（不写字面量 `null`），便于 Excel 里直接筛选出来。
- 名称里若含逗号、双引号或换行，会按 RFC 4180 加引号转义，行结构不会被破坏。

## 文件与路径规则

- **编码**：先按 UTF-8 解码，失败则回退 GBK（中文 Windows 另存为 ANSI 的 txt 很常见）；自动剥离 UTF-8 BOM。
- **行处理**：`\n` 与 `\r\n` 都支持；跳过 trim 后为空的行；其余行 trim 后作为名称；不识别表头。
- **目录**：`output_path` 的父目录不存在会自动创建；同名文件会被覆盖（方便重跑）。
- **保护输入**：`output_path` 与 `reference_path` 或 `target_path` 指向同一文件时直接报错拒绝，不会覆盖输入数据。
- **错误**：文件不存在/不是普通文件/读失败返回 `invalid_params`（报文里带具体路径）；CSV 写失败返回 `internal_error`。
- **边界**：B 为空时写出「仅表头」的 CSV，`result_count` 为 0；A 为空时每行都为空匹配、`匹配度` 为 0。

## 匹配算法

1. **归一化**：全角转半角、Unicode 大小写折叠、去掉所有空白与标点/括号类字符（`京东 科技（北京）有限公司`
   与 `京东科技北京有限公司` 归一为同一键）。归一化后完全相等直接判 `1.0`。
2. **候选召回**：为 A 建立归一化字符 bigram 倒排索引，对每个 B 取共享 bigram 最多的前 200 个候选；
   单字名称等无 bigram 的情况回退为全量比较。
3. **打分**：候选上计算 Jaro-Winkler（权重 0.7）与字符 bigram Jaccard（权重 0.3）的加权均值，
   取最高分；同分时取 A 中下标最小者，保证输出确定性。

B 侧使用 rayon 并行，A 的归一化结果与索引只构建一次。A 中同一条名称可以被多个 B 行同时命中。

## 构建

### Windows x64（一键脚本）

```bat
build-x64.bat
```

脚本会：切换到自身所在目录 → 检查并自动补装 `x86_64-pc-windows-msvc` target →
执行 `cargo build --release --target x86_64-pc-windows-msvc` → 把产物复制到 `dist\` 并打印路径与大小。
任一步失败都会打印可读错误并返回非零退出码。

### 通用 Cargo 命令

```powershell
cargo build --release
cargo build --release --target x86_64-pc-windows-msvc
```

产物路径：

- 默认构建：`target\release\name-match-mcp.exe`
- 指定 target：`target\x86_64-pc-windows-msvc\release\name-match-mcp.exe`
- `build-x64.bat` 拷贝后：`dist\name-match-mcp.exe`

## 测试

```powershell
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
```

单元测试覆盖归一化、Jaro-Winkler / Jaccard 数值、精确命中、阈值边界、输出长度与顺序、
同一 A 项被多个 B 命中、空集合边界、1 万 × 1 万性能，以及文件层（UTF-8/GBK 解码、BOM、混合换行、
空行、CSV 转义、父目录创建、输出路径冲突）。`tests/stdio.rs` 会真实启动编译产物，走完
`initialize → tools/list → tools/call`，并检查 CSV 内容、行数与摘要计数。

## 在 MCP 客户端中注册

以 JSON 配置为例（把路径替换为你的实际产物路径），建议把工作目录设成数据所在目录，
这样调用时可以只传相对路径：

```json
{
  "mcpServers": {
    "name-match": {
      "command": "C:\\path\\to\\dist\\name-match-mcp.exe",
      "args": [],
      "cwd": "C:\\work\\名称匹配"
    }
  }
}
```

服务通过 stdin/stdout 使用 JSON-RPC，stdout 只承载协议消息；诊断信息只写 stderr。

## 直接调用示例

改完代码或换了数据后，可以这样手工验证一次（PowerShell 7）：

```powershell
$server = ".\dist\name-match-mcp.exe"
$init = '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"probe","version":"1.0"}}}'
$ready = '{"jsonrpc":"2.0","method":"notifications/initialized"}'
$call = @{ jsonrpc='2.0'; id=2; method='tools/call'; params=@{
    name='match_names'
    arguments=@{
        reference_path = '参考名称.txt'
        target_path    = '待匹配名称.txt'
        output_path    = '匹配结果.csv'
    }
} } | ConvertTo-Json -Compress -Depth 8

@($init, $ready, $call) | & $server | ForEach-Object { $_ }
Import-Csv -LiteralPath '匹配结果.csv' | Select-Object -First 5
```

在 Codex 之类的 MCP 客户端里则直接让模型调用 tool 即可，例如：
「用 name-match 把 `待匹配名称.txt` 和 `参考名称.txt` 匹配，结果写 `匹配结果.csv`」。

参考数据集（A 约 1.5 千条、B 约 1.4 千条，板材名称）实测约 0.26 秒返回，
其中约 61% 为归一化精确命中。剩余条目里若出现 0.67–0.74 的弱匹配，
通常是该型号在 A 中确实不存在（实测 `春韵别处`、`云令涧`、`次板`、`工程单` 等），
把 `threshold` 提到 `0.75` 左右即可过滤掉这类噪声。

## 项目结构

- `src/lib.rs`：归一化、相似度算法、候选召回与 `match_names` 主逻辑（纯函数，可单测）。
- `src/files.rs`：txt 读取（编码回退、行切分）与 CSV 渲染、写出、路径冲突判断。
- `src/server.rs`：MCP tool 定义、参数校验与 `ServerHandler` 实现。
- `src/main.rs`：tokio 启动 stdio 服务。
- `tests/stdio.rs`：子进程 stdio 集成测试。
- `build-x64.bat`：Windows x64 构建与产物拷贝脚本。
