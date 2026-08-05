use crate::base::{now_iso, preprocess_messages, resource_type_dir};
use crate::error::{ExportError, ExportResultT};
use crate::types::{CancellationToken, ChatInfo, CleanMessage, MessageResource};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const ACCOUNT_SCHEMA_VERSION: i64 = 2;
const ARCHIVE_APPLICATION_ID: i64 = 0x5143_4541;
const ACCOUNT_ARCHIVE_README: &str = include_str!("../../docs/QCEARCHIVE_README.md");

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountArchiveAccount {
    pub uid: Option<String>,
    pub uin: Option<String>,
    pub name: String,
    pub avatar_url: Option<String>,
}

impl AccountArchiveAccount {
    #[must_use]
    pub fn stable_id(&self) -> String {
        self.uin
            .as_deref()
            .filter(|value| !value.is_empty())
            .map_or_else(
                || {
                    self.uid
                        .as_deref()
                        .filter(|value| !value.is_empty())
                        .map_or_else(|| "account:unknown".to_owned(), |uid| format!("uid:{uid}"))
                },
                |uin| format!("uin:{uin}"),
            )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountConversationCategory {
    Private,
    Group,
    Other,
}

impl AccountConversationCategory {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Private => "private",
            Self::Group => "group",
            Self::Other => "other",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountRelationshipStatus {
    Friend,
    NonFriend,
    Group,
    UnavailableGroup,
    Other,
}

impl AccountRelationshipStatus {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Friend => "friend",
            Self::NonFriend => "non_friend",
            Self::Group => "group",
            Self::UnavailableGroup => "unavailable_group",
            Self::Other => "other",
        }
    }
}

#[derive(Debug, Clone)]
pub struct AccountArchiveConversation {
    pub conversation_id: String,
    pub chat_type: i64,
    pub category: AccountConversationCategory,
    pub relationship_status: AccountRelationshipStatus,
    pub source: String,
    pub chat_info: ChatInfo,
    pub aliases: Vec<(String, String)>,
    pub messages: Vec<CleanMessage>,
    pub resource_map: HashMap<String, Vec<MessageResource>>,
}

#[derive(Debug, Clone)]
pub struct AccountArchiveExtraResource {
    pub kind: String,
    pub conversation_id: Option<String>,
    pub collection_id: Option<String>,
    pub logical_id: String,
    pub parent_id: Option<String>,
    pub display_name: String,
    pub expects_file: bool,
    pub local_path: Option<PathBuf>,
    pub source_url: Option<String>,
    pub metadata: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountArchiveWarning {
    pub scope: String,
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conversation_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AccountArchiveOptions {
    pub output_path: PathBuf,
    pub account: AccountArchiveAccount,
    pub source_database_path: PathBuf,
    pub source_database_name: String,
    pub source_database_format: String,
    pub exporter_version: String,
    pub cancellation: CancellationToken,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountArchiveOutcome {
    pub file_path: PathBuf,
    pub file_size: u64,
    pub archive_id: String,
    pub conversation_count: usize,
    pub message_count: usize,
    pub resource_count: usize,
    pub missing_resource_count: usize,
    pub warning_count: usize,
    pub first_message_ms: Option<i64>,
    pub last_message_ms: Option<i64>,
}

#[derive(Debug)]
struct StoredBlob {
    sha256: String,
    archive_path: String,
    byte_size: u64,
    mime_type: Option<String>,
}

pub struct AccountArchiveBuilder {
    options: AccountArchiveOptions,
    archive_id: String,
    created_at: String,
    work_dir: PathBuf,
    partial_path: PathBuf,
    connection: Option<Connection>,
    source_sha256: String,
    source_size: u64,
    conversation_count: usize,
    message_count: usize,
    resource_count: usize,
    missing_resource_count: usize,
    first_message_ms: Option<i64>,
    last_message_ms: Option<i64>,
    category_counts: HashMap<String, usize>,
    warnings: Vec<AccountArchiveWarning>,
    finalized: bool,
}

impl AccountArchiveBuilder {
    pub fn create(options: AccountArchiveOptions) -> ExportResultT<Self> {
        cancellation_check(&options.cancellation)?;
        if options.output_path.exists() {
            return Err(ExportError::OutputDirConflict(options.output_path));
        }
        if !options.source_database_path.is_file() {
            return Err(ExportError::InvalidOptions(
                "账户归档缺少已解密的源数据库".to_owned(),
            ));
        }
        let parent = options
            .output_path
            .parent()
            .unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)
            .map_err(|error| ExportError::io("createAccountArchiveParent", parent, error))?;
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let archive_id = uuid_like_id(nonce);
        let suffix = format!("{}-{nonce}", std::process::id());
        let work_dir = parent.join(format!(".qce-account-{suffix}"));
        let partial_path = parent.join(format!(".qce-account-{suffix}.partial"));
        fs::create_dir(&work_dir)
            .map_err(|error| ExportError::io("createAccountArchiveTempDir", &work_dir, error))?;

        let build_result = (|| {
            let source_dir = work_dir.join("source");
            fs::create_dir_all(&source_dir).map_err(|error| {
                ExportError::io("createAccountArchiveSourceDir", &source_dir, error)
            })?;
            let source_target = source_dir.join("nt_msg.sqlite");
            copy_file_with_cancel(
                &options.source_database_path,
                &source_target,
                &options.cancellation,
            )?;
            let source_sha256 = hash_file_with_cancel(&source_target, &options.cancellation)?;
            let source_size = fs::metadata(&source_target)
                .map_err(|error| {
                    ExportError::io("statAccountArchiveSource", &source_target, error)
                })?
                .len();
            let database_path = work_dir.join("messages.sqlite");
            let connection = Connection::open(&database_path)?;
            create_schema(&connection)?;
            Ok((connection, source_sha256, source_size))
        })();

        let (connection, source_sha256, source_size) = match build_result {
            Ok(value) => value,
            Err(error) => {
                let _ = fs::remove_dir_all(&work_dir);
                return Err(error);
            }
        };

        Ok(Self {
            options,
            archive_id,
            created_at: now_iso(),
            work_dir,
            partial_path,
            connection: Some(connection),
            source_sha256,
            source_size,
            conversation_count: 0,
            message_count: 0,
            resource_count: 0,
            missing_resource_count: 0,
            first_message_ms: None,
            last_message_ms: None,
            category_counts: HashMap::new(),
            warnings: Vec::new(),
            finalized: false,
        })
    }

    pub fn add_warning(&mut self, warning: AccountArchiveWarning) -> ExportResultT<()> {
        cancellation_check(&self.options.cancellation)?;
        let connection = self.connection()?;
        connection.execute(
            "INSERT INTO export_warnings(scope, code, message, conversation_id, resource_id) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![warning.scope, warning.code, warning.message, warning.conversation_id, warning.resource_id],
        )?;
        self.warnings.push(warning);
        Ok(())
    }

    pub fn add_warnings(
        &mut self,
        warnings: impl IntoIterator<Item = AccountArchiveWarning>,
    ) -> ExportResultT<()> {
        for warning in warnings {
            self.add_warning(warning)?;
        }
        Ok(())
    }

    pub fn add_conversation(
        &mut self,
        mut conversation: AccountArchiveConversation,
    ) -> ExportResultT<()> {
        cancellation_check(&self.options.cancellation)?;
        conversation.messages = preprocess_messages(conversation.messages);
        let prepared = self.prepare_message_resources(&conversation.resource_map)?;
        for message in &mut conversation.messages {
            let mut value = serde_json::to_value(&*message)?;
            rewrite_resource_paths(&mut value, &prepared.rewrite_map, None);
            *message = serde_json::from_value(value)?;
        }

        let account_id = self.options.account.stable_id();
        let account = self.options.account.clone();
        let cancellation = self.options.cancellation.clone();
        let transaction = self.connection_mut()?.transaction()?;
        insert_account(&transaction, &account_id, &account)?;
        transaction.execute(
            r#"
            INSERT INTO conversations(
                conversation_id, account_id, chat_type, category, relationship_status, source,
                peer_uid, peer_uin, display_name, avatar_url
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
            ON CONFLICT(conversation_id) DO UPDATE SET
                category = excluded.category,
                relationship_status = excluded.relationship_status,
                source = excluded.source,
                peer_uid = excluded.peer_uid,
                peer_uin = COALESCE(excluded.peer_uin, conversations.peer_uin),
                display_name = excluded.display_name,
                avatar_url = COALESCE(excluded.avatar_url, conversations.avatar_url)
            "#,
            params![
                conversation.conversation_id,
                account_id,
                conversation.chat_type,
                conversation.category.as_str(),
                conversation.relationship_status.as_str(),
                conversation.source,
                conversation.chat_info.peer_uid,
                conversation.chat_info.peer_uin,
                conversation.chat_info.name,
                conversation.chat_info.avatar,
            ],
        )?;
        for (alias_type, alias_value) in &conversation.aliases {
            if alias_value.trim().is_empty() {
                continue;
            }
            transaction.execute(
                "INSERT OR IGNORE INTO conversation_aliases(conversation_id, alias_type, alias_value) VALUES (?1, ?2, ?3)",
                params![conversation.conversation_id, alias_type, alias_value],
            )?;
        }

        let stats = insert_messages(
            &transaction,
            &conversation,
            &prepared.attachments,
            &cancellation,
        )?;
        transaction.commit()?;

        self.conversation_count += 1;
        self.message_count += stats.message_count;
        self.missing_resource_count += prepared.missing_count;
        self.first_message_ms = min_optional(self.first_message_ms, stats.first_message_ms);
        self.last_message_ms = max_optional(self.last_message_ms, stats.last_message_ms);
        *self
            .category_counts
            .entry(conversation.category.as_str().to_owned())
            .or_default() += 1;
        Ok(())
    }

    pub fn add_extra_resource(
        &mut self,
        resource: AccountArchiveExtraResource,
    ) -> ExportResultT<bool> {
        cancellation_check(&self.options.cancellation)?;
        let blob = resource
            .local_path
            .as_deref()
            .filter(|path| path.is_file())
            .map(|path| self.store_blob(path))
            .transpose()?;
        let copied = blob.is_some();
        if !copied && resource.expects_file {
            self.missing_resource_count += 1;
        }
        let connection = self.connection()?;
        connection.execute(
            r#"
            INSERT INTO extra_resources(
                kind, conversation_id, collection_id, logical_id, parent_id, display_name,
                source_url, blob_sha256, copied, metadata_json
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
            "#,
            params![
                resource.kind,
                resource.conversation_id,
                resource.collection_id,
                resource.logical_id,
                resource.parent_id,
                resource.display_name,
                resource.source_url,
                blob.as_ref().map(|value| value.sha256.as_str()),
                i64::from(copied),
                serde_json::to_string(&resource.metadata)?,
            ],
        )?;
        Ok(copied || !resource.expects_file)
    }

    pub fn add_extra_resources(
        &mut self,
        resources: impl IntoIterator<Item = AccountArchiveExtraResource>,
    ) -> ExportResultT<(usize, usize)> {
        let mut copied = 0usize;
        let mut missing = 0usize;
        for resource in resources {
            if self.add_extra_resource(resource)? {
                copied += 1;
            } else {
                missing += 1;
            }
        }
        Ok((copied, missing))
    }

    pub fn finish(mut self) -> ExportResultT<AccountArchiveOutcome> {
        cancellation_check(&self.options.cancellation)?;
        self.resource_count =
            self.connection()?
                .query_row("SELECT COUNT(*) FROM resource_blobs", [], |row| row.get(0))?;
        let connection = self
            .connection
            .take()
            .ok_or_else(|| ExportError::Archive("账户归档数据库已经关闭".to_owned()))?;
        write_meta(&connection, &self)?;
        connection.execute_batch("ANALYZE; PRAGMA optimize;")?;
        let integrity: String =
            connection.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
        if integrity != "ok" {
            return Err(ExportError::Archive(format!(
                "账户归档 SQLite integrity_check 失败: {integrity}"
            )));
        }
        drop(connection);

        let manifest = self.manifest();
        let manifest_path = self.work_dir.join("manifest.json");
        fs::write(&manifest_path, serde_json::to_vec_pretty(&manifest)?).map_err(|error| {
            ExportError::io("writeAccountArchiveManifest", &manifest_path, error)
        })?;
        let readme_path = self.work_dir.join("README.md");
        fs::write(&readme_path, ACCOUNT_ARCHIVE_README.as_bytes())
            .map_err(|error| ExportError::io("writeAccountArchiveReadme", &readme_path, error))?;

        write_zip64(
            &self.work_dir,
            &self.partial_path,
            &self.options.cancellation,
        )?;
        fs::rename(&self.partial_path, &self.options.output_path).map_err(|error| {
            ExportError::io("commitAccountArchive", &self.options.output_path, error)
        })?;
        let file_size =
            fs::metadata(&self.options.output_path).map_or(0, |metadata| metadata.len());
        let outcome = AccountArchiveOutcome {
            file_path: self.options.output_path.clone(),
            file_size,
            archive_id: self.archive_id.clone(),
            conversation_count: self.conversation_count,
            message_count: self.message_count,
            resource_count: self.resource_count,
            missing_resource_count: self.missing_resource_count,
            warning_count: self.warnings.len(),
            first_message_ms: self.first_message_ms,
            last_message_ms: self.last_message_ms,
        };
        self.finalized = true;
        let _ = fs::remove_dir_all(&self.work_dir);
        Ok(outcome)
    }

    fn prepare_message_resources(
        &mut self,
        resource_map: &HashMap<String, Vec<MessageResource>>,
    ) -> ExportResultT<PreparedResources> {
        let mut attachments = Vec::new();
        let mut rewrite_map = HashMap::new();
        let mut missing_count = 0usize;
        let mut message_ids: Vec<_> = resource_map.keys().collect();
        message_ids.sort_unstable();
        for message_id in message_ids {
            for (index, resource) in resource_map[message_id].iter().enumerate() {
                cancellation_check(&self.options.cancellation)?;
                let local_path = resource.local_path.as_deref().map(Path::new);
                let blob = local_path
                    .filter(|path| path.is_file())
                    .map(|path| self.store_blob(path))
                    .transpose()?;
                if blob.is_none() {
                    missing_count += 1;
                }
                let source_name = resource
                    .filename
                    .clone()
                    .or_else(|| {
                        local_path.and_then(|path| {
                            path.file_name()
                                .map(|value| value.to_string_lossy().to_string())
                        })
                    })
                    .unwrap_or_default();
                if let Some(stored) = &blob {
                    for candidate in [
                        resource
                            .local_path
                            .as_deref()
                            .unwrap_or_default()
                            .to_owned(),
                        resource.url.as_deref().unwrap_or_default().to_owned(),
                        format!(
                            "resources/{}/{}",
                            resource_type_dir(&resource.resource_type),
                            source_name
                        ),
                        format!(
                            "{}/{}",
                            resource_type_dir(&resource.resource_type),
                            source_name
                        ),
                    ] {
                        if !candidate.is_empty() {
                            rewrite_map.insert(candidate, stored.archive_path.clone());
                        }
                    }
                }
                attachments.push(PreparedAttachment {
                    source_message_id: message_id.clone(),
                    resource_index: index,
                    resource_type: resource.resource_type.clone(),
                    file_name: (!source_name.is_empty()).then_some(source_name),
                    byte_size: blob.as_ref().map(|value| value.byte_size).or(resource.size),
                    source_url: resource.url.clone(),
                    blob_sha256: blob.as_ref().map(|value| value.sha256.clone()),
                    archive_path: blob.as_ref().map(|value| value.archive_path.clone()),
                    mime_type: blob.as_ref().and_then(|value| value.mime_type.clone()),
                    copied: blob.is_some(),
                });
            }
        }
        Ok(PreparedResources {
            attachments,
            rewrite_map,
            missing_count,
        })
    }

    fn store_blob(&mut self, source_path: &Path) -> ExportResultT<StoredBlob> {
        cancellation_check(&self.options.cancellation)?;
        let sha256 = hash_file_with_cancel(source_path, &self.options.cancellation)?;
        if let Some(existing) = self
            .connection()?
            .query_row(
                "SELECT archive_path, byte_size, mime_type FROM resource_blobs WHERE sha256 = ?1",
                params![sha256],
                |row| {
                    Ok(StoredBlob {
                        sha256: sha256.clone(),
                        archive_path: row.get(0)?,
                        byte_size: row.get::<_, i64>(1)?.try_into().unwrap_or_default(),
                        mime_type: row.get(2)?,
                    })
                },
            )
            .optional()?
        {
            return Ok(existing);
        }
        let metadata = fs::metadata(source_path)
            .map_err(|error| ExportError::io("statAccountArchiveResource", source_path, error))?;
        let extension = source_path
            .extension()
            .and_then(|value| value.to_str())
            .map(|value| format!(".{}", sanitize_extension(value)))
            .unwrap_or_default();
        let archive_path = format!("resources/blobs/{}/{}{}", &sha256[..2], sha256, extension);
        let target = self
            .work_dir
            .join(archive_path.replace('/', std::path::MAIN_SEPARATOR_STR));
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| ExportError::io("createAccountArchiveBlobDir", parent, error))?;
        }
        copy_file_with_cancel_for_operation(
            source_path,
            &target,
            &self.options.cancellation,
            "copyAccountArchiveResource",
        )?;
        let mime_type = mime_guess::from_path(source_path)
            .first_raw()
            .map(str::to_owned);
        self.connection()?.execute(
            "INSERT INTO resource_blobs(sha256, archive_path, byte_size, mime_type) VALUES (?1, ?2, ?3, ?4)",
            params![sha256, archive_path, metadata.len(), mime_type],
        )?;
        Ok(StoredBlob {
            sha256,
            archive_path,
            byte_size: metadata.len(),
            mime_type,
        })
    }

    fn connection(&self) -> ExportResultT<&Connection> {
        self.connection
            .as_ref()
            .ok_or_else(|| ExportError::Archive("账户归档数据库已经关闭".to_owned()))
    }

    fn connection_mut(&mut self) -> ExportResultT<&mut Connection> {
        self.connection
            .as_mut()
            .ok_or_else(|| ExportError::Archive("账户归档数据库已经关闭".to_owned()))
    }

    fn manifest(&self) -> Value {
        json!({
            "format": "qcearchive",
            "schemaVersion": ACCOUNT_SCHEMA_VERSION,
            "archiveKind": "account",
            "archiveId": self.archive_id,
            "createdAt": self.created_at,
            "generator": {
                "name": "QQ Chat Exporter",
                "version": self.options.exporter_version,
                "url": "https://github.com/shuakami/qq-chat-exporter"
            },
            "account": self.options.account,
            "sources": [{
                "kind": "imported_ntqq_database",
                "name": self.options.source_database_name,
                "format": self.options.source_database_format,
                "association": "user_selected"
            }, {
                "kind": "current_napcat_account"
            }],
            "counts": {
                "conversations": self.conversation_count,
                "messages": self.message_count,
                "resourceFiles": self.resource_count,
                "missingResources": self.missing_resource_count,
                "warnings": self.warnings.len(),
                "categories": self.category_counts,
            },
            "timeRange": {
                "firstMessageMs": self.first_message_ms,
                "lastMessageMs": self.last_message_ms,
            },
            "coverage": {
                "complete": self.warnings.is_empty() && self.missing_resource_count == 0,
                "warningCount": self.warnings.len(),
                "missingResourceCount": self.missing_resource_count,
            },
            "database": {
                "path": "messages.sqlite",
                "engine": "SQLite 3",
                "encrypted": false,
                "userVersion": ACCOUNT_SCHEMA_VERSION,
                "ftsTable": "message_search",
                "ftsTokenizer": "trigram"
            },
            "sourceDatabase": {
                "path": "source/nt_msg.sqlite",
                "engine": "SQLite 3",
                "encrypted": false,
                "format": self.options.source_database_format,
                "byteSize": self.source_size,
                "sha256": self.source_sha256,
            },
            "resources": {
                "root": "resources/",
                "blobRoot": "resources/blobs/",
                "pathSeparator": "/",
                "hash": "sha256"
            },
            "readme": "README.md"
        })
    }
}

impl Drop for AccountArchiveBuilder {
    fn drop(&mut self) {
        if !self.finalized {
            self.connection.take();
            let _ = fs::remove_file(&self.partial_path);
            let _ = fs::remove_dir_all(&self.work_dir);
        }
    }
}

#[derive(Debug)]
struct PreparedAttachment {
    source_message_id: String,
    resource_index: usize,
    resource_type: String,
    file_name: Option<String>,
    byte_size: Option<u64>,
    source_url: Option<String>,
    blob_sha256: Option<String>,
    archive_path: Option<String>,
    mime_type: Option<String>,
    copied: bool,
}

#[derive(Debug)]
struct PreparedResources {
    attachments: Vec<PreparedAttachment>,
    rewrite_map: HashMap<String, String>,
    missing_count: usize,
}

#[derive(Debug)]
struct InsertStats {
    message_count: usize,
    first_message_ms: Option<i64>,
    last_message_ms: Option<i64>,
}

fn create_schema(connection: &Connection) -> ExportResultT<()> {
    connection.execute_batch(&format!(
        r#"
        PRAGMA application_id = {ARCHIVE_APPLICATION_ID};
        PRAGMA user_version = {ACCOUNT_SCHEMA_VERSION};
        PRAGMA foreign_keys = ON;
        PRAGMA journal_mode = DELETE;
        PRAGMA synchronous = FULL;

        CREATE TABLE archive_meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
        CREATE TABLE accounts(
            account_id TEXT PRIMARY KEY,
            uid TEXT,
            uin TEXT,
            display_name TEXT NOT NULL,
            avatar_url TEXT
        );
        CREATE TABLE conversations(
            conversation_id TEXT PRIMARY KEY,
            account_id TEXT NOT NULL REFERENCES accounts(account_id),
            chat_type INTEGER NOT NULL,
            category TEXT NOT NULL CHECK(category IN ('private', 'group', 'other')),
            relationship_status TEXT NOT NULL CHECK(relationship_status IN ('friend', 'non_friend', 'group', 'unavailable_group', 'other')),
            source TEXT NOT NULL,
            peer_uid TEXT NOT NULL,
            peer_uin TEXT,
            display_name TEXT NOT NULL,
            avatar_url TEXT
        );
        CREATE INDEX idx_conversations_account_category ON conversations(account_id, category, relationship_status);
        CREATE TABLE conversation_aliases(
            conversation_id TEXT NOT NULL REFERENCES conversations(conversation_id) ON DELETE CASCADE,
            alias_type TEXT NOT NULL,
            alias_value TEXT NOT NULL,
            PRIMARY KEY(conversation_id, alias_type, alias_value)
        );
        CREATE INDEX idx_conversation_alias_value ON conversation_aliases(alias_value);
        CREATE TABLE participants(
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
        CREATE INDEX idx_participants_uid ON participants(uid) WHERE uid IS NOT NULL AND uid <> '';
        CREATE INDEX idx_participants_uin ON participants(uin) WHERE uin IS NOT NULL AND uin <> '';
        CREATE TABLE messages(
            message_key TEXT PRIMARY KEY,
            source_message_id TEXT NOT NULL,
            conversation_id TEXT NOT NULL REFERENCES conversations(conversation_id),
            seq TEXT NOT NULL,
            timestamp_ms INTEGER NOT NULL,
            time_text TEXT NOT NULL,
            sender_id TEXT NOT NULL REFERENCES participants(participant_id),
            sender_name TEXT NOT NULL,
            is_outgoing INTEGER CHECK(is_outgoing IN (0, 1) OR is_outgoing IS NULL),
            message_type TEXT NOT NULL,
            text_content TEXT NOT NULL,
            recalled INTEGER NOT NULL CHECK(recalled IN (0, 1)),
            system INTEGER NOT NULL CHECK(system IN (0, 1)),
            content_json TEXT NOT NULL,
            raw_json TEXT,
            message_json TEXT NOT NULL
        );
        CREATE INDEX idx_messages_conversation_time ON messages(conversation_id, timestamp_ms, message_key);
        CREATE INDEX idx_messages_sender_time ON messages(sender_id, timestamp_ms);
        CREATE INDEX idx_messages_source_id ON messages(conversation_id, source_message_id);
        CREATE TABLE message_elements(
            message_key TEXT NOT NULL REFERENCES messages(message_key) ON DELETE CASCADE,
            element_index INTEGER NOT NULL,
            element_type TEXT NOT NULL,
            data_json TEXT NOT NULL,
            PRIMARY KEY(message_key, element_index)
        );
        CREATE INDEX idx_message_elements_type ON message_elements(element_type);
        CREATE TABLE resource_blobs(
            sha256 TEXT PRIMARY KEY,
            archive_path TEXT NOT NULL UNIQUE,
            byte_size INTEGER NOT NULL,
            mime_type TEXT
        );
        CREATE TABLE attachments(
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
            sha256 TEXT REFERENCES resource_blobs(sha256),
            copied INTEGER NOT NULL CHECK(copied IN (0, 1))
        );
        CREATE INDEX idx_attachments_message ON attachments(message_key, resource_index);
        CREATE INDEX idx_attachments_sha256 ON attachments(sha256) WHERE sha256 IS NOT NULL;
        CREATE TABLE extra_resources(
            resource_id INTEGER PRIMARY KEY,
            kind TEXT NOT NULL,
            conversation_id TEXT REFERENCES conversations(conversation_id),
            collection_id TEXT,
            logical_id TEXT NOT NULL,
            parent_id TEXT,
            display_name TEXT NOT NULL,
            source_url TEXT,
            blob_sha256 TEXT REFERENCES resource_blobs(sha256),
            copied INTEGER NOT NULL CHECK(copied IN (0, 1)),
            metadata_json TEXT NOT NULL
        );
        CREATE INDEX idx_extra_resources_kind_conversation ON extra_resources(kind, conversation_id);
        CREATE TABLE export_warnings(
            warning_id INTEGER PRIMARY KEY,
            scope TEXT NOT NULL,
            code TEXT NOT NULL,
            message TEXT NOT NULL,
            conversation_id TEXT,
            resource_id TEXT
        );
        CREATE VIRTUAL TABLE message_search USING fts5(
            message_key UNINDEXED,
            conversation_name,
            sender_name,
            content,
            tokenize = 'trigram'
        );
        "#
    ))?;
    Ok(())
}

fn insert_account(
    transaction: &Transaction<'_>,
    account_id: &str,
    account: &AccountArchiveAccount,
) -> ExportResultT<()> {
    transaction.execute(
        r#"
        INSERT INTO accounts(account_id, uid, uin, display_name, avatar_url)
        VALUES (?1, ?2, ?3, ?4, ?5)
        ON CONFLICT(account_id) DO UPDATE SET
            uid = COALESCE(excluded.uid, accounts.uid),
            uin = COALESCE(excluded.uin, accounts.uin),
            display_name = excluded.display_name,
            avatar_url = COALESCE(excluded.avatar_url, accounts.avatar_url)
        "#,
        params![
            account_id,
            account.uid,
            account.uin,
            account.name,
            account.avatar_url
        ],
    )?;
    Ok(())
}

fn insert_messages(
    transaction: &Transaction<'_>,
    conversation: &AccountArchiveConversation,
    attachments: &[PreparedAttachment],
    cancellation: &CancellationToken,
) -> ExportResultT<InsertStats> {
    let mut source_to_message_key = HashMap::new();
    let mut existing = HashSet::new();
    let mut first_message_ms = None;
    let mut last_message_ms = None;
    for (index, message) in conversation.messages.iter().enumerate() {
        cancellation_check(cancellation)?;
        let participant_id = participant_id(message);
        transaction.execute(
            r#"
            INSERT INTO participants(participant_id, uid, uin, display_name, nickname, group_card, remark, title, avatar_base64)
            VALUES (?1, NULLIF(?2, ''), ?3, ?4, ?5, ?6, ?7, ?8, ?9)
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
        let message_key = unique_message_key(
            &conversation.conversation_id,
            &message.id,
            index,
            &mut existing,
        );
        source_to_message_key
            .entry(message.id.clone())
            .or_insert_with(|| message_key.clone());
        let timestamp_ms = normalize_timestamp_ms(message.timestamp);
        first_message_ms = min_optional(first_message_ms, Some(timestamp_ms));
        last_message_ms = max_optional(last_message_ms, Some(timestamp_ms));
        let text = searchable_text(message);
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
                message.id,
                conversation.conversation_id,
                message.seq,
                timestamp_ms,
                message.time,
                participant_id,
                message.sender.name,
                outgoing_flag(message, &conversation.chat_info),
                message.message_type,
                text,
                i64::from(message.recalled),
                i64::from(message.system),
                serde_json::to_string(&message.content)?,
                message
                    .raw_message
                    .as_ref()
                    .map(serde_json::to_string)
                    .transpose()?,
                serde_json::to_string(message)?,
            ],
        )?;
        transaction.execute(
            "INSERT INTO message_search(message_key, conversation_name, sender_name, content) VALUES (?1, ?2, ?3, ?4)",
            params![message_key, conversation.chat_info.name, message.sender.name, text],
        )?;
        for (element_index, element) in message.content.elements.iter().enumerate() {
            transaction.execute(
                "INSERT INTO message_elements(message_key, element_index, element_type, data_json) VALUES (?1, ?2, ?3, ?4)",
                params![message_key, element_index, element.element_type, serde_json::to_string(&element.data)?],
            )?;
        }
    }
    for attachment in attachments {
        cancellation_check(cancellation)?;
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
                attachment.blob_sha256,
                i64::from(attachment.copied),
            ],
        )?;
    }
    Ok(InsertStats {
        message_count: conversation.messages.len(),
        first_message_ms,
        last_message_ms,
    })
}

fn write_meta(connection: &Connection, builder: &AccountArchiveBuilder) -> ExportResultT<()> {
    let meta = [
        ("format", "qcearchive".to_owned()),
        ("archive_kind", "account".to_owned()),
        ("archive_id", builder.archive_id.clone()),
        ("schema_version", ACCOUNT_SCHEMA_VERSION.to_string()),
        ("created_at", builder.created_at.clone()),
        (
            "generator_version",
            builder.options.exporter_version.clone(),
        ),
        ("conversation_count", builder.conversation_count.to_string()),
        ("message_count", builder.message_count.to_string()),
        ("resource_count", builder.resource_count.to_string()),
        (
            "missing_resource_count",
            builder.missing_resource_count.to_string(),
        ),
        ("warning_count", builder.warnings.len().to_string()),
    ];
    for (key, value) in meta {
        connection.execute(
            "INSERT INTO archive_meta(key, value) VALUES (?1, ?2)",
            params![key, value],
        )?;
    }
    Ok(())
}

fn write_zip64(
    work_dir: &Path,
    output_path: &Path,
    cancellation: &CancellationToken,
) -> ExportResultT<()> {
    let output = File::create(output_path)
        .map_err(|error| ExportError::io("createAccountArchive", output_path, error))?;
    let mut zip = zip::ZipWriter::new(output);
    let mut files: Vec<PathBuf> = walkdir::WalkDir::new(work_dir)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .map(walkdir::DirEntry::into_path)
        .collect();
    files.sort();
    for directory in ["source/", "resources/", "resources/blobs/"] {
        zip.add_directory(
            directory,
            zip::write::SimpleFileOptions::default().unix_permissions(0o755),
        )
        .map_err(|error| ExportError::Archive(error.to_string()))?;
    }
    for path in files {
        cancellation_check(cancellation)?;
        let relative = path
            .strip_prefix(work_dir)
            .map_err(|error| ExportError::Archive(error.to_string()))?
            .to_string_lossy()
            .replace('\\', "/");
        let metadata = fs::metadata(&path)
            .map_err(|error| ExportError::io("statAccountArchiveEntry", &path, error))?;
        let method = if is_already_compressed(&path) {
            zip::CompressionMethod::Stored
        } else {
            zip::CompressionMethod::Deflated
        };
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(method)
            .large_file(metadata.len() > u64::from(u32::MAX))
            .unix_permissions(0o644);
        zip.start_file(relative, options)
            .map_err(|error| ExportError::Archive(error.to_string()))?;
        let mut input = File::open(&path)
            .map_err(|error| ExportError::io("readAccountArchiveEntry", &path, error))?;
        copy_stream_with_cancel(
            &mut input,
            &mut zip,
            cancellation,
            "writeAccountArchiveEntry",
            &path,
        )?;
    }
    zip.finish()
        .map_err(|error| ExportError::Archive(error.to_string()))?;
    Ok(())
}

fn hash_file_with_cancel(path: &Path, cancellation: &CancellationToken) -> ExportResultT<String> {
    let mut file =
        File::open(path).map_err(|error| ExportError::io("hashAccountArchiveFile", path, error))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 128 * 1024];
    loop {
        cancellation_check(cancellation)?;
        let read = file
            .read(&mut buffer)
            .map_err(|error| ExportError::io("hashAccountArchiveFile", path, error))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn copy_file_with_cancel(
    source: &Path,
    target: &Path,
    cancellation: &CancellationToken,
) -> ExportResultT<u64> {
    copy_file_with_cancel_for_operation(
        source,
        target,
        cancellation,
        "copyAccountArchiveSourceDatabase",
    )
}

fn copy_file_with_cancel_for_operation(
    source: &Path,
    target: &Path,
    cancellation: &CancellationToken,
    operation: &'static str,
) -> ExportResultT<u64> {
    let mut input =
        File::open(source).map_err(|error| ExportError::io(operation, source, error))?;
    let mut output =
        File::create(target).map_err(|error| ExportError::io(operation, target, error))?;
    copy_stream_with_cancel(&mut input, &mut output, cancellation, operation, target)
}

fn copy_stream_with_cancel<R: Read, W: Write>(
    input: &mut R,
    output: &mut W,
    cancellation: &CancellationToken,
    operation: &'static str,
    path: &Path,
) -> ExportResultT<u64> {
    let mut copied = 0_u64;
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        cancellation_check(cancellation)?;
        let read = input
            .read(&mut buffer)
            .map_err(|error| ExportError::io(operation, path, error))?;
        if read == 0 {
            break;
        }
        output
            .write_all(&buffer[..read])
            .map_err(|error| ExportError::io(operation, path, error))?;
        copied = copied.saturating_add(read as u64);
    }
    Ok(copied)
}

fn rewrite_resource_paths(value: &mut Value, map: &HashMap<String, String>, key: Option<&str>) {
    match value {
        Value::Object(object) => {
            for (child_key, child) in object {
                rewrite_resource_paths(child, map, Some(child_key));
            }
        }
        Value::Array(array) => {
            for child in array {
                rewrite_resource_paths(child, map, key);
            }
        }
        Value::String(path) if matches!(key, Some("localPath" | "url" | "path" | "src")) => {
            if let Some(replacement) = map.get(path) {
                *path = replacement.clone();
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
        format!("uid:{}", message.sender.uid)
    } else if let Some(uin) = message
        .sender
        .uin
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        format!("uin:{uin}")
    } else {
        format!("unknown:{}", message.sender.name)
    }
}

fn unique_message_key(
    conversation_id: &str,
    source_id: &str,
    index: usize,
    existing: &mut HashSet<String>,
) -> String {
    let source = if source_id.trim().is_empty() {
        format!("message-{}", index + 1)
    } else {
        source_id.to_owned()
    };
    let base = format!("{conversation_id}:{source}");
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

fn min_optional(left: Option<i64>, right: Option<i64>) -> Option<i64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (left, right) => left.or(right),
    }
}

fn max_optional(left: Option<i64>, right: Option<i64>) -> Option<i64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.max(right)),
        (left, right) => left.or(right),
    }
}

