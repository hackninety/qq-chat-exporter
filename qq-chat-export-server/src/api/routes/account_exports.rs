use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use axum::extract::{Extension, Json, State};
use axum::response::Response;
use chrono::Local;
use qce_exporter::account_archive_exporter::{
    AccountArchiveAccount, AccountArchiveBuilder, AccountArchiveConversation,
    AccountArchiveExtraResource, AccountArchiveOptions, AccountArchiveOutcome,
    AccountArchiveWarning, AccountConversationCategory, AccountRelationshipStatus,
};
use qce_exporter::types::{CancellationToken, ChatInfo, CleanMessage};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::api::helpers::backfill_self_sender_names;
use crate::api::http_security::http_download_to_file;
use crate::api::response::{self, ApiError, ErrorType, RequestId};
use crate::api::routes::albums::{fetch_album_list, fetch_album_media};
use crate::api::routes::files::{download_file_to, fetch_all_files_recursive};
use crate::api::routes::messages::{
    broadcast_progress, fill_group_member_names, generate_download_url, generate_task_id, now_iso,
    prepare_output_directory, register_task, release_export_path, reserve_export_file_name,
    to_exporter_resource_map, update_task,
};
use crate::api::routes::stickers::get_sticker_packs;
use crate::api::state::SharedState;
use crate::backup_import::ImportedSession;
use crate::export_debug::ExportDebugSession;
use crate::fetcher::{BatchFetchConfig, BatchMessageFetcher, MessageFilter, Peer};
use crate::parser::{ForwardFetcher, SimpleMessageParser, SimpleParserOptions};

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountExportRequest {
    backup_import_id: String,
    #[serde(default)]
    debug_export: bool,
    #[serde(default)]
    output_dir: String,
}

#[derive(Debug, Clone)]
struct AccountSession {
    conversation_id: String,
    chat_type: i64,
    peer_uid: String,
    peer_uin: Option<String>,
    name: String,
    avatar_url: Option<String>,
    category: AccountConversationCategory,
    relationship_status: AccountRelationshipStatus,
    source: String,
    aliases: Vec<(String, String)>,
    backup_peers: Vec<String>,
}

impl AccountSession {
    fn alias_keys(&self) -> Vec<String> {
        let mut keys = vec![format!("{}|{}", self.chat_type, self.peer_uid)];
        if let Some(uin) = self.peer_uin.as_deref().filter(|value| !value.is_empty()) {
            keys.push(format!("{}|{uin}", self.chat_type));
        }
        keys
    }
}

#[derive(Debug)]
struct AccountInventory {
    account: AccountArchiveAccount,
    sessions: Vec<AccountSession>,
    historical_session_count: usize,
    current_friend_count: usize,
    current_group_count: usize,
    local_session_count: usize,
    warnings: Vec<AccountArchiveWarning>,
}

impl AccountInventory {
    fn counts(&self) -> Value {
        let mut friend = 0usize;
        let mut non_friend = 0usize;
        let mut group = 0usize;
        let mut unavailable_group = 0usize;
        let mut other = 0usize;
        for session in &self.sessions {
            match session.relationship_status {
                AccountRelationshipStatus::Friend => friend += 1,
                AccountRelationshipStatus::NonFriend => non_friend += 1,
                AccountRelationshipStatus::Group => group += 1,
                AccountRelationshipStatus::UnavailableGroup => unavailable_group += 1,
                AccountRelationshipStatus::Other => other += 1,
            }
        }
        json!({
            "total": self.sessions.len(),
            "friend": friend,
            "nonFriend": non_friend,
            "group": group,
            "unavailableGroup": unavailable_group,
            "other": other,
        })
    }
}

pub async fn preview_account_export(
    State(state): State<SharedState>,
    Extension(RequestId(request_id)): Extension<RequestId>,
    Json(request): Json<AccountExportRequest>,
) -> Response {
    match prepare_account_export(&state, &request, false).await {
        Ok(prepared) => response::success(
            json!({
                "account": prepared.inventory.account,
                "backup": prepared.backup,
                "counts": prepared.inventory.counts(),
                "historicalSessionCount": prepared.inventory.historical_session_count,
                "currentFriendCount": prepared.inventory.current_friend_count,
                "currentGroupCount": prepared.inventory.current_group_count,
                "localSessionCount": prepared.inventory.local_session_count,
                "warningCount": prepared.inventory.warnings.len(),
                "warnings": prepared.inventory.warnings,
                "fixedIncludes": [
                    "messages.sqlite（规范化消息与 FTS5 索引）",
                    "source/nt_msg.sqlite（已解密源库副本）",
                    "消息媒体、头像和表情包",
                    "可访问群的群文件和群相册",
                ],
                "notice": "所选已解密备份将关联到当前登录账号；导出阶段不再次校验数据库所属账号。",
            }),
            &request_id,
        ),
        Err(error) => response::error(&error, &request_id),
    }
}

pub async fn create_account_export(
    State(state): State<SharedState>,
    Extension(RequestId(request_id)): Extension<RequestId>,
    Json(request): Json<AccountExportRequest>,
) -> Response {
    let prepared = match prepare_account_export(&state, &request, true).await {
        Ok(prepared) => prepared,
        Err(error) => return response::error(&error, &request_id),
    };
    let account_uin = prepared
        .inventory
        .account
        .uin
        .clone()
        .unwrap_or_else(|| "unknown".to_owned());
    let base_file_name = format!(
        "account_{}_{}.qcearchive",
        safe_file_component(&account_uin),
        Local::now().format("%Y%m%d_%H%M%S")
    );
    let file_name = reserve_export_file_name(&prepared.output_dir, &base_file_name);
    let output_path = prepared.output_dir.join(&file_name);
    let download_url = generate_download_url(
        &output_path,
        &file_name,
        &prepared.custom_output_dir,
        "/downloads/",
    );
    let task_id = generate_task_id("account_export");
    let task = json!({
        "taskId": task_id,
        "format": "QCEARCHIVE_ACCOUNT",
        "archiveKind": "account",
        "sessionName": format!("全账号归档 · {account_uin}"),
        "account": prepared.inventory.account,
        "backupImportId": request.backup_import_id,
        "fileName": file_name,
        "downloadUrl": download_url,
        "status": "running",
        "progress": 0,
        "messageCount": 0,
        "conversationCount": prepared.inventory.sessions.len(),
        "resourceCount": 0,
        "missingResourceCount": 0,
        "createdAt": now_iso(),
        "options": { "debugExport": request.debug_export, "outputDir": request.output_dir },
    });
    if !register_task(&state, &task).await {
        release_export_path(&output_path);
        let error = ApiError::new(
            ErrorType::Api,
            "运行中的导出任务已达到上限",
            "EXPORT_TASK_LIMIT_REACHED",
        )
        .with_status(axum::http::StatusCode::TOO_MANY_REQUESTS);
        return response::error(&error, &request_id);
    }

    let reply = json!({
        "taskId": task_id,
        "format": "QCEARCHIVE_ACCOUNT",
        "archiveKind": "account",
        "fileName": file_name,
        "downloadUrl": download_url,
        "status": "running",
        "conversationCount": prepared.inventory.sessions.len(),
    });
    let background_state = Arc::clone(&state);
    tokio::spawn(async move {
        run_account_export(
            background_state,
            task_id,
            request,
            prepared,
            output_path,
            file_name,
            download_url,
        )
        .await;
    });
    response::success(reply, &request_id)
}

