//! QQ 聊天记录备份的只读导入与解析。
//!
//! NTQQ 数据库格式、SQLCipher 参数、表字段和 Protobuf 字段来自 GPL-3.0 项目
//! `QQBackup/nt_msg_db_util` 的公开研究。Windows 下可只读扫描已登录 QQ 的进程内存，
//! 并仅返回能打开用户所选数据库的密钥；QCE 不调试或修改 QQ 进程，也不会修改源文件。
//! 加密数据库会解密为 QCE 私有目录中的明文副本，密钥不会持久化。

mod key_detection;

pub use key_detection::{KeyDetection, KeyDetectionError};

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::{
    params, params_from_iter, types::Value as SqlValue, Connection, ErrorCode, OpenFlags, Row,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

const NTQQ_HEADER_SIZE: u64 = 1024;
const NTQQ_HEADER_MAGIC: &[u8; 8] = b"QQ_NT DB";
const COPY_BUFFER_SIZE: usize = 8 * 1024 * 1024;
const OWNER_INFERENCE_VERSION: u8 = 1;
const RECOVERY_BATCH_ROWS: i64 = 1_000;
const SUPPORTED_MESSAGE_TABLES: [&str; 6] = [
    "c2c_messages",
    "group_messages",
    "discuss_messages",
    "c2c_msg_table",
    "group_msg_table",
    "discuss_msg_table",
];

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct MessageRecovery {
    skipped_segments: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DecryptCoverage {
    Complete,
    MessageTablesOnly(MessageRecovery),
}

#[derive(Debug, thiserror::Error)]
pub enum BackupImportError {
    #[error("文件不存在或不是普通文件")]
    InvalidSource,
    #[error("NTQQ 数据库已加密，请填写数据库密钥后重试")]
    KeyRequired,
    #[error("数据库密钥不正确，或文件不是受支持的 NTQQ 数据库")]
    WrongKey,
    #[error("这是传统 PCQQ 加密 .bak 文件；该格式的离线解密链目前尚未公开完成。请先用 QQ 消息管理器导入，再选择 NTQQ 的 nt_msg.db，或导入 nt_msg_db_util 生成的 nt_msg_export.db")]
    LegacyBak,
    #[error("SQLite 文件中没有找到受支持的 QQ 消息表")]
    UnsupportedSchema,
    #[error("不支持的聊天记录备份格式")]
    UnsupportedFormat,
    #[error("文件操作失败: {0}")]
    Io(#[from] std::io::Error),
    #[error("数据库解析失败: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("导入清单损坏: {0}")]
    Manifest(#[from] serde_json::Error),
    #[error("导入记录不存在")]
    NotFound,
    #[error("聊天类型不受支持")]
    UnsupportedChatType,
    #[error("所选数据库属于 QQ {detected}，与当前登录账号 QQ {current} 不一致")]
    AccountMismatch { detected: String, current: String },
}

impl BackupImportError {
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidSource => "INVALID_BACKUP_SOURCE",
            Self::KeyRequired => "BACKUP_KEY_REQUIRED",
            Self::WrongKey => "BACKUP_KEY_INVALID",
            Self::LegacyBak => "LEGACY_BAK_ENCRYPTED",
            Self::UnsupportedSchema => "UNSUPPORTED_BACKUP_SCHEMA",
            Self::UnsupportedFormat => "UNSUPPORTED_BACKUP_FORMAT",
            Self::Io(_) => "BACKUP_IO_FAILED",
            Self::Database(_) => "BACKUP_DATABASE_FAILED",
            Self::Manifest(_) => "BACKUP_MANIFEST_INVALID",
            Self::NotFound => "BACKUP_IMPORT_NOT_FOUND",
            Self::UnsupportedChatType => "UNSUPPORTED_CHAT_TYPE",
            Self::AccountMismatch { .. } => "BACKUP_ACCOUNT_MISMATCH",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BackupSchema {
    NtMsgExport,
    NtMsgRaw,
}

impl BackupSchema {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::NtMsgExport => "nt_msg_export",
            Self::NtMsgRaw => "nt_msg_raw",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupImport {
    pub id: String,
    pub account_uin: String,
    pub file_name: String,
    pub format: String,
    pub created_at: String,
    pub file_size: u64,
    pub session_count: usize,
    pub message_count: u64,
    pub recovered: bool,
    pub recovery_skipped_segments: usize,
    #[serde(skip)]
    schema: BackupSchema,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BackupManifest {
    id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    account_uin: Option<String>,
    #[serde(default)]
    owner_inference_version: u8,
    file_name: String,
    schema: BackupSchema,
    created_at: String,
    file_size: u64,
    session_count: usize,
    message_count: u64,
    #[serde(default)]
    includes_discussions: bool,
    #[serde(default)]
    recovered: bool,
    #[serde(default)]
    recovery_skipped_segments: usize,
}

impl From<BackupManifest> for BackupImport {
    fn from(value: BackupManifest) -> Self {
        Self {
            id: value.id,
            account_uin: value.account_uin.unwrap_or_default(),
            file_name: value.file_name,
            format: value.schema.label().to_string(),
            created_at: value.created_at,
            file_size: value.file_size,
            session_count: value.session_count,
            message_count: value.message_count,
            recovered: value.recovered,
            recovery_skipped_segments: value.recovery_skipped_segments,
            schema: value.schema,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportedSession {
    pub import_id: String,
    pub source_name: String,
    pub format: String,
    pub chat_type: i64,
    pub peer_uid: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peer_uin: Option<String>,
    pub name: String,
    pub avatar_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_msg_time: Option<String>,
    pub message_count: u64,
}

#[derive(Debug)]
pub struct ImportedMessagesPage {
    pub messages: Vec<Value>,
    pub total_count: usize,
    pub current_page: i64,
    pub total_pages: usize,
    pub has_next: bool,
}

#[derive(Debug)]
pub struct BackupImportManager {
    root: PathBuf,
}

impl BackupImportManager {
    #[must_use]
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub async fn initialize(&self) -> Result<(), BackupImportError> {
        tokio::fs::create_dir_all(&self.root).await?;
        tokio::fs::create_dir_all(self.uploads_dir()).await?;
        Ok(())
    }

    #[must_use]
    pub fn uploads_dir(&self) -> PathBuf {
        self.root.join(".uploads")
    }

    pub async fn import_path(
        &self,
        source: PathBuf,
        display_name: Option<String>,
        key: Option<String>,
        account_uin: String,
    ) -> Result<BackupImport, BackupImportError> {
        let root = self.root.clone();
        tokio::task::spawn_blocking(move || {
            import_path_blocking(&root, &source, display_name, key, account_uin)
        })
        .await
        .map_err(std::io::Error::other)?
    }

    pub async fn detect_key(
        &self,
        sample: PathBuf,
        display_name: Option<String>,
        original_size: Option<u64>,
    ) -> Result<KeyDetection, KeyDetectionError> {
        key_detection::detect_key(sample, display_name, original_size).await
    }

    pub async fn list_imports(
        &self,
        account_uin: String,
    ) -> Result<Vec<BackupImport>, BackupImportError> {
        let root = self.root.clone();
        tokio::task::spawn_blocking(move || {
            Ok(list_imports_blocking(&root)?
                .into_iter()
                .filter(|import| import.account_uin == account_uin)
                .collect())
        })
        .await
        .map_err(std::io::Error::other)?
    }

    pub async fn get_import(
        &self,
        account_uin: String,
        import_id: String,
    ) -> Result<BackupImport, BackupImportError> {
        let imports = self.list_imports(account_uin).await?;
        imports
            .into_iter()
            .find(|import| import.id == import_id)
            .ok_or(BackupImportError::NotFound)
    }

    pub async fn list_sessions_for_import(
        &self,
        account_uin: String,
        import_id: String,
    ) -> Result<Vec<ImportedSession>, BackupImportError> {
        let import = self.get_import(account_uin, import_id).await?;
        let root = self.root.clone();
        tokio::task::spawn_blocking(move || list_sessions_for_import(&root, &import))
            .await
            .map_err(std::io::Error::other)?
    }

    pub async fn decrypted_database_path(
        &self,
        account_uin: String,
        import_id: String,
    ) -> Result<PathBuf, BackupImportError> {
        let import = self.get_import(account_uin, import_id.clone()).await?;
        let path = self.root.join(&import.id).join("database.sqlite");
        let metadata = tokio::fs::metadata(&path)
            .await
            .map_err(|_| BackupImportError::NotFound)?;
        if !metadata.is_file() {
            return Err(BackupImportError::NotFound);
        }
        Ok(path)
    }

    pub async fn list_sessions(
        &self,
        account_uin: String,
    ) -> Result<Vec<ImportedSession>, BackupImportError> {
        let imports = self.list_imports(account_uin).await?;
        let root = self.root.clone();
        tokio::task::spawn_blocking(move || {
            let mut sessions = Vec::new();
            for import in imports {
                sessions.extend(list_sessions_for_import(&root, &import)?);
            }
            sessions.sort_by(|left, right| right.last_msg_time.cmp(&left.last_msg_time));
            Ok(sessions)
        })
        .await
        .map_err(std::io::Error::other)?
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn fetch_messages(
        &self,
        account_uin: String,
        import_id: String,
        chat_type: i64,
        peer_uid: String,
        page: i64,
        limit: i64,
        start_time_ms: Option<i64>,
        end_time_ms: Option<i64>,
        search: Option<String>,
    ) -> Result<ImportedMessagesPage, BackupImportError> {
        self.get_import(account_uin, import_id.clone()).await?;
        let root = self.root.clone();
        tokio::task::spawn_blocking(move || {
            fetch_messages_blocking(
                &root,
                &import_id,
                chat_type,
                &peer_uid,
                page,
                limit,
                start_time_ms,
                end_time_ms,
                search.as_deref(),
            )
        })
        .await
        .map_err(std::io::Error::other)?
    }

    pub async fn fetch_all_messages(
        &self,
        account_uin: String,
        import_id: String,
        chat_type: i64,
        peer_uid: String,
        start_time_ms: Option<i64>,
        end_time_ms: Option<i64>,
    ) -> Result<Vec<Value>, BackupImportError> {
        self.get_import(account_uin, import_id.clone()).await?;
        let root = self.root.clone();
        tokio::task::spawn_blocking(move || {
            fetch_all_messages_blocking(
                &root,
                &import_id,
                chat_type,
                &peer_uid,
                start_time_ms,
                end_time_ms,
            )
        })
        .await
        .map_err(std::io::Error::other)?
    }
}

fn now_iso() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn sanitize_file_name(value: &str) -> String {
    let cleaned = value
        .chars()
        .map(|ch| {
            if ch.is_control() || matches!(ch, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')
            {
                '_'
            } else {
                ch
            }
        })
        .collect::<String>();
    let trimmed = cleaned.trim().trim_matches('.').trim();
    if trimmed.is_empty() {
        "聊天记录备份".to_string()
    } else {
        trimmed.chars().take(160).collect()
    }
}

fn import_path_blocking(
    root: &Path,
    source: &Path,
    display_name: Option<String>,
    key: Option<String>,
    account_uin: String,
) -> Result<BackupImport, BackupImportError> {
    let source_meta = fs::metadata(source).map_err(|_| BackupImportError::InvalidSource)?;
    if !source_meta.is_file() {
        return Err(BackupImportError::InvalidSource);
    }
    let mut header = [0_u8; NTQQ_HEADER_SIZE as usize];
    let header_len = File::open(source)?.read(&mut header)?;
    let file_name = sanitize_file_name(display_name.as_deref().unwrap_or_else(|| {
        source
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("聊天记录备份")
    }));
    let extension = Path::new(&file_name)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let is_ntqq_wrapped = header_len >= 40 && &header[32..40] == NTQQ_HEADER_MAGIC;
    let is_plain_sqlite =
        !is_ntqq_wrapped && header_len >= 16 && &header[..16] == b"SQLite format 3\0";
    let looks_legacy_bak = extension == "bak"
        && header_len >= 32
        && header[..16].iter().all(|byte| *byte == 0)
        && !is_ntqq_wrapped;
    if looks_legacy_bak {
        return Err(BackupImportError::LegacyBak);
    }
    if is_ntqq_wrapped && key.as_deref().is_none_or(str::is_empty) {
        return Err(BackupImportError::KeyRequired);
    }
    if !is_plain_sqlite && !is_ntqq_wrapped && key.as_deref().is_none_or(str::is_empty) {
        return Err(BackupImportError::UnsupportedFormat);
    }

    fs::create_dir_all(root)?;
    let id = uuid::Uuid::new_v4().simple().to_string();
    let import_dir = root.join(&id);
    fs::create_dir(&import_dir)?;
    let database_path = import_dir.join("database.sqlite");
    let result = (|| {
        let mut recovered = false;
        let mut recovery_skipped_segments = 0;
        if is_plain_sqlite {
            copy_with_skip(source, &database_path, 0)?;
        } else {
            let encrypted_path = import_dir.join("encrypted.sqlite");
            copy_with_skip(
                source,
                &encrypted_path,
                if is_ntqq_wrapped { NTQQ_HEADER_SIZE } else { 0 },
            )?;
            let coverage = decrypt_sqlcipher(
                &encrypted_path,
                &database_path,
                key.as_deref().ok_or(BackupImportError::KeyRequired)?,
            )?;
            if let DecryptCoverage::MessageTablesOnly(recovery) = coverage {
                recovered = true;
                recovery_skipped_segments = recovery.skipped_segments;
            }
            fs::remove_file(encrypted_path)?;
        }

        let schema = detect_schema(&database_path)?;
        if let Some(detected) = detect_backup_account_uin(&database_path, schema)? {
            if detected != account_uin {
                return Err(BackupImportError::AccountMismatch {
                    detected,
                    current: account_uin,
                });
            }
        }
        let provisional = BackupImport {
            id: id.clone(),
            account_uin: account_uin.clone(),
            file_name: file_name.clone(),
            format: schema.label().to_string(),
            created_at: now_iso(),
            file_size: source_meta.len(),
            session_count: 0,
            message_count: 0,
            recovered,
            recovery_skipped_segments,
            schema,
        };
        let sessions = list_sessions_for_import(root, &provisional)?;
        let message_count = sessions.iter().map(|session| session.message_count).sum();
        let manifest = BackupManifest {
            id: id.clone(),
            account_uin: Some(account_uin),
            owner_inference_version: OWNER_INFERENCE_VERSION,
            file_name: file_name.clone(),
            schema,
            created_at: provisional.created_at,
            file_size: source_meta.len(),
            session_count: sessions.len(),
            message_count,
            includes_discussions: true,
            recovered,
            recovery_skipped_segments,
        };
        write_manifest(&import_dir, &manifest)?;
        Ok(BackupImport::from(manifest))
    })();

    if result.is_err() {
        let _ = fs::remove_dir_all(&import_dir);
    }
    result
}

fn copy_with_skip(source: &Path, destination: &Path, skip: u64) -> Result<(), BackupImportError> {
    let mut reader = BufReader::with_capacity(COPY_BUFFER_SIZE, File::open(source)?);
    reader.seek(SeekFrom::Start(skip))?;
    let mut writer = BufWriter::with_capacity(COPY_BUFFER_SIZE, File::create(destination)?);
    std::io::copy(&mut reader, &mut writer)?;
    writer.flush()?;
    Ok(())
}

fn decrypt_sqlcipher(
    encrypted_path: &Path,
    plain_path: &Path,
    key: &str,
) -> Result<DecryptCoverage, BackupImportError> {
    let connection = Connection::open(encrypted_path)?;
    configure_sqlcipher(&connection, key)?;
    connection
        .query_row("SELECT count(*) FROM sqlite_master", [], |row| {
            row.get::<_, i64>(0)
        })
        .map_err(|_| BackupImportError::WrongKey)?;
    match export_complete_database(&connection, plain_path) {
        Ok(()) => Ok(DecryptCoverage::Complete),
        Err(error) if is_database_corruption(&error) => {
            tracing::warn!(
                "full SQLCipher export found corrupt pages; retrying with supported message tables"
            );
            drop(connection);
            remove_sqlite_artifacts(plain_path);
            let recovery = export_supported_message_tables(encrypted_path, plain_path, key)?;
            Ok(DecryptCoverage::MessageTablesOnly(recovery))
        }
        Err(error) => Err(BackupImportError::Database(error)),
    }
}

fn export_complete_database(
    connection: &Connection,
    plain_path: &Path,
) -> Result<(), rusqlite::Error> {
    let plain = plain_path.to_string_lossy().to_string();
    connection.execute("ATTACH DATABASE ?1 AS qce_plain KEY ''", params![plain])?;
    let export_result =
        connection.query_row("SELECT sqlcipher_export('qce_plain')", [], |_row| Ok(()));
    let detach_result = connection.execute_batch("DETACH DATABASE qce_plain;");
    export_result?;
    detach_result?;
    Ok(())
}

fn export_supported_message_tables(
    encrypted_path: &Path,
    plain_path: &Path,
    key: &str,
) -> Result<MessageRecovery, BackupImportError> {
    let connection = Connection::open(encrypted_path)?;
    configure_sqlcipher(&connection, key)?;
    let names = table_names(&connection)?;
    let selected = SUPPORTED_MESSAGE_TABLES
        .iter()
        .filter(|table| names.contains(**table))
        .copied()
        .collect::<Vec<_>>();
    if selected.is_empty() {
        return Err(BackupImportError::UnsupportedSchema);
    }
    drop(connection);

    let mut recovery = MessageRecovery::default();
    for table in selected {
        match copy_message_table(encrypted_path, plain_path, key, table) {
            Ok(()) => {}
            Err(BackupImportError::Database(error)) if is_database_corruption(&error) => {
                tracing::warn!(
                    table,
                    "message table contains corrupt pages; switching to segmented recovery"
                );
                drop_plain_table(plain_path, table)?;
                let table_recovery = recover_message_table(encrypted_path, plain_path, key, table)?;
                recovery.skipped_segments += table_recovery.skipped_segments;
            }
            Err(error) => return Err(error),
        }
    }
    Ok(recovery)
}

fn copy_message_table(
    encrypted_path: &Path,
    plain_path: &Path,
    key: &str,
    table: &str,
) -> Result<(), BackupImportError> {
    let connection = Connection::open(encrypted_path)?;
    configure_sqlcipher(&connection, key)?;
    let plain = plain_path.to_string_lossy().to_string();
    let identifier = quote_identifier(table);
    connection.execute("ATTACH DATABASE ?1 AS qce_plain KEY ''", params![plain])?;
    let result = connection.execute_batch(&format!(
        "BEGIN; \
         CREATE TABLE qce_plain.{identifier} AS SELECT * FROM main.{identifier} WHERE 0; \
         INSERT INTO qce_plain.{identifier} SELECT * FROM main.{identifier}; \
         COMMIT;"
    ));
    if result.is_err() {
        let _ = connection.execute_batch("ROLLBACK;");
    }
    let _ = connection.execute_batch("DETACH DATABASE qce_plain;");
    result.map_err(BackupImportError::Database)
}

fn recover_message_table(
    encrypted_path: &Path,
    plain_path: &Path,
    key: &str,
    table: &str,
) -> Result<MessageRecovery, BackupImportError> {
    create_empty_plain_table(encrypted_path, plain_path, key, table)?;
    let source = Connection::open(encrypted_path)?;
    configure_sqlcipher(&source, key)?;
    let mut destination = Connection::open(plain_path)?;
    let identifier = quote_identifier(table);
    let column_count = source
        .prepare(&format!("SELECT * FROM {identifier} LIMIT 0"))?
        .column_count();
    if column_count == 0 {
        return Ok(MessageRecovery::default());
    }

    let placeholders = (1..=column_count)
        .map(|index| format!("?{index}"))
        .collect::<Vec<_>>()
        .join(",");
    let insert_sql = format!("INSERT INTO {identifier} VALUES ({placeholders})");
    let mut cursor = None;
    let mut recovery = MessageRecovery::default();

    loop {
        let (values, last_rowid, scan_error) =
            read_recovery_batch(&source, table, cursor, column_count)?;
        if !values.is_empty() {
            insert_recovery_batch(&mut destination, &insert_sql, values)?;
        }
        if let Some(last_rowid) = last_rowid {
            cursor = Some(last_rowid);
        }

        match scan_error {
            None => {
                if last_rowid.is_none() {
                    break;
                }
            }
            Some(error) if is_database_corruption(&error) => {
                let resume = find_recovery_resume_rowid(&source, table, cursor)?;
                recovery.skipped_segments += 1;
                tracing::warn!(
                    table,
                    after_rowid = ?cursor,
                    resume_rowid = ?resume,
                    "skipped an unreadable message-table segment"
                );
                let Some(resume) = resume else {
                    break;
                };
                cursor = Some(resume.saturating_sub(1));
            }
            Some(error) => return Err(BackupImportError::Database(error)),
        }
    }

    Ok(recovery)
}

type RecoveryBatch = (Vec<Vec<SqlValue>>, Option<i64>, Option<rusqlite::Error>);

fn read_recovery_batch(
    source: &Connection,
    table: &str,
    after_rowid: Option<i64>,
    column_count: usize,
) -> Result<RecoveryBatch, BackupImportError> {
    let identifier = quote_identifier(table);
    let sql = after_rowid.map_or_else(
        || format!("SELECT rowid, * FROM {identifier} ORDER BY rowid LIMIT ?1"),
        |_| format!("SELECT rowid, * FROM {identifier} WHERE rowid > ?1 ORDER BY rowid LIMIT ?2"),
    );
    let mut statement = source.prepare(&sql)?;
    let mut rows = match after_rowid {
        Some(rowid) => statement.query(params![rowid, RECOVERY_BATCH_ROWS])?,
        None => statement.query(params![RECOVERY_BATCH_ROWS])?,
    };
    let mut values = Vec::with_capacity(RECOVERY_BATCH_ROWS as usize);
    let mut last_rowid = None;
    let mut scan_error = None;
    loop {
        match rows.next() {
            Ok(Some(row)) => {
                last_rowid = Some(row.get::<_, i64>(0)?);
                let mut row_values = Vec::with_capacity(column_count);
                for column in 0..column_count {
                    row_values.push(row.get::<_, SqlValue>(column + 1)?);
                }
                values.push(row_values);
            }
            Ok(None) => break,
            Err(error) => {
                scan_error = Some(error);
                break;
            }
        }
    }
    Ok((values, last_rowid, scan_error))
}

fn insert_recovery_batch(
    destination: &mut Connection,
    insert_sql: &str,
    values: Vec<Vec<SqlValue>>,
) -> Result<(), BackupImportError> {
    let transaction = destination.transaction()?;
    {
        let mut statement = transaction.prepare_cached(insert_sql)?;
        for row in values {
            statement.execute(params_from_iter(row))?;
        }
    }
    transaction.commit()?;
    Ok(())
}

fn find_recovery_resume_rowid(
    source: &Connection,
    table: &str,
    after_rowid: Option<i64>,
) -> Result<Option<i64>, BackupImportError> {
    let failed_lower = after_rowid.unwrap_or(0).saturating_add(1);
    let mut failed = failed_lower;
    let mut step = 1_i128;
    let mut successful = None;

    for _ in 0..64 {
        let candidate = (i128::from(failed_lower) + step).min(i128::from(i64::MAX)) as i64;
        match probe_next_rowid(source, table, candidate) {
            Ok(result) => {
                successful = Some((candidate, result));
                break;
            }
            Err(error) if is_database_corruption(&error) => failed = candidate,
            Err(error) => return Err(BackupImportError::Database(error)),
        }
        if candidate == i64::MAX {
            break;
        }
        step = step.saturating_mul(2);
    }

    let Some((mut readable, mut readable_result)) = successful else {
        return Ok(None);
    };
    while i128::from(readable) - i128::from(failed) > 1 {
        let midpoint = ((i128::from(readable) + i128::from(failed)) / 2) as i64;
        match probe_next_rowid(source, table, midpoint) {
            Ok(result) => {
                readable = midpoint;
                readable_result = result;
            }
            Err(error) if is_database_corruption(&error) => failed = midpoint,
            Err(error) => return Err(BackupImportError::Database(error)),
        }
    }
    Ok(readable_result)
}

fn probe_next_rowid(
    source: &Connection,
    table: &str,
    lower_bound: i64,
) -> Result<Option<i64>, rusqlite::Error> {
    let identifier = quote_identifier(table);
    let mut statement = source.prepare(&format!(
        "SELECT rowid FROM {identifier} WHERE rowid >= ?1 ORDER BY rowid LIMIT 1"
    ))?;
    let mut rows = statement.query(params![lower_bound])?;
    rows.next()?.map(|row| row.get::<_, i64>(0)).transpose()
}

fn create_empty_plain_table(
    encrypted_path: &Path,
    plain_path: &Path,
    key: &str,
    table: &str,
) -> Result<(), BackupImportError> {
    let connection = Connection::open(encrypted_path)?;
    configure_sqlcipher(&connection, key)?;
    let plain = plain_path.to_string_lossy().to_string();
    let identifier = quote_identifier(table);
    connection.execute("ATTACH DATABASE ?1 AS qce_plain KEY ''", params![plain])?;
    connection.execute_batch(&format!(
        "CREATE TABLE qce_plain.{identifier} AS SELECT * FROM main.{identifier} WHERE 0;"
    ))?;
    connection.execute_batch("DETACH DATABASE qce_plain;")?;
    Ok(())
}

fn drop_plain_table(plain_path: &Path, table: &str) -> Result<(), BackupImportError> {
    let connection = Connection::open(plain_path)?;
    connection.execute_batch(&format!(
        "DROP TABLE IF EXISTS {};",
        quote_identifier(table)
    ))?;
    Ok(())
}

fn quote_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

fn is_database_corruption(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(failure, _)
            if failure.code == ErrorCode::DatabaseCorrupt
    ) || error
        .to_string()
        .contains("database disk image is malformed")
}

fn remove_sqlite_artifacts(path: &Path) {
    let _ = fs::remove_file(path);
    for suffix in ["-wal", "-shm"] {
        let mut sidecar = path.as_os_str().to_os_string();
        sidecar.push(suffix);
        let _ = fs::remove_file(PathBuf::from(sidecar));
    }
}

fn configure_sqlcipher(connection: &Connection, key: &str) -> Result<(), BackupImportError> {
    if key.is_empty() || key.len() > 256 || key.chars().any(char::is_control) {
        return Err(BackupImportError::WrongKey);
    }
    let escaped_key = key.replace('\'', "''");
    let pragmas = format!(
        "PRAGMA cipher_page_size = 4096;\nPRAGMA key = '{escaped_key}';\nPRAGMA kdf_iter = 4000;\nPRAGMA cipher_hmac_algorithm = HMAC_SHA1;\nPRAGMA cipher_kdf_algorithm = PBKDF2_HMAC_SHA512;"
    );
    connection.execute_batch(&pragmas)?;
    Ok(())
}

pub(super) fn sqlcipher_key_valid(path: &Path, key: &str) -> bool {
    let Ok(connection) = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) else {
        return false;
    };
    if configure_sqlcipher(&connection, key).is_err() {
        return false;
    }
    // Key detection receives only the leading database sample. Walking
    // sqlite_master can follow B-tree pages beyond that sample and falsely
    // report a valid key as wrong. schema_version lives on the first decrypted
    // page, which is enough to make SQLCipher authenticate the supplied key.
    connection
        .query_row("PRAGMA schema_version", [], |row| row.get::<_, i64>(0))
        .is_ok()
}

fn open_read_only(path: &Path) -> Result<Connection, BackupImportError> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    connection.execute_batch("PRAGMA query_only = ON; PRAGMA trusted_schema = OFF;")?;
    Ok(connection)
}

fn table_names(connection: &Connection) -> Result<HashSet<String>, BackupImportError> {
    let mut statement = connection.prepare(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
    )?;
    let names = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<HashSet<_>, _>>()?;
    Ok(names)
}

fn detect_schema(path: &Path) -> Result<BackupSchema, BackupImportError> {
    let connection = open_read_only(path)?;
    let names = table_names(&connection)?;
    if names.contains("c2c_messages")
        || names.contains("group_messages")
        || names.contains("discuss_messages")
    {
        return Ok(BackupSchema::NtMsgExport);
    }
    if names.contains("c2c_msg_table")
        || names.contains("group_msg_table")
        || names.contains("discuss_msg_table")
    {
        return Ok(BackupSchema::NtMsgRaw);
    }
    Err(BackupImportError::UnsupportedSchema)
}

fn detect_backup_account_uin(
    path: &Path,
    schema: BackupSchema,
) -> Result<Option<String>, BackupImportError> {
    let connection = open_read_only(path)?;
    let names = table_names(&connection)?;
    let tables: &[(&str, &str, &str)] = match schema {
        BackupSchema::NtMsgExport => &[
            ("c2c_messages", "direction", "sender_qq"),
            ("group_messages", "direction", "sender_qq"),
            ("discuss_messages", "direction", "sender_qq"),
        ],
        BackupSchema::NtMsgRaw => &[
            ("c2c_msg_table", "40013", "40033"),
            ("group_msg_table", "40013", "40033"),
            ("discuss_msg_table", "40013", "40033"),
        ],
    };
    let mut candidates = HashMap::<String, u64>::new();
    for (table, direction_column, sender_column) in tables {
        if !names.contains(*table) {
            continue;
        }
        let sql = format!(
            "SELECT CAST(\"{sender_column}\" AS TEXT), COUNT(*) \
             FROM \"{table}\" WHERE \"{direction_column}\" = 1 \
             AND \"{sender_column}\" BETWEEN 1000 AND 999999999999 \
             GROUP BY \"{sender_column}\""
        );
        let mut statement = connection.prepare(&sql)?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?))
        })?;
        for row in rows {
            let (uin, count) = row?;
            if (4..=12).contains(&uin.len()) && uin.chars().all(|ch| ch.is_ascii_digit()) {
                *candidates.entry(uin).or_default() += count;
            }
        }
    }
    let mut candidates = candidates.into_iter().collect::<Vec<_>>();
    candidates.sort_by_key(|candidate| std::cmp::Reverse(candidate.1));
    let Some((detected, top_count)) = candidates.first() else {
        return Ok(None);
    };
    let runner_up = candidates.get(1).map_or(0, |candidate| candidate.1);
    Ok((*top_count > runner_up).then(|| detected.clone()))
}

fn manifest_path(import_dir: &Path) -> PathBuf {
    import_dir.join("manifest.json")
}

fn write_manifest(import_dir: &Path, manifest: &BackupManifest) -> Result<(), BackupImportError> {
    let destination = manifest_path(import_dir);
    let temporary = import_dir.join("manifest.json.part");
    fs::write(&temporary, serde_json::to_vec_pretty(manifest)?)?;
    fs::rename(temporary, destination)?;
    Ok(())
}

fn read_manifest(root: &Path, id: &str) -> Result<BackupImport, BackupImportError> {
    if id.is_empty() || !id.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return Err(BackupImportError::NotFound);
    }
    let bytes = fs::read(manifest_path(&root.join(id))).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            BackupImportError::NotFound
        } else {
            BackupImportError::Io(error)
        }
    })?;
    let manifest: BackupManifest = serde_json::from_slice(&bytes)?;
    if manifest.id != id {
        return Err(BackupImportError::NotFound);
    }
    Ok(manifest.into())
}

fn list_imports_blocking(root: &Path) -> Result<Vec<BackupImport>, BackupImportError> {
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut imports = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() || entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let Ok(bytes) = fs::read(manifest_path(&entry.path())) else {
            continue;
        };
        let Ok(mut manifest) = serde_json::from_slice::<BackupManifest>(&bytes) else {
            continue;
        };
        let mut manifest_changed = false;
        if manifest.account_uin.is_none()
            && manifest.owner_inference_version < OWNER_INFERENCE_VERSION
        {
            let database_path = entry.path().join("database.sqlite");
            match detect_backup_account_uin(&database_path, manifest.schema) {
                Ok(Some(account_uin)) => {
                    manifest.account_uin = Some(account_uin);
                }
                Ok(None) => {
                    tracing::warn!(
                        "legacy backup import {} has no detectable account owner; keeping it unassigned",
                        manifest.id
                    );
                }
                Err(error) => {
                    tracing::warn!(
                        "failed to infer owner for legacy backup import {}: {error}",
                        manifest.id
                    );
                }
            }
            manifest.owner_inference_version = OWNER_INFERENCE_VERSION;
            manifest_changed = true;
        }
        if !manifest.includes_discussions {
            let provisional = BackupImport::from(manifest.clone());
            if let Ok(sessions) = list_sessions_for_import(root, &provisional) {
                manifest.session_count = sessions.len();
                manifest.message_count = sessions.iter().map(|session| session.message_count).sum();
                manifest.includes_discussions = true;
                manifest_changed = true;
            }
        }
        if manifest_changed {
            if let Err(error) = write_manifest(&entry.path(), &manifest) {
                tracing::warn!(
                    "failed to persist upgraded backup manifest for {}: {error}",
                    manifest.id
                );
            }
        }
        imports.push(BackupImport::from(manifest));
    }
    imports.sort_by(|left, right| right.created_at.cmp(&left.created_at));
    Ok(imports)
}

