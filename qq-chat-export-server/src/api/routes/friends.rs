use std::collections::{HashMap, HashSet};
use std::time::Duration;

use axum::extract::{Extension, Path, Query, State};
use axum::response::Response;
use futures_util::future::join_all;
use serde_json::{json, Value};

use crate::api::response::{self, ApiError, ErrorType, RequestId};
use crate::api::state::SharedState;
use crate::backup_import::ImportedSession;

/// 分页参数解析（page 默认 1、limit 默认 999）。
fn page_and_limit(params: &HashMap<String, String>) -> (usize, usize) {
    let page = params
        .get("page")
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|p| *p >= 1)
        .unwrap_or(1)
        .min(1_000_000);
    let limit = params
        .get("limit")
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|l| *l >= 1)
        .unwrap_or(999)
        .min(2_000);
    (page, limit)
}

/// 宽松取字符串（字符串或数字字段）。
fn str_of(value: &Value, key: &str) -> String {
    match value.get(key) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

fn int_of(value: &Value, key: &str) -> Option<i64> {
    match value.get(key) {
        Some(Value::Number(number)) => number.as_i64(),
        Some(Value::String(number)) => number.parse().ok(),
        _ => None,
    }
}

fn recent_contact_limit(params: &HashMap<String, String>) -> i64 {
    params
        .get("limit")
        .and_then(|value| value.parse::<i64>().ok())
        .filter(|limit| *limit >= 1)
        .map_or(100, |limit| limit.min(2_000))
}

fn inactive_session_limit(params: &HashMap<String, String>) -> i64 {
    params
        .get("limit")
        .and_then(|value| value.parse::<i64>().ok())
        .filter(|limit| *limit >= 1)
        .map_or(2_000, |limit| limit.min(2_000))
}

struct LoadedRecentContacts {
    contacts: Vec<Value>,
    source: &'static str,
}

fn extract_recent_contacts(payload: &Value) -> Vec<Value> {
    payload
        .get("info")
        .and_then(|info| info.get("changedList"))
        .and_then(Value::as_array)
        .or_else(|| payload.get("changedList").and_then(Value::as_array))
        .or_else(|| payload.as_array())
        .cloned()
        .unwrap_or_default()
}

fn recent_contact_key(contact: &Value) -> Option<String> {
    let chat_type = int_of(contact, "chatType")?;
    let peer_uid = str_of(contact, "peerUid");
    (!peer_uid.is_empty()).then(|| format!("{chat_type}|{peer_uid}"))
}

fn merge_recent_contacts(lists: impl IntoIterator<Item = Vec<Value>>) -> Vec<Value> {
    let mut seen = HashSet::new();
    let mut merged = Vec::new();
    for list in lists {
        for contact in list {
            let Some(key) = recent_contact_key(&contact) else {
                continue;
            };
            if seen.insert(key) {
                merged.push(contact);
            }
        }
    }
    merged
}

async fn load_recent_contacts(
    state: &SharedState,
    limit: i64,
    include_all: bool,
) -> Result<LoadedRecentContacts, ApiError> {
    let snapshot = state
        .napcat
        .get_recent_contact_list_snapshot(limit)
        .await
        .map_err(|error| {
            ApiError::new(ErrorType::Api, error.to_string(), "RECENT_CONTACTS_FAILED")
        })?;
    if snapshot
        .get("info")
        .and_then(|info| info.get("errCode"))
        .and_then(Value::as_i64)
        .is_some_and(|code| code != 0)
    {
        let message = snapshot
            .get("info")
            .and_then(|info| info.get("errMsg"))
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        return Err(ApiError::new(
            ErrorType::Api,
            format!("获取最近联系人失败: {message}"),
            "RECENT_CONTACTS_FAILED",
        ));
    }

    let mut lists = vec![extract_recent_contacts(&snapshot)];
    let mut source = "snapshot";
    if include_all {
        let full_list = match tokio::time::timeout(
            Duration::from_secs(5),
            state.napcat.get_recent_contact_list_sync(),
        )
        .await
        {
            Ok(Ok(value)) => Some(value),
            Ok(Err(error)) => {
                tracing::debug!("getRecentContactListSync unavailable: {error}");
                None
            }
            Err(_) => {
                tracing::debug!("getRecentContactListSync timed out");
                None
            }
        };
        let full_list = if full_list.is_some() {
            full_list
        } else {
            match tokio::time::timeout(
                Duration::from_secs(5),
                state.napcat.get_recent_contact_list(),
            )
            .await
            {
                Ok(Ok(value)) => Some(value),
                Ok(Err(error)) => {
                    tracing::debug!("getRecentContactList unavailable: {error}");
                    None
                }
                Err(_) => {
                    tracing::debug!("getRecentContactList timed out");
                    None
                }
            }
        };
        if let Some(value) = full_list {
            lists.push(extract_recent_contacts(&value));
            source = "full";
        }
    }

    Ok(LoadedRecentContacts {
        contacts: merge_recent_contacts(lists),
        source,
    })
}

fn list_payload<'a>(value: &'a Value, wrapper_keys: &[&str]) -> Option<&'a [Value]> {
    value.as_array().map(Vec::as_slice).or_else(|| {
        wrapper_keys
            .iter()
            .find_map(|key| value.get(*key).and_then(Value::as_array).map(Vec::as_slice))
    })
}

