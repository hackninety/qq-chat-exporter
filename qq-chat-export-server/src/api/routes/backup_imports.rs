use std::path::PathBuf;

use axum::extract::{Extension, Multipart, State};
use axum::http::header::{CACHE_CONTROL, HOST, ORIGIN, PRAGMA};
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri};
use axum::response::Response;
use serde_json::json;
use tokio::io::AsyncWriteExt;

use crate::api::helpers::current_account_uin;
use crate::api::response::{self, ApiError, ErrorType, RequestId};
use crate::api::state::SharedState;
use crate::backup_import::{BackupImportError, KeyDetectionError};

const MAX_BACKUP_SIZE: u64 = 8 * 1024 * 1024 * 1024;
const MAX_KEY_SAMPLE_SIZE: u64 = 16 * 1024 * 1024;

fn import_error(error: BackupImportError) -> ApiError {
    let status = match error {
        BackupImportError::NotFound => StatusCode::NOT_FOUND,
        BackupImportError::Io(_)
        | BackupImportError::Database(_)
        | BackupImportError::Manifest(_) => StatusCode::INTERNAL_SERVER_ERROR,
        _ => StatusCode::BAD_REQUEST,
    };
    let error_type = if status == StatusCode::INTERNAL_SERVER_ERROR {
        ErrorType::Database
    } else {
        ErrorType::Validation
    };
    let code = error.code();
    ApiError::new(error_type, error.to_string(), code).with_status(status)
}

fn key_detection_error(error: KeyDetectionError) -> ApiError {
    let status = match error {
        KeyDetectionError::Io(_) | KeyDetectionError::ScannerFailed => {
            StatusCode::INTERNAL_SERVER_ERROR
        }
        KeyDetectionError::UnsupportedPlatform => StatusCode::NOT_IMPLEMENTED,
        _ => StatusCode::BAD_REQUEST,
    };
    ApiError::new(
        if status == StatusCode::INTERNAL_SERVER_ERROR {
            ErrorType::Unknown
        } else {
            ErrorType::Validation
        },
        error.to_string(),
        error.code(),
    )
    .with_status(status)
}

fn no_store(mut response: Response) -> Response {
    response.headers_mut().insert(
        CACHE_CONTROL,
        HeaderValue::from_static("no-store, max-age=0"),
    );
    response
        .headers_mut()
        .insert(PRAGMA, HeaderValue::from_static("no-cache"));
    response
}