fn list_sessions_for_import(
    root: &Path,
    import: &BackupImport,
) -> Result<Vec<ImportedSession>, BackupImportError> {
    let connection = open_read_only(&root.join(&import.id).join("database.sqlite"))?;
    match import.schema {
        BackupSchema::NtMsgExport => list_structured_sessions(&connection, import),
        BackupSchema::NtMsgRaw => list_raw_sessions(&connection, import),
    }
}

#[allow(clippy::too_many_arguments)]
fn push_session(
    sessions: &mut Vec<ImportedSession>,
    import: &BackupImport,
    chat_type: i64,
    peer_uid: String,
    peer_uin: Option<String>,
    last_time: i64,
    message_count: u64,
    peer_name: Option<String>,
) {
    if peer_uid.is_empty() {
        return;
    }
    let identifier = peer_uin.as_deref().unwrap_or(&peer_uid);
    let is_group_like = matches!(chat_type, 2 | 3);
    let peer_name = peer_name
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty() && name != identifier && name != &peer_uid);
    sessions.push(ImportedSession {
        import_id: import.id.clone(),
        source_name: import.file_name.clone(),
        format: import.format.clone(),
        chat_type,
        peer_uid: peer_uid.clone(),
        peer_uin: peer_uin.clone(),
        name: peer_name.unwrap_or_else(|| {
            if chat_type == 2 {
                format!("群聊 {identifier}")
            } else if chat_type == 3 {
                format!("讨论组 {identifier}")
            } else {
                format!("QQ {identifier}")
            }
        }),
        avatar_url: if is_group_like {
            format!("https://p.qlogo.cn/gh/{identifier}/{identifier}/640/")
        } else {
            format!("https://q1.qlogo.cn/g?b=qq&nk={identifier}&s=640")
        },
        last_msg_time: timestamp_iso(last_time),
        message_count,
    });
}