fn friend_identifiers(friends: &Value) -> Option<HashSet<String>> {
    let mut identifiers = HashSet::new();
    for friend in list_payload(friends, &["friends", "data"])? {
        let core = friend.get("coreInfo").unwrap_or(friend);
        for key in ["uid", "uin"] {
            let identifier = {
                let nested = str_of(core, key);
                if nested.is_empty() {
                    str_of(friend, key)
                } else {
                    nested
                }
            };
            if !identifier.is_empty() {
                identifiers.insert(identifier);
            }
        }
    }
    Some(identifiers)
}

fn group_codes(groups: &Value) -> Option<HashSet<String>> {
    Some(
        list_payload(groups, &["groups", "data"])?
            .iter()
            .map(|group| str_of(group, "groupCode"))
            .filter(|code| !code.is_empty())
            .collect(),
    )
}

fn inactive_membership_sets(
    friends: &Value,
    groups: &Value,
) -> Result<(HashSet<String>, HashSet<String>), &'static str> {
    let friend_ids = friend_identifiers(friends).ok_or("好友")?;
    let active_group_codes = group_codes(groups).ok_or("群")?;
    Ok((friend_ids, active_group_codes))
}

fn contact_classification(
    chat_type: i64,
    peer_uid: &str,
    friend_uids: &HashSet<String>,
    include_all: bool,
) -> &'static str {
    if chat_type == 2 {
        "group"
    } else if chat_type == 1 && friend_uids.contains(peer_uid) {
        "friend"
    } else if chat_type == 1 && !include_all {
        "private"
    } else {
        "special"
    }
}

/// 数据线 / 设备会话（手机 QQ 的「我的电脑 / 我的手机 / 我的设备」）。
/// NapCat 的 KCHATTYPEDATALINE(8) / KCHATTYPEDATALINEMQQ(134)。（Issue #609）
fn is_device_chat_type(chat_type: i64) -> bool {
    chat_type == 8 || chat_type == 134
}

fn contact_name(contact: &Value, chat_type: i64, peer_uid: &str) -> String {
    for key in ["peerName", "sendNickName", "sendMemberName", "remark"] {
        let value = str_of(contact, key);
        if !value.is_empty() {
            return value;
        }
    }
    // 设备会话往往没有 peerName，用友好兜底而不是暴露原始 peerUid。
    if is_device_chat_type(chat_type) {
        return "我的设备".to_string();
    }
    peer_uid.to_string()
}

fn contact_time_iso(contact: &Value) -> Option<String> {
    let raw = contact.get("msgTime")?;
    if let Some(value) = raw.as_str() {
        if chrono::DateTime::parse_from_rfc3339(value).is_ok() {
            return Some(value.to_string());
        }
    }
    let timestamp = match raw {
        Value::Number(number) => number.as_i64()?,
        Value::String(number) => number.parse().ok()?,
        _ => return None,
    };
    let millis = if timestamp.abs() < 100_000_000_000 {
        timestamp.checked_mul(1_000)?
    } else {
        timestamp
    };
    chrono::DateTime::from_timestamp_millis(millis)
        .map(|time| time.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
}

fn map_recent_contact(
    contact: &Value,
    friend_uids: &HashSet<String>,
    include_all: bool,
) -> Option<Value> {
    let chat_type = int_of(contact, "chatType")?;
    let peer_uid = str_of(contact, "peerUid");
    if peer_uid.is_empty() {
        return None;
    }
    let peer_uin = str_of(contact, "peerUin");
    let peer_name = str_of(contact, "peerName");
    let remark = str_of(contact, "remark");
    let send_nick_name = str_of(contact, "sendNickName");
    let send_member_name = str_of(contact, "sendMemberName");
    let avatar_key = if peer_uin.is_empty() {
        &peer_uid
    } else {
        &peer_uin
    };
    Some(json!({
        "chatType": chat_type,
        "peerUid": peer_uid,
        "peerUin": peer_uin,
        "peerName": peer_name,
        "remark": remark,
        "msgTime": contact.get("msgTime").cloned().unwrap_or(Value::Null),
        "sendNickName": send_nick_name,
        "sendMemberName": send_member_name,
        "name": contact_name(contact, chat_type, &peer_uid),
        "avatarUrl": format!("https://q1.qlogo.cn/g?b=qq&nk={avatar_key}&s=640"),
        "lastMsgId": str_of(contact, "msgId"),
        "lastMsgTime": contact_time_iso(contact),
        "classification": contact_classification(
            chat_type,
            &peer_uid,
            friend_uids,
            include_all,
        ),
    }))
}

fn inactive_session_kind(
    contact: &Value,
    friend_ids: &HashSet<String>,
    active_group_codes: &HashSet<String>,
) -> Option<&'static str> {
    let chat_type = int_of(contact, "chatType")?;
    let peer_uid = str_of(contact, "peerUid");
    if peer_uid.is_empty() {
        return None;
    }
    match chat_type {
        1 => {
            let peer_uin = str_of(contact, "peerUin");
            (!friend_ids.contains(&peer_uid)
                && (peer_uin.is_empty() || !friend_ids.contains(&peer_uin)))
            .then_some("non_friend")
        }
        2 => (!active_group_codes.contains(&peer_uid)).then_some("unavailable_group"),
        _ => None,
    }
}