struct PreparedAccountExport {
    backup: crate::backup_import::BackupImport,
    source_database_path: PathBuf,
    inventory: AccountInventory,
    output_dir: PathBuf,
    custom_output_dir: String,
}

async fn prepare_account_export(
    state: &SharedState,
    request: &AccountExportRequest,
    validate_output: bool,
) -> Result<PreparedAccountExport, ApiError> {
    if request.backup_import_id.trim().is_empty() {
        return Err(ApiError::validation(
            "请选择一个已导入并解密的聊天记录数据库",
            "BACKUP_IMPORT_REQUIRED",
        ));
    }
    let backup = state
        .backup_import_manager
        .get_import(request.backup_import_id.clone())
        .await
        .map_err(|error| {
            ApiError::new(
                ErrorType::Database,
                format!("读取已导入数据库失败: {error}"),
                error.code(),
            )
            .with_status(axum::http::StatusCode::BAD_REQUEST)
        })?;
    let source_database_path = state
        .backup_import_manager
        .decrypted_database_path(request.backup_import_id.clone())
        .await
        .map_err(|error| {
            ApiError::new(
                ErrorType::Database,
                format!("已解密数据库不可读: {error}"),
                error.code(),
            )
        })?;
    let imported = state
        .backup_import_manager
        .list_sessions_for_import(request.backup_import_id.clone())
        .await
        .map_err(|error| {
            ApiError::new(
                ErrorType::Database,
                format!("枚举历史会话失败: {error}"),
                error.code(),
            )
        })?;
    let inventory = build_inventory(state, imported).await?;

    let custom_output_dir = crate::paths::PathManager::sanitize_path(&request.output_dir);
    let requested_output_dir = if custom_output_dir.trim().is_empty() {
        state.path_manager.exports_dir()
    } else {
        PathBuf::from(&custom_output_dir)
    };
    let output_dir = if validate_output {
        prepare_output_directory(
            &requested_output_dir,
            &[
                state.path_manager.exports_dir(),
                state.path_manager.scheduled_exports_dir(),
            ],
        )
        .await?
    } else {
        requested_output_dir
    };
    Ok(PreparedAccountExport {
        backup,
        source_database_path,
        inventory,
        output_dir,
        custom_output_dir,
    })
}

async fn build_inventory(
    state: &SharedState,
    imported: Vec<ImportedSession>,
) -> Result<AccountInventory, ApiError> {
    let historical_session_count = imported.len();
    let (self_info, friends, groups) = tokio::join!(
        state.napcat.self_info(),
        state.napcat.get_friends(true),
        state.napcat.get_groups(true),
    );
    let self_info = self_info.map_err(|error| {
        ApiError::new(
            ErrorType::Auth,
            format!("当前 QQ 账号尚未登录或账号资料不可用: {error}"),
            "ACCOUNT_NOT_LOGGED_IN",
        )
    })?;
    let friend_values = list_payload(
        &friends.map_err(|error| {
            ApiError::new(
                ErrorType::Api,
                format!("获取当前好友列表失败: {error}"),
                "ACCOUNT_EXPORT_FRIENDS_FAILED",
            )
        })?,
        &["friends", "data"],
    )
    .ok_or_else(|| {
        ApiError::new(
            ErrorType::Api,
            "当前好友列表响应结构异常",
            "ACCOUNT_EXPORT_FRIENDS_FAILED",
        )
    })?;
    let group_values = list_payload(
        &groups.map_err(|error| {
            ApiError::new(
                ErrorType::Api,
                format!("获取当前群列表失败: {error}"),
                "ACCOUNT_EXPORT_GROUPS_FAILED",
            )
        })?,
        &["groups", "data"],
    )
    .ok_or_else(|| {
        ApiError::new(
            ErrorType::Api,
            "当前群列表响应结构异常",
            "ACCOUNT_EXPORT_GROUPS_FAILED",
        )
    })?;

    let current_friend_count = friend_values.len();
    let current_group_count = group_values.len();
    let account = AccountArchiveAccount {
        uid: string_field(&self_info, &["uid"]),
        uin: string_field(&self_info, &["uin", "qq"]),
        name: string_field(&self_info, &["nick", "name"]).unwrap_or_else(|| "QQ 账号".to_owned()),
        avatar_url: string_field(&self_info, &["uin", "qq"])
            .map(|uin| format!("https://q1.qlogo.cn/g?b=qq&nk={uin}&s=640")),
    };

    let friends = friend_values
        .into_iter()
        .filter_map(parse_friend)
        .collect::<Vec<_>>();
    let groups = group_values
        .into_iter()
        .filter_map(parse_group)
        .collect::<Vec<_>>();
    let friend_ids = friends
        .iter()
        .flat_map(|friend| [Some(friend.peer_uid.clone()), friend.peer_uin.clone()])
        .flatten()
        .collect::<HashSet<_>>();
    let group_ids = groups
        .iter()
        .map(|group| group.peer_uid.clone())
        .collect::<HashSet<_>>();

    let mut sessions = imported
        .into_iter()
        .map(|session| imported_session(session, &friend_ids, &group_ids))
        .collect::<Vec<_>>();
    for session in friends.into_iter().chain(groups) {
        merge_session(&mut sessions, session, true);
    }

    let contacts = load_local_contacts(state).await;
    let local_session_count = contacts.len();
    for contact in contacts {
        if let Some(session) = contact_session(&contact, &friend_ids, &group_ids) {
            merge_session(&mut sessions, session, false);
        }
    }
    sessions.sort_by(|left, right| {
        category_rank(&left.category)
            .cmp(&category_rank(&right.category))
            .then_with(|| left.name.cmp(&right.name))
    });
    Ok(AccountInventory {
        account,
        sessions,
        historical_session_count,
        current_friend_count,
        current_group_count,
        local_session_count,
        warnings: Vec::new(),
    })
}