fn timestamp_iso(timestamp: i64) -> Option<String> {
    DateTime::<Utc>::from_timestamp(timestamp, 0)
        .map(|time| time.to_rfc3339_opts(SecondsFormat::Secs, true))
}

fn list_structured_sessions(
    connection: &Connection,
    import: &BackupImport,
) -> Result<Vec<ImportedSession>, BackupImportError> {
    let names = table_names(connection)?;
    let mut sessions = Vec::new();
    if names.contains("c2c_messages") {
        let mut statement = connection.prepare(
            "SELECT peer_uid, MAX(peer_qq), MAX(timestamp), COUNT(*) FROM c2c_messages GROUP BY peer_uid ORDER BY MAX(timestamp) DESC",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<i64>>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, u64>(3)?,
            ))
        })?;
        for row in rows {
            let (peer_uid, peer_qq, last_time, count) = row?;
            push_session(
                &mut sessions,
                import,
                1,
                peer_uid,
                peer_qq
                    .filter(|value| *value > 0)
                    .map(|value| value.to_string()),
                last_time,
                count,
                None,
            );
        }
    }
    if names.contains("group_messages") {
        let mut statement = connection.prepare(
            "SELECT group_id, MAX(group_qq), MAX(timestamp), COUNT(*) FROM group_messages GROUP BY group_id ORDER BY MAX(timestamp) DESC",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<i64>>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, u64>(3)?,
            ))
        })?;
        for row in rows {
            let (group_id, group_qq, last_time, count) = row?;
            let group_code = group_qq
                .filter(|value| *value > 0)
                .map_or_else(|| group_id.clone(), |value| value.to_string());
            push_session(
                &mut sessions,
                import,
                2,
                group_code,
                group_qq
                    .filter(|value| *value > 0)
                    .map(|value| value.to_string()),
                last_time,
                count,
                None,
            );
        }
    }
    if names.contains("discuss_messages") {
        let mut statement = connection.prepare(
            "SELECT discuss_id, MAX(discuss_qq), MAX(timestamp), COUNT(*) FROM discuss_messages GROUP BY discuss_id ORDER BY MAX(timestamp) DESC",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<i64>>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, u64>(3)?,
            ))
        })?;
        for row in rows {
            let (discuss_id, discuss_qq, last_time, count) = row?;
            let discuss_code = discuss_qq
                .filter(|value| *value > 0)
                .map_or_else(|| discuss_id.clone(), |value| value.to_string());
            push_session(
                &mut sessions,
                import,
                3,
                discuss_code,
                discuss_qq
                    .filter(|value| *value > 0)
                    .map(|value| value.to_string()),
                last_time,
                count,
                None,
            );
        }
    }
    Ok(sessions)
}