fn key_detection_origin_allowed(headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(ORIGIN).and_then(|value| value.to_str().ok()) else {
        // Non-browser loopback clients do not send Origin. A local process that
        // can call this API can already request PROCESS_VM_READ itself.
        return true;
    };
    let Some(host) = headers.get(HOST).and_then(|value| value.to_str().ok()) else {
        return false;
    };
    let Ok(uri) = origin.parse::<Uri>() else {
        return false;
    };
    if !matches!(uri.scheme_str(), Some("http" | "https")) {
        return false;
    }
    let Some(authority) = uri.authority() else {
        return false;
    };
    if !authority.as_str().eq_ignore_ascii_case(host) {
        return false;
    }
    let origin_host = authority.host().trim_matches(['[', ']']);
    origin_host.eq_ignore_ascii_case("localhost")
        || origin_host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

/// `GET /api/chat-backups` — 列出已导入的聊天记录数据库。
pub async fn list_backups(
    State(state): State<SharedState>,
    Extension(RequestId(request_id)): Extension<RequestId>,
) -> Response {
    let account_uin = match current_account_uin(&state.napcat).await {
        Ok(account_uin) => account_uin,
        Err(error) => return response::error(&error, &request_id),
    };
    match state.backup_import_manager.list_imports(account_uin).await {
        Ok(imports) => response::success(json!({ "imports": imports }), &request_id),
        Err(error) => response::error(&import_error(error), &request_id),
    }
}

/// `GET /api/chat-backups/sessions` — 汇总全部导入库中的私聊和群聊。
pub async fn list_backup_sessions(
    State(state): State<SharedState>,
    Extension(RequestId(request_id)): Extension<RequestId>,
) -> Response {
    let account_uin = match current_account_uin(&state.napcat).await {
        Ok(account_uin) => account_uin,
        Err(error) => return response::error(&error, &request_id),
    };
    match state.backup_import_manager.list_sessions(account_uin).await {
        Ok(sessions) => response::success(
            json!({ "totalCount": sessions.len(), "sessions": sessions }),
            &request_id,
        ),
        Err(error) => response::error(&import_error(error), &request_id),
    }
}

/// `POST /api/chat-backups/detect-key` — 从本机已登录 QQ 只读检测并验证数据库密钥。
pub async fn detect_backup_key(
    State(state): State<SharedState>,
    Extension(RequestId(request_id)): Extension<RequestId>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> Response {
    if !key_detection_origin_allowed(&headers) {
        return no_store(response::error(
            &ApiError::new(
                ErrorType::Auth,
                "数据库密钥只能从本机 QCE 页面自动检测",
                "BACKUP_KEY_ORIGIN_FORBIDDEN",
            )
            .with_status(StatusCode::FORBIDDEN),
            &request_id,
        ));
    }

    let upload_id = uuid::Uuid::new_v4().simple().to_string();
    let temporary_path = state
        .backup_import_manager
        .uploads_dir()
        .join(format!("{upload_id}.key-sample"));
    let mut uploaded = false;
    let mut original_name: Option<String> = None;
    let mut original_size: Option<u64> = None;
    let mut error: Option<ApiError> = None;

    loop {
        let mut field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(field_error) => {
                error = Some(ApiError::validation(
                    format!("读取密钥检测请求失败: {field_error}"),
                    "BACKUP_KEY_SAMPLE_INVALID",
                ));
                break;
            }
        };
        if field.name() == Some("originalSize") && original_size.is_none() {
            match field.text().await {
                Ok(value) => match value.parse::<u64>() {
                    Ok(size) if size > 0 && size <= MAX_BACKUP_SIZE => {
                        original_size = Some(size);
                    }
                    _ => {
                        error = Some(ApiError::validation(
                            "原始数据库大小无效",
                            "BACKUP_KEY_SAMPLE_INVALID",
                        ));
                        break;
                    }
                },
                Err(_) => {
                    error = Some(ApiError::validation(
                        "读取原始数据库大小失败",
                        "BACKUP_KEY_SAMPLE_INVALID",
                    ));
                    break;
                }
            }
            continue;
        }
        if field.name() != Some("file") || uploaded {
            continue;
        }
        original_name = field.file_name().map(str::to_string);
        let Ok(mut file) = tokio::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary_path)
            .await
        else {
            error = Some(ApiError::new(
                ErrorType::FileSystem,
                "创建密钥检测临时文件失败",
                "BACKUP_KEY_SCAN_FAILED",
            ));
            break;
        };
        let mut total = 0_u64;
        loop {
            match field.chunk().await {
                Ok(Some(chunk)) => {
                    total = total.saturating_add(chunk.len() as u64);
                    if total > MAX_KEY_SAMPLE_SIZE {
                        error = Some(ApiError::validation(
                            "密钥检测样本不能超过 16 MiB",
                            "BACKUP_KEY_SAMPLE_TOO_LARGE",
                        ));
                        break;
                    }
                    if file.write_all(&chunk).await.is_err() {
                        error = Some(ApiError::new(
                            ErrorType::FileSystem,
                            "写入密钥检测临时文件失败",
                            "BACKUP_KEY_SCAN_FAILED",
                        ));
                        break;
                    }
                }
                Ok(None) => break,
                Err(_) => {
                    error = Some(ApiError::validation(
                        "读取密钥检测样本失败",
                        "BACKUP_KEY_SAMPLE_INVALID",
                    ));
                    break;
                }
            }
        }
        if error.is_none() && total > 0 && file.flush().await.is_ok() {
            uploaded = true;
        }
        break;
    }

    let result = if let Some(error) = error {
        response::error(&error, &request_id)
    } else if uploaded {
        match state
            .backup_import_manager
            .detect_key(temporary_path.clone(), original_name, original_size)
            .await
        {
            Ok(detection) => response::success(json!(detection), &request_id),
            Err(detection_error) => {
                response::error(&key_detection_error(detection_error), &request_id)
            }
        }
    } else {
        response::error(
            &ApiError::validation("请求中没有数据库检测样本", "BACKUP_KEY_SAMPLE_REQUIRED"),
            &request_id,
        )
    };
    let _ = tokio::fs::remove_file(&temporary_path).await;
    no_store(result)
}