fn sanitize_extension(extension: &str) -> String {
    let value: String = extension
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(12)
        .collect();
    if value.is_empty() {
        "bin".to_owned()
    } else {
        value.to_ascii_lowercase()
    }
}

fn is_already_compressed(path: &Path) -> bool {
    path.extension()
        .and_then(|value| value.to_str())
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

fn cancellation_check(token: &CancellationToken) -> ExportResultT<()> {
    if token.is_cancelled() {
        Err(ExportError::Cancelled)
    } else {
        Ok(())
    }
}

fn uuid_like_id(nonce: u128) -> String {
    let mut hasher = Sha256::new();
    hasher.update(std::process::id().to_le_bytes());
    hasher.update(nonce.to_le_bytes());
    let hash = format!("{:x}", hasher.finalize());
    format!(
        "{}-{}-{}-{}-{}",
        &hash[0..8],
        &hash[8..12],
        &hash[12..16],
        &hash[16..20],
        &hash[20..32]
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{MessageContent, Sender};
    use std::io::Read;

    struct CancellingReader {
        cancellation: CancellationToken,
        emitted: bool,
    }

    impl Read for CancellingReader {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            if self.emitted {
                return Ok(0);
            }
            buffer[..4].copy_from_slice(b"test");
            self.emitted = true;
            self.cancellation.cancel();
            Ok(4)
        }
    }

    #[test]
    fn chunked_copy_stops_when_account_export_is_cancelled() {
        let cancellation = CancellationToken::new();
        let mut input = CancellingReader {
            cancellation: cancellation.clone(),
            emitted: false,
        };
        let mut output = Vec::new();
        let error = copy_stream_with_cancel(
            &mut input,
            &mut output,
            &cancellation,
            "testAccountArchiveCancellation",
            Path::new("test.bin"),
        )
        .expect_err("copy should stop after cancellation");
        assert!(matches!(error, ExportError::Cancelled));
        assert_eq!(output, b"test");
    }

    #[test]
    fn creates_multi_conversation_account_archive_with_plain_source_and_deduped_blobs() {
        let root = std::env::temp_dir().join(format!(
            "qce-account-archive-test-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        fs::create_dir_all(&root).expect("create temp");
        let source_db = root.join("source.sqlite");
        let source = Connection::open(&source_db).expect("open source");
        source
            .execute_batch("CREATE TABLE original_data(value TEXT); INSERT INTO original_data VALUES ('kept');")
            .expect("create source schema");
        drop(source);
        let expected_source_hash = hash_file_with_cancel(&source_db, &CancellationToken::new())
            .expect("hash source database");
        let media = root.join("picture.png");
        fs::write(&media, b"same-resource").expect("write media");
        let output = root.join("account.qcearchive");
        let mut builder = AccountArchiveBuilder::create(AccountArchiveOptions {
            output_path: output.clone(),
            account: AccountArchiveAccount {
                uid: Some("u_self".to_owned()),
                uin: Some("123456789".to_owned()),
                name: "测试账号".to_owned(),
                avatar_url: None,
            },
            source_database_path: source_db,
            source_database_name: "nt_msg.db".to_owned(),
            source_database_format: "nt_msg_raw".to_owned(),
            exporter_version: "test".to_owned(),
            cancellation: CancellationToken::new(),
        })
        .expect("create builder");

        for (id, chat_type, category, status) in [
            (
                "1:u_peer",
                1,
                AccountConversationCategory::Private,
                AccountRelationshipStatus::NonFriend,
            ),
            (
                "2:123456",
                2,
                AccountConversationCategory::Group,
                AccountRelationshipStatus::UnavailableGroup,
            ),
        ] {
            let message_id = format!("message-{chat_type}");
            builder
                .add_conversation(AccountArchiveConversation {
                    conversation_id: id.to_owned(),
                    chat_type,
                    category,
                    relationship_status: status,
                    source: "merged".to_owned(),
                    chat_info: ChatInfo {
                        name: format!("会话 {chat_type}"),
                        chat_type: if chat_type == 2 { "group" } else { "private" }.to_owned(),
                        self_uid: Some("u_self".to_owned()),
                        self_uin: Some("123456789".to_owned()),
                        peer_uid: Some(if chat_type == 2 { "123456" } else { "u_peer" }.to_owned()),
                        peer_uin: (chat_type == 1).then(|| "10001".to_owned()),
                        ..ChatInfo::default()
                    },
                    aliases: vec![("uin".to_owned(), "10001".to_owned())],
                    messages: vec![CleanMessage {
                        id: message_id.clone(),
                        seq: "1".to_owned(),
                        timestamp: 1_700_000_000,
                        time: "2023-11-14 22:13:20".to_owned(),
                        sender: Sender {
                            uid: "u_peer".to_owned(),
                            uin: Some("10001".to_owned()),
                            name: "发送者".to_owned(),
                            ..Sender::default()
                        },
                        message_type: "text".to_owned(),
                        content: MessageContent {
                            text: "账户归档可检索消息".to_owned(),
                            ..MessageContent::default()
                        },
                        recalled: false,
                        system: false,
                        raw_message: Some(json!({"msgId": message_id})),
                    }],
                    resource_map: HashMap::from([(
                        message_id,
                        vec![MessageResource {
                            resource_type: "image".to_owned(),
                            filename: Some("picture.png".to_owned()),
                            local_path: Some(media.to_string_lossy().to_string()),
                            ..MessageResource::default()
                        }],
                    )]),
                })
                .expect("add conversation");
        }
        builder
            .add_warning(AccountArchiveWarning {
                scope: "group_album".to_owned(),
                code: "UNAVAILABLE_GROUP".to_owned(),
                message: "已退出群无法读取相册".to_owned(),
                conversation_id: Some("2:123456".to_owned()),
                resource_id: None,
            })
            .expect("add warning");
        let outcome = builder.finish().expect("finish archive");
        assert_eq!(outcome.conversation_count, 2);
        assert_eq!(outcome.message_count, 2);
        assert_eq!(outcome.resource_count, 1);
        assert_eq!(outcome.warning_count, 1);

        let file = File::open(&output).expect("open archive");
        let mut zip = zip::ZipArchive::new(file).expect("read archive");
        let manifest: Value =
            serde_json::from_reader(zip.by_name("manifest.json").expect("manifest"))
                .expect("parse manifest");
        assert_eq!(manifest["archiveKind"], "account");
        assert_eq!(manifest["schemaVersion"], 2);
        assert_eq!(manifest["coverage"]["complete"], false);
        assert_eq!(manifest["sourceDatabase"]["sha256"], expected_source_hash);
        let blob_entries = (0..zip.len())
            .filter_map(|index| {
                zip.by_index(index)
                    .ok()
                    .map(|entry| entry.name().to_owned())
            })
            .filter(|name| name.starts_with("resources/blobs/") && !name.ends_with('/'))
            .count();
        assert_eq!(blob_entries, 1);
        let mut database_bytes = Vec::new();
        zip.by_name("messages.sqlite")
            .expect("messages db")
            .read_to_end(&mut database_bytes)
            .expect("read db");
        let mut source_bytes = Vec::new();
        zip.by_name("source/nt_msg.sqlite")
            .expect("plain source db")
            .read_to_end(&mut source_bytes)
            .expect("read source db");
        drop(zip);
        let extracted = root.join("messages.sqlite");
        fs::write(&extracted, database_bytes).expect("write extracted db");
        let connection = Connection::open(extracted).expect("open account db");
        let version: i64 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .expect("version");
        assert_eq!(version, 2);
        let counts: (i64, i64, i64) = connection
            .query_row(
                "SELECT (SELECT COUNT(*) FROM conversations), (SELECT COUNT(*) FROM messages), (SELECT COUNT(*) FROM resource_blobs)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("counts");
        assert_eq!(counts, (2, 2, 1));
        drop(connection);
        let extracted_source = root.join("source-extracted.sqlite");
        fs::write(&extracted_source, source_bytes).expect("write extracted source");
        let source_connection = Connection::open(extracted_source).expect("open extracted source");
        let original_value: String = source_connection
            .query_row("SELECT value FROM original_data", [], |row| row.get(0))
            .expect("read source data");
        assert_eq!(original_value, "kept");
        drop(source_connection);
        let _ = fs::remove_dir_all(root);
    }
}