fn list_raw_sessions(
    connection: &Connection,
    import: &BackupImport,
) -> Result<Vec<ImportedSession>, BackupImportError> {
    let names = table_names(connection)?;
    let mut sessions = Vec::new();
    if names.contains("c2c_msg_table") {
        let columns = table_columns(connection, "c2c_msg_table")?;
        let peer_number = raw_column(&columns, "40030", "0");
        let peer_id = raw_column(&columns, "40021", "''");
        let timestamp = raw_column(&columns, "40050", "0");
        let sender_uid = raw_column(&columns, "40020", "''");
        let sender_number = raw_column(&columns, "40033", "0");
        let sender_member_name = raw_column(&columns, "40090", "''");
        let sender_nick_name = raw_column(&columns, "40093", "''");
        let peer = format!("COALESCE(NULLIF({peer_id}, ''), CAST({peer_number} AS TEXT))");
        let sender_name = format!(
            "COALESCE(NULLIF(TRIM(CAST({sender_member_name} AS TEXT)), ''), NULLIF(TRIM(CAST({sender_nick_name} AS TEXT)), ''))"
        );
        let mut statement = connection.prepare(&format!(
            "SELECT {peer}, MAX(CAST({peer_number} AS INTEGER)), MAX(CAST({timestamp} AS INTEGER)), COUNT(*), \
             SUBSTR(MAX(CASE WHEN ({sender_uid} = {peer} OR (CAST({peer_number} AS INTEGER) > 0 AND CAST({sender_number} AS INTEGER) = CAST({peer_number} AS INTEGER))) AND {sender_name} IS NOT NULL \
             THEN printf('%020d', CAST({timestamp} AS INTEGER)) || {sender_name} END), 21) \
             FROM c2c_msg_table GROUP BY {peer} ORDER BY MAX(CAST({timestamp} AS INTEGER)) DESC"
        ))?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<i64>>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, u64>(3)?,
                row.get::<_, Option<String>>(4)?,
            ))
        })?;
        for row in rows {
            let (peer_uid, peer_number, last_time, count, peer_name) = row?;
            push_session(
                &mut sessions,
                import,
                1,
                peer_uid,
                peer_number
                    .filter(|value| *value > 0)
                    .map(|value| value.to_string()),
                last_time,
                count,
                peer_name,
            );
        }
    }
    if names.contains("group_msg_table") {
        let mut group_names = raw_recent_group_names(connection, &names)?;
        let columns = table_columns(connection, "group_msg_table")?;
        let peer_number = raw_column(&columns, "40030", "0");
        let peer_id = raw_column(&columns, "40021", "''");
        let timestamp = raw_column(&columns, "40050", "0");
        let peer = format!("COALESCE(NULLIF(CAST({peer_number} AS TEXT), '0'), {peer_id})");
        let mut statement = connection.prepare(&format!(
            "SELECT {peer}, MAX(CAST({peer_number} AS INTEGER)), MAX(CAST({timestamp} AS INTEGER)), COUNT(*) FROM group_msg_table GROUP BY {peer} ORDER BY MAX(CAST({timestamp} AS INTEGER)) DESC"
        ))?;
        let rows = statement.query_map([], aggregate_session_row)?;
        for row in rows {
            let (peer_uid, peer_number, last_time, count) = row?;
            let peer_uin = peer_number
                .filter(|value| *value > 0)
                .map(|value| value.to_string());
            let group_code = peer_uin.clone().unwrap_or(peer_uid);
            let group_name = group_names.remove(&group_code);
            push_session(
                &mut sessions,
                import,
                2,
                group_code,
                peer_uin,
                last_time,
                count,
                group_name,
            );
        }
    }
    if names.contains("discuss_msg_table") {
        let columns = table_columns(connection, "discuss_msg_table")?;
        let peer_number = raw_column(&columns, "40030", "0");
        let peer_id = raw_column(&columns, "40021", "''");
        let timestamp = raw_column(&columns, "40050", "0");
        let peer = format!("COALESCE(NULLIF(CAST({peer_number} AS TEXT), '0'), {peer_id})");
        let mut statement = connection.prepare(&format!(
            "SELECT {peer}, MAX(CAST({peer_number} AS INTEGER)), MAX(CAST({timestamp} AS INTEGER)), COUNT(*) FROM discuss_msg_table GROUP BY {peer} ORDER BY MAX(CAST({timestamp} AS INTEGER)) DESC"
        ))?;
        let rows = statement.query_map([], aggregate_session_row)?;
        for row in rows {
            let (peer_uid, peer_number, last_time, count) = row?;
            let peer_uin = peer_number
                .filter(|value| *value > 0)
                .map(|value| value.to_string());
            let discuss_code = peer_uin.clone().unwrap_or(peer_uid);
            push_session(
                &mut sessions,
                import,
                3,
                discuss_code,
                peer_uin,
                last_time,
                count,
                None,
            );
        }
    }
    Ok(sessions)
}

fn raw_recent_group_names(
    connection: &Connection,
    table_names: &HashSet<String>,
) -> Result<HashMap<String, String>, BackupImportError> {
    const TABLE: &str = "recent_contact_v3_table";
    if !table_names.contains(TABLE) {
        return Ok(HashMap::new());
    }
    let columns = table_columns(connection, TABLE)?;
    if !["40010", "40021", "40050", "40094"]
        .into_iter()
        .all(|column| columns.contains(column))
    {
        return Ok(HashMap::new());
    }

    let mut statement = connection.prepare(
        r#"
        SELECT CAST("40021" AS TEXT),
               SUBSTR(MAX(printf('%020d', CAST("40050" AS INTEGER)) || TRIM(CAST("40094" AS TEXT))), 21)
        FROM recent_contact_v3_table
        WHERE CAST("40010" AS INTEGER) = 2
          AND CAST("40021" AS TEXT) <> ''
          AND TRIM(CAST("40094" AS TEXT)) <> ''
        GROUP BY CAST("40021" AS TEXT)
        "#,
    )?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    rows.collect::<Result<HashMap<_, _>, _>>()
        .map_err(BackupImportError::from)
}

fn aggregate_session_row(row: &Row<'_>) -> rusqlite::Result<(String, Option<i64>, i64, u64)> {
    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
}

#[allow(clippy::too_many_arguments)]
fn fetch_messages_blocking(
    root: &Path,
    import_id: &str,
    chat_type: i64,
    peer_uid: &str,
    page: i64,
    limit: i64,
    start_time_ms: Option<i64>,
    end_time_ms: Option<i64>,
    search: Option<&str>,
) -> Result<ImportedMessagesPage, BackupImportError> {
    let import = read_manifest(root, import_id)?;
    let connection = open_read_only(&root.join(import_id).join("database.sqlite"))?;
    let page = page.max(1);
    let limit = limit.clamp(1, 2000);
    let start_seconds = start_time_ms.map_or(0, milliseconds_to_seconds);
    let end_seconds = end_time_ms.map_or(i64::MAX, milliseconds_to_seconds);
    let offset = (page - 1).saturating_mul(limit);
    let (total_count, messages) = match import.schema {
        BackupSchema::NtMsgExport => fetch_structured_messages(
            &connection,
            chat_type,
            peer_uid,
            start_seconds,
            end_seconds,
            search,
            Some((limit, offset)),
        )?,
        BackupSchema::NtMsgRaw => {
            let search = search.map(str::trim).filter(|value| !value.is_empty());
            let (total, messages) = fetch_raw_messages(
                &connection,
                chat_type,
                peer_uid,
                start_seconds,
                end_seconds,
                search.is_none().then_some((limit, offset)),
            )?;
            if let Some(search) = search {
                paginate_searched_messages(messages, search, limit, offset)
            } else {
                (total, messages)
            }
        }
    };
    let limit_usize = usize::try_from(limit).unwrap_or(1);
    let total_pages = total_count.div_ceil(limit_usize);
    Ok(ImportedMessagesPage {
        messages,
        total_count,
        current_page: page,
        total_pages,
        has_next: usize::try_from(page).unwrap_or(usize::MAX) < total_pages,
    })
}

fn paginate_searched_messages(
    mut messages: Vec<Value>,
    search: &str,
    limit: i64,
    offset: i64,
) -> (usize, Vec<Value>) {
    let needle = search.to_lowercase();
    messages.retain(|message| {
        json_contains_text(&message["elements"], &needle)
            || json_contains_text(&message["sendNickName"], &needle)
            || json_contains_text(&message["sendMemberName"], &needle)
    });
    let total = messages.len();
    let start = usize::try_from(offset).unwrap_or(usize::MAX).min(total);
    let end = start
        .saturating_add(usize::try_from(limit).unwrap_or_default())
        .min(total);
    (total, messages[start..end].to_vec())
}

fn json_contains_text(value: &Value, needle: &str) -> bool {
    match value {
        Value::String(text) => text.to_lowercase().contains(needle),
        Value::Array(values) => values.iter().any(|value| json_contains_text(value, needle)),
        Value::Object(values) => values
            .values()
            .any(|value| json_contains_text(value, needle)),
        _ => false,
    }
}

fn fetch_all_messages_blocking(
    root: &Path,
    import_id: &str,
    chat_type: i64,
    peer_uid: &str,
    start_time_ms: Option<i64>,
    end_time_ms: Option<i64>,
) -> Result<Vec<Value>, BackupImportError> {
    let import = read_manifest(root, import_id)?;
    let connection = open_read_only(&root.join(import_id).join("database.sqlite"))?;
    let start_seconds = start_time_ms.map_or(0, milliseconds_to_seconds);
    let end_seconds = end_time_ms.map_or(i64::MAX, milliseconds_to_seconds);
    let (_, messages) = match import.schema {
        BackupSchema::NtMsgExport => fetch_structured_messages(
            &connection,
            chat_type,
            peer_uid,
            start_seconds,
            end_seconds,
            None,
            None,
        )?,
        BackupSchema::NtMsgRaw => fetch_raw_messages(
            &connection,
            chat_type,
            peer_uid,
            start_seconds,
            end_seconds,
            None,
        )?,
    };
    Ok(messages)
}

fn milliseconds_to_seconds(value: i64) -> i64 {
    if value.abs() > 10_000_000_000 {
        value / 1000
    } else {
        value
    }
}

#[allow(clippy::too_many_arguments)]
fn fetch_structured_messages(
    connection: &Connection,
    chat_type: i64,
    peer_uid: &str,
    start_seconds: i64,
    end_seconds: i64,
    search: Option<&str>,
    pagination: Option<(i64, i64)>,
) -> Result<(usize, Vec<Value>), BackupImportError> {
    let (table, peer_column, number_column) = match chat_type {
        1 => ("c2c_messages", "peer_uid", "peer_qq"),
        2 => ("group_messages", "group_id", "group_qq"),
        3 => ("discuss_messages", "discuss_id", "discuss_qq"),
        _ => return Err(BackupImportError::UnsupportedChatType),
    };
    if !table_names(connection)?.contains(table) {
        return Ok((0, Vec::new()));
    }
    let search_pattern = search
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| format!("%{value}%"));
    let search_clause = if search_pattern.is_some() {
        " AND COALESCE(text, '') LIKE ?4 ESCAPE '\\'"
    } else {
        ""
    };
    let condition = format!(
        "({peer_column} = ?1 OR CAST({number_column} AS TEXT) = ?1) AND timestamp BETWEEN ?2 AND ?3{search_clause}"
    );
    let count_sql = format!("SELECT COUNT(*) FROM {table} WHERE {condition}");
    let total: i64 = if let Some(pattern) = &search_pattern {
        connection.query_row(
            &count_sql,
            params![peer_uid, start_seconds, end_seconds, pattern],
            |row| row.get(0),
        )?
    } else {
        connection.query_row(
            &count_sql,
            params![peer_uid, start_seconds, end_seconds],
            |row| row.get(0),
        )?
    };
    let (peer_select, peer_number_select) = match chat_type {
        1 => ("peer_uid", "peer_qq"),
        2 => ("group_id", "group_qq"),
        3 => ("discuss_id", "discuss_qq"),
        _ => unreachable!(),
    };
    let mut query = format!(
        "SELECT msg_id, timestamp, direction, sender_uid, sender_qq, {peer_select}, {peer_number_select}, msg_type, content_type, text, content FROM {table} WHERE {condition} ORDER BY timestamp DESC, msg_id DESC"
    );
    if pagination.is_some() {
        query.push_str(" LIMIT ?5 OFFSET ?6");
    }
    let mut statement = connection.prepare(&query)?;
    let map_row = |row: &Row<'_>| structured_message_row(row, chat_type, peer_uid);
    let rows = match (search_pattern.as_ref(), pagination) {
        (Some(pattern), Some((limit, offset))) => statement
            .query_map(
                params![peer_uid, start_seconds, end_seconds, pattern, limit, offset],
                map_row,
            )?
            .collect::<Result<Vec<_>, _>>()?,
        (None, Some((limit, offset))) => statement
            .query_map(
                params![peer_uid, start_seconds, end_seconds, "", limit, offset],
                map_row,
            )?
            .collect::<Result<Vec<_>, _>>()?,
        (Some(pattern), None) => statement
            .query_map(
                params![peer_uid, start_seconds, end_seconds, pattern],
                map_row,
            )?
            .collect::<Result<Vec<_>, _>>()?,
        (None, None) => statement
            .query_map(params![peer_uid, start_seconds, end_seconds], map_row)?
            .collect::<Result<Vec<_>, _>>()?,
    };
    Ok((usize::try_from(total).unwrap_or(0), rows))
}