#[derive(Debug)]
struct CurrentPeer {
    peer_uid: String,
    peer_uin: Option<String>,
    name: String,
    avatar_url: Option<String>,
    is_group: bool,
}

fn parse_friend(value: Value) -> Option<AccountSession> {
    let core = value.get("coreInfo").unwrap_or(&value);
    let uid = string_field(core, &["uid"]).or_else(|| string_field(&value, &["uid"]))?;
    let uin = string_field(core, &["uin"]).or_else(|| string_field(&value, &["uin"]));
    let name = string_field(core, &["remark", "nick", "nickName"])
        .or_else(|| string_field(&value, &["remark", "nick", "nickName"]))
        .or_else(|| uin.clone())
        .unwrap_or_else(|| uid.clone());
    Some(peer_session(CurrentPeer {
        peer_uid: uid,
        peer_uin: uin,
        name,
        avatar_url: None,
        is_group: false,
    }))
}

fn parse_group(value: Value) -> Option<AccountSession> {
    let code = string_field(&value, &["groupCode", "groupUin"])?;
    let name =
        string_field(&value, &["groupName", "name"]).unwrap_or_else(|| format!("群聊 {code}"));
    Some(peer_session(CurrentPeer {
        peer_uid: code.clone(),
        peer_uin: None,
        name,
        avatar_url: Some(format!("https://p.qlogo.cn/gh/{code}/{code}/640/")),
        is_group: true,
    }))
}

fn peer_session(peer: CurrentPeer) -> AccountSession {
    let chat_type = if peer.is_group { 2 } else { 1 };
    let identifier = peer.peer_uin.as_deref().unwrap_or(&peer.peer_uid);
    let avatar_url = peer.avatar_url.or_else(|| {
        (!peer.is_group).then(|| format!("https://q1.qlogo.cn/g?b=qq&nk={identifier}&s=640"))
    });
    AccountSession {
        conversation_id: conversation_id(chat_type, identifier),
        chat_type,
        peer_uid: peer.peer_uid.clone(),
        peer_uin: peer.peer_uin.clone(),
        name: peer.name,
        avatar_url,
        category: if peer.is_group {
            AccountConversationCategory::Group
        } else {
            AccountConversationCategory::Private
        },
        relationship_status: if peer.is_group {
            AccountRelationshipStatus::Group
        } else {
            AccountRelationshipStatus::Friend
        },
        source: "current".to_owned(),
        aliases: session_aliases(chat_type, &peer.peer_uid, peer.peer_uin.as_deref()),
        backup_peers: Vec::new(),
    }
}

fn imported_session(
    imported: ImportedSession,
    friend_ids: &HashSet<String>,
    group_ids: &HashSet<String>,
) -> AccountSession {
    let category = match imported.chat_type {
        1 => AccountConversationCategory::Private,
        2 => AccountConversationCategory::Group,
        _ => AccountConversationCategory::Other,
    };
    let relationship_status = match imported.chat_type {
        1 if friend_ids.contains(&imported.peer_uid)
            || imported
                .peer_uin
                .as_ref()
                .is_some_and(|value| friend_ids.contains(value)) =>
        {
            AccountRelationshipStatus::Friend
        }
        1 => AccountRelationshipStatus::NonFriend,
        2 if group_ids.contains(&imported.peer_uid)
            || imported
                .peer_uin
                .as_ref()
                .is_some_and(|value| group_ids.contains(value)) =>
        {
            AccountRelationshipStatus::Group
        }
        2 => AccountRelationshipStatus::UnavailableGroup,
        _ => AccountRelationshipStatus::Other,
    };
    let identifier = imported.peer_uin.as_deref().unwrap_or(&imported.peer_uid);
    AccountSession {
        conversation_id: conversation_id(imported.chat_type, identifier),
        chat_type: imported.chat_type,
        peer_uid: imported.peer_uid.clone(),
        peer_uin: imported.peer_uin.clone(),
        name: imported.name,
        avatar_url: Some(imported.avatar_url),
        category,
        relationship_status,
        source: "backup".to_owned(),
        aliases: session_aliases(
            imported.chat_type,
            &imported.peer_uid,
            imported.peer_uin.as_deref(),
        ),
        backup_peers: vec![imported.peer_uid],
    }
}

fn contact_session(
    contact: &Value,
    friend_ids: &HashSet<String>,
    group_ids: &HashSet<String>,
) -> Option<AccountSession> {
    let chat_type = int_field(contact, &["chatType"])?;
    let peer_uid = string_field(contact, &["peerUid"])?;
    let peer_uin = string_field(contact, &["peerUin"]);
    let name = string_field(
        contact,
        &["peerName", "remark", "sendMemberName", "sendNickName"],
    )
    .or_else(|| peer_uin.clone())
    .unwrap_or_else(|| peer_uid.clone());
    let (category, relationship_status, avatar_url) = match chat_type {
        1 => (
            AccountConversationCategory::Private,
            if friend_ids.contains(&peer_uid)
                || peer_uin
                    .as_ref()
                    .is_some_and(|value| friend_ids.contains(value))
            {
                AccountRelationshipStatus::Friend
            } else {
                AccountRelationshipStatus::NonFriend
            },
            Some(format!(
                "https://q1.qlogo.cn/g?b=qq&nk={}&s=640",
                peer_uin.as_deref().unwrap_or(&peer_uid)
            )),
        ),
        2 => (
            AccountConversationCategory::Group,
            if group_ids.contains(&peer_uid) {
                AccountRelationshipStatus::Group
            } else {
                AccountRelationshipStatus::UnavailableGroup
            },
            Some(format!("https://p.qlogo.cn/gh/{peer_uid}/{peer_uid}/640/")),
        ),
        _ => (
            AccountConversationCategory::Other,
            AccountRelationshipStatus::Other,
            None,
        ),
    };
    let identifier = peer_uin.as_deref().unwrap_or(&peer_uid);
    Some(AccountSession {
        conversation_id: conversation_id(chat_type, identifier),
        chat_type,
        peer_uid: peer_uid.clone(),
        peer_uin: peer_uin.clone(),
        name,
        avatar_url,
        category,
        relationship_status,
        source: "local".to_owned(),
        aliases: session_aliases(chat_type, &peer_uid, peer_uin.as_deref()),
        backup_peers: Vec::new(),
    })
}

