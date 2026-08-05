# QCE Archive (`.qcearchive`) 解析说明

`.qcearchive` 是 QQ Chat Exporter 提供给离线读取、检索和二次开发工具的自描述归档。文件是 ZIP（支持 ZIP64），扩展名固定为 `.qcearchive`。归档内 SQLite 均为标准、未加密的 SQLite 3 数据库，读取归档不需要 QQ 登录或 NTQQ 数据库 key。

解析器必须先读取 `manifest.json`，再根据 `schemaVersion` 和 `archiveKind` 选择读取方式。不要只根据文件扩展名猜测结构。

## 版本与兼容性

| schemaVersion | archiveKind | 说明 |
| --- | --- | --- |
| 1 | 缺省为 `conversation` | 单个私聊或群聊归档 |
| 2 | `account` | 一个 QQ 账号的一次完整导出快照，可包含多个会话和资源集合 |

解析器应忽略未知的可选 manifest 字段、SQLite 列、表和索引；遇到未知的更高 `schemaVersion` 时可以拒绝读取。v1 的字段和表保持兼容，v2 不改变既有 v1 文件。

## v1：单会话归档

```text
conversation.qcearchive
├─ manifest.json
├─ messages.sqlite
├─ README.md
└─ media/
   ├─ images/
   ├─ videos/
   ├─ audios/
   └─ files/
```

v1 `manifest.json` 使用 `conversation` 描述唯一会话。`messages.sqlite` 的 `conversations` 表当前只有一行，但解析器不应依赖这一点。

## v2：全账号归档

```text
account_123456_20260805_220000.qcearchive
├─ manifest.json
├─ messages.sqlite
├─ README.md
├─ source/
│  └─ nt_msg.sqlite
└─ resources/
   └─ blobs/
      └─ <sha256 前两位>/<sha256>.<扩展名>
```

- `messages.sqlite`：规范化消息、关系状态、FTS5 索引和资源目录，是管理工具应优先读取的数据库。
- `source/nt_msg.sqlite`：用户选定并已由 QCE 解密的源数据库副本。它用于保真存档，不是规范查询接口。
- `resources/blobs/`：消息附件、头像、群文件、群相册和表情等资源的内容寻址存储；相同 SHA-256 只保存一次。
- `README.md`：本说明的归档内副本。

`source/nt_msg.sqlite` 不包含 key，且 `sourceDatabase.encrypted` 固定为 `false`。直接提供仍加密的原始 NTQQ 数据库时，QCE 仍需先完成一次正确解密；这个限制与读取 `.qcearchive` 无关。

## v2 manifest

主要字段如下：

| 字段 | 说明 |
| --- | --- |
| `format` | 固定为 `qcearchive` |
| `schemaVersion` | v2 固定为 `2` |
| `archiveKind` | 固定为 `account` |
| `archiveId` | 本次快照的唯一 ID |
| `createdAt` | UTC ISO 8601 生成时间 |
| `account` | 当前账号 UID、UIN、名称和头像 URL |
| `sources` | 已导入 NTQQ 数据库与当前 NapCat 账号的数据源说明 |
| `counts` | 会话、消息、资源、缺失资源、警告和分类数量 |
| `timeRange` | 全账号首尾消息的 Unix 毫秒时间戳 |
| `coverage` | 是否完整、警告数和缺失资源数；`complete=false` 时应提示用户 |
| `database` | 规范库路径、版本和 FTS 信息 |
| `sourceDatabase` | 源库路径、格式、字节数和 SHA-256 |
| `resources` | Blob 根目录和哈希算法 |

## messages.sqlite 公共表

### `archive_meta`

键值元数据，包括格式、归档类型、版本、生成时间和数量统计。

### `accounts`（v2）

账号维表：`account_id`、`uid`、`uin`、`display_name`、`avatar_url`。

### `conversations`

v1 包含会话 ID、`chat_type`、UID/UIN、名称和头像。v2 增加：

| 列 | 说明 |
| --- | --- |
| `account_id` | 所属 `accounts.account_id` |
| `category` | `private`、`group` 或 `other` |
| `relationship_status` | `friend`、`non_friend`、`group`、`unavailable_group` 或 `other` |
| `source` | 会话数据来源，例如 `backup`、`live` 或 `merged` |

`chat_type` 保存 NTQQ/NapCat 原始数值，不应只假定为 1 或 2。`conversation_aliases` 保存同一私聊的 UID/UIN 等匹配别名。

### `participants`

发送者维表。业务匹配优先使用 `uid`，同时保留可用的 `uin`、昵称、群名片、备注、头衔和可选 Base64 头像。

### `messages`