fn structured_message_row(
    row: &Row<'_>,
    chat_type: i64,
    requested_peer_uid: &str,
) -> rusqlite::Result<Value> {
    let msg_id = row.get::<_, i64>(0)?;
    let timestamp = row.get::<_, i64>(1)?;
    let direction = row.get::<_, i64>(2)?;
    let sender_uid = row.get::<_, String>(3)?;
    let sender_qq = row.get::<_, Option<i64>>(4)?;
    let stored_peer = row.get::<_, String>(5)?;
    let peer_number = row.get::<_, Option<i64>>(6)?;
    let msg_type = row.get::<_, i64>(7)?;
    let content_type = row.get::<_, Option<i64>>(8)?;
    let text = row.get::<_, Option<String>>(9)?;
    let content = row.get::<_, Option<String>>(10)?;
    let elements = structured_elements(content.as_deref(), text.as_deref());
    let peer = if matches!(chat_type, 2 | 3) {
        peer_number
            .filter(|value| *value > 0)
            .map_or_else(|| requested_peer_uid.to_string(), |value| value.to_string())
    } else {
        stored_peer
    };
    let sender_number = sender_qq
        .filter(|value| *value > 0)
        .map(|value| value.to_string());
    Ok(raw_message_json(
        msg_id,
        msg_id,
        timestamp,
        chat_type,
        &peer,
        peer_number.map(|value| value.to_string()).as_deref(),
        &sender_uid,
        sender_number.as_deref(),
        direction,
        msg_type,
        content_type.unwrap_or_default(),
        None,
        None,
        elements,
    ))
}

fn structured_elements(content: Option<&str>, fallback_text: Option<&str>) -> Vec<Value> {
    let mut elements = Vec::new();
    if let Some(content) = content.and_then(|raw| serde_json::from_str::<Value>(raw).ok()) {
        append_structured_content(&content, &mut elements);
    }
    if elements.is_empty() {
        if let Some(text) = fallback_text.filter(|value| !value.is_empty()) {
            elements.push(text_element(text));
        }
    }
    elements
}

fn append_structured_content(content: &Value, elements: &mut Vec<Value>) {
    let content_type = content
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match content_type {
        "text" => push_json_text(content.get("text"), elements),
        "image" => elements.push(json!({
            "elementType": 2,
            "picElement": {
                "fileName": content.get("filename").and_then(Value::as_str).unwrap_or("image"),
                "fileSize": value_string(content.get("filesize")),
                "picWidth": content.get("width").and_then(Value::as_i64).unwrap_or(0),
                "picHeight": content.get("height").and_then(Value::as_i64).unwrap_or(0),
                "md5HexStr": content.get("md5_hex").and_then(Value::as_str).unwrap_or(""),
                "originImageUrl": content.get("cdn_url").and_then(Value::as_str).unwrap_or(""),
                "sourcePath": content.get("local_path").and_then(Value::as_str).unwrap_or("")
            }
        })),
        "video" => elements.push(json!({
            "elementType": 4,
            "videoElement": {
                "fileName": content.get("filename").and_then(Value::as_str).unwrap_or("video"),
                "fileSize": value_string(content.get("filesize")),
                "duration": content.get("duration").and_then(Value::as_i64).unwrap_or(0),
                "filePath": content.get("local_path").and_then(Value::as_str).unwrap_or("")
            }
        })),
        "file" => elements.push(json!({
            "elementType": 3,
            "fileElement": {
                "fileName": content.get("filename").and_then(Value::as_str).unwrap_or("文件"),
                "fileSize": value_string(content.get("filesize")),
                "fileMd5": content.get("md5_hex").and_then(Value::as_str).unwrap_or("")
            }
        })),
        "sticker" => elements.push(text_element(
            content
                .get("text_fallback")
                .and_then(Value::as_str)
                .unwrap_or("[表情]"),
        )),
        "reply" => {
            elements.push(json!({
                "elementType": 7,
                "replyElement": {
                    "senderUid": content.get("ref_uid").and_then(Value::as_str).unwrap_or(""),
                    "senderNick": content.get("ref_nickname").and_then(Value::as_str).unwrap_or(""),
                    "sourceMsgTextElems": [{
                        "textElemContent": content.get("ref_summary").and_then(Value::as_str).unwrap_or("")
                    }]
                }
            }));
            push_json_text(content.get("text"), elements);
        }
        "contact" => elements.push(text_element(&format!(
            "[名片] {}",
            content
                .get("nickname")
                .and_then(Value::as_str)
                .unwrap_or_default()
        ))),
        "forward" | "legacy_forward" => elements.push(json!({
            "elementType": 16,
            "multiForwardMsgElement": {
                "resId": content.get("uuid").and_then(Value::as_str).unwrap_or(""),
                "xmlContent": content.get("xml").and_then(Value::as_str).unwrap_or("")
            }
        })),
        "call" => elements.push(text_element(
            content.get("desc").and_then(Value::as_str).unwrap_or("[通话]"),
        )),
        "sys" => elements.push(json!({
            "elementType": 8,
            "grayTipElement": {
                "xmlElement": {
                    "content": content.get("content").and_then(Value::as_str).unwrap_or("[系统消息]")
                }
            }
        })),
        "mixed" => {
            if let Some(segments) = content.get("segments").and_then(Value::as_array) {
                for segment in segments {
                    append_structured_content(segment, elements);
                }
            }
        }
        "msg_body" => {
            if let Some(segments) = content.get("segments").and_then(Value::as_array) {
                for segment in segments {
                    append_generic_segment(segment, elements);
                }
            }
        }
        _ => append_generic_segment(content, elements),
    }
}

fn append_generic_segment(segment: &Value, elements: &mut Vec<Value>) {
    if let Some(text) = segment
        .get("text")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
    {
        elements.push(text_element(text));
    } else if let Some(file_name) = segment.get("filename").and_then(Value::as_str) {
        elements.push(json!({
            "elementType": 3,
            "fileElement": { "fileName": file_name, "fileSize": value_string(segment.get("filesize")) }
        }));
    }
}

fn push_json_text(value: Option<&Value>, elements: &mut Vec<Value>) {
    if let Some(text) = value
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
    {
        elements.push(text_element(text));
    }
}

fn value_string(value: Option<&Value>) -> String {
    value.map_or_else(String::new, |value| {
        value
            .as_str()
            .map_or_else(|| value.to_string(), str::to_string)
    })
}

fn text_element(text: &str) -> Value {
    json!({ "elementType": 1, "textElement": { "content": text } })
}

fn table_columns(
    connection: &Connection,
    table: &str,
) -> Result<HashSet<String>, BackupImportError> {
    let mut statement = connection.prepare(&format!("PRAGMA table_info(\"{table}\")"))?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<HashSet<_>, _>>()?;
    Ok(columns)
}

fn raw_column(columns: &HashSet<String>, name: &str, fallback: &str) -> String {
    if columns.contains(name) {
        format!("\"{name}\"")
    } else {
        fallback.to_string()
    }
}

fn fetch_raw_messages(
    connection: &Connection,
    chat_type: i64,
    peer_uid: &str,
    start_seconds: i64,
    end_seconds: i64,
    pagination: Option<(i64, i64)>,
) -> Result<(usize, Vec<Value>), BackupImportError> {
    let table = match chat_type {
        1 => "c2c_msg_table",
        2 => "group_msg_table",
        3 => "discuss_msg_table",
        _ => return Err(BackupImportError::UnsupportedChatType),
    };
    if !table_names(connection)?.contains(table) {
        return Ok((0, Vec::new()));
    }
    let columns = table_columns(connection, table)?;
    let peer_number = raw_column(&columns, "40030", "0");
    let peer_id = raw_column(&columns, "40021", "''");
    let timestamp = raw_column(&columns, "40050", "0");
    let condition = format!(
        "(CAST({peer_number} AS TEXT) = ?1 OR {peer_id} = ?1) AND CAST({timestamp} AS INTEGER) BETWEEN ?2 AND ?3"
    );
    let count_sql = format!("SELECT COUNT(*) FROM {table} WHERE {condition}");
    let total: i64 = connection.query_row(
        &count_sql,
        params![peer_uid, start_seconds, end_seconds],
        |row| row.get(0),
    )?;
    let selected = [
        raw_column(&columns, "40001", "rowid"),
        raw_column(&columns, "40003", "rowid"),
        timestamp.clone(),
        raw_column(&columns, "40013", "0"),
        raw_column(&columns, "40020", "''"),
        raw_column(&columns, "40033", "0"),
        raw_column(&columns, "40090", "''"),
        raw_column(&columns, "40093", "''"),
        raw_column(&columns, "40011", "0"),
        raw_column(&columns, "40012", "0"),
        raw_column(&columns, "40800", "NULL"),
        peer_number,
        peer_id,
    ];
    let mut query = format!(
        "SELECT {} FROM {table} WHERE {condition} ORDER BY CAST({timestamp} AS INTEGER) DESC, CAST({} AS INTEGER) DESC",
        selected.join(", "),
        selected[0]
    );
    if pagination.is_some() {
        query.push_str(" LIMIT ?4 OFFSET ?5");
    }
    let mut statement = connection.prepare(&query)?;
    let map_row = |row: &Row<'_>| Ok(raw_ntqq_message_row(row, chat_type, peer_uid));
    let messages = if let Some((limit, offset)) = pagination {
        statement
            .query_map(
                params![peer_uid, start_seconds, end_seconds, limit, offset],
                map_row,
            )?
            .collect::<Result<Vec<_>, _>>()?
    } else {
        statement
            .query_map(params![peer_uid, start_seconds, end_seconds], map_row)?
            .collect::<Result<Vec<_>, _>>()?
    };
    Ok((usize::try_from(total).unwrap_or(0), messages))
}

fn raw_ntqq_message_row(row: &Row<'_>, chat_type: i64, requested_peer_uid: &str) -> Value {
    let msg_id = row.get::<_, i64>(0).unwrap_or_default();
    let msg_seq = row.get::<_, i64>(1).unwrap_or(msg_id);
    let timestamp = row.get::<_, i64>(2).unwrap_or_default();
    let direction = row.get::<_, i64>(3).unwrap_or_default();
    let sender_uid = row.get::<_, String>(4).unwrap_or_default();
    let sender_qq = row.get::<_, i64>(5).unwrap_or_default();
    let sender_member_name = row.get::<_, String>(6).unwrap_or_default();
    let sender_nick_name = row.get::<_, String>(7).unwrap_or_default();
    let msg_type = row.get::<_, i64>(8).unwrap_or_default();
    let sub_msg_type = row.get::<_, i64>(9).unwrap_or_default();
    let blob = row.get::<_, Option<Vec<u8>>>(10).unwrap_or_default();
    let peer_number = row.get::<_, i64>(11).unwrap_or_default();
    let stored_peer_uid = row.get::<_, String>(12).unwrap_or_default();
    let peer = if matches!(chat_type, 2 | 3) && peer_number > 0 {
        peer_number.to_string()
    } else if stored_peer_uid.is_empty() {
        requested_peer_uid.to_string()
    } else {
        stored_peer_uid
    };
    let elements = blob
        .as_deref()
        .map(|blob| parse_ntqq_elements(blob, msg_type))
        .unwrap_or_default();
    raw_message_json(
        msg_id,
        msg_seq,
        timestamp,
        chat_type,
        &peer,
        (peer_number > 0)
            .then(|| peer_number.to_string())
            .as_deref(),
        &sender_uid,
        (sender_qq > 0).then(|| sender_qq.to_string()).as_deref(),
        direction,
        msg_type,
        sub_msg_type,
        (!sender_member_name.is_empty()).then_some(sender_member_name.as_str()),
        (!sender_nick_name.is_empty()).then_some(sender_nick_name.as_str()),
        elements,
    )
}