fn build_inactive_sessions(
    contacts: &[Value],
    friend_ids: &HashSet<String>,
    active_group_codes: &HashSet<String>,
) -> Vec<Value> {
    contacts
        .iter()
        .filter_map(|contact| {
            let kind = inactive_session_kind(contact, friend_ids, active_group_codes)?;
            let chat_type = int_of(contact, "chatType")?;
            let peer_uid = str_of(contact, "peerUid");
            let peer_uin = str_of(contact, "peerUin");
            let raw_name = str_of(contact, "name");
            let name = if raw_name.is_empty() || raw_name == peer_uid {
                if chat_type == 2 {
                    format!("群聊 {peer_uid}")
                } else if peer_uin.is_empty() {
                    peer_uid.clone()
                } else {
                    peer_uin.clone()
                }
            } else {
                raw_name
            };
            let avatar_url = if chat_type == 2 {
                format!("https://p.qlogo.cn/gh/{peer_uid}/{peer_uid}/640/")
            } else {
                str_of(contact, "avatarUrl")
            };
            let mut session = json!({
                "kind": kind,
                "chatType": chat_type,
                "peerUid": peer_uid,
                "name": name,
                "avatarUrl": avatar_url,
            });
            let object = session.as_object_mut()?;
            if !peer_uin.is_empty() {
                object.insert("peerUin".to_string(), Value::String(peer_uin));
            }
            if let Some(last_msg_time) = contact.get("lastMsgTime").and_then(Value::as_str) {
                object.insert(
                    "lastMsgTime".to_string(),
                    Value::String(last_msg_time.to_string()),
                );
            }
            Some(session)
        })
        .collect()
}

fn imported_session_aliases(session: &ImportedSession) -> Vec<String> {
    let mut aliases = vec![format!("{}|{}", session.chat_type, session.peer_uid)];
    if let Some(peer_uin) = session.peer_uin.as_deref() {
        if !peer_uin.is_empty() && peer_uin != session.peer_uid {
            aliases.push(format!("{}|{peer_uin}", session.chat_type));
        }
    }
    aliases
}

fn value_session_aliases(session: &Value) -> Vec<String> {
    let Some(chat_type) = int_of(session, "chatType") else {
        return Vec::new();
    };
    let peer_uid = str_of(session, "peerUid");
    let peer_uin = str_of(session, "peerUin");
    [peer_uid, peer_uin]
        .into_iter()
        .filter(|identifier| !identifier.is_empty())
        .map(|identifier| format!("{chat_type}|{identifier}"))
        .collect()
}

fn imported_inactive_session(
    session: &ImportedSession,
    friend_ids: &HashSet<String>,
    active_group_codes: &HashSet<String>,
) -> Option<Value> {
    let peer_uin = session.peer_uin.as_deref().unwrap_or_default();
    let kind = match session.chat_type {
        1 if !friend_ids.contains(&session.peer_uid)
            && (peer_uin.is_empty() || !friend_ids.contains(peer_uin)) =>
        {
            "non_friend"
        }
        2 if !active_group_codes.contains(&session.peer_uid)
            && (peer_uin.is_empty() || !active_group_codes.contains(peer_uin)) =>
        {
            "unavailable_group"
        }
        _ => return None,
    };
    let mut value = json!({
        "kind": kind,
        "chatType": session.chat_type,
        "peerUid": session.peer_uid,
        "name": session.name,
        "avatarUrl": session.avatar_url,
        "backupImportId": session.import_id,
        "sourceName": session.source_name,
        "messageCount": session.message_count,
    });
    let object = value.as_object_mut()?;
    if !peer_uin.is_empty() {
        object.insert("peerUin".to_string(), Value::String(peer_uin.to_string()));
    }
    if let Some(last_msg_time) = session.last_msg_time.as_deref() {
        object.insert(
            "lastMsgTime".to_string(),
            Value::String(last_msg_time.to_string()),
        );
    }
    Some(value)
}

