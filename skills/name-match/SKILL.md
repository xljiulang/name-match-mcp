---
name: name-match
description: 用同一工作簿里某张表某列的标准名称，匹配另一张表某列并把「匹配名称」「匹配度」写回原表；适用于 xlsx 名称对齐、跨表按名称补列、检查某列哪些名称对不上标准名。
---

# 名称匹配（name-match CLI）

调用 `name-match` 命令行工具，完成「一张表的名称列 → 按另一张表的标准名称匹配 → 回写原工作簿」。
匹配、写回、统计都由该工具完成，不要自己用脚本重写这些逻辑。

## 何时使用

- 用户要求把某个 xlsx 里的某列名称，按同一工作簿另一张表的名称列对齐/替换/补列。
- 典型场景：`9月压面!商品名称` 是标准名称，`9月贴面!组装商品1` 需要对齐到它。

不要用于：跨工作簿匹配（本工具只在同一工作簿内工作）、`.xls`/`.xlsm`、以及需要人工确认后才写入的场景。

## 调用方式

```bash
name-match match \
  --workbook 统计.xlsx \
  --reference-sheet 9月压面 --reference-column 商品名称 \
  --target-sheet 9月贴面 --target-column 组装商品1
```

五个必填参数：`--workbook`（工作簿路径）、`--reference-sheet` / `--reference-column`（标准名称的表与列）、
`--target-sheet` / `--target-column`（待匹配的表与列）。可选：

| 参数 | 默认 | 说明 |
|---|---|---|
| `--header-row` | `1` | 表头行号（从 1 开始），数据从下一行读 |
| `--match-column-name` | `匹配名称` | 结果列的表头 |
| `--score-column-name` | `匹配度` | 分数列的表头 |
| `--threshold` | `0.6` | 模糊匹配最低分，`0.0~1.0` |

`--help` 与 `--version` 可用。若 `name-match` 不在 PATH 上，用它的完整路径调用（例如
`D:\mcps\name-match\name-match.exe`）；部署位置由用户决定，不要凭空假设。

## 执行前必须确认

1. **先确认工作簿没有在 Excel 里打开**：被占用时写入会失败（退出码 1、stderr 提示），此时应先请用户关闭。
2. **没有备份**：工具原地覆盖原文件且不生成备份。若这是重要文件，先提醒用户自行复制一份，或在动手前
   用 `Copy-Item` 复制到你自己的临时目录再操作。
3. **确认真实参数值**：不要猜表名和列名。若不确定，先用 Python（`openpyxl`）读一下工作簿的
   `sheetnames` 和表头行文字，再传参。传错时工具会报错并列出候选，可据此修正。

## 输出解读

stdout 只有 JSON；成功时形如：

```json
{"xlsx_path":"…","sheet":"9月贴面","column":"组装商品1","reference_count":2804,
 "rows_scanned":2487,"matched_count":2485,"unmatched_count":2,"exact_count":2478,
 "fuzzy_count":7,"match_column":"Q","score_column":"R","reused_columns":false,"elapsed_ms":93}
```

- `exact_count` 是归一化后完全相等的行数，`fuzzy_count` 是走相似度的行数，两者之和即 `matched_count`。
- `unmatched_count > 0` 说明这些行低于 `--threshold`：它们的「匹配名称」为空、「匹配度」仍有分数。
  需要复核时，读回该列（`--match-column-name` 那列）空值的行，向用户说明这些名称在标准列里找不到，
  必要时提高或降低 `--threshold` 重跑（重跑会覆盖同一对结果列，不会叠加）。
- `reused_columns` 为 `true` 表示复用了已有结果列（重复调用），`false` 表示本次新追加了两列。
- `match_column` / `score_column` 是实际写入的列字母，向用户汇报时给出这两个字母最直观。

失败时 stdout 是 `{"error":{"kind":"usage|invalid|io","message":"…"}}`，退出码为 1。
把 `message` 原样或转述给用户，不要吞掉。

## 结果怎么用

- 工具只在工作簿里加两列（默认「匹配名称」「匹配度」），**原列不动**，所以可以直接让用户在 Excel 里
  筛选「匹配名称」为空的行做人工确认，或按「匹配度」排序复核低分项。
- 结果列只反映**最后一次**调用：换 `--target-column` 重跑会清空上一批结果。要同时保留多列结果，
  请用 `--match-column-name` / `--score-column-name` 指定不同列名。
- 一次调用只处理一个工作簿的一列；多列需要多次调用，且必须**等上一次返回后再发起下一次**
  （同一文件并发调用不保证结果正确，也没有备份可回退）。