#[allow(clippy::too_many_arguments)]
fn raw_message_json(
    msg_id: i64,
    msg_seq: i64,
    timestamp: i64,
    chat_type: i64,
    peer_uid: &str,
    peer_uin: Option<&str>,
    sender_uid: &str,
    sender_uin: Option<&str>,
    direction: i64,
    msg_type: i64,
    sub_msg_type: i64,
    sender_member_name: Option<&str>,
    sender_nick_name: Option<&str>,
    elements: Vec<Value>,
) -> Value {
    let sender_fallback = sender_uin.map(|uin| format!("QQ {uin}"));
    json!({
        "msgId": msg_id.to_string(),
        "msgSeq": msg_seq.to_string(),
        "msgTime": timestamp.to_string(),
        "senderUid": sender_uid,
        "senderUin": sender_uin.unwrap_or(""),
        "peerUid": peer_uid,
        "peerUin": peer_uin.unwrap_or(""),
        "chatType": chat_type,
        "sendType": direction,
        "msgType": msg_type,
        "subMsgType": sub_msg_type,
        "sendMemberName": sender_member_name.unwrap_or(""),
        "sendNickName": sender_nick_name
            .or(sender_member_name)
            .or(sender_fallback.as_deref())
            .unwrap_or("未知用户"),
        "elements": elements,
    })
}

#[derive(Debug, Clone, Copy)]
enum WireValue<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
}

#[derive(Debug, Clone, Copy)]
struct WireField<'a> {
    number: u64,
    value: WireValue<'a>,
}

fn read_varint(data: &[u8], offset: &mut usize) -> Option<u64> {
    let mut value = 0_u64;
    let mut shift = 0_u32;
    while *offset < data.len() && shift <= 63 {
        let byte = data[*offset];
        *offset += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte < 0x80 {
            return Some(value);
        }
        shift += 7;
    }
    None
}

fn parse_wire(data: &[u8]) -> Option<Vec<WireField<'_>>> {
    let mut fields = Vec::new();
    let mut offset = 0_usize;
    while offset < data.len() {
        let key = read_varint(data, &mut offset)?;
        let number = key >> 3;
        if number == 0 {
            return None;
        }
        match key & 7 {
            0 => fields.push(WireField {
                number,
                value: WireValue::Varint(read_varint(data, &mut offset)?),
            }),
            1 => {
                let end = offset.checked_add(8)?;
                let bytes = data.get(offset..end)?;
                fields.push(WireField {
                    number,
                    value: WireValue::Bytes(bytes),
                });
                offset = end;
            }
            2 => {
                let length = usize::try_from(read_varint(data, &mut offset)?).ok()?;
                let end = offset.checked_add(length)?;
                let bytes = data.get(offset..end)?;
                fields.push(WireField {
                    number,
                    value: WireValue::Bytes(bytes),
                });
                offset = end;
            }
            5 => {
                let end = offset.checked_add(4)?;
                let bytes = data.get(offset..end)?;
                fields.push(WireField {
                    number,
                    value: WireValue::Bytes(bytes),
                });
                offset = end;
            }
            _ => return None,
        }
    }
    Some(fields)
}

fn wire_string(fields: &[WireField<'_>], number: u64) -> Option<String> {
    fields.iter().find_map(|field| match field {
        WireField {
            number: current,
            value: WireValue::Bytes(bytes),
        } if *current == number => std::str::from_utf8(bytes).ok().map(str::to_string),
        _ => None,
    })
}

fn wire_bytes<'a>(fields: &[WireField<'a>], number: u64) -> Option<&'a [u8]> {
    fields.iter().find_map(|field| match field {
        WireField {
            number: current,
            value: WireValue::Bytes(bytes),
        } if *current == number => Some(*bytes),
        _ => None,
    })
}