fn merge_imported_inactive_sessions(
    sessions: &mut Vec<Value>,
    imported: &[ImportedSession],
    friend_ids: &HashSet<String>,
    active_group_codes: &HashSet<String>,
) {
    let mut seen: HashSet<String> = sessions.iter().flat_map(value_session_aliases).collect();
    for imported_session in imported {
        let aliases = imported_session_aliases(imported_session);
        if aliases.iter().any(|alias| seen.contains(alias)) {
            continue;
        }
        let Some(session) =
            imported_inactive_session(imported_session, friend_ids, active_group_codes)
        else {
            continue;
        };
        seen.extend(aliases);
        sessions.push(session);
    }
}

fn build_recent_contacts(
    raw_contacts: &[Value],
    friend_uids: &HashSet<String>,
    include_all: bool,
) -> Vec<Value> {
    raw_contacts
        .iter()
        .filter_map(|contact| map_recent_contact(contact, friend_uids, include_all))
        .filter(|contact| {
            include_all
                || matches!(
                    contact.get("classification").and_then(Value::as_str),
                    Some("private" | "special")
                )
        })
        .collect()
}

fn detail_name(detail: &Value) -> Option<String> {
    for key in ["remark", "nick", "nickName"] {
        let value = str_of(detail, key);
        if !value.is_empty() {
            return Some(value);
        }
    }
    let core = detail
        .get("simpleInfo")
        .and_then(|simple| simple.get("coreInfo"))?;
    for key in ["remark", "nick"] {
        let value = str_of(core, key);
        if !value.is_empty() {
            return Some(value);
        }
    }
    None
}

async fn detail_name_with_timeout<F, E>(timeout: Duration, lookup: F) -> Option<String>
where
    F: std::future::Future<Output = Result<Value, E>>,
{
    let detail = tokio::time::timeout(timeout, lookup).await.ok()?.ok()?;
    detail_name(&detail)
}

fn unnamed_special_contacts(contacts: &[Value], limit: usize) -> Vec<(usize, String)> {
    contacts
        .iter()
        .enumerate()
        .filter_map(|(index, contact)| {
            let peer_uid = contact.get("peerUid")?.as_str()?;
            let name = contact.get("name")?.as_str()?;
            (contact.get("classification").and_then(Value::as_str) == Some("special")
                && name == peer_uid
                && peer_uid.starts_with("u_"))
            .then(|| (index, peer_uid.to_string()))
        })
        .take(limit)
        .collect()
}

async fn enrich_contact_names(contacts: &mut [Value], state: &SharedState, include_all: bool) {
    if !include_all {
        return;
    }
    let lookups = unnamed_special_contacts(contacts, 30)
        .into_iter()
        .map(|(index, peer_uid)| {
            let napcat = state.napcat.clone();
            async move {
                detail_name_with_timeout(
                    Duration::from_secs(2),
                    napcat.get_user_detail_info(&peer_uid),
                )
                .await
                .map(|name| (index, name))
            }
        });
    for (index, name) in join_all(lookups).await.into_iter().flatten() {
        if let Some(contact) = contacts.get_mut(index).and_then(Value::as_object_mut) {
            contact.insert("name".to_string(), Value::String(name));
        }
    }
}

/// 单个好友映射。
fn map_friend(friend: &Value, category_id: &Value) -> Value {
    // NapCat 新结构：字段在 coreInfo / baseInfo 下；旧结构直接平铺。
    let core = friend.get("coreInfo").unwrap_or(friend);
    let base = friend.get("baseInfo").unwrap_or(friend);
    let status = friend.get("status").cloned().unwrap_or(Value::Null);
    let uid = str_of(core, "uid");
    let uin = {
        let u = str_of(core, "uin");
        if u.is_empty() {
            str_of(friend, "uin")
        } else {
            u
        }
    };
    let nick = {
        let n = str_of(core, "nick");
        if n.is_empty() {
            str_of(friend, "nick")
        } else {
            n
        }
    };
    let remark = {
        let r = str_of(core, "remark");
        if r.is_empty() {
            str_of(friend, "remark")
        } else {
            r
        }
    };
    let is_online = base
        .get("isOnline")
        .and_then(Value::as_bool)
        .or_else(|| friend.get("isOnline").and_then(Value::as_bool))
        .unwrap_or(false);
    json!({
        "uid": if uid.is_empty() { str_of(friend, "uid") } else { uid },
        "uin": uin,
        "nick": nick,
        "remark": remark,
        "avatarUrl": format!("https://q1.qlogo.cn/g?b=qq&nk={uin}&s=640"),
        "isOnline": is_online,
        "status": status,
        "categoryId": category_id,
    })
}