fn merge_session(
    sessions: &mut Vec<AccountSession>,
    incoming: AccountSession,
    authoritative: bool,
) {
    let incoming_keys = incoming.alias_keys();
    let Some(existing) = sessions.iter_mut().find(|session| {
        session
            .alias_keys()
            .iter()
            .any(|key| incoming_keys.contains(key))
    }) else {
        sessions.push(incoming);
        return;
    };
    if authoritative {
        existing.name.clone_from(&incoming.name);
        existing.avatar_url.clone_from(&incoming.avatar_url);
        existing.relationship_status = incoming.relationship_status;
        existing.category = incoming.category;
        existing.peer_uid.clone_from(&incoming.peer_uid);
        existing.peer_uin.clone_from(&incoming.peer_uin);
    } else {
        if is_fallback_name(
            &existing.name,
            &existing.peer_uid,
            existing.peer_uin.as_deref(),
        ) {
            existing.name.clone_from(&incoming.name);
        }
        if existing.avatar_url.is_none() {
            existing.avatar_url.clone_from(&incoming.avatar_url);
        }
    }
    if existing.source != incoming.source {
        existing.source = "merged".to_owned();
    }
    for alias in incoming.aliases {
        if !existing.aliases.contains(&alias) {
            existing.aliases.push(alias);
        }
    }
    for peer in incoming.backup_peers {
        if !existing.backup_peers.contains(&peer) {
            existing.backup_peers.push(peer);
        }
    }
}

async fn load_local_contacts(state: &SharedState) -> Vec<Value> {
    let payload = match state.napcat.get_recent_contact_list_sync().await {
        Ok(full) => Some(full),
        Err(_) => state.napcat.get_recent_contact_list().await.ok(),
    };
    let snapshot = state
        .napcat
        .get_recent_contact_list_snapshot(2000)
        .await
        .ok();
    let mut contacts = Vec::new();
    let mut seen = HashSet::new();
    for payload in payload.into_iter().chain(snapshot) {
        for contact in extract_contacts(&payload) {
            let key = format!(
                "{}|{}",
                int_field(&contact, &["chatType"]).unwrap_or_default(),
                string_field(&contact, &["peerUid"]).unwrap_or_default()
            );
            if !key.ends_with('|') && seen.insert(key) {
                contacts.push(contact);
            }
        }
    }
    contacts
}

fn extract_contacts(payload: &Value) -> Vec<Value> {
    payload
        .pointer("/info/changedList")
        .and_then(Value::as_array)
        .or_else(|| payload.get("changedList").and_then(Value::as_array))
        .or_else(|| payload.as_array())
        .cloned()
        .unwrap_or_default()
}

async fn run_account_export(
    state: SharedState,
    task_id: String,
    request: AccountExportRequest,
    prepared: PreparedAccountExport,
    output_path: PathBuf,
    file_name: String,
    download_url: String,
) {
    let cancelled_before_registration = state.cancelled_task_ids.lock().await.contains(&task_id);
    let cancel_flag = Arc::new(AtomicBool::new(cancelled_before_registration));
    state
        .running_export_cancel_flags
        .lock()
        .await
        .insert(task_id.clone(), Arc::clone(&cancel_flag));
    let debug_directory = Arc::new(tokio::sync::Mutex::new(None));
    let permit = tokio::select! {
        permit = state.account_export_semaphore.acquire() => {
            permit.map_err(|_| "全账号导出队列已经关闭".to_owned())
        }
        () = wait_for_cancellation(&cancel_flag) => Err("任务已被用户停止".to_owned()),
    };
    let result = match permit {
        Ok(_permit) => {
            process_account_export(
                &state,
                &task_id,
                &request,
                prepared,
                &output_path,
                &file_name,
                &download_url,
                Arc::clone(&cancel_flag),
                Arc::clone(&debug_directory),
            )
            .await
        }
        Err(error) => Err(error),
    };
    release_export_path(&output_path);
    if let Err(error) = result {
        let cancelled = cancel_flag.load(Ordering::SeqCst)
            || state.cancelled_task_ids.lock().await.contains(&task_id);
        if let Some(directory) = debug_directory.lock().await.as_ref() {
            let status = if cancelled { "cancelled" } else { "failed" };
            if let Ok(payload) = serde_json::to_vec_pretty(&json!({
                "status": status,
                "error": error,
                "completedAt": now_iso(),
            })) {
                let _ = tokio::fs::write(directory.join("summary.json"), payload).await;
            }
        }
        if cancelled {
            update_task(
                &state,
                &task_id,
                json!({"status":"cancelled","message":"任务已停止","completedAt":now_iso()}),
            )
            .await;
        } else {
            update_task(
                &state,
                &task_id,
                json!({"status":"failed","error":error,"completedAt":now_iso()}),
            )
            .await;
            state.broadcast_ws(&json!({
                "type":"export_error",
                "data":{"taskId":task_id,"status":"failed","error":error}
            }));
        }
    }
    state
        .running_export_cancel_flags
        .lock()
        .await
        .remove(&task_id);
    state.cancelled_task_ids.lock().await.remove(&task_id);
}

