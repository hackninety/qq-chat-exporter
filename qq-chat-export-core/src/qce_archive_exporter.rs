use crate::base::{now_iso, preprocess_messages, resource_type_dir, ExporterContext};
use crate::error::{ExportError, ExportResultT};
use crate::types::{
    ChatInfo, CleanMessage, ExportFormat, ExportOptions, ExportOutcome, MessageResource,
};
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const ARCHIVE_SCHEMA_VERSION: i64 = 1;
const ARCHIVE_APPLICATION_ID: i64 = 0x5143_4541; // "QCEA"
const ARCHIVE_README: &str = include_str!("../../docs/QCEARCHIVE_README.md");

/// QCE Archive 格式选项。
#[derive(Debug, Clone, Default)]
pub struct QceArchiveFormatOptions {
    /// 主程序版本，写入 manifest 和 SQLite 元数据。
    pub exporter_version: Option<String>,
}

/// 可供二次开发工具读取的 SQLite + 媒体归档导出器。
pub struct QceArchiveExporter {
    ctx: ExporterContext,
    format_options: QceArchiveFormatOptions,
}

impl QceArchiveExporter {
    /// 创建导出器。
    #[must_use]
    pub fn new(options: ExportOptions, format_options: QceArchiveFormatOptions) -> Self {
        Self {
            ctx: ExporterContext::new(ExportFormat::QceArchive, options),
            format_options,
        }
    }

    /// 导出 `.qcearchive`。
    pub async fn export(
        self,
        messages: Vec<CleanMessage>,
        chat_info: &ChatInfo,
    ) -> ExportResultT<ExportOutcome> {
        let started = Instant::now();
        let messages = preprocess_messages(messages);
        self.ctx.ensure_output_directory().await?;
        self.ctx.check_cancelled()?;
        self.ctx
            .update_progress(0, messages.len(), "正在生成 QCE Archive...");

        let output_path = self.ctx.options.output_path.clone();
        if tokio::fs::try_exists(&output_path).await.unwrap_or(false) {
            return Err(ExportError::OutputDirConflict(output_path));
        }

        let chat_info = chat_info.clone();
        let resource_map = self.ctx.options.resource_map.clone();
        let exporter_version = self
            .format_options
            .exporter_version
            .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_owned());
        let cancellation = self.ctx.cancellation.clone();
        let message_count = messages.len();
        let build = tokio::task::spawn_blocking(move || {
            build_archive(
                &output_path,
                messages,
                &chat_info,
                &resource_map,
                &exporter_version,
                &cancellation,
            )
        })
        .await??;

        self.ctx
            .update_progress(message_count, message_count, "QCE Archive 已生成");
        Ok(ExportOutcome {
            task_id: String::new(),
            format: ExportFormat::QceArchive,
            file_path: self.ctx.options.output_path,
            file_size: build.file_size,
            message_count,
            resource_count: build.media_file_count,
            export_time: started.elapsed().as_millis(),
            completed_at: now_iso(),
        })
    }
}

#[derive(Debug)]
struct ArchiveBuildOutcome {
    file_size: u64,
    media_file_count: usize,
}

#[derive(Debug, Clone)]
struct PreparedAttachment {
    source_message_id: String,
    resource_index: usize,
    resource_type: String,
    file_name: Option<String>,
    byte_size: Option<u64>,
    source_url: Option<String>,
    archive_path: Option<String>,
    mime_type: Option<String>,
    sha256: Option<String>,
    copied: bool,
}

#[derive(Debug)]
struct MediaBuildResult {
    attachments: Vec<PreparedAttachment>,
    rewrite_map: HashMap<String, String>,
    copied_file_count: usize,
}

#[derive(Debug)]
struct DatabaseBuildResult {
    participant_count: usize,
    attachment_count: usize,
    missing_attachment_count: usize,
    first_message_ms: Option<i64>,
    last_message_ms: Option<i64>,
}

