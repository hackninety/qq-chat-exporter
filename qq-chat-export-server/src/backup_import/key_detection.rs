//! Automatic NTQQ database-key detection.
//!
//! The Windows scanner is deliberately read-only: it opens QQ processes with
//! query/read permissions, looks for SQLCipher's length-prefixed `HMAC_SHA1`
//! marker, and returns nearby 16-byte candidates. A candidate is never trusted
//! until the user-selected database sample accepts it through SQLCipher.
//! The memory-layout strategy is documented by `QQBackup/x_key_scanner`; this
//! implementation was independently authored because that repository has no
//! license granting source-code reuse.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde::Serialize;

use super::{sqlcipher_key_valid, NTQQ_HEADER_MAGIC, NTQQ_HEADER_SIZE};

const SQLITE_MAGIC: &[u8; 16] = b"SQLite format 3\0";
const MAX_LOGICAL_DATABASE_SIZE: u64 = 8 * 1024 * 1024 * 1024;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyDetection {
    pub required: bool,
    pub detected: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<&'static str>,
}

impl KeyDetection {
    fn plaintext() -> Self {
        Self {
            required: false,
            detected: false,
            key: None,
            source: None,
        }
    }

    fn memory(key: String) -> Self {
        Self {
            required: true,
            detected: true,
            key: Some(key),
            source: Some("qq_memory"),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum KeyDetectionError {
    #[error("上传的密钥检测样本无效")]
    InvalidSample,
    #[error("传统 PCQQ 加密 .bak 不使用 NTQQ 数据库密钥，无法自动检测")]
    LegacyBak,
    #[error("自动检测 NTQQ 数据库密钥目前仅支持 Windows")]
    UnsupportedPlatform,
    #[error("未发现正在运行的 QQ，请先启动并登录包含该数据库的 QQ 账号")]
    QqNotRunning,
    #[error("QQ 尚未加载聊天数据库，请确认账号已登录并进入过消息界面")]
    QqNotReady,
    #[error("无法只读访问 QQ 进程；请尝试以管理员身份运行 QCE")]
    AccessDenied,
    #[error("已扫描 QQ 进程，但没有找到数据库密钥候选")]
    NoCandidate,
    #[error("找到的密钥候选均不能打开所选数据库；请确认数据库属于当前登录账号")]
    NoMatchingKey,
    #[error("密钥检测超时，请保持 QQ 登录后重试")]
    Timeout,
    #[error("本机密钥扫描器运行失败")]
    ScannerFailed,
    #[error("密钥检测文件操作失败: {0}")]
    Io(#[from] std::io::Error),
}

impl KeyDetectionError {
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidSample => "BACKUP_KEY_SAMPLE_INVALID",
            Self::LegacyBak => "LEGACY_BAK_ENCRYPTED",
            Self::UnsupportedPlatform => "BACKUP_KEY_DETECTION_UNSUPPORTED",
            Self::QqNotRunning => "QQ_NOT_RUNNING",
            Self::QqNotReady => "QQ_DATABASE_NOT_READY",
            Self::AccessDenied => "QQ_PROCESS_ACCESS_DENIED",
            Self::NoCandidate => "QQ_KEY_NOT_FOUND",
            Self::NoMatchingKey => "QQ_KEY_NOT_MATCHED",
            Self::Timeout => "QQ_KEY_SCAN_TIMEOUT",
            Self::ScannerFailed | Self::Io(_) => "QQ_KEY_SCAN_FAILED",
        }
    }
}

struct SampleInspection {
    plaintext: bool,
    wrapped: bool,
    legacy_bak: bool,
    sample_size: u64,
}

fn inspect_sample(
    path: &Path,
    display_name: Option<&str>,
) -> Result<SampleInspection, KeyDetectionError> {
    let metadata = std::fs::metadata(path).map_err(|_| KeyDetectionError::InvalidSample)?;
    if !metadata.is_file() {
        return Err(KeyDetectionError::InvalidSample);
    }
    let mut header = [0_u8; NTQQ_HEADER_SIZE as usize];
    let header_len = File::open(path)?.read(&mut header)?;
    if header_len < SQLITE_MAGIC.len() {
        return Err(KeyDetectionError::InvalidSample);
    }
    let wrapped = header_len >= 40 && &header[32..40] == NTQQ_HEADER_MAGIC;
    let plaintext = !wrapped && &header[..SQLITE_MAGIC.len()] == SQLITE_MAGIC;
    let extension = display_name
        .and_then(|name| Path::new(name).extension())
        .and_then(|extension| extension.to_str())
        .unwrap_or_default();
    let legacy_bak = extension.eq_ignore_ascii_case("bak")
        && header_len >= 32
        && header[..16].iter().all(|byte| *byte == 0)
        && !wrapped;
    Ok(SampleInspection {
        plaintext,
        wrapped,
        legacy_bak,
        sample_size: metadata.len(),
    })
}

pub(super) fn copy_cipher_sample(
    source: &Path,
    destination: &Path,
    skip: u64,
    logical_source_size: u64,
) -> std::io::Result<()> {
    let mut input = File::open(source)?;
    input.seek(SeekFrom::Start(skip))?;
    let mut output = File::create(destination)?;
    std::io::copy(&mut input, &mut output)?;
    output.set_len(logical_source_size.saturating_sub(skip))?;
    output.flush()?;
    Ok(())
}

pub(super) async fn detect_key(
    sample: PathBuf,
    display_name: Option<String>,
    original_size: Option<u64>,
) -> Result<KeyDetection, KeyDetectionError> {
    let inspection = inspect_sample(&sample, display_name.as_deref())?;
    if inspection.legacy_bak {
        return Err(KeyDetectionError::LegacyBak);
    }
    if inspection.plaintext {
        return Ok(KeyDetection::plaintext());
    }

    let logical_source_size = original_size.unwrap_or(inspection.sample_size);
    if logical_source_size < inspection.sample_size
        || logical_source_size > MAX_LOGICAL_DATABASE_SIZE
        || (inspection.wrapped && logical_source_size <= NTQQ_HEADER_SIZE)
    {
        return Err(KeyDetectionError::InvalidSample);
    }

    let cipher_sample = sample.with_file_name(format!(
        ".key-scan-{}.sqlite",
        uuid::Uuid::new_v4().simple()
    ));
    copy_cipher_sample(
        &sample,
        &cipher_sample,
        if inspection.wrapped {
            NTQQ_HEADER_SIZE
        } else {
            0
        },
        logical_source_size,
    )?;

    let result = async {
        let candidates = scan_candidates(false).await?;
        let validation_path = cipher_sample.clone();
        tokio::task::spawn_blocking(move || {
            find_matching_key(&validation_path, candidates)
                .map(KeyDetection::memory)
                .ok_or(KeyDetectionError::NoMatchingKey)
        })
        .await
        .map_err(|_| KeyDetectionError::ScannerFailed)?
    }
    .await;
    let _ = tokio::fs::remove_file(&cipher_sample).await;
    result
}

pub(super) fn find_matching_key(path: &Path, candidates: Vec<String>) -> Option<String> {
    candidates
        .into_iter()
        .find(|candidate| sqlcipher_key_valid(path, candidate))
}

#[cfg(windows)]
async fn scan_candidates(self_test: bool) -> Result<Vec<String>, KeyDetectionError> {
    use std::process::Stdio;
    use std::time::Duration;

    use tokio::io::AsyncWriteExt;
    use tokio::process::Command;

    const SCRIPT: &str = include_str!("windows_key_scanner.ps1");

    let script_path = std::env::temp_dir().join(format!(
        "qce-key-scan-{}.ps1",
        uuid::Uuid::new_v4().simple()
    ));
    let mut script_file = tokio::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&script_path)
        .await
        .map_err(|_| KeyDetectionError::ScannerFailed)?;
    let write_result = async {
        script_file
            .write_all(SCRIPT.as_bytes())
            .await
            .map_err(|_| KeyDetectionError::ScannerFailed)?;
        script_file
            .flush()
            .await
            .map_err(|_| KeyDetectionError::ScannerFailed)
    }
    .await;
    drop(script_file);
    if let Err(error) = write_result {
        let _ = tokio::fs::remove_file(&script_path).await;
        return Err(error);
    }

    let result = async {
        let mut command = Command::new("powershell.exe");
        command
            .args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
            ])
            .arg(&script_path)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        if self_test {
            command.env("QCE_KEY_SCANNER_SELF_TEST", "1");
        } else {
            command.env_remove("QCE_KEY_SCANNER_SELF_TEST");
        }
        let child = command
            .spawn()
            .map_err(|_| KeyDetectionError::ScannerFailed)?;
        let output = tokio::time::timeout(Duration::from_secs(45), child.wait_with_output())
            .await
            .map_err(|_| KeyDetectionError::Timeout)?
            .map_err(|_| KeyDetectionError::ScannerFailed)?;
        parse_scanner_output(&output.stdout, output.status.success())
    }
    .await;
    let _ = tokio::fs::remove_file(&script_path).await;
    result
}

