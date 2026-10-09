# name-match

一个 Rust 编写的命令行工具：在**同一个 xlsx 工作簿内**，用一张表的某列作为标准名称，
匹配另一张表的某列，结果以**新增两列**的形式写回原工作簿。

典型用途是把上游工序的标准商品名对齐到下游工序的单据上，例如用 `9月压面!商品名称`
匹配 `9月贴面!组装商品1`，在贴面表里补出 `匹配名称` 与 `匹配度`。

配套提供一个 Codex skill（`skills/name-match/`），让模型在该场景下直接调用本工具。

## 命令行

```bash
name-match match \
  --workbook 统计.xlsx \
  --reference-sheet 9月压面 --reference-column 商品名称 \
  --target-sheet 9月贴面 --target-column 组装商品1 \
  [--header-row 1] \
  [--match-column-name 匹配名称] [--score-column-name 匹配度] \
  [--threshold 0.6]
```

`name-match --help` 输出完整中文用法，`name-match --version` 输出版本。

### 参数

| 参数 | 必填 | 默认 | 说明 |
|---|---|---|---|
| `--workbook` | 是 | — | 待处理的 `.xlsx`，**原地修改**（不生成备份） |
| `--reference-sheet` | 是 | — | 标准名称所在工作表，如 `9月压面` |
| `--reference-column` | 是 | — | 标准名称列的列名（表头文字），如 `商品名称` |
| `--target-sheet` | 是 | — | 待匹配工作表，如 `9月贴面` |
| `--target-column` | 是 | — | 待匹配列的列名，如 `组装商品1`；该列原值不动 |
| `--header-row` | 否 | `1` | 表头行号（从 1 开始），数据从下一行读 |
| `--match-column-name` | 否 | `匹配名称` | 匹配名称列的表头 |
| `--score-column-name` | 否 | `匹配度` | 匹配度列的表头 |
| `--threshold` | 否 | `0.6` | 模糊匹配最低分 `0.0~1.0` |

支持 `--flag value` 与 `--flag=value` 两种写法；参数可省略子命令前的 `--`。

### 输出

**stdout 只输出 JSON**，诊断信息写 stderr。成功：

```json
{"xlsx_path":"C:\\work\\统计.xlsx","sheet":"9月贴面","column":"组装商品1",
 "reference_count":2804,"rows_scanned":2487,"matched_count":2485,"unmatched_count":2,
 "exact_count":2478,"fuzzy_count":7,"match_column":"Q","score_column":"R",
 "reused_columns":false,"elapsed_ms":93}
```

- `exact_count + fuzzy_count + unmatched_count` 恒等于 `rows_scanned`（目标列非空数据行数）。
- `match_column` / `score_column` 是实际写入的列字母。
- `reused_columns` 为 `true` 表示复用了已有结果列，`false` 表示本次新追加。

失败时输出 `{"error":{"kind":"usage|invalid|io","message":"…"}}`。

### 退出码

成功 `0`，**任何失败 `1`**。`error.kind` 供脚本细分：`usage`（参数问题）、
`invalid`（工作表/列不存在、非 xlsx 等）、`io`（读写失败）。

## 行为

- **先精确后模糊**：目标值与标准值在「全角半角折叠 + 大小写折叠 + 去掉空白与标点括号」后完全相等，
  直接判 `匹配度 = 1`；否则用 Jaro-Winkler（0.7）+ 字符 bigram Jaccard（0.3）打分取最优，
  低于 `--threshold` 时 `匹配名称` 留空、`匹配度` 照写，便于筛选复核。
- **新增列，保留原列**：结果写在工作表最后一列的右侧，原有列（含目标列）一字不改。
- **可重复调用，结果列会被覆盖**：已有同名结果列时复用该列并**整列重建**——先清空该列数据行
  再写入本次结果。因此同一参数跑多次不会产生重复单元格、也不会越跑列越多；换 `--target-column`
  重跑会清空上一批结果，这对结果列只反映**最后一次**调用。
- **只动目标表**：其余工作表、透视表、图片、打印设置等 zip 条目按原始字节复制，不做整体重存。
- 空单元格跳过；只接受 `.xlsx`，`.xls`/`.xlsm` 报错。
- 找不到工作表时错误信息会列出全部工作表名；找不到列时列出该表头行的所有列名。