fn build_archive(
    output_path: &Path,
    messages: Vec<CleanMessage>,
    chat_info: &ChatInfo,
    resource_map: &HashMap<String, Vec<MessageResource>>,
    exporter_version: &str,
    cancellation: &crate::types::CancellationToken,
) -> ExportResultT<ArchiveBuildOutcome> {
    let parent = output_path.parent().unwrap_or_else(|| Path::new("."));
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let suffix = format!("{}-{nonce}", std::process::id());
    let work_dir = parent.join(format!(".qcearchive-{suffix}"));
    let partial_path = parent.join(format!(".qcearchive-{suffix}.partial"));

    fs::create_dir(&work_dir)
        .map_err(|error| ExportError::io("createArchiveTempDir", &work_dir, error))?;
    let result = build_archive_inner(
        output_path,
        &partial_path,
        &work_dir,
        messages,
        chat_info,
        resource_map,
        exporter_version,
        cancellation,
    );
    let _ = fs::remove_dir_all(&work_dir);
    if result.is_err() {
        let _ = fs::remove_file(&partial_path);
    }
    result
}

#[allow(clippy::too_many_arguments)]
fn build_archive_inner(
    output_path: &Path,
    partial_path: &Path,
    work_dir: &Path,
    mut messages: Vec<CleanMessage>,
    chat_info: &ChatInfo,
    resource_map: &HashMap<String, Vec<MessageResource>>,
    exporter_version: &str,
    cancellation: &crate::types::CancellationToken,
) -> ExportResultT<ArchiveBuildOutcome> {
    cancellation_check(cancellation)?;
    let media = prepare_media(work_dir, resource_map, cancellation)?;
    for message in &mut messages {
        let mut value = serde_json::to_value(&*message)?;
        rewrite_resource_paths(&mut value, &media.rewrite_map, None);
        *message = serde_json::from_value(value)?;
    }

    let created_at = now_iso();
    let database_path = work_dir.join("messages.sqlite");
    let database = build_database(
        &database_path,
        &messages,
        chat_info,
        &media.attachments,
        exporter_version,
        &created_at,
        cancellation,
    )?;

    let chat_type = numeric_chat_type(chat_info);
    let manifest = json!({
        "format": "qcearchive",
        "schemaVersion": ARCHIVE_SCHEMA_VERSION,
        "createdAt": created_at,
        "generator": {
            "name": "QQ Chat Exporter",
            "version": exporter_version,
            "url": "https://github.com/shuakami/qq-chat-exporter"
        },
        "conversation": {
            "chatType": chat_type,
            "peerUid": chat_info.peer_uid,
            "peerUin": chat_info.peer_uin,
            "name": chat_info.name,
            "avatarUrl": chat_info.avatar
        },
        "counts": {
            "messages": messages.len(),
            "participants": database.participant_count,
            "attachmentReferences": database.attachment_count,
            "mediaFiles": media.copied_file_count,
            "missingMedia": database.missing_attachment_count
        },
        "timeRange": {
            "firstMessageMs": database.first_message_ms,
            "lastMessageMs": database.last_message_ms
        },
        "database": {
            "path": "messages.sqlite",
            "engine": "SQLite 3",
            "encrypted": false,
            "userVersion": ARCHIVE_SCHEMA_VERSION,
            "ftsTable": "message_search",
            "ftsTokenizer": "trigram"
        },
        "media": {
            "root": "media/",
            "pathSeparator": "/",
            "hash": "sha256"
        },
        "readme": "README.md"
    });
    let manifest_path = work_dir.join("manifest.json");
    let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
    fs::write(&manifest_path, manifest_bytes)
        .map_err(|error| ExportError::io("writeArchiveManifest", &manifest_path, error))?;
    let readme_path = work_dir.join("README.md");
    fs::write(&readme_path, ARCHIVE_README.as_bytes())
        .map_err(|error| ExportError::io("writeArchiveReadme", &readme_path, error))?;

    cancellation_check(cancellation)?;
    write_zip(work_dir, partial_path, cancellation)?;
    fs::rename(partial_path, output_path)
        .map_err(|error| ExportError::io("commitArchive", output_path, error))?;
    let file_size = fs::metadata(output_path).map_or(0, |metadata| metadata.len());
    Ok(ArchiveBuildOutcome {
        file_size,
        media_file_count: media.copied_file_count,
    })
}