#[cfg(not(windows))]
async fn scan_candidates(_self_test: bool) -> Result<Vec<String>, KeyDetectionError> {
    Err(KeyDetectionError::UnsupportedPlatform)
}

fn parse_scanner_output(
    stdout: &[u8],
    process_succeeded: bool,
) -> Result<Vec<String>, KeyDetectionError> {
    let output = String::from_utf8_lossy(stdout);
    let mut status = None;
    let mut candidates = Vec::new();
    for line in output.lines().map(str::trim) {
        if let Some(value) = line.strip_prefix("QCE_STATUS:") {
            status = Some(value);
        } else if let Some(value) = line.strip_prefix("QCE_KEY:") {
            if let Some(candidate) = decode_candidate(value) {
                candidates.push(candidate);
            }
        }
    }
    if !process_succeeded || status == Some("ERROR") {
        return Err(KeyDetectionError::ScannerFailed);
    }
    match status {
        Some("OK") if !candidates.is_empty() => Ok(candidates),
        Some("NO_QQ") => Err(KeyDetectionError::QqNotRunning),
        Some("QQ_NOT_READY") => Err(KeyDetectionError::QqNotReady),
        Some("ACCESS_DENIED") => Err(KeyDetectionError::AccessDenied),
        Some("NO_CANDIDATE" | "OK") => Err(KeyDetectionError::NoCandidate),
        _ => Err(KeyDetectionError::ScannerFailed),
    }
}