| 列 | 说明 |
| --- | --- |
| `message_key` | 归档内唯一键；v2 会包含会话前缀 |
| `source_message_id` | QQ/QCE 源消息 ID，可能重复 |
| `conversation_id` | 所属会话 |
| `seq` | QQ 消息序号，以文本保存 |
| `timestamp_ms` | Unix 毫秒时间戳 |
| `sender_id/sender_name` | 发送者引用与导出时展示名 |
| `is_outgoing` | 1=当前账号发送，0=其他，NULL=无法判断 |
| `message_type/text_content` | 规范消息类型与检索文本 |
| `recalled/system` | 撤回与系统消息标记 |
| `content_json` | 规范化内容和归档内资源路径 |
| `raw_json` | 可选的源消息 JSON |
| `message_json` | 完整 QCE `CleanMessage` 兼容载荷 |

### `message_elements`

把顶层 `content.elements[]` 展开为行。合并转发等嵌套结构仍以 `content_json` / `message_json` 为准。

### `attachments`

消息资源引用。v1 通过 `archive_path` 指向 `media/`；v2 同时以 `sha256` 引用 `resource_blobs`。`copied=0` 表示仅保留元数据，通常对应失效链接或无法访问的历史资源。

### `resource_blobs`（v2）

内容寻址资源表：`sha256`、`archive_path`、`byte_size`、`mime_type`。读取资源前应校验路径边界，必要时校验 SHA-256。

### `extra_resources`（v2）

账号级资源目录。`kind` 可表示头像、表情包、群文件、群相册等；`conversation_id`、`collection_id`、`logical_id` 和 `parent_id` 用于恢复群与目录层级，`metadata_json` 保存来源元数据。

### `export_warnings`（v2）

导出覆盖警告，包含 `scope`、`code`、`message` 和可选会话/资源 ID。管理工具应在 `coverage.complete=false` 时展示这些记录。

### `message_search`

FTS5 虚拟表，字段为 `message_key`（不索引）、`conversation_name`、`sender_name`、`content`，使用 `trigram` tokenizer。少于 3 个 Unicode 字符的关键词建议回退到 `instr` 或参数化 `LIKE`。

## 常用查询

账号快照中的会话列表：

```sql
SELECT c.conversation_id,
       c.category,
       c.relationship_status,
       c.display_name,
       c.peer_uid,
       c.peer_uin,
       COUNT(m.message_key) AS message_count,
       MAX(m.timestamp_ms) AS last_message_ms
FROM conversations AS c
LEFT JOIN messages AS m USING (conversation_id)
GROUP BY c.conversation_id
ORDER BY last_message_ms DESC;
```

限定会话全文检索：

```sql
SELECT m.message_key,
       m.timestamp_ms,
       m.sender_name,
       m.text_content
FROM message_search AS s
JOIN messages AS m ON m.message_key = s.message_key
WHERE message_search MATCH ?1
  AND m.conversation_id = ?2
ORDER BY m.timestamp_ms DESC
LIMIT ?3 OFFSET ?4;
```

读取消息附件：

```sql
SELECT a.resource_type,
       a.file_name,
       a.archive_path,
       a.mime_type,
       a.byte_size,
       a.sha256
FROM attachments AS a
WHERE a.message_key = ?1 AND a.copied = 1
ORDER BY a.resource_index;
```

读取群文件、相册或表情资源：

```sql
SELECT kind,
       conversation_id,
       collection_id,
       logical_id,
       parent_id,
       display_name,
       blob_sha256,
       metadata_json
FROM extra_resources
WHERE kind = ?1
ORDER BY conversation_id, collection_id, parent_id, display_name;
```

## `.debug` 旁路目录

启用调试导出时，会在归档旁生成同名 `<归档名>.debug/` 目录。它不是 `.qcearchive` 的必需部分，也不应重复导入为聊天消息。目录保存会话映射、原始/解析/最终 JSONL、资源调用事件、警告和完成/失败摘要，用于排查导出覆盖问题。

## 安全与完整性

1. 解压时拒绝绝对路径、`..` 和越出目标根目录的条目。
2. 使用只读连接打开 `messages.sqlite`，例如 `Mode=ReadOnly` 或 `file:messages.sqlite?mode=ro&immutable=1`。
3. 先读取 `PRAGMA user_version`，再选择 v1/v2 解析器。
4. 资源按 manifest 指定的 `/` 分隔路径读取；不要自动请求归档中遗留的临时 `source_url`。
5. 大归档可能使用 ZIP64；解析器和文件系统必须支持超过 4 GiB 的条目。
6. 未知列和表应忽略；不要依赖 SQLite 行的物理顺序或 ZIP 条目顺序。