/// `GET /api/friends` — 好友列表（分页）。
pub async fn list_friends(
    State(state): State<SharedState>,
    Extension(RequestId(request_id)): Extension<RequestId>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let (page, limit) = page_and_limit(&params);
    let force_refresh = params.get("forceRefresh").map(String::as_str) == Some("true");

    let categories = match state.napcat.get_buddy_v2_ex_with_cate(force_refresh).await {
        Ok(value) => value,
        Err(error) => {
            let err = ApiError::new(ErrorType::Api, error.to_string(), "GET_FRIENDS_FAILED");
            return response::error(&err, &request_id);
        }
    };

    let empty: Vec<Value> = Vec::new();
    let cats = categories.as_array().unwrap_or(&empty);
    let mut friends: Vec<Value> = Vec::new();
    for cat in cats {
        let category_id = cat.get("categoryId").cloned().unwrap_or(Value::Null);
        if let Some(list) = cat.get("buddyList").and_then(Value::as_array) {
            for friend in list {
                friends.push(map_friend(friend, &category_id));
            }
        }
    }

    let total = friends.len();
    let start_index = (page - 1).saturating_mul(limit);
    let end_index = start_index.saturating_add(limit);
    let paginated: Vec<Value> = friends
        .iter()
        .skip(start_index)
        .take(limit)
        .cloned()
        .collect();

    response::success(
        json!({
            "friends": paginated,
            "totalCount": total,
            "currentPage": page,
            "totalPages": total.div_ceil(limit),
            "hasNext": end_index < total,
            "hasPrev": page > 1,
        }),
        &request_id,
    )
}

/// `GET /api/friends/:uid` — 好友详情。
pub async fn friend_detail(
    State(state): State<SharedState>,
    Extension(RequestId(request_id)): Extension<RequestId>,
    Path(uid): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    if uid.is_empty() {
        let err = ApiError::validation("用户ID不能为空", "INVALID_UID");
        return response::error(&err, &request_id);
    }
    let _no_cache = params.get("no_cache").map(String::as_str) == Some("true");
    match state.napcat.get_user_detail_info(&uid).await {
        Ok(Value::Null) => {
            let err = ApiError::new(ErrorType::Api, "用户不存在", "USER_NOT_FOUND");
            response::error(&err, &request_id)
        }
        Ok(detail) => response::success(detail, &request_id),
        Err(error) => {
            let err = ApiError::new(ErrorType::Api, error.to_string(), "FRIEND_DETAIL_FAILED");
            response::error(&err, &request_id)
        }
    }
}

/// `GET /api/recent-contacts` — 最近联系人中不属于好友 / 群聊的会话。
pub async fn recent_contacts(
    State(state): State<SharedState>,
    Extension(RequestId(request_id)): Extension<RequestId>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let limit = recent_contact_limit(&params);
    let include_all = params.get("includeAll").map(String::as_str) == Some("true");

    let loaded = match load_recent_contacts(&state, limit, include_all).await {
        Ok(loaded) => loaded,
        Err(error) => return response::error(&error, &request_id),
    };
    let raw_contacts = loaded.contacts;

    let friend_uids = state
        .napcat
        .get_friends(false)
        .await
        .ok()
        .and_then(|friends| friend_identifiers(&friends))
        .unwrap_or_default();

    let mut contacts = build_recent_contacts(&raw_contacts, &friend_uids, include_all);
    enrich_contact_names(&mut contacts, &state, include_all).await;

    response::success(
        json!({
            "contacts": contacts,
            "totalCount": contacts.len(),
            "rawCount": raw_contacts.len(),
        }),
        &request_id,
    )
}