fn decode_candidate(value: &str) -> Option<String> {
    if value.len() != 32 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let mut bytes = [0_u8; 16];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16).ok()?;
    }
    if !bytes.iter().all(|byte| (0x21..0x7f).contains(byte)) {
        return None;
    }
    String::from_utf8(bytes.to_vec()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_only_tagged_printable_candidates() {
        let output =
            b"noise\nQCE_STATUS:OK\nQCE_KEY:516365546573744b657921323334353f\nQCE_KEY:not-a-key\n";
        let candidates = parse_scanner_output(output, true).expect("valid scanner output");
        assert_eq!(candidates, vec!["QceTestKey!2345?"]);
    }

    #[test]
    fn maps_scanner_status_without_echoing_output() {
        let error = parse_scanner_output(b"QCE_STATUS:ACCESS_DENIED\n", true)
            .expect_err("access denial must fail");
        assert!(matches!(error, KeyDetectionError::AccessDenied));
        assert_eq!(error.code(), "QQ_PROCESS_ACCESS_DENIED");
    }

    #[tokio::test]
    async fn plaintext_sample_does_not_require_process_scanning() {
        let sample_path = std::env::temp_dir().join(format!(
            "qce-plaintext-key-test-{}.db",
            uuid::Uuid::new_v4().simple()
        ));
        File::create(&sample_path)
            .expect("temp sample")
            .write_all(b"SQLite format 3\0plaintext-test")
            .expect("write plaintext header");
        let result = detect_key(sample_path.clone(), Some("export.db".to_string()), None).await;
        let _ = std::fs::remove_file(sample_path);
        let detection = result.expect("plaintext detection");
        assert!(!detection.required);
        assert!(!detection.detected);
        assert!(detection.key.is_none());
    }

    #[test]
    fn wrapped_ntqq_header_is_not_misclassified_as_plaintext() {
        let sample_path = std::env::temp_dir().join(format!(
            "qce-wrapped-key-test-{}.db",
            uuid::Uuid::new_v4().simple()
        ));
        let mut header = vec![0_u8; NTQQ_HEADER_SIZE as usize];
        header[..SQLITE_MAGIC.len()].copy_from_slice(SQLITE_MAGIC);
        header[32..40].copy_from_slice(NTQQ_HEADER_MAGIC);
        std::fs::write(&sample_path, header).expect("write wrapped NTQQ header");

        let inspection =
            inspect_sample(&sample_path, Some("nt_msg.db")).expect("inspect wrapped NTQQ sample");
        let _ = std::fs::remove_file(sample_path);

        assert!(inspection.wrapped);
        assert!(!inspection.plaintext);
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn embedded_windows_scanner_self_test_finds_candidate() {
        let candidates = scan_candidates(true).await.expect("scanner self test");
        assert!(candidates
            .iter()
            .any(|candidate| candidate == "QceTestKey!2345?"));
    }
}