/// `POST /api/chat-backups/upload` — 流式上传备份文件后导入。
pub async fn upload_backup(
    State(state): State<SharedState>,
    Extension(RequestId(request_id)): Extension<RequestId>,
    mut multipart: Multipart,
) -> Response {
    let account_uin = match current_account_uin(&state.napcat).await {
        Ok(account_uin) => account_uin,
        Err(error) => return response::error(&error, &request_id),
    };
    let upload_id = uuid::Uuid::new_v4().simple().to_string();
    let temporary_path = state
        .backup_import_manager
        .uploads_dir()
        .join(format!("{upload_id}.part"));
    let mut uploaded_path: Option<PathBuf> = None;
    let mut original_name: Option<String> = None;
    let mut key: Option<String> = None;
    let mut error: Option<ApiError> = None;

    loop {
        let mut field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(field_error) => {
                error = Some(ApiError::validation(
                    format!("读取上传请求失败: {field_error}"),
                    "BACKUP_UPLOAD_INVALID",
                ));
                break;
            }
        };
        match field.name() {
            Some("key") => match field.text().await {
                Ok(value) if value.len() <= 256 => {
                    key = (!value.trim().is_empty()).then(|| value.trim().to_string());
                }
                Ok(_) => {
                    error = Some(ApiError::validation("数据库密钥过长", "BACKUP_KEY_INVALID"));
                    break;
                }
                Err(field_error) => {
                    error = Some(ApiError::validation(
                        format!("读取数据库密钥失败: {field_error}"),
                        "BACKUP_UPLOAD_INVALID",
                    ));
                    break;
                }
            },
            Some("file") if uploaded_path.is_none() => {
                original_name = field.file_name().map(str::to_string);
                let mut file = match tokio::fs::OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .open(&temporary_path)
                    .await
                {
                    Ok(file) => file,
                    Err(file_error) => {
                        error = Some(ApiError::new(
                            ErrorType::FileSystem,
                            format!("创建上传暂存文件失败: {file_error}"),
                            "BACKUP_UPLOAD_FAILED",
                        ));
                        break;
                    }
                };
                let mut total = 0_u64;
                loop {
                    match field.chunk().await {
                        Ok(Some(chunk)) => {
                            total = total.saturating_add(chunk.len() as u64);
                            if total > MAX_BACKUP_SIZE {
                                error = Some(ApiError::validation(
                                    "聊天记录备份不能超过 8 GiB",
                                    "BACKUP_TOO_LARGE",
                                ));
                                break;
                            }
                            if let Err(write_error) = file.write_all(&chunk).await {
                                error = Some(ApiError::new(
                                    ErrorType::FileSystem,
                                    format!("写入上传文件失败: {write_error}"),
                                    "BACKUP_UPLOAD_FAILED",
                                ));
                                break;
                            }
                        }
                        Ok(None) => break,
                        Err(field_error) => {
                            error = Some(ApiError::validation(
                                format!("读取上传内容失败: {field_error}"),
                                "BACKUP_UPLOAD_INVALID",
                            ));
                            break;
                        }
                    }
                }
                if error.is_none() {
                    if let Err(flush_error) = file.flush().await {
                        error = Some(ApiError::new(
                            ErrorType::FileSystem,
                            format!("保存上传文件失败: {flush_error}"),
                            "BACKUP_UPLOAD_FAILED",
                        ));
                    } else {
                        uploaded_path = Some(temporary_path.clone());
                    }
                }
                if error.is_some() {
                    break;
                }
            }
            _ => {}
        }
    }

    let response = if let Some(error) = error {
        response::error(&error, &request_id)
    } else if let Some(path) = uploaded_path {
        match state
            .backup_import_manager
            .import_path(path, original_name, key, account_uin)
            .await
        {
            Ok(imported) => response::success(json!({ "import": imported }), &request_id),
            Err(import_error_value) => {
                response::error(&import_error(import_error_value), &request_id)
            }
        }
    } else {
        response::error(
            &ApiError::validation("请求中没有备份文件", "BACKUP_FILE_REQUIRED"),
            &request_id,
        )
    };
    let _ = tokio::fs::remove_file(&temporary_path).await;
    response
}

#[cfg(test)]
mod tests {
    use axum::http::header::{HOST, ORIGIN};
    use axum::http::{HeaderMap, HeaderValue};

    use super::{key_detection_origin_allowed, MAX_BACKUP_SIZE, MAX_KEY_SAMPLE_SIZE};

    #[test]
    fn backup_upload_limit_covers_large_ntqq_databases() {
        assert_eq!(MAX_BACKUP_SIZE, 8 * 1024 * 1024 * 1024);
        assert_eq!(MAX_KEY_SAMPLE_SIZE, 16 * 1024 * 1024);
    }

    #[test]
    fn key_detection_only_accepts_same_loopback_browser_origin() {
        let mut headers = HeaderMap::new();
        headers.insert(HOST, HeaderValue::from_static("127.0.0.1:40653"));
        headers.insert(ORIGIN, HeaderValue::from_static("http://127.0.0.1:40653"));
        assert!(key_detection_origin_allowed(&headers));

        headers.insert(ORIGIN, HeaderValue::from_static("https://example.com"));
        assert!(!key_detection_origin_allowed(&headers));

        headers.remove(ORIGIN);
        assert!(key_detection_origin_allowed(&headers));
    }
}