fn wire_has_field(fields: &[WireField<'_>], number: u64) -> bool {
    fields.iter().any(|field| field.number == number)
}

fn wire_u64(fields: &[WireField<'_>], number: u64) -> Option<u64> {
    fields.iter().find_map(|field| match field {
        WireField {
            number: current,
            value: WireValue::Varint(value),
        } if *current == number => Some(*value),
        _ => None,
    })
}

fn is_non_forward_ark_json(content: &str) -> bool {
    serde_json::from_str::<Value>(content)
        .ok()
        .is_some_and(|value| {
            value.get("app").and_then(Value::as_str) != Some("com.tencent.multimsg")
        })
}

fn parse_ntqq_elements(blob: &[u8], msg_type: i64) -> Vec<Value> {
    let Some(fields) = parse_wire(blob) else {
        return vec![text_element("[消息数据损坏或格式暂不支持]")];
    };
    let mut elements = Vec::new();
    for segment in fields.iter().filter_map(|field| match field {
        WireField {
            number: 40800,
            value: WireValue::Bytes(bytes),
        } => Some(*bytes),
        _ => None,
    }) {
        let Some(segment_fields) = parse_wire(segment) else {
            continue;
        };
        let content_type = wire_u64(&segment_fields, 45002).unwrap_or_default();
        let text = wire_string(&segment_fields, 45101).filter(|value| !value.is_empty());
        let file_name = wire_string(&segment_fields, 45402).filter(|value| !value.is_empty());
        let file_size = wire_u64(&segment_fields, 45405).unwrap_or_default();
        let image_url = wire_string(&segment_fields, 45802)
            .or_else(|| wire_string(&segment_fields, 45803))
            .or_else(|| wire_string(&segment_fields, 45804));
        let local_path = wire_string(&segment_fields, 45812);
        let image_width = wire_u64(&segment_fields, 45411).unwrap_or_default();
        let image_height = wire_u64(&segment_fields, 45412).unwrap_or_default();
        let reply_msg_id =
            wire_u64(&segment_fields, 47401).or_else(|| wire_u64(&segment_fields, 47402));
        let nested_reply_summary = wire_bytes(&segment_fields, 47710)
            .and_then(parse_wire)
            .and_then(|fields| wire_string(&fields, 45101));
        let reply_summary = wire_string(&segment_fields, 47413)
            .or_else(|| wire_string(&segment_fields, 47713))
            .or(nested_reply_summary);
        if msg_type == 5
            && (reply_summary.is_some()
                || reply_msg_id.is_some()
                || wire_has_field(&segment_fields, 47710))
        {
            elements.push(json!({
                "elementType": 7,
                "replyElement": {
                    "replayMsgId": reply_msg_id.unwrap_or_default().to_string(),
                    "sourceMsgText": reply_summary.unwrap_or_default()
                }
            }));
        }

        let forward_res_id = wire_string(&segment_fields, 48601)
            .or_else(|| wire_string(&segment_fields, 47904))
            .or_else(|| wire_string(&segment_fields, 47902));
        let forward_content =
            wire_string(&segment_fields, 48602).or_else(|| wire_string(&segment_fields, 47901));
        if matches!(msg_type, 8 | 11) && (forward_res_id.is_some() || forward_content.is_some()) {
            let content = forward_content.unwrap_or_default();
            if is_non_forward_ark_json(&content) {
                elements.push(json!({
                    "elementType": 10,
                    "arkElement": { "bytesData": content }
                }));
            } else {
                elements.push(json!({
                    "elementType": 16,
                    "multiForwardMsgElement": {
                        "resId": forward_res_id.unwrap_or_default(),
                        "xmlContent": content
                    }
                }));
            }
            continue;
        }

        if msg_type == 19 {
            let description =
                wire_string(&segment_fields, 48153).unwrap_or_else(|| "[通话]".to_string());
            let duration = wire_u64(&segment_fields, 48152).unwrap_or_default();
            elements.push(text_element(&if duration > 0 {
                format!("{description}（{duration} 秒）")
            } else {
                description
            }));
            continue;
        }

        if msg_type == 17 {
            let system = wire_string(&segment_fields, 80900)
                .or_else(|| wire_string(&segment_fields, 80824))
                .unwrap_or_else(|| "[系统消息]".to_string());
            elements.push(text_element(&system));
            continue;
        }

        if msg_type == 6 {
            let nickname = wire_string(&segment_fields, 47705)
                .or_else(|| wire_string(&segment_fields, 47714))
                .unwrap_or_default();
            let label = if nickname.is_empty() {
                "[联系人名片]".to_string()
            } else {
                format!("[联系人名片] {nickname}")
            };
            elements.push(text_element(&label));
            continue;
        }

        let video_duration = wire_u64(&segment_fields, 45415).unwrap_or_default();
        let video_flag = wire_u64(&segment_fields, 47601).unwrap_or_default();
        if msg_type == 7 || content_type == 9 || video_duration > 0 || video_flag > 0 {
            elements.push(json!({
                "elementType": 4,
                "videoElement": {
                    "fileName": file_name.as_deref().unwrap_or("video"),
                    "fileSize": file_size.to_string(),
                    "duration": video_duration,
                    "filePath": local_path.unwrap_or_default(),
                    "fileUrl": image_url.unwrap_or_default()
                }
            }));
        } else if msg_type == 3 {
            elements.push(json!({
                "elementType": 3,
                "fileElement": {
                    "fileName": file_name.as_deref().unwrap_or("文件"),
                    "fileSize": file_size.to_string()
                }
            }));
        } else if let Some(text) = text {
            elements.push(text_element(&text));
        } else if content_type == 2 || image_url.is_some() || local_path.is_some() {
            elements.push(json!({
                "elementType": 2,
                "picElement": {
                    "fileName": file_name.as_deref().unwrap_or("image"),
                    "fileSize": file_size.to_string(),
                    "picWidth": image_width,
                    "picHeight": image_height,
                    "originImageUrl": image_url.unwrap_or_default(),
                    "sourcePath": local_path.unwrap_or_default()
                }
            }));
        } else if let Some(file_name) = file_name {
            elements.push(json!({
                "elementType": 3,
                "fileElement": { "fileName": file_name, "fileSize": file_size.to_string() }
            }));
        } else if content_type == 5 || wire_has_field(&segment_fields, 45600) {
            let fallback =
                wire_string(&segment_fields, 45815).unwrap_or_else(|| "[表情]".to_string());
            elements.push(text_element(&fallback));
        }
    }
    if elements.is_empty() && !blob.is_empty() {
        elements.push(text_element("[暂不支持的消息类型]"));
    }
    elements
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "qce-backup-{name}-{}",
            uuid::Uuid::new_v4().simple()
        ))
    }

    fn create_structured_fixture(path: &Path) {
        let connection = Connection::open(path).expect("create fixture");
        connection
            .execute_batch(
                r#"
                CREATE TABLE c2c_messages (
                    msg_id INTEGER PRIMARY KEY, timestamp INTEGER NOT NULL, direction INTEGER NOT NULL,
                    sender_uid TEXT NOT NULL, sender_qq INTEGER, peer_uid TEXT NOT NULL, peer_qq INTEGER NOT NULL,
                    msg_type INTEGER NOT NULL, content_type INTEGER, proto_ver TEXT, inner_ts INTEGER,
                    text TEXT, content TEXT
                );
                CREATE TABLE group_messages (
                    msg_id INTEGER PRIMARY KEY, timestamp INTEGER NOT NULL, direction INTEGER NOT NULL,
                    sender_uid TEXT NOT NULL, sender_qq INTEGER, group_id TEXT NOT NULL, group_qq INTEGER NOT NULL,
                    msg_type INTEGER NOT NULL, subtype INTEGER, content_type INTEGER, text TEXT,
                    parse_status TEXT NOT NULL, content TEXT
                );
                INSERT INTO c2c_messages VALUES
                    (1, 1700000000, 1, 'u_sender', 10001, 'u_peer', 20002, 2, 1, NULL, NULL, '你好', '{"type":"text","text":"你好"}');
                INSERT INTO group_messages VALUES
                    (2, 1700000100, 0, 'u_sender', 10001, 'g_internal', 30003, 2, 1, 1, '群消息', 'typed', '{"type":"msg_body","segments":[{"text":"群消息"}]}');
                "#,
            )
            .expect("seed fixture");
    }

    fn ntqq_text_blob(text: &str) -> Vec<u8> {
        let mut segment = Vec::new();
        put_string(45101, text, &mut segment);
        wrap_ntqq_segment(&segment)
    }

    fn wrap_ntqq_segment(segment: &[u8]) -> Vec<u8> {
        let mut outer = Vec::new();
        put_varint((40800 << 3) | 2, &mut outer);
        put_varint(segment.len() as u64, &mut outer);
        outer.extend_from_slice(segment);
        outer
    }

    fn create_raw_fixture(path: &Path) {
        let connection = Connection::open(path).expect("create raw fixture");
        connection
            .execute_batch(
                r#"
                CREATE TABLE c2c_msg_table (
                    "40001" INTEGER, "40003" INTEGER, "40013" INTEGER,
                    "40020" TEXT, "40021" TEXT, "40030" INTEGER, "40033" INTEGER,
                    "40050" INTEGER, "40090" TEXT, "40093" TEXT,
                    "40011" INTEGER, "40012" INTEGER, "40800" BLOB
                );
                CREATE TABLE group_msg_table (
                    "40001" INTEGER, "40003" INTEGER, "40013" INTEGER,
                    "40020" TEXT, "40021" TEXT, "40030" INTEGER, "40033" INTEGER,
                    "40050" INTEGER, "40090" TEXT, "40093" TEXT,
                    "40011" INTEGER, "40012" INTEGER, "40800" BLOB
                );
                CREATE TABLE discuss_msg_table (
                    "40001" INTEGER, "40003" INTEGER, "40013" INTEGER,
                    "40020" TEXT, "40021" TEXT, "40030" INTEGER, "40033" INTEGER,
                    "40050" INTEGER, "40090" TEXT, "40093" TEXT,
                    "40011" INTEGER, "40012" INTEGER, "40800" BLOB
                );
                CREATE TABLE recent_contact_v3_table (
                    "40010" INTEGER, "40021" TEXT, "40050" INTEGER, "40094" TEXT
                );
                INSERT INTO recent_contact_v3_table VALUES
                    (2, '87654', 1700000300, '备份中的已退出群');
                "#,
            )
            .expect("create raw schema");
        connection
            .execute(
                r#"INSERT INTO c2c_msg_table VALUES (?1, ?2, 1, 'u_sender', 'u_deleted', 45678, 10001, 1700000200, '', '发送者', 2, 1, ?3)"#,
                params![11_i64, 21_i64, ntqq_text_blob("删除好友的历史消息")],
            )
            .expect("seed raw c2c");
        connection
            .execute(
                r#"INSERT INTO c2c_msg_table VALUES (?1, ?2, 0, 'u_deleted', 'u_deleted', 45678, 45678, 1700000190, '', '手写☆风弄', 2, 1, ?3)"#,
                params![13_i64, 23_i64, ntqq_text_blob("旧好友发来的消息")],
            )
            .expect("seed raw c2c peer nickname");
        connection
            .execute(
                r#"INSERT INTO group_msg_table VALUES (?1, ?2, 0, 'u_sender', 'g_left', 87654, 10001, 1700000300, '群成员', '', 2, 1, ?3)"#,
                params![12_i64, 22_i64, ntqq_text_blob("退出群的历史消息")],
            )
            .expect("seed raw group");
        connection
            .execute(
                r#"INSERT INTO discuss_msg_table VALUES (?1, ?2, 0, 'u_discuss_sender', 'd_old', 99887, 10002, 1700000400, '讨论组成员', '', 2, 1, ?3)"#,
                params![14_i64, 24_i64, ntqq_text_blob("旧讨论组的历史消息")],
            )
            .expect("seed raw discussion");
    }

    #[test]
    fn imports_structured_database_and_lists_sessions() {
        let root = temp_root("structured");
        fs::create_dir_all(&root).unwrap();
        let source = root.join("source.db");
        create_structured_fixture(&source);
        let imports = root.join("imports");

        let imported =
            import_path_blocking(&imports, &source, None, None, "10001".to_owned()).unwrap();
        assert_eq!(imported.format, "nt_msg_export");
        assert_eq!(imported.account_uin, "10001");
        assert_eq!(imported.session_count, 2);
        assert_eq!(imported.message_count, 2);
        assert!(source.exists(), "source file must remain untouched");

        let sessions = list_sessions_for_import(&imports, &imported).unwrap();
        assert!(sessions
            .iter()
            .any(|session| session.peer_uin.as_deref() == Some("20002")));
        assert!(sessions.iter().any(|session| session.peer_uid == "30003"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_a_backup_detected_as_another_account() {
        let root = temp_root("account-mismatch");
        fs::create_dir_all(&root).unwrap();
        let source = root.join("source.db");
        create_structured_fixture(&source);
        let imports = root.join("imports");

        let error =
            import_path_blocking(&imports, &source, None, None, "20002".to_owned()).unwrap_err();
        assert!(matches!(
            error,
            BackupImportError::AccountMismatch { detected, current }
                if detected == "10001" && current == "20002"
        ));
        assert_eq!(fs::read_dir(&imports).unwrap().count(), 0);
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn manager_only_lists_and_opens_backups_for_the_current_account() {
        let root = temp_root("account-scope");
        fs::create_dir_all(&root).unwrap();
        let source = root.join("source.db");
        create_structured_fixture(&source);
        let imports = root.join("imports");
        let imported =
            import_path_blocking(&imports, &source, None, None, "10001".to_owned()).unwrap();
        let manager = BackupImportManager::new(imports);

        assert_eq!(
            manager
                .list_imports("10001".to_owned())
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(manager
            .list_imports("20002".to_owned())
            .await
            .unwrap()
            .is_empty());
        assert!(matches!(
            manager.get_import("20002".to_owned(), imported.id).await,
            Err(BackupImportError::NotFound)
        ));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn fetches_structured_messages_as_napcat_shape() {
        let root = temp_root("messages");
        fs::create_dir_all(&root).unwrap();
        let source = root.join("source.db");
        create_structured_fixture(&source);
        let imports = root.join("imports");
        let imported =
            import_path_blocking(&imports, &source, None, None, "10001".to_owned()).unwrap();

        let page = fetch_messages_blocking(
            &imports,
            &imported.id,
            1,
            "u_peer",
            1,
            50,
            None,
            None,
            Some("你好"),
        )
        .unwrap();
        assert_eq!(page.total_count, 1);
        assert_eq!(page.messages[0]["chatType"], 1);
        assert_eq!(
            page.messages[0]["elements"][0]["textElement"]["content"],
            "你好"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn imports_raw_database_and_searches_decoded_messages() {
        let root = temp_root("raw");
        fs::create_dir_all(&root).unwrap();
        let source = root.join("nt_msg.db");
        create_raw_fixture(&source);
        let imports = root.join("imports");

        let imported =
            import_path_blocking(&imports, &source, None, None, "10001".to_owned()).unwrap();
        assert_eq!(imported.format, "nt_msg_raw");
        assert_eq!(imported.account_uin, "10001");
        assert_eq!(imported.session_count, 3);
        assert_eq!(imported.message_count, 4);
        let sessions = list_sessions_for_import(&imports, &imported).unwrap();
        let private_session = sessions
            .iter()
            .find(|session| session.chat_type == 1 && session.peer_uid == "u_deleted")
            .expect("raw private session");
        assert_eq!(private_session.name, "手写☆风弄");
        assert_eq!(private_session.peer_uin.as_deref(), Some("45678"));
        assert!(sessions
            .iter()
            .any(|session| session.chat_type == 2 && session.peer_uid == "87654"));
        assert!(sessions.iter().any(|session| {
            session.chat_type == 2
                && session.peer_uid == "87654"
                && session.name == "备份中的已退出群"
        }));
        let discussion = sessions
            .iter()
            .find(|session| session.chat_type == 3 && session.peer_uid == "99887")
            .expect("raw discussion session");
        assert_eq!(discussion.name, "讨论组 99887");
        assert_eq!(discussion.message_count, 1);

        let page = fetch_messages_blocking(
            &imports,
            &imported.id,
            1,
            "u_deleted",
            1,
            50,
            None,
            None,
            Some("历史消息"),
        )
        .unwrap();
        assert_eq!(page.total_count, 1);
        assert_eq!(
            page.messages[0]["elements"][0]["textElement"]["content"],
            "删除好友的历史消息"
        );

        let empty = fetch_messages_blocking(
            &imports,
            &imported.id,
            1,
            "u_deleted",
            1,
            50,
            None,
            None,
            Some("不存在"),
        )
        .unwrap();
        assert_eq!(empty.total_count, 0);
        assert!(empty.messages.is_empty());

        let discussion_page = fetch_messages_blocking(
            &imports,
            &imported.id,
            3,
            "99887",
            1,
            50,
            None,
            None,
            Some("讨论组"),
        )
        .unwrap();
        assert_eq!(discussion_page.total_count, 1);
        assert_eq!(discussion_page.messages[0]["chatType"], 3);
        assert_eq!(
            discussion_page.messages[0]["elements"][0]["textElement"]["content"],
            "旧讨论组的历史消息"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn upgrades_old_manifest_counts_to_include_discussions() {
        let root = temp_root("manifest-discussions");
        fs::create_dir_all(&root).unwrap();
        let source = root.join("nt_msg.db");
        create_raw_fixture(&source);
        let imports = root.join("imports");
        let imported =
            import_path_blocking(&imports, &source, None, None, "10001".to_owned()).unwrap();
        let path = manifest_path(&imports.join(&imported.id));
        let mut legacy: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        let object = legacy.as_object_mut().unwrap();
        object.remove("accountUin");
        object.remove("ownerInferenceVersion");
        object.remove("includesDiscussions");
        object.insert("sessionCount".to_owned(), json!(2));
        object.insert("messageCount".to_owned(), json!(3));
        fs::write(&path, serde_json::to_vec_pretty(&legacy).unwrap()).unwrap();

        let imports_list = list_imports_blocking(&imports).unwrap();
        assert_eq!(imports_list[0].session_count, 3);
        assert_eq!(imports_list[0].message_count, 4);
        assert_eq!(imports_list[0].account_uin, "10001");
        let upgraded: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(upgraded["includesDiscussions"], true);
        assert_eq!(upgraded["accountUin"], "10001");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn decrypts_wrapped_sqlcipher_database_with_supplied_key() {
        let root = temp_root("encrypted");
        fs::create_dir_all(&root).unwrap();
        let encrypted = root.join("encrypted.sqlite");
        let key = "0123456789abcdef";
        {
            let connection = Connection::open(&encrypted).expect("create encrypted fixture");
            connection
                .execute_batch(&format!(
                    "PRAGMA cipher_page_size = 4096; PRAGMA key = '{key}'; PRAGMA kdf_iter = 4000; PRAGMA cipher_hmac_algorithm = HMAC_SHA1; PRAGMA cipher_kdf_algorithm = PBKDF2_HMAC_SHA512;"
                ))
                .expect("configure fixture encryption");
            connection
                .execute_batch(
                    r#"
                    CREATE TABLE c2c_messages (
                        msg_id INTEGER PRIMARY KEY, timestamp INTEGER NOT NULL, direction INTEGER NOT NULL,
                        sender_uid TEXT NOT NULL, sender_qq INTEGER, peer_uid TEXT NOT NULL, peer_qq INTEGER NOT NULL,
                        msg_type INTEGER NOT NULL, content_type INTEGER, proto_ver TEXT, inner_ts INTEGER,
                        text TEXT, content TEXT
                    );
                    INSERT INTO c2c_messages VALUES
                        (1, 1700000400, 0, 'u_sender', 10001, 'u_encrypted', 90909, 2, 1, NULL, NULL, '加密历史', '{"type":"text","text":"加密历史"}');
                    "#,
                )
                .expect("seed encrypted fixture");
        }
        let detected = key_detection::find_matching_key(
            &encrypted,
            vec!["wrong-key-value".to_string(), key.to_string()],
        );
        assert_eq!(detected.as_deref(), Some(key));
        let encrypted_size = fs::metadata(&encrypted).unwrap().len();
        assert!(encrypted_size > 4096);
        let mut wrapped = vec![0_u8; NTQQ_HEADER_SIZE as usize];
        wrapped[..16].copy_from_slice(b"SQLite format 3\0");
        wrapped[32..40].copy_from_slice(NTQQ_HEADER_MAGIC);
        wrapped.extend_from_slice(&fs::read(&encrypted).unwrap());
        let source = root.join("nt_msg.db");
        fs::write(&source, wrapped).unwrap();
        let source_size = fs::metadata(&source).unwrap().len();
        let leading_wrapped_sample = root.join("nt_msg-leading-sample.db");
        {
            let input = File::open(&source).unwrap();
            let mut output = File::create(&leading_wrapped_sample).unwrap();
            std::io::copy(&mut input.take(NTQQ_HEADER_SIZE + 4096), &mut output).unwrap();
        }
        let sparse_cipher_sample = root.join("encrypted-leading-sample.sqlite");
        key_detection::copy_cipher_sample(
            &leading_wrapped_sample,
            &sparse_cipher_sample,
            NTQQ_HEADER_SIZE,
            source_size,
        )
        .unwrap();
        assert_eq!(
            fs::metadata(&sparse_cipher_sample).unwrap().len(),
            encrypted_size
        );
        assert!(sqlcipher_key_valid(&sparse_cipher_sample, key));
        assert!(!sqlcipher_key_valid(
            &sparse_cipher_sample,
            "wrong-key-value"
        ));
        let imports = root.join("imports");

        let imported = import_path_blocking(
            &imports,
            &source,
            None,
            Some(key.to_string()),
            "10001".to_owned(),
        )
        .unwrap();
        assert_eq!(imported.format, "nt_msg_export");
        assert_eq!(imported.session_count, 1);
        assert!(!imports.join(&imported.id).join("encrypted.sqlite").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn recovers_message_tables_when_an_unrelated_encrypted_page_is_corrupt() {
        let root = temp_root("encrypted-recovery");
        fs::create_dir_all(&root).unwrap();
        let encrypted = root.join("encrypted.sqlite");
        let key = "0123456789abcdef";
        let corrupt_root_page;
        {
            let connection = Connection::open(&encrypted).expect("create encrypted fixture");
            connection
                .execute_batch(&format!(
                    "PRAGMA cipher_page_size = 4096; PRAGMA key = '{key}'; PRAGMA kdf_iter = 4000; PRAGMA cipher_hmac_algorithm = HMAC_SHA1; PRAGMA cipher_kdf_algorithm = PBKDF2_HMAC_SHA512;"
                ))
                .expect("configure fixture encryption");
            connection
                .execute_batch(
                    r#"
                    CREATE TABLE c2c_messages (
                        msg_id INTEGER PRIMARY KEY, timestamp INTEGER NOT NULL, direction INTEGER NOT NULL,
                        sender_uid TEXT NOT NULL, sender_qq INTEGER, peer_uid TEXT NOT NULL, peer_qq INTEGER NOT NULL,
                        msg_type INTEGER NOT NULL, content_type INTEGER, proto_ver TEXT, inner_ts INTEGER,
                        text TEXT, content TEXT
                    );
                    INSERT INTO c2c_messages VALUES
                        (1, 1700000400, 0, 'u_sender', 10001, 'u_recovered', 90909, 2, 1, NULL, NULL, '恢复消息', '{"type":"text","text":"恢复消息"}');
                    CREATE TABLE unrelated_cache (payload BLOB NOT NULL);
                    INSERT INTO unrelated_cache VALUES (zeroblob(8192));
                    "#,
                )
                .expect("seed encrypted recovery fixture");
            corrupt_root_page = connection
                .query_row(
                    "SELECT rootpage FROM sqlite_master WHERE name = 'unrelated_cache'",
                    [],
                    |row| row.get::<_, u64>(0),
                )
                .expect("read unrelated table root page");
        }
        {
            let mut file = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&encrypted)
                .expect("open encrypted fixture for corruption");
            let offset = (corrupt_root_page - 1) * 4096 + 64;
            file.seek(SeekFrom::Start(offset)).unwrap();
            let mut byte = [0_u8; 1];
            file.read_exact(&mut byte).unwrap();
            byte[0] ^= 0xff;
            file.seek(SeekFrom::Start(offset)).unwrap();
            file.write_all(&byte).unwrap();
            file.flush().unwrap();
        }

        let mut wrapped = vec![0_u8; NTQQ_HEADER_SIZE as usize];
        wrapped[..16].copy_from_slice(b"SQLite format 3\0");
        wrapped[32..40].copy_from_slice(NTQQ_HEADER_MAGIC);
        wrapped.extend_from_slice(&fs::read(&encrypted).unwrap());
        let source = root.join("nt_msg.db");
        fs::write(&source, wrapped).unwrap();
        let imports = root.join("imports");

        let imported = import_path_blocking(
            &imports,
            &source,
            None,
            Some(key.to_owned()),
            "10001".to_owned(),
        )
        .expect("recover supported message tables");

        assert!(imported.recovered);
        assert_eq!(imported.session_count, 1);
        assert_eq!(imported.message_count, 1);
        let recovered = open_read_only(&imports.join(&imported.id).join("database.sqlite"))
            .expect("open recovered database");
        let names = table_names(&recovered).expect("list recovered tables");
        assert!(names.contains("c2c_messages"));
        assert!(!names.contains("unrelated_cache"));
        drop(recovered);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn skips_an_unreadable_message_table_without_failing_the_import() {
        let root = temp_root("encrypted-message-recovery");
        fs::create_dir_all(&root).unwrap();
        let encrypted = root.join("encrypted.sqlite");
        let key = "0123456789abcdef";
        let corrupt_root_page;
        {
            let connection = Connection::open(&encrypted).expect("create encrypted fixture");
            connection
                .execute_batch(&format!(
                    "PRAGMA cipher_page_size = 4096; PRAGMA key = '{key}'; PRAGMA kdf_iter = 4000; PRAGMA cipher_hmac_algorithm = HMAC_SHA1; PRAGMA cipher_kdf_algorithm = PBKDF2_HMAC_SHA512;"
                ))
                .expect("configure fixture encryption");
            connection
                .execute_batch(
                    r#"
                    CREATE TABLE c2c_messages (
                        msg_id INTEGER PRIMARY KEY, timestamp INTEGER NOT NULL, direction INTEGER NOT NULL,
                        sender_uid TEXT NOT NULL, sender_qq INTEGER, peer_uid TEXT NOT NULL, peer_qq INTEGER NOT NULL,
                        msg_type INTEGER NOT NULL, content_type INTEGER, proto_ver TEXT, inner_ts INTEGER,
                        text TEXT, content TEXT
                    );
                    CREATE TABLE group_messages (
                        msg_id INTEGER PRIMARY KEY, timestamp INTEGER NOT NULL, direction INTEGER NOT NULL,
                        sender_uid TEXT NOT NULL, sender_qq INTEGER, group_id TEXT NOT NULL, group_qq INTEGER NOT NULL,
                        msg_type INTEGER NOT NULL, subtype INTEGER, content_type INTEGER, text TEXT,
                        parse_status TEXT NOT NULL, content TEXT
                    );
                    INSERT INTO c2c_messages VALUES
                        (1, 1700000400, 0, 'u_sender', 10001, 'u_corrupt', 90909, 2, 1, NULL, NULL, 'unreadable', '{}');
                    INSERT INTO group_messages VALUES
                        (2, 1700000500, 0, 'u_sender', 10001, 'g_recovered', 80808, 2, 1, 1, 'recovered', 'typed', '{}');
                    "#,
                )
                .expect("seed encrypted recovery fixture");
            corrupt_root_page = connection
                .query_row(
                    "SELECT rootpage FROM sqlite_master WHERE name = 'c2c_messages'",
                    [],
                    |row| row.get::<_, u64>(0),
                )
                .expect("read corrupt message table root page");
        }
        {
            let mut file = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&encrypted)
                .expect("open encrypted fixture for corruption");
            let offset = (corrupt_root_page - 1) * 4096 + 64;
            file.seek(SeekFrom::Start(offset)).unwrap();
            let mut byte = [0_u8; 1];
            file.read_exact(&mut byte).unwrap();
            byte[0] ^= 0xff;
            file.seek(SeekFrom::Start(offset)).unwrap();
            file.write_all(&byte).unwrap();
            file.flush().unwrap();
        }

        let mut wrapped = vec![0_u8; NTQQ_HEADER_SIZE as usize];
        wrapped[..16].copy_from_slice(b"SQLite format 3\0");
        wrapped[32..40].copy_from_slice(NTQQ_HEADER_MAGIC);
        wrapped.extend_from_slice(&fs::read(&encrypted).unwrap());
        let source = root.join("nt_msg.db");
        fs::write(&source, wrapped).unwrap();
        let imports = root.join("imports");

        let imported = import_path_blocking(
            &imports,
            &source,
            None,
            Some(key.to_owned()),
            "10001".to_owned(),
        )
        .expect("recover remaining readable message tables");

        assert!(imported.recovered);
        assert_eq!(imported.recovery_skipped_segments, 1);
        assert_eq!(imported.session_count, 1);
        assert_eq!(imported.message_count, 1);
        let recovered = open_read_only(&imports.join(&imported.id).join("database.sqlite"))
            .expect("open recovered database");
        assert_eq!(
            recovered
                .query_row("SELECT COUNT(*) FROM c2c_messages", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            recovered
                .query_row("SELECT COUNT(*) FROM group_messages", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
        drop(recovered);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn recognizes_legacy_encrypted_bak_without_creating_import() {
        let root = temp_root("legacy");
        fs::create_dir_all(&root).unwrap();
        let source = root.join("history.bak");
        let mut bytes = vec![0_u8; 4096];
        bytes[16..32].copy_from_slice(&[0x5a; 16]);
        fs::write(&source, bytes).unwrap();
        let imports = root.join("imports");

        let error =
            import_path_blocking(&imports, &source, None, None, "10001".to_owned()).unwrap_err();
        assert!(matches!(error, BackupImportError::LegacyBak));
        assert!(!imports.exists());
        fs::remove_dir_all(root).unwrap();
    }

    fn put_varint(mut value: u64, out: &mut Vec<u8>) {
        loop {
            let mut byte = (value & 0x7f) as u8;
            value >>= 7;
            if value != 0 {
                byte |= 0x80;
            }
            out.push(byte);
            if value == 0 {
                break;
            }
        }
    }

    fn put_string(number: u64, value: &str, out: &mut Vec<u8>) {
        put_varint((number << 3) | 2, out);
        put_varint(value.len() as u64, out);
        out.extend_from_slice(value.as_bytes());
    }

    fn put_u64(number: u64, value: u64, out: &mut Vec<u8>) {
        put_varint(number << 3, out);
        put_varint(value, out);
    }

    #[test]
    fn parses_ntqq_40800_text_wire_payload() {
        let elements = parse_ntqq_elements(&ntqq_text_blob("历史消息"), 2);
        assert_eq!(elements[0]["textElement"]["content"], "历史消息");
    }

    #[test]
    fn distinguishes_json_cards_from_multi_forward_messages() {
        let mut card = Vec::new();
        put_string(
            48602,
            r#"{"app":"com.tencent.gamecenter.mall","prompt":"活动卡片"}"#,
            &mut card,
        );
        let card_elements = parse_ntqq_elements(&wrap_ntqq_segment(&card), 11);
        assert_eq!(card_elements[0]["elementType"], 10);
        assert!(card_elements[0].get("arkElement").is_some());
        assert!(card_elements[0].get("multiForwardMsgElement").is_none());

        let mut forward = Vec::new();
        put_string(48601, "forward-resource", &mut forward);
        put_string(
            48602,
            r#"{"app":"com.tencent.multimsg","prompt":"[聊天记录]"}"#,
            &mut forward,
        );
        let forward_elements = parse_ntqq_elements(&wrap_ntqq_segment(&forward), 11);
        assert_eq!(forward_elements[0]["elementType"], 16);
        assert_eq!(
            forward_elements[0]["multiForwardMsgElement"]["resId"],
            "forward-resource"
        );
    }

    #[test]
    fn parses_ntqq_reply_and_video_fields_from_public_schema() {
        let mut reply = Vec::new();
        put_u64(47401, 9988, &mut reply);
        put_string(47413, "被回复的消息", &mut reply);
        put_string(45101, "回复正文", &mut reply);
        let reply_elements = parse_ntqq_elements(&wrap_ntqq_segment(&reply), 5);
        assert_eq!(reply_elements[0]["replyElement"]["replayMsgId"], "9988");
        assert_eq!(
            reply_elements[0]["replyElement"]["sourceMsgText"],
            "被回复的消息"
        );
        assert_eq!(reply_elements[1]["textElement"]["content"], "回复正文");

        let mut video = Vec::new();
        put_string(45402, "history.mp4", &mut video);
        put_u64(45405, 1024, &mut video);
        put_u64(45415, 12, &mut video);
        let video_elements = parse_ntqq_elements(&wrap_ntqq_segment(&video), 7);
        assert_eq!(video_elements[0]["elementType"], 4);
        assert_eq!(video_elements[0]["videoElement"]["duration"], 12);
    }
}
