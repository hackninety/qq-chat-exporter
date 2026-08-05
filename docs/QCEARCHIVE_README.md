# QCE Archive (`.qcearchive`) 解析说明

`.qcearchive` 是 QQ Chat Exporter 用于二次开发和离线检索的自描述归档格式。文件本身是一个 ZIP 容器，扩展名固定为 `.qcearchive`。解析工具应先把它解压到临时目录，再以只读方式打开 `messages.sqlite`。

## 容器结构

```text
archive.qcearchive
├─ manifest.json
├─ messages.sqlite
├─ README.md
└─ media/
   ├─ images/
   ├─ videos/
   ├─ audios/
   └─ files/
```

- `manifest.json`：容器版本、生成器版本、会话信息和数量汇总。
- `messages.sqlite`：未加密的标准 SQLite 3 数据库，UTF-8 编码，`PRAGMA user_version = 1`。
- `README.md`：本文档的归档内副本。
- `media/`：已成功下载的媒体。SQLite 中的路径都是相对于解压根目录的 `/` 分隔路径。

ZIP 条目名不允许作为目标绝对路径使用。解压时仍应防御 `..` 和绝对路径，并对总解压大小设上限。

## `manifest.json`

当前 `schemaVersion` 为 `1`，`format` 固定为 `qcearchive`。解析器应当先检查主版本：未知的更高版本可以拒绝读取，但不应根据文件扩展名猜测结构。

主要字段：

| 字段 | 说明 |
| --- | --- |
| `format` | 固定为 `qcearchive` |
| `schemaVersion` | 数据库和容器结构版本 |
| `createdAt` | UTC ISO 8601 生成时间 |
| `generator.name/version` | 生成器和 QCE 版本 |
| `conversation` | `chatType`(1=私聊，2=群聊)、`peerUid`、`peerUin`、名称和头像 |
| `counts` | 消息、参与者、资源引用、已封装媒体、缺失媒体数 |
| `timeRange` | 首尾消息的 Unix 毫秒时间戳，无消息时为 `null` |
| `database.path` | 固定为 `messages.sqlite` |
| `media.root` | 固定为 `media/` |

## SQLite 表

### `archive_meta`

键值元数据，包含 `format`、`schema_version`、`created_at`、`generator_version`、`message_count`。值均为文本。

### `conversations`

归档内会话。版本 1 的单次导出只写入一行，但解析器不应假定永远只有一行。

| 列 | 类型 | 说明 |
| --- | --- | --- |
| `conversation_id` | TEXT PK | 归档内稳定主键 |
| `chat_type` | INTEGER | 1=私聊，2=群聊 |
| `peer_uid` | TEXT | NTQQ 内部 UID，群聊时通常为群号 |
| `peer_uin` | TEXT NULL | 私聊 QQ 号；未解析时为 NULL |
| `display_name` | TEXT | 会话名称 |
| `avatar_url` | TEXT NULL | 导出时的头像 URL |

### `participants`

发送者维表。`participant_id` 是归档内主键，业务匹配时应优先用 `uid`，并在可用时同时保留 `uin`。其余字段包含 `display_name`、`nickname`、`group_card`、`remark`、`title`和可选 `avatar_base64`。

### `messages`

| 列 | 类型 | 说明 |
| --- | --- | --- |
| `message_key` | TEXT PK | 归档内唯一键；不要猜测它等于 QQ 原始 ID |
| `source_message_id` | TEXT | QQ/QCE 源消息 ID，可能重复 |
| `conversation_id` | TEXT FK | 所属会话 |
| `seq` | TEXT | QQ 消息序号，保留为文本避免整数溢出 |
| `timestamp_ms` | INTEGER | Unix 毫秒时间戳 |
| `time_text` | TEXT | QCE 生成的可读时间，仅用于展示 |
| `sender_id` | TEXT FK | `participants.participant_id` |
| `sender_name` | TEXT | 导出时展示名 |
| `is_outgoing` | INTEGER | 1=当前登录账号发送，0=其他，NULL=无法判定 |
| `message_type` | TEXT | QCE 规范化消息类型 |
| `text_content` | TEXT | 用于列表和全文检索的文本 |
| `recalled/system` | INTEGER | 0/1 标记 |
| `content_json` | TEXT(JSON) | 规范化 `content`，其媒体路径已指向 `media/` |
| `raw_json` | TEXT(JSON) NULL | QCE 解析时保留的源消息 |
| `message_json` | TEXT(JSON) | 完整的 QCE `CleanMessage`，用于向前兼容 |