## 匹配算法

1. **归一化**：全角转半角、Unicode 大小写折叠、去掉所有空白与标点/括号类字符
   （`京东 科技（北京）有限公司` 与 `京东科技北京有限公司` 归一为同一键）。
2. **精确查表**：归一化后的标准值建哈希表，命中即 1.0。实测真实数据 2487 行里 2478 行走这条路。
3. **候选召回**：未命中的值用字符 bigram 倒排索引取共享 bigram 最多的前 200 个候选，
   单字名称等无 bigram 的情况回退全量。
4. **打分**：候选上取 Jaro-Winkler 与 bigram Jaccard 的加权均值，同分取标准列中靠前者。

## 构建

### Windows x64（一键脚本）

```bat
build-x64.bat
```

切到脚本所在目录 → 检查并自动补装 `x86_64-pc-windows-msvc` target →
`cargo build --release --target x86_64-pc-windows-msvc` → 产物复制到 `dist\`。任一步失败返回非零。

产物路径：

- 默认构建：`target\release\name-match.exe`
- 指定 target：`target\x86_64-pc-windows-msvc\release\name-match.exe`
- `build-x64.bat` 拷贝后：`dist\name-match.exe`

把该 exe 放到 PATH 可访问的目录后，skill 里就能直接用 `name-match` 调用。

## 测试

```powershell
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
```

覆盖内容：归一化与相似度数值、精确优先与模糊兜底、计数分区不变式；xlsx 解析（共享字符串、
行内字符串、空单元格、自闭合行、dimension / autoFilter 扩展、列字母换算）；写入行为
（新增列、重复调用不产生重复单元格、换目标列时清空上一批结果、自定义结果列名、数值型分数、
空匹配留空但保留样式）；参数解析（必填/可选/`=` 写法/重复/未知/非法数值）；错误场景
（工作表或列不存在、非 xlsx、表头行越界、阈值越界）；以及进程级 CLI 端到端
（stdout JSON、退出码、`--help`/`--version`）。

## Codex skill

skill 源码在仓库内 `skills/name-match/`（含 `SKILL.md` 与 `agents/openai.yaml`）。
Codex 只从 `$CODEX_HOME/skills`（本机为 `C:\Users\laojiu\.codex\skills`）自动发现 skill，
因此仓库内这份需要**安装/拷贝**到该目录才生效，本机已安装：

```powershell
Copy-Item -Recurse skills\name-match "$env:USERPROFILE\.codex\skills\"
```

更新 skill 后重复上面命令覆盖即可。

## 项目结构

- `src/lib.rs`：归一化、相似度算法、候选召回与路径解析。
- `src/xlsx.rs`：xlsx 外科手术——按 zip 条目读写、sheet XML 定位与改列、结果列整列重建。
- `src/replacement.rs`：纯逻辑编排——精确查表、模糊兜底、写回值生成。
- `src/cli.rs`：参数解析、任务编排、JSON 输出与错误分类。
- `src/main.rs`：进程入口（退出码与 stdout/stderr 约定）。
- `src/test_support.rs`：测试用最小 xlsx 构造器。
- `tests/cli.rs`：进程级 CLI 集成测试。
- `skills/name-match/`：Codex skill 源码。
- `build-x64.bat`：Windows x64 构建与产物拷贝脚本。

## 已知边界

- **没有备份、没有锁**：直接原地修改工作簿。写入走「同目录临时文件 + 原子替换」，单次写入要么
  完整生效、要么原文件不变；但**并发对同一文件调用不保证结果正确**，且出错时没有备份可回退，
  请自行保留副本、避免并发（多列匹配应串行调用）。
- 新列写在整张表最后一列的右侧；若该位置已被占用，会占用其右侧紧邻的空列。
- 不刷新透视表缓存，Excel 打开后按需自行刷新。
- 工作簿被 Excel 占用时写入会失败（退出码 1，原文件保持不变），先关闭文件再重试。
- 结果列固定为「匹配名称」「匹配度」这一对；要保留多列结果请用 `--match-column-name` /
  `--score-column-name` 指定不同列名。一次调用只处理一个工作簿的一列。