#[allow(clippy::too_many_arguments)]
async fn process_account_export(
    state: &SharedState,
    task_id: &str,
    request: &AccountExportRequest,
    prepared: PreparedAccountExport,
    output_path: &Path,
    file_name: &str,
    download_url: &str,
    cancel_flag: Arc<AtomicBool>,
    debug_directory: Arc<tokio::sync::Mutex<Option<PathBuf>>>,
) -> Result<(), String> {
    let debug = if request.debug_export {
        Some(ExportDebugSession::start(&prepared.output_dir, file_name).await?)
    } else {
        None
    };
    if let Some(debug) = &debug {
        *debug_directory.lock().await = Some(debug.directory().to_path_buf());
        debug
            .trace()
            .record(json!({"type":"account_export_started","taskId":task_id}))
            .await;
    }
    let cancellation = CancellationToken::new();
    let monitor_flag = Arc::clone(&cancel_flag);
    let monitor_cancellation = cancellation.clone();
    let _cancellation_monitor = AbortTaskOnDrop(tokio::spawn(async move {
        wait_for_cancellation(&monitor_flag).await;
        monitor_cancellation.cancel();
    }));
    let options = AccountArchiveOptions {
        output_path: output_path.to_path_buf(),
        account: prepared.inventory.account.clone(),
        source_database_path: prepared.source_database_path.clone(),
        source_database_name: prepared.backup.file_name.clone(),
        source_database_format: prepared.backup.format.clone(),
        exporter_version: crate::version::VERSION.get().to_string(),
        cancellation: cancellation.clone(),
    };
    let mut builder = tokio::task::spawn_blocking(move || AccountArchiveBuilder::create(options))
        .await
        .map_err(|error| error.to_string())?
        .map_err(|error| error.to_string())?;
    let mut warnings = prepared.inventory.warnings.clone();
    let total_sessions = prepared.inventory.sessions.len().max(1);
    let mut total_messages = 0usize;
    let mut participant_uins = HashSet::new();

    for (index, session) in prepared.inventory.sessions.iter().enumerate() {
        ensure_not_cancelled(&cancel_flag, &cancellation)?;
        let progress = i64::try_from((index * 70) / total_sessions).unwrap_or(0);
        let message = format!(
            "正在归档会话 {}/{}：{}",
            index + 1,
            prepared.inventory.sessions.len(),
            session.name
        );
        update_task(
            state,
            task_id,
            json!({"progress":progress,"message":message,"conversationProgress":{"completed":index,"total":prepared.inventory.sessions.len()}}),
        )
        .await;
        broadcast_progress(state, task_id, progress, &message, total_messages);

        let mut raw_messages = Vec::new();
        for backup_peer in &session.backup_peers {
            match state
                .backup_import_manager
                .fetch_all_messages(
                    request.backup_import_id.clone(),
                    session.chat_type,
                    backup_peer.clone(),
                    None,
                    None,
                )
                .await
            {
                Ok(messages) => raw_messages.extend(messages),
                Err(error) => warnings.push(warning(
                    "conversation",
                    "BACKUP_MESSAGES_FAILED",
                    format!("读取 {} 的历史消息失败: {error}", session.name),
                    Some(&session.conversation_id),
                    None,
                )),
            }
        }
        match fetch_live_messages(state, session, &cancel_flag).await {
            Ok(live) => raw_messages = merge_raw_messages(raw_messages, live),
            Err(error) => warnings.push(warning(
                "conversation",
                "LIVE_MESSAGES_FAILED",
                format!("在线补齐 {} 失败，已保留备份消息: {error}", session.name),
                Some(&session.conversation_id),
                None,
            )),
        }
        raw_messages.sort_by_key(raw_message_time_ms);
        if session.chat_type == 2 && !raw_messages.is_empty() {
            fill_group_member_names(state, &session.peer_uid, &mut raw_messages).await;
        }
        if let Some(debug) = &debug {
            let directory = debug
                .directory()
                .join("conversations")
                .join(format!("{:05}", index + 1));
            tokio::fs::create_dir_all(&directory)
                .await
                .map_err(|error| error.to_string())?;
            debug
                .write_jsonl(
                    &format!("conversations/{:05}/01-raw-messages.jsonl", index + 1),
                    &raw_messages,
                )
                .await?;
        }

        let mut parser = SimpleMessageParser::new(SimpleParserOptions {
            html_enabled: true,
            prefer_group_member_name: true,
            sender_title_resolver: None,
            forward_fetcher: Some(Arc::new(state.napcat.clone()) as Arc<dyn ForwardFetcher>),
        });
        let mut clean_messages = parser.parse_messages(&raw_messages).await;
        let self_uid = prepared.inventory.account.uid.as_deref();
        let self_uin = prepared.inventory.account.uin.as_deref();
        let self_name = Some(prepared.inventory.account.name.as_str());
        backfill_self_sender_names(&mut clean_messages, self_uid, self_uin, self_name);
        for message in &clean_messages {
            if let Some(uin) = message
                .sender
                .uin
                .as_deref()
                .filter(|value| !value.is_empty())
            {
                participant_uins.insert(uin.to_owned());
            }
        }

        let mut resource_messages = raw_messages.clone();
        resource_messages.extend(parser.take_forward_raw_messages());
        let (resource_map, summary) = state
            .resource_handler
            .process_message_resources_with_batch_config(
                &resource_messages,
                Arc::clone(&cancel_flag),
                debug.as_ref().map(ExportDebugSession::trace),
                None,
                Vec::new(),
            )
            .await;
        if summary.failed > 0 {
            warnings.push(warning(
                "conversation",
                "MESSAGE_RESOURCE_FAILED",
                format!("{} 有 {} 个消息资源未能下载", session.name, summary.failed),
                Some(&session.conversation_id),
                None,
            ));
        }
        let value_resource_map = resource_map
            .iter()
            .map(|(message_id, resources)| {
                (
                    message_id.clone(),
                    resources
                        .iter()
                        .filter_map(|resource| serde_json::to_value(resource).ok())
                        .collect::<Vec<_>>(),
                )
            })
            .collect::<HashMap<_, _>>();
        for message in &mut clean_messages {
            SimpleMessageParser::update_message_resource_paths_recursive(
                message,
                &value_resource_map,
            );
        }
        SimpleMessageParser::backfill_reply_preview_local_paths(&mut clean_messages);
        if let Some(debug) = &debug {
            debug
                .write_jsonl(
                    &format!("conversations/{:05}/02-final-messages.jsonl", index + 1),
                    &clean_messages,
                )
                .await?;
        }
        total_messages += clean_messages.len();
        let conversation = account_conversation(
            session,
            &prepared.inventory.account,
            clean_messages,
            to_exporter_resource_map(&resource_map),
        );
        builder = builder_add_conversation(builder, conversation).await?;
    }

    ensure_not_cancelled(&cancel_flag, &cancellation)?;
    update_task(
        state,
        task_id,
        json!({"progress":72,"message":"正在收集头像、表情包、群文件和群相册...","messageCount":total_messages}),
    )
    .await;
    let resource_temp = prepared
        .output_dir
        .join(format!(".qce-account-resources-{}", uuid::Uuid::new_v4()));
    tokio::fs::create_dir(&resource_temp)
        .await
        .map_err(|error| error.to_string())?;
    let _resource_temp_cleanup = TemporaryDirectoryCleanup(resource_temp.clone());
    let resources = collect_account_resources(
        state,
        &prepared.inventory,
        &participant_uins,
        &resource_temp,
        &cancel_flag,
        &mut warnings,
    )
    .await;
    let resource_total = resources.len();
    builder = builder_add_resources(builder, resources).await?;
    builder = builder_add_warnings(builder, warnings.clone()).await?;
    ensure_not_cancelled(&cancel_flag, &cancellation)?;
    update_task(
        state,
        task_id,
        json!({"progress":94,"message":"正在校验 SQLite 并封装 ZIP64 归档...","resourceCount":resource_total}),
    )
    .await;
    let outcome = tokio::task::spawn_blocking(move || builder.finish())
        .await
        .map_err(|error| error.to_string())?
        .map_err(|error| error.to_string());
    let _ = tokio::fs::remove_dir_all(&resource_temp).await;
    let outcome = outcome?;
    if let Some(debug) = debug {
        debug
            .write_json(
                "summary.json",
                &json!({"status":"completed","outcome":outcome,"warnings":warnings}),
            )
            .await?;
        let _ = debug.finish().await?;
    }
    finish_task_success(state, task_id, &outcome, file_name, download_url).await;
    Ok(())
}