fn prepare_media(
    work_dir: &Path,
    resource_map: &HashMap<String, Vec<MessageResource>>,
    cancellation: &crate::types::CancellationToken,
) -> ExportResultT<MediaBuildResult> {
    let mut attachments = Vec::new();
    let mut rewrite_map = HashMap::new();
    let mut copied_by_hash: HashMap<(String, String), String> = HashMap::new();
    let mut target_hashes: HashMap<String, String> = HashMap::new();
    let mut copied_file_count = 0usize;
    let mut message_ids: Vec<&String> = resource_map.keys().collect();
    message_ids.sort_unstable();

    for message_id in message_ids {
        for (resource_index, resource) in resource_map[message_id].iter().enumerate() {
            cancellation_check(cancellation)?;
            let mut prepared = PreparedAttachment {
                source_message_id: message_id.clone(),
                resource_index,
                resource_type: resource.resource_type.clone(),
                file_name: resource.filename.clone(),
                byte_size: resource.size,
                source_url: resource.url.clone(),
                archive_path: None,
                mime_type: None,
                sha256: None,
                copied: false,
            };
            let Some(local_path) = resource
                .local_path
                .as_deref()
                .filter(|path| !path.trim().is_empty())
            else {
                attachments.push(prepared);
                continue;
            };
            let source_path = Path::new(local_path);
            let source_name = source_path
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_default();
            if !source_name.is_empty() {
                prepared.file_name = Some(source_name.clone());
                prepared.mime_type = mime_guess::from_path(&source_name)
                    .first_raw()
                    .map(str::to_owned);
            }
            let Ok(metadata) = fs::metadata(source_path) else {
                attachments.push(prepared);
                continue;
            };
            if !metadata.is_file() {
                attachments.push(prepared);
                continue;
            }

            let Ok(sha256) = hash_file(source_path) else {
                attachments.push(prepared);
                continue;
            };
            let type_dir = resource_type_dir(&resource.resource_type).to_owned();
            let dedupe_key = (type_dir.clone(), sha256.clone());
            let archive_path = if let Some(existing) = copied_by_hash.get(&dedupe_key) {
                existing.clone()
            } else {
                let original_name = source_path
                    .file_name()
                    .map(|name| name.to_string_lossy().to_string())
                    .filter(|name| !name.is_empty())
                    .unwrap_or_else(|| format!("{}.bin", &sha256[..16]));
                let safe_name = sanitize_file_name(&original_name);
                let mut relative = format!("media/{type_dir}/{safe_name}");
                if target_hashes
                    .get(&relative)
                    .is_some_and(|existing_hash| existing_hash != &sha256)
                {
                    relative = format!("media/{type_dir}/{}_{safe_name}", &sha256[..12]);
                }
                let target = work_dir.join(relative.replace('/', std::path::MAIN_SEPARATOR_STR));
                if let Some(parent) = target.parent() {
                    if fs::create_dir_all(parent).is_err() {
                        attachments.push(prepared);
                        continue;
                    }
                }
                if fs::copy(source_path, &target).is_err() {
                    let _ = fs::remove_file(&target);
                    attachments.push(prepared);
                    continue;
                }
                target_hashes.insert(relative.clone(), sha256.clone());
                copied_by_hash.insert(dedupe_key, relative.clone());
                copied_file_count += 1;
                relative
            };

            rewrite_map.insert(
                format!("resources/{type_dir}/{source_name}"),
                archive_path.clone(),
            );
            rewrite_map.insert(format!("{type_dir}/{source_name}"), archive_path.clone());
            prepared.byte_size = Some(metadata.len());
            prepared.archive_path = Some(archive_path);
            prepared.sha256 = Some(sha256);
            prepared.copied = true;
            attachments.push(prepared);
        }
    }

    Ok(MediaBuildResult {
        attachments,
        rewrite_map,
        copied_file_count,
    })
}