常用索引：

- `idx_messages_conversation_time (conversation_id, timestamp_ms, message_key)`
- `idx_messages_sender_time (sender_id, timestamp_ms)`
- `idx_messages_source_id (source_message_id)`

### `message_elements`

把顶层 `content.elements[]` 展开为行：`message_key`、`element_index`、`element_type`、`data_json`。嵌套合并转发的完整结构仍以 `messages.content_json` / `message_json` 为准。

### `attachments`

每个消息资源引用一行，并通过 `archive_path` 指向容器内文件。

- `message_key` 可为 NULL：这表示资源来自嵌套转发，只能通过 `source_message_id` 追溯。
- `copied = 1` 表示文件已封装；`copied = 0` 表示仅保留元数据。
- `sha256` 是已封装文件的小写十六进制 SHA-256，可用于校验和去重。
- `source_url` 可能是过期的临时地址，解析器不应自动请求它。

### `message_search` (FTS5)

FTS5 虚拟表，字段为 `message_key`(不建索)、`conversation_name`、`sender_name`、`content`。使用 `trigram` tokenizer，便于中文和字符串片段检索。不足 3 个 Unicode 字符的关键字建议回退到 `LIKE`。

## 查询示例

按最后消息时间查看会话：

```sql
SELECT c.conversation_id,
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

按 UID / QQ 号统计发言数：

```sql
SELECT p.uid,
       p.uin,
       p.display_name,
       COUNT(*) AS message_count,
       MAX(m.timestamp_ms) AS last_message_ms
FROM messages AS m
JOIN participants AS p ON p.participant_id = m.sender_id
GROUP BY p.participant_id
ORDER BY message_count DESC;
```

全文检索（绑定参数，不要拼接用户输入）：

```sql
SELECT m.message_key,
       m.timestamp_ms,
       m.sender_name,
       m.text_content
FROM message_search AS s
JOIN messages AS m ON m.message_key = s.message_key
WHERE message_search MATCH ?1
ORDER BY m.timestamp_ms DESC
LIMIT ?2 OFFSET ?3;
```

短关键字回退：

```sql
SELECT message_key, timestamp_ms, sender_name, text_content
FROM messages
WHERE text_content LIKE '%' || ?1 || '%'
ORDER BY timestamp_ms DESC
LIMIT ?2 OFFSET ?3;
```

读取某条消息的媒体：

```sql
SELECT resource_type, file_name, archive_path, mime_type, byte_size, sha256
FROM attachments
WHERE message_key = ?1 AND copied = 1
ORDER BY resource_index;
```

## 解析建议

1. 检查 ZIP 文件头和 `manifest.json`，不要只信任扩展名。
2. 安全解压到独立临时目录；打开 SQLite 时使用只读 URI，例如 `file:messages.sqlite?mode=ro&immutable=1`。
3. 先读 `PRAGMA user_version`，再按版本选择解析器。
4. 列表展示优先读规范化列；需要完整消息语义时解析 `message_json`。
5. 展示媒体前，把 `archive_path` 与解压根目录安全拼接并再次验证边界。
6. 未知列应忽略；不应依赖 SQLite 列的物理顺序或 ZIP 条目顺序。

## 兼容性约定

- 同一 `schemaVersion` 内可能新增可选 manifest 字段、SQLite 列、表或索引；解析器应忽略未知内容。
- 现有列的语义发生不兼容变化时，`schemaVersion` 会升级。
- `message_json` 是保真兼容字段；其子字段可随 QCE 消息解析器演进，不应取代对 `schemaVersion` 的检查。
- 归档中不包含 NTQQ 数据库密钥，`messages.sqlite` 也不是 NTQQ 原始数据库的镜像。