fn account_conversation(
    session: &AccountSession,
    account: &AccountArchiveAccount,
    messages: Vec<CleanMessage>,
    resource_map: HashMap<String, Vec<qce_exporter::types::MessageResource>>,
) -> AccountArchiveConversation {
    AccountArchiveConversation {
        conversation_id: session.conversation_id.clone(),
        chat_type: session.chat_type,
        category: session.category.clone(),
        relationship_status: session.relationship_status.clone(),
        source: session.source.clone(),
        chat_info: ChatInfo {
            name: session.name.clone(),
            chat_type: match session.category {
                AccountConversationCategory::Private => "private",
                AccountConversationCategory::Group => "group",
                AccountConversationCategory::Other => "other",
            }
            .to_owned(),
            avatar: session.avatar_url.clone(),
            participant_count: None,
            self_uid: account.uid.clone(),
            self_uin: account.uin.clone(),
            self_name: Some(account.name.clone()),
            peer_uid: Some(session.peer_uid.clone()),
            peer_uin: session.peer_uin.clone(),
        },
        aliases: session.aliases.clone(),
        messages,
        resource_map,
    }
}

async fn fetch_live_messages(
    state: &SharedState,
    session: &AccountSession,
    cancel_flag: &Arc<AtomicBool>,
) -> Result<Vec<Value>, String> {
    let fetcher = BatchMessageFetcher::new(
        Arc::new(state.napcat.clone()),
        BatchFetchConfig {
            batch_size: 5000,
            timeout_ms: 120_000,
            retry_count: 3,
            ..BatchFetchConfig::default()
        },
    );
    let peer = Peer {
        chat_type: session.chat_type,
        peer_uid: session.peer_uid.clone(),
        guild_id: None,
    };
    let filter = MessageFilter::default();
    let mut previous = None;
    let mut messages = Vec::new();
    loop {
        if cancel_flag.load(Ordering::SeqCst) {
            fetcher.cancel();
            return Err("任务已被用户停止".to_owned());
        }
        match fetcher
            .fetch_next_batch(&peer, &filter, previous.as_ref())
            .await
            .map_err(|error| error.to_string())?
        {
            Some(mut batch) => {
                messages.append(&mut batch.messages);
                previous = Some(batch);
            }
            None => break,
        }
    }
    Ok(messages)
}

fn merge_raw_messages(backup: Vec<Value>, live: Vec<Value>) -> Vec<Value> {
    let mut messages = backup;
    let mut positions = HashMap::new();
    for (index, message) in messages.iter().enumerate() {
        positions.insert(raw_message_key(message), index);
    }
    for message in live {
        let key = raw_message_key(&message);
        if let Some(index) = positions.get(&key).copied() {
            messages[index] = message;
        } else {
            positions.insert(key, messages.len());
            messages.push(message);
        }
    }
    messages
}

fn raw_message_key(message: &Value) -> String {
    if let Some(id) = string_field(message, &["msgId"]).filter(|id| id != "0") {
        return format!("id:{id}");
    }
    format!(
        "composite:{}|{}|{}|{}",
        string_field(message, &["msgSeq", "clientSeq"]).unwrap_or_default(),
        raw_message_time_ms(message),
        string_field(message, &["senderUid", "senderUin"]).unwrap_or_default(),
        string_field(message, &["msgType"]).unwrap_or_default(),
    )
}

fn raw_message_time_ms(message: &Value) -> i64 {
    let value = int_field(message, &["msgTime", "timestamp"]).unwrap_or_default();
    if value.abs() < 100_000_000_000 {
        value.saturating_mul(1000)
    } else {
        value
    }
}