fn build_database(
    path: &Path,
    messages: &[CleanMessage],
    chat_info: &ChatInfo,
    attachments: &[PreparedAttachment],
    exporter_version: &str,
    created_at: &str,
    cancellation: &crate::types::CancellationToken,
) -> ExportResultT<DatabaseBuildResult> {
    let mut connection = Connection::open(path)?;
    connection.execute_batch(&format!(
        r#"
        PRAGMA application_id = {ARCHIVE_APPLICATION_ID};
        PRAGMA user_version = {ARCHIVE_SCHEMA_VERSION};
        PRAGMA foreign_keys = ON;
        PRAGMA journal_mode = DELETE;
        PRAGMA synchronous = FULL;

        CREATE TABLE archive_meta (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );
        CREATE TABLE conversations (
            conversation_id TEXT PRIMARY KEY,
            chat_type INTEGER NOT NULL CHECK (chat_type IN (1, 2)),
            peer_uid TEXT NOT NULL,
            peer_uin TEXT,
            display_name TEXT NOT NULL,
            avatar_url TEXT
        );
        CREATE TABLE participants (
            participant_id TEXT PRIMARY KEY,
            uid TEXT,
            uin TEXT,
            display_name TEXT NOT NULL,
            nickname TEXT,
            group_card TEXT,
            remark TEXT,
            title TEXT,
            avatar_base64 TEXT
        );
        CREATE UNIQUE INDEX idx_participants_uid ON participants(uid) WHERE uid IS NOT NULL AND uid <> '';
        CREATE INDEX idx_participants_uin ON participants(uin) WHERE uin IS NOT NULL AND uin <> '';
        CREATE TABLE messages (
            message_key TEXT PRIMARY KEY,
            source_message_id TEXT NOT NULL,
            conversation_id TEXT NOT NULL REFERENCES conversations(conversation_id),
            seq TEXT NOT NULL,
            timestamp_ms INTEGER NOT NULL,
            time_text TEXT NOT NULL,
            sender_id TEXT NOT NULL REFERENCES participants(participant_id),
            sender_name TEXT NOT NULL,
            is_outgoing INTEGER CHECK (is_outgoing IN (0, 1) OR is_outgoing IS NULL),
            message_type TEXT NOT NULL,
            text_content TEXT NOT NULL,
            recalled INTEGER NOT NULL CHECK (recalled IN (0, 1)),
            system INTEGER NOT NULL CHECK (system IN (0, 1)),
            content_json TEXT NOT NULL,
            raw_json TEXT,
            message_json TEXT NOT NULL
        );
        CREATE INDEX idx_messages_conversation_time
            ON messages(conversation_id, timestamp_ms, message_key);
        CREATE INDEX idx_messages_sender_time ON messages(sender_id, timestamp_ms);
        CREATE INDEX idx_messages_source_id ON messages(source_message_id);
        CREATE TABLE message_elements (
            message_key TEXT NOT NULL REFERENCES messages(message_key) ON DELETE CASCADE,
            element_index INTEGER NOT NULL,
            element_type TEXT NOT NULL,
            data_json TEXT NOT NULL,
            PRIMARY KEY (message_key, element_index)
        );
        CREATE INDEX idx_message_elements_type ON message_elements(element_type);
        CREATE TABLE attachments (
            attachment_id INTEGER PRIMARY KEY,
            message_key TEXT REFERENCES messages(message_key) ON DELETE SET NULL,
            source_message_id TEXT NOT NULL,
            resource_index INTEGER NOT NULL,
            resource_type TEXT NOT NULL,
            file_name TEXT,
            byte_size INTEGER,
            source_url TEXT,
            archive_path TEXT,
            mime_type TEXT,
            sha256 TEXT,
            copied INTEGER NOT NULL CHECK (copied IN (0, 1))
        );
        CREATE INDEX idx_attachments_message ON attachments(message_key, resource_index);
        CREATE INDEX idx_attachments_source_message ON attachments(source_message_id);
        CREATE INDEX idx_attachments_sha256 ON attachments(sha256) WHERE sha256 IS NOT NULL;
        CREATE VIRTUAL TABLE message_search USING fts5(
            message_key UNINDEXED,
            conversation_name,
            sender_name,
            content,
            tokenize = 'trigram'
        );
        "#
    ))?;

    let chat_type = numeric_chat_type(chat_info);
    let peer_uid = chat_info.peer_uid.as_deref().unwrap_or_default();
    let conversation_id = format!("{chat_type}:{peer_uid}");
    let transaction = connection.transaction()?;
    transaction.execute(
        "INSERT INTO conversations(conversation_id, chat_type, peer_uid, peer_uin, display_name, avatar_url) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![conversation_id, chat_type, peer_uid, chat_info.peer_uin, chat_info.name, chat_info.avatar],
    )?;

    let meta = [
        ("format", "qcearchive".to_owned()),
        ("schema_version", ARCHIVE_SCHEMA_VERSION.to_string()),
        ("created_at", created_at.to_owned()),
        ("generator_version", exporter_version.to_owned()),
        ("message_count", messages.len().to_string()),
    ];
    for (key, value) in meta {
        transaction.execute(
            "INSERT INTO archive_meta(key, value) VALUES (?1, ?2)",
            params![key, value],
        )?;
    }

    let mut participant_ids = HashSet::new();
    let mut message_keys = HashSet::new();
    let mut source_to_message_key: HashMap<String, String> = HashMap::new();
    let mut first_message_ms: Option<i64> = None;
    let mut last_message_ms: Option<i64> = None;

    for (index, message) in messages.iter().enumerate() {
        cancellation_check(cancellation)?;
        let participant_id = participant_id(message);
        participant_ids.insert(participant_id.clone());
        transaction.execute(
            r#"
            INSERT INTO participants(
                participant_id, uid, uin, display_name, nickname, group_card, remark, title, avatar_base64
            ) VALUES (?1, NULLIF(?2, ''), ?3, ?4, ?5, ?6, ?7, ?8, ?9)
            ON CONFLICT(participant_id) DO UPDATE SET
                uid = COALESCE(excluded.uid, participants.uid),
                uin = COALESCE(excluded.uin, participants.uin),
                display_name = CASE WHEN excluded.display_name <> '' THEN excluded.display_name ELSE participants.display_name END,
                nickname = COALESCE(excluded.nickname, participants.nickname),
                group_card = COALESCE(excluded.group_card, participants.group_card),
                remark = COALESCE(excluded.remark, participants.remark),
                title = COALESCE(excluded.title, participants.title),
                avatar_base64 = COALESCE(excluded.avatar_base64, participants.avatar_base64)
            "#,
            params![
                participant_id,
                message.sender.uid,
                message.sender.uin,
                message.sender.name,
                message.sender.nickname,
                message.sender.group_card,
                message.sender.remark,
                message.sender.title,
                message.sender.avatar_base64,
            ],
        )?;

        let source_message_id = message.id.clone();
        let message_key = unique_message_key(&source_message_id, index, &mut message_keys);
        source_to_message_key
            .entry(source_message_id.clone())
            .or_insert_with(|| message_key.clone());
        let timestamp_ms = normalize_timestamp_ms(message.timestamp);
        first_message_ms =
            Some(first_message_ms.map_or(timestamp_ms, |value| value.min(timestamp_ms)));
        last_message_ms =
            Some(last_message_ms.map_or(timestamp_ms, |value| value.max(timestamp_ms)));
        let is_outgoing = outgoing_flag(message, chat_info);
        let searchable_text = searchable_text(message);
        let content_json = serde_json::to_string(&message.content)?;
        let raw_json = message
            .raw_message
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;
        let message_json = serde_json::to_string(message)?;
        transaction.execute(
            r#"
            INSERT INTO messages(
                message_key, source_message_id, conversation_id, seq, timestamp_ms, time_text,
                sender_id, sender_name, is_outgoing, message_type, text_content, recalled, system,
                content_json, raw_json, message_json
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)
            "#,
            params![
                message_key,
                source_message_id,
                conversation_id,
                message.seq,
                timestamp_ms,
                message.time,
                participant_id,
                message.sender.name,
                is_outgoing,
                message.message_type,
                searchable_text,
                i64::from(message.recalled),
                i64::from(message.system),
                content_json,
                raw_json,
                message_json,
            ],
        )?;
        transaction.execute(
            "INSERT INTO message_search(message_key, conversation_name, sender_name, content) VALUES (?1, ?2, ?3, ?4)",
            params![message_key, chat_info.name, message.sender.name, searchable_text],
        )?;
        for (element_index, element) in message.content.elements.iter().enumerate() {
            transaction.execute(
                "INSERT INTO message_elements(message_key, element_index, element_type, data_json) VALUES (?1, ?2, ?3, ?4)",
                params![message_key, element_index, element.element_type, serde_json::to_string(&element.data)?],
            )?;
        }
    }

    let mut missing_attachment_count = 0usize;
    for attachment in attachments {
        cancellation_check(cancellation)?;
        if !attachment.copied {
            missing_attachment_count += 1;
        }
        transaction.execute(
            r#"
            INSERT INTO attachments(
                message_key, source_message_id, resource_index, resource_type, file_name,
                byte_size, source_url, archive_path, mime_type, sha256, copied
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
            "#,
            params![
                source_to_message_key.get(&attachment.source_message_id),
                attachment.source_message_id,
                attachment.resource_index,
                attachment.resource_type,
                attachment.file_name,
                attachment.byte_size,
                attachment.source_url,
                attachment.archive_path,
                attachment.mime_type,
                attachment.sha256,
                i64::from(attachment.copied),
            ],
        )?;
    }
    transaction.commit()?;
    connection.execute_batch("ANALYZE; PRAGMA optimize;")?;
    let integrity: String = connection.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    if integrity != "ok" {
        return Err(ExportError::Archive(format!(
            "SQLite integrity_check 失败: {integrity}"
        )));
    }
    drop(connection);

    Ok(DatabaseBuildResult {
        participant_count: participant_ids.len(),
        attachment_count: attachments.len(),
        missing_attachment_count,
        first_message_ms,
        last_message_ms,
    })
}

fn write_zip(
    work_dir: &Path,
    output_path: &Path,
    cancellation: &crate::types::CancellationToken,
) -> ExportResultT<()> {
    let output = File::create(output_path)
        .map_err(|error| ExportError::io("createArchive", output_path, error))?;
    let mut zip = zip::ZipWriter::new(output);
    let mut files: Vec<PathBuf> = walkdir::WalkDir::new(work_dir)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .map(walkdir::DirEntry::into_path)
        .collect();
    files.sort();
    zip.add_directory(
        "media/",
        zip::write::SimpleFileOptions::default().unix_permissions(0o755),
    )
    .map_err(|error| ExportError::Archive(error.to_string()))?;

    for path in files {
        cancellation_check(cancellation)?;
        let relative = path
            .strip_prefix(work_dir)
            .map_err(|error| ExportError::Archive(error.to_string()))?
            .to_string_lossy()
            .replace('\\', "/");
        let method = if is_already_compressed(&path) {
            zip::CompressionMethod::Stored
        } else {
            zip::CompressionMethod::Deflated
        };
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(method)
            .unix_permissions(0o644);
        zip.start_file(relative, options)
            .map_err(|error| ExportError::Archive(error.to_string()))?;
        let mut input =
            File::open(&path).map_err(|error| ExportError::io("readArchiveEntry", &path, error))?;
        std::io::copy(&mut input, &mut zip)
            .map_err(|error| ExportError::io("writeArchiveEntry", &path, error))?;
    }
    zip.finish()
        .map_err(|error| ExportError::Archive(error.to_string()))?;
    Ok(())
}

fn hash_file(path: &Path) -> ExportResultT<String> {
    let mut file =
        File::open(path).map_err(|error| ExportError::io("hashArchiveMedia", path, error))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| ExportError::io("hashArchiveMedia", path, error))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn rewrite_resource_paths(
    value: &mut Value,
    rewrite_map: &HashMap<String, String>,
    key: Option<&str>,
) {
    match value {
        Value::Object(object) => {
            for (child_key, child_value) in object {
                rewrite_resource_paths(child_value, rewrite_map, Some(child_key));
            }
        }
        Value::Array(array) => {
            for child in array {
                rewrite_resource_paths(child, rewrite_map, key);
            }
        }
        Value::String(path) if matches!(key, Some("localPath" | "url" | "path" | "src")) => {
            if let Some(mapped) = rewrite_map.get(path) {
                *path = mapped.clone();
            } else if let Some(relative) = path.strip_prefix("resources/") {
                *path = format!("media/{relative}");
            } else if ["images/", "videos/", "audios/", "files/"]
                .iter()
                .any(|prefix| path.starts_with(prefix))
            {
                *path = format!("media/{path}");
            }
        }
        _ => {}
    }
}

fn searchable_text(message: &CleanMessage) -> String {
    let mut parts = Vec::new();
    push_search_part(&mut parts, &message.content.text);
    for element in &message.content.elements {
        collect_search_strings(&element.data, None, &mut parts, 0);
    }
    let mut seen = HashSet::new();
    parts.retain(|part| seen.insert(part.clone()));
    parts.join("\n")
}

fn collect_search_strings(
    value: &Value,
    key: Option<&str>,
    output: &mut Vec<String>,
    depth: usize,
) {
    if depth > 12 {
        return;
    }
    match value {
        Value::Object(object) => {
            for (child_key, child) in object {
                collect_search_strings(child, Some(child_key), output, depth + 1);
            }
        }
        Value::Array(array) => {
            for child in array {
                collect_search_strings(child, key, output, depth + 1);
            }
        }
        Value::String(text)
            if matches!(
                key,
                Some(
                    "text"
                        | "content"
                        | "title"
                        | "summary"
                        | "description"
                        | "name"
                        | "address"
                        | "senderName"
                        | "nickName"
                )
            ) =>
        {
            push_search_part(output, text);
        }
        _ => {}
    }
}

fn push_search_part(output: &mut Vec<String>, text: &str) {
    let text = text.trim();
    if !text.is_empty() && text.len() <= 16 * 1024 {
        output.push(text.to_owned());
    }
}

fn participant_id(message: &CleanMessage) -> String {
    if !message.sender.uid.trim().is_empty() {
        return format!("uid:{}", message.sender.uid);
    }
    if let Some(uin) = message
        .sender
        .uin
        .as_deref()
        .filter(|uin| !uin.trim().is_empty())
    {
        return format!("uin:{uin}");
    }
    format!("unknown:{}", message.sender.name)
}

fn unique_message_key(source_id: &str, index: usize, existing: &mut HashSet<String>) -> String {
    let base = if source_id.trim().is_empty() {
        format!("message-{}", index + 1)
    } else {
        source_id.to_owned()
    };
    if existing.insert(base.clone()) {
        return base;
    }
    let mut suffix = 2usize;
    loop {
        let candidate = format!("{base}#{suffix}");
        if existing.insert(candidate.clone()) {
            return candidate;
        }
        suffix += 1;
    }
}

fn outgoing_flag(message: &CleanMessage, chat_info: &ChatInfo) -> Option<i64> {
    if chat_info
        .self_uid
        .as_deref()
        .is_some_and(|uid| !uid.is_empty() && uid == message.sender.uid)
    {
        return Some(1);
    }
    if let (Some(self_uin), Some(sender_uin)) =
        (chat_info.self_uin.as_deref(), message.sender.uin.as_deref())
    {
        if !self_uin.is_empty() && self_uin == sender_uin {
            return Some(1);
        }
    }
    if chat_info.self_uid.is_some() || chat_info.self_uin.is_some() {
        Some(0)
    } else {
        None
    }
}

fn normalize_timestamp_ms(timestamp: i64) -> i64 {
    if timestamp.unsigned_abs() < 10_000_000_000 {
        timestamp.saturating_mul(1000)
    } else {
        timestamp
    }
}

fn numeric_chat_type(chat_info: &ChatInfo) -> i64 {
    if chat_info.chat_type.eq_ignore_ascii_case("group") {
        2
    } else {
        1
    }
}

fn sanitize_file_name(file_name: &str) -> String {
    let mut sanitized: String = file_name
        .chars()
        .map(|character| {
            if character.is_control()
                || matches!(
                    character,
                    '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'
                )
            {
                '_'
            } else {
                character
            }
        })
        .take(180)
        .collect();
    sanitized = sanitized.trim_matches([' ', '.']).to_owned();
    if sanitized.is_empty() {
        "resource.bin".to_owned()
    } else {
        sanitized
    }
}

fn is_already_compressed(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .is_some_and(|extension| {
            matches!(
                extension.as_str(),
                "jpg"
                    | "jpeg"
                    | "png"
                    | "gif"
                    | "webp"
                    | "mp3"
                    | "m4a"
                    | "aac"
                    | "ogg"
                    | "mp4"
                    | "mov"
                    | "avi"
                    | "mkv"
                    | "zip"
                    | "7z"
                    | "rar"
                    | "gz"
                    | "pdf"
            )
        })
}

fn cancellation_check(token: &crate::types::CancellationToken) -> ExportResultT<()> {
    if token.is_cancelled() {
        Err(ExportError::Cancelled)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{QceArchiveExporter, QceArchiveFormatOptions};
    use crate::types::{
        ChatInfo, CleanMessage, ExportOptions, MessageContent, MessageElement, MessageResource,
        Sender,
    };
    use rusqlite::Connection;
    use serde_json::json;
    use std::collections::HashMap;
    use std::io::Read;

    #[tokio::test]
    async fn creates_self_describing_searchable_archive_with_media() {
        let root = std::env::temp_dir().join(format!(
            "qce-archive-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).expect("create temp dir");
        let media_path = root.join("source.jpg");
        std::fs::write(&media_path, b"fake-jpeg-data").expect("write media");
        let archive_path = root.join("conversation.qcearchive");
        let message = CleanMessage {
            id: "10001".to_owned(),
            seq: "7".to_owned(),
            timestamp: 1_700_000_000_123,
            time: "2023-11-15 06:13:20".to_owned(),
            sender: Sender {
                uid: "u_peer".to_owned(),
                uin: Some("123456".to_owned()),
                name: "对方".to_owned(),
                ..Sender::default()
            },
            message_type: "text".to_owned(),
            content: MessageContent {
                text: "这是一条可检索的聊天记录".to_owned(),
                elements: vec![MessageElement {
                    element_type: "image".to_owned(),
                    data: json!({
                        "localPath": "images/source.jpg",
                        "url": "resources/images/source.jpg"
                    }),
                }],
                resources: vec![MessageResource {
                    resource_type: "image".to_owned(),
                    filename: Some("source.jpg".to_owned()),
                    size: Some(14),
                    url: Some("resources/images/source.jpg".to_owned()),
                    local_path: Some("images/source.jpg".to_owned()),
                    ..MessageResource::default()
                }],
                ..MessageContent::default()
            },
            recalled: false,
            system: false,
            raw_message: Some(json!({"msgId": "10001"})),
        };
        let options = ExportOptions {
            output_path: archive_path.clone(),
            resource_map: HashMap::from([(
                "10001".to_owned(),
                vec![
                    MessageResource {
                        resource_type: "image".to_owned(),
                        filename: Some("source.jpg".to_owned()),
                        size: Some(14),
                        url: Some("https://example.invalid/source.jpg".to_owned()),
                        local_path: Some(media_path.to_string_lossy().to_string()),
                        ..MessageResource::default()
                    },
                    MessageResource {
                        resource_type: "file".to_owned(),
                        filename: Some("missing.bin".to_owned()),
                        local_path: Some(root.join("missing.bin").to_string_lossy().to_string()),
                        ..MessageResource::default()
                    },
                ],
            )]),
            ..ExportOptions::default()
        };
        let chat = ChatInfo {
            name: "测试会话".to_owned(),
            chat_type: "private".to_owned(),
            self_uid: Some("u_self".to_owned()),
            self_uin: Some("565122807".to_owned()),
            peer_uid: Some("u_peer".to_owned()),
            peer_uin: Some("123456".to_owned()),
            ..ChatInfo::default()
        };
        let exporter = QceArchiveExporter::new(
            options,
            QceArchiveFormatOptions {
                exporter_version: Some("test-version".to_owned()),
            },
        );
        let outcome = exporter
            .export(vec![message], &chat)
            .await
            .expect("export archive");
        assert_eq!(outcome.message_count, 1);
        assert_eq!(outcome.resource_count, 1);

        let file = std::fs::File::open(&archive_path).expect("open archive");
        let mut zip = zip::ZipArchive::new(file).expect("read zip");
        assert!(zip.by_name("manifest.json").is_ok());
        assert!(zip.by_name("README.md").is_ok());
        assert!(zip.by_name("media/").is_ok());
        assert!(zip.by_name("media/images/source.jpg").is_ok());
        let mut database_bytes = Vec::new();
        zip.by_name("messages.sqlite")
            .expect("sqlite entry")
            .read_to_end(&mut database_bytes)
            .expect("read sqlite");
        assert!(database_bytes.starts_with(b"SQLite format 3\0"));
        drop(zip);
        let extracted_database = root.join("extracted.sqlite");
        std::fs::write(&extracted_database, database_bytes).expect("extract sqlite");
        let connection = Connection::open(&extracted_database).expect("open sqlite");
        let user_version: i64 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .expect("user version");
        assert_eq!(user_version, 1);
        let matched: String = connection
            .query_row(
                "SELECT message_key FROM message_search WHERE message_search MATCH '可检索的'",
                [],
                |row| row.get(0),
            )
            .expect("fts match");
        assert_eq!(matched, "10001");
        let (content_json, archive_media_path): (String, String) = connection
            .query_row(
                "SELECT m.content_json, a.archive_path FROM messages m JOIN attachments a USING(message_key)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("message and attachment");
        assert!(content_json.contains("media/images/source.jpg"));
        assert_eq!(archive_media_path, "media/images/source.jpg");
        let (attachment_count, copied_count): (i64, i64) = connection
            .query_row(
                "SELECT COUNT(*), COALESCE(SUM(copied), 0) FROM attachments",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("attachment summary");
        assert_eq!(attachment_count, 2);
        assert_eq!(copied_count, 1);

        drop(connection);
        let _ = std::fs::remove_dir_all(root);
    }
}