/// `GET /api/inactive-sessions` — 本机历史会话中已不在当前好友 / 群列表的会话。
pub async fn inactive_sessions(
    State(state): State<SharedState>,
    Extension(RequestId(request_id)): Extension<RequestId>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let limit = inactive_session_limit(&params);
    let loaded = match load_recent_contacts(&state, limit, true).await {
        Ok(loaded) => loaded,
        Err(error) => return response::error(&error, &request_id),
    };

    let (friends, groups) = tokio::join!(
        state.napcat.get_friends(false),
        state.napcat.get_groups(false),
    );
    let friends = match friends {
        Ok(friends) => friends,
        Err(error) => {
            let err = ApiError::new(
                ErrorType::Api,
                format!("获取当前好友列表失败: {error}"),
                "INACTIVE_SESSIONS_FAILED",
            );
            return response::error(&err, &request_id);
        }
    };
    let groups = match groups {
        Ok(groups) => groups,
        Err(error) => {
            let err = ApiError::new(
                ErrorType::Api,
                format!("获取当前群列表失败: {error}"),
                "INACTIVE_SESSIONS_FAILED",
            );
            return response::error(&err, &request_id);
        }
    };

    let (friend_ids, active_group_codes) = match inactive_membership_sets(&friends, &groups) {
        Ok(sets) => sets,
        Err(list_name) => {
            let err = ApiError::new(
                ErrorType::Api,
                format!("当前{list_name}列表响应结构异常"),
                "INACTIVE_SESSIONS_FAILED",
            );
            return response::error(&err, &request_id);
        }
    };
    let raw_count = loaded.contacts.len();
    let mut contacts: Vec<Value> = loaded
        .contacts
        .iter()
        .filter_map(|contact| map_recent_contact(contact, &friend_ids, true))
        .collect();
    enrich_contact_names(&mut contacts, &state, true).await;
    let mut sessions = build_inactive_sessions(&contacts, &friend_ids, &active_group_codes);
    let imported_sessions = match state.backup_import_manager.list_sessions().await {
        Ok(sessions) => sessions,
        Err(error) => {
            tracing::warn!(
                "failed to supplement inactive sessions from imported databases: {error}"
            );
            Vec::new()
        }
    };
    let database_raw_count = imported_sessions.len();
    merge_imported_inactive_sessions(
        &mut sessions,
        &imported_sessions,
        &friend_ids,
        &active_group_codes,
    );
    sessions.sort_by(|left, right| {
        str_of(right, "lastMsgTime")
            .cmp(&str_of(left, "lastMsgTime"))
            .then_with(|| str_of(left, "name").cmp(&str_of(right, "name")))
    });
    sessions.truncate(limit as usize);
    let non_friend_count = sessions
        .iter()
        .filter(|session| session.get("kind").and_then(Value::as_str) == Some("non_friend"))
        .count();
    let total_count = sessions.len();
    let unavailable_group_count = total_count.saturating_sub(non_friend_count);

    response::success(
        json!({
            "sessions": sessions,
            "totalCount": total_count,
            "nonFriendCount": non_friend_count,
            "unavailableGroupCount": unavailable_group_count,
            "rawCount": raw_count,
            "databaseRawCount": database_raw_count,
            "indexSource": loaded.source,
            "source": if database_raw_count > 0 { "database" } else { loaded.source },
        }),
        &request_id,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_snapshot_count_without_rejecting_valid_values() {
        assert_eq!(recent_contact_limit(&HashMap::new()), 100);
        assert_eq!(inactive_session_limit(&HashMap::new()), 2_000);
        assert_eq!(
            recent_contact_limit(&HashMap::from([("limit".to_string(), "500".to_string())])),
            500
        );
        assert_eq!(
            recent_contact_limit(&HashMap::from([("limit".to_string(), "9000".to_string())])),
            2_000
        );
        assert_eq!(
            inactive_session_limit(&HashMap::from([("limit".to_string(), "50".to_string())])),
            50
        );
    }

    #[test]
    fn merges_snapshot_and_local_contacts_with_stable_deduplication() {
        let merged = merge_recent_contacts([
            vec![
                json!({"chatType": 1, "peerUid": "bot", "peerName": "snapshot"}),
                json!({"chatType": 2, "peerUid": "group"}),
            ],
            vec![
                json!({"chatType": 1, "peerUid": "bot", "peerName": "local"}),
                json!({"chatType": 118, "peerUid": "service"}),
                json!({"chatType": 100}),
            ],
        ]);
        assert_eq!(merged.len(), 3);
        assert_eq!(merged[0]["peerName"], "snapshot");
        assert_eq!(merged[2]["peerUid"], "service");
    }

    #[test]
    fn maps_non_friend_private_sessions_for_frontend_merging() {
        let contact = json!({
            "chatType": 1,
            "peerUid": "u_bot",
            "peerUin": "123456",
            "sendNickName": "QQ Bot",
            "msgId": "message-1",
            "msgTime": 1_783_950_000,
        });
        let mapped = map_recent_contact(&contact, &HashSet::new(), true).unwrap();
        assert_eq!(mapped["classification"], "special");
        assert_eq!(mapped["name"], "QQ Bot");
        assert_eq!(mapped["lastMsgId"], "message-1");
        assert_eq!(mapped["lastMsgTime"], "2026-07-13T13:40:00.000Z");
    }

    #[test]
    fn include_all_keeps_friends_distinct_from_non_friend_private_sessions() {
        let friend_uids = HashSet::from(["u_friend".to_string()]);
        let friend = map_recent_contact(
            &json!({"chatType": 1, "peerUid": "u_friend"}),
            &friend_uids,
            true,
        )
        .unwrap();
        let non_friend = map_recent_contact(
            &json!({"chatType": 1, "peerUid": "u_bot"}),
            &friend_uids,
            true,
        )
        .unwrap();
        assert_eq!(friend["classification"], "friend");
        assert_eq!(non_friend["classification"], "special");
    }

    #[test]
    fn friend_identifiers_support_nested_and_flat_payloads() {
        let identifiers = friend_identifiers(&json!([
            {"coreInfo": {"uid": "u_nested", "uin": "10001"}},
            {"uid": "u_flat", "uin": 10002}
        ]))
        .expect("array payload should be accepted");
        assert_eq!(identifiers.len(), 4);
        for expected in ["u_nested", "10001", "u_flat", "10002"] {
            assert!(identifiers.contains(expected));
        }

        let wrapped = friend_identifiers(&json!({
            "friends": [{"uid": "u_wrapped", "uin": "10003"}]
        }))
        .expect("wrapped payload should be accepted");
        assert!(wrapped.contains("u_wrapped"));
        assert!(wrapped.contains("10003"));
    }

    #[test]
    fn inactive_membership_requires_both_current_lists() {
        let contacts = [json!({
            "chatType": 1,
            "peerUid": "u_active",
            "peerUin": "10001",
            "name": "当前好友"
        })];

        assert!(inactive_membership_sets(&Value::Null, &json!([])).is_err());
        assert!(inactive_membership_sets(&json!([]), &json!({"unexpected": []})).is_err());

        let (friend_ids, active_group_codes) = inactive_membership_sets(
            &json!({"data": [{"uid": "u_active", "uin": "10001"}]}),
            &json!({"groups": []}),
        )
        .expect("supported wrappers should be accepted");
        assert!(build_inactive_sessions(&contacts, &friend_ids, &active_group_codes).is_empty());
    }

    #[test]
    fn inactive_sessions_exclude_active_peers_and_unrelated_chat_types() {
        let friend_ids = HashSet::from([
            "u_active".to_string(),
            "10001".to_string(),
            "10002".to_string(),
        ]);
        let active_group_codes = HashSet::from(["20001".to_string()]);
        let raw_contacts = [
            json!({"chatType": 1, "peerUid": "u_active", "peerUin": "10001"}),
            json!({"chatType": 1, "peerUid": "u_changed", "peerUin": "10002"}),
            json!({
                "chatType": 1,
                "peerUid": "u_removed",
                "peerUin": "10003",
                "peerName": "旧好友",
                "msgTime": 1_783_950_000
            }),
            json!({"chatType": 2, "peerUid": "20001", "peerName": "当前群"}),
            json!({"chatType": 2, "peerUid": "20002"}),
            json!({"chatType": 118, "peerUid": "u_service", "peerName": "服务号"}),
        ];
        let contacts: Vec<Value> = raw_contacts
            .iter()
            .filter_map(|contact| map_recent_contact(contact, &friend_ids, true))
            .collect();
        let sessions = build_inactive_sessions(&contacts, &friend_ids, &active_group_codes);

        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0]["kind"], "non_friend");
        assert_eq!(sessions[0]["name"], "旧好友");
        assert_eq!(sessions[0]["peerUin"], "10003");
        assert_eq!(sessions[0]["lastMsgTime"], "2026-07-13T13:40:00.000Z");
        assert_eq!(sessions[1]["kind"], "unavailable_group");
        assert_eq!(sessions[1]["name"], "群聊 20002");
        assert_eq!(
            sessions[1]["avatarUrl"],
            "https://p.qlogo.cn/gh/20002/20002/640/"
        );
    }

    #[test]
    fn imported_message_tables_supplement_the_recent_contact_index() {
        let friend_ids = HashSet::from(["u_active".to_string(), "10001".to_string()]);
        let active_group_codes = HashSet::from(["20001".to_string()]);
        let mut sessions = vec![json!({
            "kind": "non_friend",
            "chatType": 1,
            "peerUid": "u_recent_removed",
            "peerUin": "10002",
            "name": "最近索引中的旧好友"
        })];
        let imported = vec![
            ImportedSession {
                import_id: "backup-1".to_string(),
                source_name: "nt_msg.db".to_string(),
                format: "nt_msg_raw".to_string(),
                chat_type: 1,
                peer_uid: "u_active".to_string(),
                peer_uin: Some("10001".to_string()),
                name: "QQ 10001".to_string(),
                avatar_url: String::new(),
                last_msg_time: None,
                message_count: 10,
            },
            ImportedSession {
                import_id: "backup-1".to_string(),
                source_name: "nt_msg.db".to_string(),
                format: "nt_msg_raw".to_string(),
                chat_type: 1,
                peer_uid: "u_historical_removed".to_string(),
                peer_uin: Some("10003".to_string()),
                name: "QQ 10003".to_string(),
                avatar_url: "private-avatar".to_string(),
                last_msg_time: Some("2014-01-02T00:00:00Z".to_string()),
                message_count: 200,
            },
            ImportedSession {
                import_id: "backup-1".to_string(),
                source_name: "nt_msg.db".to_string(),
                format: "nt_msg_raw".to_string(),
                chat_type: 2,
                peer_uid: "20002".to_string(),
                peer_uin: Some("20002".to_string()),
                name: "群聊 20002".to_string(),
                avatar_url: "group-avatar".to_string(),
                last_msg_time: Some("2020-01-02T00:00:00Z".to_string()),
                message_count: 300,
            },
        ];

        merge_imported_inactive_sessions(
            &mut sessions,
            &imported,
            &friend_ids,
            &active_group_codes,
        );

        assert_eq!(sessions.len(), 3, "active friends must remain excluded");
        assert_eq!(sessions[1]["backupImportId"], "backup-1");
        assert_eq!(sessions[1]["kind"], "non_friend");
        assert_eq!(sessions[1]["messageCount"], 200);
        assert_eq!(sessions[2]["kind"], "unavailable_group");
    }

    #[test]
    fn device_sessions_are_special_and_named() {
        // 数据线设备会话（chatType 8/134）：无 peerName 时兜底「我的设备」，且归类 special。
        for chat_type in [8, 134] {
            let contact = json!({
                "chatType": chat_type,
                "peerUid": "u_device_peer",
            });
            let mapped = map_recent_contact(&contact, &HashSet::new(), true).unwrap();
            assert_eq!(mapped["classification"], "special");
            assert_eq!(mapped["name"], "我的设备");
        }
        // NapCat 提供了 peerName（如「我的电脑」）时应保留原名。
        let named = map_recent_contact(
            &json!({"chatType": 8, "peerUid": "u_device_peer", "peerName": "我的电脑"}),
            &HashSet::new(),
            true,
        )
        .unwrap();
        assert_eq!(named["name"], "我的电脑");
    }

    #[test]
    fn preserves_private_group_and_special_classification() {
        let friend_uids = HashSet::from(["u_friend".to_string()]);
        let cases = [
            (json!({"chatType": 1, "peerUid": "u_friend"}), "friend"),
            (json!({"chatType": 1, "peerUid": "u_bot"}), "private"),
            (json!({"chatType": 2, "peerUid": "12345"}), "group"),
            (json!({"chatType": 100, "peerUid": "u_temp"}), "special"),
            (json!({"chatType": 118, "peerUid": "u_service"}), "special"),
            (json!({"chatType": 201, "peerUid": "u_public"}), "special"),
            (json!({"chatType": 9, "peerUid": "u_guild"}), "special"),
            (json!({"chatType": 16, "peerUid": "u_channel"}), "special"),
        ];
        for (contact, expected) in cases {
            assert_eq!(
                map_recent_contact(&contact, &friend_uids, false).unwrap()["classification"],
                expected
            );
        }
    }

    #[test]
    fn default_response_excludes_friends_and_groups() {
        let friend_uids = HashSet::from(["u_friend".to_string()]);
        let contacts = build_recent_contacts(
            &[
                json!({"chatType": 1, "peerUid": "u_friend"}),
                json!({"chatType": 1, "peerUid": "u_bot"}),
                json!({"chatType": 2, "peerUid": "12345"}),
                json!({"chatType": 118, "peerUid": "u_service"}),
            ],
            &friend_uids,
            false,
        );
        assert_eq!(contacts.len(), 2);
        assert_eq!(contacts[0]["classification"], "private");
        assert_eq!(contacts[1]["classification"], "special");
    }

    #[test]
    fn unnamed_enrichment_is_bounded_to_thirty_special_contacts() {
        let contacts: Vec<Value> = (0..35)
            .map(|index| {
                json!({
                    "peerUid": format!("u_{index}"),
                    "name": format!("u_{index}"),
                    "classification": "special",
                })
            })
            .collect();
        let candidates = unnamed_special_contacts(&contacts, 30);
        assert_eq!(candidates.len(), 30);
        assert_eq!(candidates[0], (0, "u_0".to_string()));
        assert_eq!(candidates[29], (29, "u_29".to_string()));
    }

    #[tokio::test]
    async fn detail_name_lookup_times_out_and_tolerates_errors() {
        let timed_out = detail_name_with_timeout(
            Duration::from_millis(1),
            std::future::pending::<Result<Value, ()>>(),
        )
        .await;
        assert_eq!(timed_out, None);

        let failed =
            detail_name_with_timeout(Duration::from_secs(1), async { Err::<Value, ()>(()) }).await;
        assert_eq!(failed, None);

        let resolved = detail_name_with_timeout(Duration::from_secs(1), async {
            Ok::<Value, ()>(json!({
                "simpleInfo": {"coreInfo": {"nick": "Readable Bot"}}
            }))
        })
        .await;
        assert_eq!(resolved.as_deref(), Some("Readable Bot"));
    }
}