async fn collect_account_resources(
    state: &SharedState,
    inventory: &AccountInventory,
    participant_uins: &HashSet<String>,
    temp: &Path,
    cancel_flag: &Arc<AtomicBool>,
    warnings: &mut Vec<AccountArchiveWarning>,
) -> Vec<AccountArchiveExtraResource> {
    let mut resources = Vec::new();
    let mut avatar_jobs = Vec::new();
    if let (Some(url), Some(uin)) = (
        inventory.account.avatar_url.clone(),
        inventory.account.uin.clone(),
    ) {
        avatar_jobs.push((
            "account_avatar",
            None,
            uin.clone(),
            inventory.account.name.clone(),
            url,
        ));
    }
    for session in &inventory.sessions {
        if let Some(url) = session.avatar_url.clone() {
            avatar_jobs.push((
                "conversation_avatar",
                Some(session.conversation_id.clone()),
                session
                    .peer_uin
                    .clone()
                    .unwrap_or_else(|| session.peer_uid.clone()),
                session.name.clone(),
                url,
            ));
        }
    }
    for uin in participant_uins {
        avatar_jobs.push((
            "participant_avatar",
            None,
            uin.clone(),
            uin.clone(),
            format!("https://q1.qlogo.cn/g?b=qq&nk={uin}&s=640"),
        ));
    }
    let mut seen_avatars = HashSet::new();
    for (kind, conversation_id, logical_id, display_name, url) in avatar_jobs {
        if cancel_flag.load(Ordering::SeqCst) {
            break;
        }
        if !seen_avatars.insert(format!("{kind}|{logical_id}")) {
            continue;
        }
        let destination = temp.join(format!("{}.avatar", uuid::Uuid::new_v4()));
        let copied = download_with_cancellation(
            http_download_to_file(&url, &destination, 32 * 1024 * 1024),
            cancel_flag,
        )
        .await;
        if cancel_flag.load(Ordering::SeqCst) {
            break;
        }
        if !copied {
            warnings.push(warning(
                "resource",
                "AVATAR_DOWNLOAD_FAILED",
                format!("头像下载失败: {display_name}"),
                conversation_id.as_deref(),
                Some(&logical_id),
            ));
        }
        resources.push(AccountArchiveExtraResource {
            kind: kind.to_owned(),
            conversation_id,
            collection_id: None,
            logical_id,
            parent_id: None,
            display_name,
            expects_file: true,
            local_path: copied.then_some(destination),
            source_url: Some(url),
            metadata: Value::Null,
        });
    }

    let packs = get_sticker_packs(state, None).await;
    for pack in packs {
        let pack_id =
            string_field(&pack, &["packId"]).unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let pack_name = string_field(&pack, &["packName"]).unwrap_or_else(|| pack_id.clone());
        resources.push(metadata_resource(
            "sticker_pack",
            None,
            &pack_id,
            &pack_name,
            pack.clone(),
        ));
        for sticker in pack
            .get("stickers")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
        {
            if cancel_flag.load(Ordering::SeqCst) {
                break;
            }
            let sticker_id = string_field(&sticker, &["stickerId", "id", "qSid"])
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
            let display_name =
                string_field(&sticker, &["name", "desc"]).unwrap_or_else(|| sticker_id.clone());
            let url = string_field(&sticker, &["url", "originalUrl", "thumbUrl"]);
            let destination = temp.join(format!("{}.sticker", uuid::Uuid::new_v4()));
            let copied = if let Some(url) = url.as_deref() {
                download_with_cancellation(
                    http_download_to_file(url, &destination, 64 * 1024 * 1024),
                    cancel_flag,
                )
                .await
            } else {
                false
            };
            resources.push(AccountArchiveExtraResource {
                kind: "sticker".to_owned(),
                conversation_id: None,
                collection_id: Some(pack_id.clone()),
                logical_id: sticker_id,
                parent_id: None,
                display_name,
                expects_file: url.is_some(),
                local_path: copied.then_some(destination),
                source_url: url,
                metadata: sticker,
            });
        }
    }

    for session in inventory
        .sessions
        .iter()
        .filter(|session| session.chat_type == 2)
    {
        if cancel_flag.load(Ordering::SeqCst) {
            break;
        }
        let (files, folders) = fetch_all_files_recursive(state, &session.peer_uid).await;
        if files.is_empty()
            && folders.is_empty()
            && matches!(
                session.relationship_status,
                AccountRelationshipStatus::UnavailableGroup
            )
        {
            warnings.push(warning(
                "group_space",
                "GROUP_FILES_UNAVAILABLE",
                format!("无法枚举已退出/不可用群 {} 的群文件", session.name),
                Some(&session.conversation_id),
                None,
            ));
        }
        for folder in folders {
            let id = string_field(&folder, &["folderId"])
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
            let name = string_field(&folder, &["folderName"]).unwrap_or_else(|| id.clone());
            resources.push(AccountArchiveExtraResource {
                kind: "group_file_folder".to_owned(),
                conversation_id: Some(session.conversation_id.clone()),
                collection_id: Some(session.peer_uid.clone()),
                logical_id: id,
                parent_id: string_field(&folder, &["parentFolderId"]),
                display_name: name,
                expects_file: false,
                local_path: None,
                source_url: None,
                metadata: folder,
            });
        }
        for file in files {
            if cancel_flag.load(Ordering::SeqCst) {
                break;
            }
            let id = string_field(&file, &["fileId", "fileUuid"])
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
            let name = string_field(&file, &["fileName"]).unwrap_or_else(|| id.clone());
            let destination = temp.join(format!("{}.group-file", uuid::Uuid::new_v4()));
            let copied = download_with_cancellation(
                download_file_to(state, &session.peer_uid, &id, &destination),
                cancel_flag,
            )
            .await;
            if cancel_flag.load(Ordering::SeqCst) {
                break;
            }
            if !copied {
                warnings.push(warning(
                    "group_file",
                    "GROUP_FILE_DOWNLOAD_FAILED",
                    format!("群文件下载失败: {} / {name}", session.name),
                    Some(&session.conversation_id),
                    Some(&id),
                ));
            }
            resources.push(AccountArchiveExtraResource {
                kind: "group_file".to_owned(),
                conversation_id: Some(session.conversation_id.clone()),
                collection_id: Some(session.peer_uid.clone()),
                logical_id: id,
                parent_id: string_field(&file, &["parentFolderId"]),
                display_name: name,
                expects_file: true,
                local_path: copied.then_some(destination),
                source_url: None,
                metadata: file,
            });
        }
        for album in fetch_album_list(state, &session.peer_uid).await {
            let album_id = string_field(&album, &["albumId"])
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
            let album_name =
                string_field(&album, &["albumName"]).unwrap_or_else(|| album_id.clone());
            resources.push(metadata_resource(
                "group_album",
                Some(session.conversation_id.clone()),
                &album_id,
                &album_name,
                album.clone(),
            ));
            for media in fetch_album_media(state, &session.peer_uid, &album_id).await {
                if cancel_flag.load(Ordering::SeqCst) {
                    break;
                }
                let id = string_field(&media, &["id"])
                    .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
                let url = string_field(&media, &["url", "thumbUrl"]);
                let destination = temp.join(format!("{}.album", uuid::Uuid::new_v4()));
                let copied = if let Some(url) = url.as_deref() {
                    download_with_cancellation(
                        http_download_to_file(url, &destination, 2 * 1024 * 1024 * 1024),
                        cancel_flag,
                    )
                    .await
                } else {
                    false
                };
                if !copied {
                    warnings.push(warning(
                        "group_album",
                        "GROUP_ALBUM_MEDIA_FAILED",
                        format!("群相册资源下载失败: {} / {album_name}", session.name),
                        Some(&session.conversation_id),
                        Some(&id),
                    ));
                }
                resources.push(AccountArchiveExtraResource {
                    kind: "group_album_media".to_owned(),
                    conversation_id: Some(session.conversation_id.clone()),
                    collection_id: Some(album_id.clone()),
                    logical_id: id.clone(),
                    parent_id: None,
                    display_name: id,
                    expects_file: true,
                    local_path: copied.then_some(destination),
                    source_url: url,
                    metadata: media,
                });
            }
        }
    }
    resources
}

fn metadata_resource(
    kind: &str,
    conversation_id: Option<String>,
    logical_id: &str,
    display_name: &str,
    metadata: Value,
) -> AccountArchiveExtraResource {
    AccountArchiveExtraResource {
        kind: kind.to_owned(),
        conversation_id,
        collection_id: None,
        logical_id: logical_id.to_owned(),
        parent_id: None,
        display_name: display_name.to_owned(),
        expects_file: false,
        local_path: None,
        source_url: None,
        metadata,
    }
}

async fn builder_add_conversation(
    mut builder: AccountArchiveBuilder,
    conversation: AccountArchiveConversation,
) -> Result<AccountArchiveBuilder, String> {
    tokio::task::spawn_blocking(move || {
        builder
            .add_conversation(conversation)
            .map_err(|error| error.to_string())?;
        Ok(builder)
    })
    .await
    .map_err(|error| error.to_string())?
}

async fn builder_add_resources(
    mut builder: AccountArchiveBuilder,
    resources: Vec<AccountArchiveExtraResource>,
) -> Result<AccountArchiveBuilder, String> {
    tokio::task::spawn_blocking(move || {
        builder
            .add_extra_resources(resources)
            .map_err(|error| error.to_string())?;
        Ok(builder)
    })
    .await
    .map_err(|error| error.to_string())?
}

async fn builder_add_warnings(
    mut builder: AccountArchiveBuilder,
    warnings: Vec<AccountArchiveWarning>,
) -> Result<AccountArchiveBuilder, String> {
    tokio::task::spawn_blocking(move || {
        builder
            .add_warnings(warnings)
            .map_err(|error| error.to_string())?;
        Ok(builder)
    })
    .await
    .map_err(|error| error.to_string())?
}

async fn finish_task_success(
    state: &SharedState,
    task_id: &str,
    outcome: &AccountArchiveOutcome,
    file_name: &str,
    download_url: &str,
) {
    let status = if outcome.warning_count > 0 || outcome.missing_resource_count > 0 {
        "completed_with_warnings"
    } else {
        "completed"
    };
    let patch = json!({
        "status": status,
        "progress": 100,
        "message": if status == "completed" { "全账号归档完成" } else { "全账号归档完成，但有资源缺失" },
        "fileName": file_name,
        "downloadUrl": download_url,
        "filePath": outcome.file_path.to_string_lossy(),
        "fileSize": outcome.file_size,
        "archiveId": outcome.archive_id,
        "conversationCount": outcome.conversation_count,
        "messageCount": outcome.message_count,
        "resourceCount": outcome.resource_count,
        "missingResourceCount": outcome.missing_resource_count,
        "warningCount": outcome.warning_count,
        "completedAt": now_iso(),
    });
    update_task(state, task_id, patch.clone()).await;
    state.broadcast_ws(&json!({
        "type":"export_complete",
        "data":{"taskId":task_id,"format":"QCEARCHIVE_ACCOUNT","status":status,"result":patch}
    }));
}

fn ensure_not_cancelled(flag: &AtomicBool, cancellation: &CancellationToken) -> Result<(), String> {
    if flag.load(Ordering::SeqCst) {
        cancellation.cancel();
        Err("任务已被用户停止".to_owned())
    } else {
        Ok(())
    }
}

async fn wait_for_cancellation(flag: &AtomicBool) {
    while !flag.load(Ordering::SeqCst) {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

async fn download_with_cancellation<F>(download: F, cancel_flag: &AtomicBool) -> bool
where
    F: Future<Output = bool>,
{
    tokio::select! {
        copied = download => copied,
        () = wait_for_cancellation(cancel_flag) => false,
    }
}

struct TemporaryDirectoryCleanup(PathBuf);

impl Drop for TemporaryDirectoryCleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct AbortTaskOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortTaskOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn warning(
    scope: &str,
    code: &str,
    message: String,
    conversation_id: Option<&str>,
    resource_id: Option<&str>,
) -> AccountArchiveWarning {
    AccountArchiveWarning {
        scope: scope.to_owned(),
        code: code.to_owned(),
        message,
        conversation_id: conversation_id.map(str::to_owned),
        resource_id: resource_id.map(str::to_owned),
    }
}

fn conversation_id(chat_type: i64, identifier: &str) -> String {
    format!("{chat_type}:{}", identifier.trim())
}

fn session_aliases(chat_type: i64, uid: &str, uin: Option<&str>) -> Vec<(String, String)> {
    let mut aliases = vec![("uid".to_owned(), uid.to_owned())];
    if let Some(uin) = uin.filter(|value| !value.is_empty() && *value != uid) {
        aliases.push(("uin".to_owned(), uin.to_owned()));
    }
    aliases.push(("chat_type".to_owned(), chat_type.to_string()));
    aliases
}

fn category_rank(category: &AccountConversationCategory) -> u8 {
    match category {
        AccountConversationCategory::Private => 0,
        AccountConversationCategory::Group => 1,
        AccountConversationCategory::Other => 2,
    }
}

fn is_fallback_name(name: &str, uid: &str, uin: Option<&str>) -> bool {
    name == uid
        || uin.is_some_and(|value| name == value)
        || name.starts_with("QQ ")
        || name.starts_with("群聊 ")
}

fn safe_file_component(value: &str) -> String {
    let value = value
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
        .collect::<String>();
    if value.trim().is_empty() {
        "unknown".to_owned()
    } else {
        value
    }
}

fn string_field(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| match value.get(*key) {
        Some(Value::String(value)) if !value.trim().is_empty() => Some(value.trim().to_owned()),
        Some(Value::Number(value)) => Some(value.to_string()),
        _ => None,
    })
}

fn int_field(value: &Value, keys: &[&str]) -> Option<i64> {
    keys.iter().find_map(|key| match value.get(*key) {
        Some(Value::Number(value)) => value.as_i64(),
        Some(Value::String(value)) => value.parse().ok(),
        _ => None,
    })
}

fn list_payload(value: &Value, wrappers: &[&str]) -> Option<Vec<Value>> {
    value.as_array().cloned().or_else(|| {
        wrappers
            .iter()
            .find_map(|key| value.get(*key).and_then(Value::as_array).cloned())
    })
}

#[cfg(test)]
mod tests {
    use super::{merge_raw_messages, merge_session, AccountSession};
    use qce_exporter::account_archive_exporter::{
        AccountConversationCategory, AccountRelationshipStatus,
    };
    use serde_json::json;

    fn session(uid: &str, uin: Option<&str>, name: &str, source: &str) -> AccountSession {
        AccountSession {
            conversation_id: format!("1:{}", uin.unwrap_or(uid)),
            chat_type: 1,
            peer_uid: uid.to_owned(),
            peer_uin: uin.map(str::to_owned),
            name: name.to_owned(),
            avatar_url: None,
            category: AccountConversationCategory::Private,
            relationship_status: AccountRelationshipStatus::NonFriend,
            source: source.to_owned(),
            aliases: vec![("uid".to_owned(), uid.to_owned())],
            backup_peers: vec![uid.to_owned()],
        }
    }

    #[test]
    fn merges_private_session_by_uid_or_uin_and_keeps_backup_peer() {
        let mut sessions = vec![session("u_1", Some("123456"), "QQ 123456", "backup")];
        let mut current = session("u_1", Some("123456"), "真实昵称", "current");
        current.relationship_status = AccountRelationshipStatus::Friend;
        current.backup_peers.clear();
        merge_session(&mut sessions, current, true);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].name, "真实昵称");
        assert!(matches!(
            sessions[0].relationship_status,
            AccountRelationshipStatus::Friend
        ));
        assert_eq!(sessions[0].backup_peers, ["u_1"]);
    }

    #[test]
    fn live_payload_replaces_backup_payload_with_same_message_id() {
        let backup = vec![json!({"msgId":"1","msgTime":1,"elements":["old"]})];
        let live = vec![json!({"msgId":"1","msgTime":1,"elements":["new"]})];
        let merged = merge_raw_messages(backup, live);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0]["elements"][0], "new");
    }
}
