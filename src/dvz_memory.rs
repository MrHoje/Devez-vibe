use std::{
    fs,
    path::Path,
    process::Command,
    sync::{
        OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};

use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use tokio::time::sleep;

const CLIENT_ID: &str = "Ov23li7IloRevc3IsEz5";
const CREDENTIAL_TARGET: &str = "DevezVibe/GitHub/MemoryHub";
const MEMORY_REPOSITORY: &str = "dvz-memory-hub";
const GITHUB_ACCEPT: &str = "application/vnd.github+json";
const GENERATED_HEADER: &str = "<!-- DevezVibe가 자동 생성하는 파일입니다. 직접 작성한 지식은 .knowledge 루트에 보관하세요. -->";

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct DvzMemoryAccount {
    pub login: String,
    pub user_id: u64,
    pub repository: String,
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    access_expires_at: Option<u64>,
    #[serde(default)]
    refresh_expires_at: Option<u64>,
}

#[cfg(test)]
impl DvzMemoryAccount {
    pub fn fixture(login: &str) -> Self {
        Self {
            login: login.to_owned(),
            user_id: 1,
            repository: format!("{login}/{MEMORY_REPOSITORY}"),
            access_token: "test-token".to_owned(),
            refresh_token: None,
            access_expires_at: None,
            refresh_expires_at: None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct DeviceAuthorization {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub expires_in: u64,
    pub interval: u64,
}

#[derive(Deserialize)]
struct DeviceAuthorizationResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    expires_in: u64,
    interval: u64,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
    interval: Option<u64>,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
    refresh_token_expires_in: Option<u64>,
}

#[derive(Deserialize)]
struct GithubUser {
    login: String,
    id: u64,
}

#[derive(Deserialize)]
struct RepositoryInfo {
    private: bool,
}

#[derive(Deserialize)]
struct RepositoryContent {
    content: String,
    sha: String,
}

pub fn account() -> Option<DvzMemoryAccount> {
    stored_account().ok().flatten()
}

fn stored_account() -> Result<Option<DvzMemoryAccount>> {
    let Some(mut secret) = read_secret()? else {
        return Ok(None);
    };
    let parsed = serde_json::from_slice(&secret).context("Memory Hub credential 형식 오류");
    secret.fill(0);
    parsed.map(Some)
}

pub fn logout() -> Result<()> {
    delete_secret()
}

pub fn activate_account(cwd: &Path, account: &DvzMemoryAccount) -> Result<()> {
    clear_local_project(cwd)?;
    write_secret(&serde_json::to_vec(account)?)
}

pub fn clear_local_project(cwd: &Path) -> Result<()> {
    let root = crate::project_memory::project_root(cwd);
    for name in ["MEMORY.md", "SUMMARY.md"] {
        let path = crate::project_memory::auto_memory_dir(&root).join(name);
        let Some(contents) = read_local_document(&path)? else {
            continue;
        };
        if contents.starts_with(GENERATED_HEADER) {
            fs::remove_file(path)?;
        }
    }
    Ok(())
}

pub async fn begin_login() -> Result<DeviceAuthorization> {
    let response = github_client()?
        .post("https://github.com/login/device/code")
        .header("Accept", "application/json")
        .form(&[("client_id", CLIENT_ID), ("scope", "repo")])
        .send()
        .await?
        .error_for_status()?
        .json::<DeviceAuthorizationResponse>()
        .await?;
    Ok(DeviceAuthorization {
        device_code: response.device_code,
        user_code: response.user_code,
        verification_uri: response.verification_uri,
        expires_in: response.expires_in,
        interval: response.interval.max(1),
    })
}

pub async fn complete_login(
    auth: DeviceAuthorization,
    cancelled: &AtomicBool,
) -> Result<DvzMemoryAccount> {
    let client = github_client()?;
    let deadline = Instant::now() + Duration::from_secs(auth.expires_in);
    let mut interval = auth.interval;
    let token = loop {
        if cancelled.load(Ordering::Relaxed) {
            bail!("GitHub login이 취소되었습니다.");
        }
        if Instant::now() >= deadline {
            bail!("GitHub login code가 만료되었습니다.");
        }
        sleep(Duration::from_secs(interval)).await;
        if cancelled.load(Ordering::Relaxed) {
            bail!("GitHub login이 취소되었습니다.");
        }
        let response = client
            .post("https://github.com/login/oauth/access_token")
            .header("Accept", "application/json")
            .form(&[
                ("client_id", CLIENT_ID),
                ("device_code", auth.device_code.as_str()),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ])
            .send()
            .await?
            .error_for_status()?
            .json::<TokenResponse>()
            .await?;
        if response.access_token.is_some() {
            break response;
        }
        match response.error.as_deref() {
            Some("authorization_pending") => {}
            Some("slow_down") => interval = response.interval.unwrap_or(interval + 5),
            Some("access_denied") => bail!("GitHub login이 취소되었습니다."),
            Some("expired_token") => bail!("GitHub login code가 만료되었습니다."),
            Some(error) => bail!(
                "GitHub login 실패: {}",
                response.error_description.as_deref().unwrap_or(error)
            ),
            None => bail!("GitHub login 응답에 access token이 없습니다."),
        }
    };
    let access_token = token
        .access_token
        .context("GitHub login 응답에 access token이 없습니다.")?;
    let user = github_get(&client, &access_token, "https://api.github.com/user")
        .await?
        .error_for_status()?
        .json::<GithubUser>()
        .await?;
    ensure_memory_repository(&client, &access_token, &user.login).await?;
    let account = DvzMemoryAccount {
        repository: format!("{}/{}", user.login, MEMORY_REPOSITORY),
        login: user.login,
        user_id: user.id,
        access_token,
        refresh_token: token.refresh_token,
        access_expires_at: token
            .expires_in
            .map(|seconds| unix_now().saturating_add(seconds)),
        refresh_expires_at: token
            .refresh_token_expires_in
            .map(|seconds| unix_now().saturating_add(seconds)),
    };
    Ok(account)
}

pub async fn download_project(cwd: &Path) -> Result<bool> {
    let Some(account) = authenticated_account().await? else {
        return Ok(false);
    };
    let project =
        github_project(cwd).context("현재 프로젝트의 GitHub origin을 찾을 수 없습니다.")?;
    let client = github_client()?;
    let mut changed = false;
    for name in ["MEMORY.md", "SUMMARY.md"] {
        let Some(contents) = read_remote_document(&client, &account, &project, name).await? else {
            continue;
        };
        if contents.len() > 64 * 1024 {
            bail!("원격 {name} 파일이 64KiB를 초과했습니다.");
        }
        changed |= write_local_document(cwd, name, &contents)?;
    }
    Ok(changed)
}

pub async fn upload_project(cwd: &Path) -> Result<bool> {
    let Some(account) = authenticated_account().await? else {
        return Ok(false);
    };
    let project =
        github_project(cwd).context("현재 프로젝트의 GitHub origin을 찾을 수 없습니다.")?;
    let client = github_client()?;
    let mut changed = false;
    for name in ["MEMORY.md", "SUMMARY.md"] {
        let root = crate::project_memory::project_root(cwd);
        let path = crate::project_memory::auto_memory_dir(&root).join(name);
        let Some(contents) = read_local_document(&path)? else {
            continue;
        };
        changed |= write_remote_document(&client, &account, &project, name, &contents).await?;
    }
    Ok(changed)
}

async fn ensure_memory_repository(client: &Client, token: &str, owner: &str) -> Result<()> {
    let url = format!("https://api.github.com/repos/{owner}/{MEMORY_REPOSITORY}");
    let response = github_get(client, token, &url).await?;
    if response.status().is_success() {
        if !response.json::<RepositoryInfo>().await?.private {
            bail!("기존 {owner}/{MEMORY_REPOSITORY} repository가 private이 아닙니다.");
        }
        return Ok(());
    }
    if response.status() != StatusCode::NOT_FOUND {
        bail!(
            "Memory Hub repository 조회 실패: {}",
            github_error(response).await
        );
    }
    let response = client
        .post("https://api.github.com/user/repos")
        .header("Accept", GITHUB_ACCEPT)
        .bearer_auth(token)
        .json(&serde_json::json!({
            "name": MEMORY_REPOSITORY,
            "description": "Private project memory shared by DevezVibe providers",
            "private": true,
            "auto_init": true
        }))
        .send()
        .await?;
    if !response.status().is_success() {
        bail!(
            "Memory Hub repository 생성 실패: {}",
            github_error(response).await
        );
    }
    Ok(())
}

async fn read_remote_document(
    client: &Client,
    account: &DvzMemoryAccount,
    project: &str,
    name: &str,
) -> Result<Option<String>> {
    let response = github_get(
        client,
        &account.access_token,
        &contents_url(account, project, name),
    )
    .await?;
    if response.status() == StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !response.status().is_success() {
        bail!("Memory Hub 다운로드 실패: {}", github_error(response).await);
    }
    let remote = response.json::<RepositoryContent>().await?;
    let encoded = remote
        .content
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .collect::<String>();
    let bytes = STANDARD
        .decode(encoded)
        .context("원격 메모리 Base64 해석 실패")?;
    Ok(Some(
        String::from_utf8(bytes).context("원격 메모리가 UTF-8이 아닙니다.")?,
    ))
}

async fn write_remote_document(
    client: &Client,
    account: &DvzMemoryAccount,
    project: &str,
    name: &str,
    contents: &str,
) -> Result<bool> {
    let url = contents_url(account, project, name);
    let response = github_get(client, &account.access_token, &url).await?;
    let existing = if response.status() == StatusCode::NOT_FOUND {
        None
    } else if response.status().is_success() {
        Some(response.json::<RepositoryContent>().await?)
    } else {
        bail!(
            "Memory Hub 상태 조회 실패: {}",
            github_error(response).await
        );
    };
    if let Some(remote) = &existing {
        let encoded = remote
            .content
            .chars()
            .filter(|ch| !ch.is_whitespace())
            .collect::<String>();
        if STANDARD.decode(encoded).ok().as_deref() == Some(contents.as_bytes()) {
            return Ok(false);
        }
    }
    let mut body = serde_json::json!({
        "message": format!("Update {project} {name}"),
        "content": STANDARD.encode(contents)
    });
    if let Some(remote) = existing {
        body["sha"] = serde_json::json!(remote.sha);
    }
    let response = client
        .put(url)
        .header("Accept", GITHUB_ACCEPT)
        .bearer_auth(&account.access_token)
        .json(&body)
        .send()
        .await?;
    if !response.status().is_success() {
        bail!("Memory Hub 업로드 실패: {}", github_error(response).await);
    }
    Ok(true)
}

fn contents_url(account: &DvzMemoryAccount, project: &str, name: &str) -> String {
    format!(
        "https://api.github.com/repos/{}/{}/contents/projects/{project}/{name}",
        account.login, MEMORY_REPOSITORY
    )
}

async fn github_get(client: &Client, token: &str, url: &str) -> Result<reqwest::Response> {
    Ok(client
        .get(url)
        .header("Accept", GITHUB_ACCEPT)
        .bearer_auth(token)
        .send()
        .await?)
}

async fn github_error(response: reqwest::Response) -> String {
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|value| {
            value
                .get("message")
                .and_then(|value| value.as_str())
                .map(str::to_owned)
        })
        .map(|message| format!("HTTP {status}: {message}"))
        .unwrap_or_else(|| format!("HTTP {status}"))
}

fn github_client() -> Result<Client> {
    Client::builder()
        .user_agent(format!("devez-vibe/{}", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(30))
        .build()
        .context("GitHub client 생성 실패")
}

async fn authenticated_account() -> Result<Option<DvzMemoryAccount>> {
    static REFRESH_LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    let _guard = REFRESH_LOCK
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await;
    let Some(mut account) = stored_account()? else {
        return Ok(None);
    };
    let expiring = account
        .access_expires_at
        .is_some_and(|expires| expires <= unix_now().saturating_add(60));
    if !expiring {
        return Ok(Some(account));
    }
    let refresh_token = account
        .refresh_token
        .as_deref()
        .context("GitHub session이 만료되었습니다. /memory-hub에서 Login을 다시 실행하세요.")?;
    if account
        .refresh_expires_at
        .is_some_and(|expires| expires <= unix_now())
    {
        bail!("GitHub refresh token이 만료되었습니다. /memory-hub에서 Login을 다시 실행하세요.");
    }
    let token = github_client()?
        .post("https://github.com/login/oauth/access_token")
        .header("Accept", "application/json")
        .form(&[
            ("client_id", CLIENT_ID),
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
        ])
        .send()
        .await?
        .error_for_status()?
        .json::<TokenResponse>()
        .await?;
    account.access_token = token
        .access_token
        .context("GitHub token refresh 응답에 access token이 없습니다.")?;
    account.refresh_token = token.refresh_token.or(account.refresh_token);
    account.access_expires_at = token
        .expires_in
        .map(|seconds| unix_now().saturating_add(seconds));
    account.refresh_expires_at = token
        .refresh_token_expires_in
        .map(|seconds| unix_now().saturating_add(seconds))
        .or(account.refresh_expires_at);
    write_secret(&serde_json::to_vec(&account)?)?;
    Ok(Some(account))
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn github_project(cwd: &Path) -> Option<String> {
    let root = crate::project_memory::project_root(cwd);
    let output = Command::new("git")
        .args(["-C", root.to_str()?, "remote", "get-url", "origin"])
        .output()
        .ok()?;
    output.status.success().then_some(())?;
    github_project_from_remote(std::str::from_utf8(&output.stdout).ok()?.trim())
}

fn github_project_from_remote(remote: &str) -> Option<String> {
    let path = remote
        .strip_prefix("git@github.com:")
        .or_else(|| remote.strip_prefix("ssh://git@github.com/"))
        .or_else(|| remote.strip_prefix("https://github.com/"))
        .or_else(|| remote.strip_prefix("http://github.com/"))?;
    let path = path
        .trim_end_matches('/')
        .strip_suffix(".git")
        .unwrap_or(path);
    let mut parts = path.split('/');
    let owner = parts.next()?;
    let repository = parts.next()?;
    if parts.next().is_some() || !safe_slug(owner) || !safe_slug(repository) {
        return None;
    }
    Some(format!("{owner}/{repository}").to_ascii_lowercase())
}

fn safe_slug(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn read_local_document(path: &Path) -> Result<Option<String>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        bail!(
            "메모리 파일이 안전한 일반 파일이 아닙니다: {}",
            path.display()
        );
    }
    let text = fs::read_to_string(path)?;
    if text.len() > 64 * 1024 {
        bail!("메모리 파일이 64KiB를 초과했습니다: {}", path.display());
    }
    Ok(Some(text))
}

fn write_local_document(cwd: &Path, name: &str, contents: &str) -> Result<bool> {
    if !contents.starts_with(GENERATED_HEADER) {
        bail!("원격 {name} 파일이 Memory Hub 형식이 아닙니다.");
    }
    let root = crate::project_memory::project_root(cwd);
    let directory = crate::project_memory::auto_memory_dir(&root);
    fs::create_dir_all(&directory)?;
    let path = directory.join(name);
    if read_local_document(&path)?.as_deref() == Some(contents) {
        return Ok(false);
    }
    if path.exists() && fs::symlink_metadata(&path)?.file_type().is_symlink() {
        bail!("메모리 파일이 심볼릭 링크여서 덮어쓰지 않았습니다.");
    }
    let temporary = path.with_extension("memory-hub.tmp");
    fs::write(&temporary, contents)?;
    if path.exists() {
        fs::remove_file(&path)?;
    }
    fs::rename(temporary, path)?;
    Ok(true)
}

#[cfg(windows)]
fn write_secret(secret: &[u8]) -> Result<()> {
    use windows_sys::Win32::Security::Credentials::{
        CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC, CREDENTIALW, CredWriteW,
    };
    if secret.len() > 2560 {
        bail!("Memory Hub credential이 Windows 제한을 초과했습니다.");
    }
    let mut target = wide(CREDENTIAL_TARGET);
    let mut username = wide("Memory Hub");
    let mut blob = secret.to_vec();
    let credential = CREDENTIALW {
        Type: CRED_TYPE_GENERIC,
        TargetName: target.as_mut_ptr(),
        CredentialBlobSize: blob.len() as u32,
        CredentialBlob: blob.as_mut_ptr(),
        Persist: CRED_PERSIST_LOCAL_MACHINE,
        UserName: username.as_mut_ptr(),
        ..Default::default()
    };
    if unsafe { CredWriteW(&credential, 0) } == 0 {
        return Err(std::io::Error::last_os_error()).context("Memory Hub credential 저장 실패");
    }
    blob.fill(0);
    Ok(())
}

#[cfg(windows)]
fn read_secret() -> Result<Option<Vec<u8>>> {
    use windows_sys::Win32::Security::Credentials::{
        CRED_TYPE_GENERIC, CREDENTIALW, CredFree, CredReadW,
    };
    let target = wide(CREDENTIAL_TARGET);
    let mut pointer = std::ptr::null_mut::<CREDENTIALW>();
    if unsafe { CredReadW(target.as_ptr(), CRED_TYPE_GENERIC, 0, &mut pointer) } == 0 {
        let error = std::io::Error::last_os_error();
        return if error.raw_os_error() == Some(1168) {
            Ok(None)
        } else {
            Err(error).context("Memory Hub credential 읽기 실패")
        };
    }
    let credential = unsafe { &*pointer };
    let secret = if credential.CredentialBlob.is_null() || credential.CredentialBlobSize == 0 {
        Vec::new()
    } else {
        unsafe {
            std::slice::from_raw_parts(
                credential.CredentialBlob,
                credential.CredentialBlobSize as usize,
            )
        }
        .to_vec()
    };
    unsafe { CredFree(pointer.cast()) };
    Ok(Some(secret))
}

#[cfg(windows)]
fn delete_secret() -> Result<()> {
    use windows_sys::Win32::Security::Credentials::{CRED_TYPE_GENERIC, CredDeleteW};
    let target = wide(CREDENTIAL_TARGET);
    if unsafe { CredDeleteW(target.as_ptr(), CRED_TYPE_GENERIC, 0) } == 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(1168) {
            return Err(error).context("Memory Hub credential 삭제 실패");
        }
    }
    Ok(())
}

#[cfg(windows)]
fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(not(windows))]
fn write_secret(_: &[u8]) -> Result<()> {
    bail!("Memory Hub secure credential storage는 현재 Windows만 지원합니다.")
}

#[cfg(not(windows))]
fn read_secret() -> Result<Option<Vec<u8>>> {
    Ok(None)
}

#[cfg(not(windows))]
fn delete_secret() -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_remotes_map_to_one_project_key() {
        for remote in [
            "git@github.com:openai/codex.git",
            "ssh://git@github.com/openai/codex.git",
            "https://github.com/openai/codex.git",
        ] {
            assert_eq!(
                github_project_from_remote(remote).as_deref(),
                Some("openai/codex")
            );
        }
    }

    #[test]
    fn non_github_and_nested_remotes_are_rejected() {
        assert!(github_project_from_remote("https://gitlab.com/openai/codex.git").is_none());
        assert!(github_project_from_remote("https://github.com/openai/codex/extra").is_none());
    }

    #[tokio::test]
    #[ignore = "creates a real GitHub authorization and private repository"]
    async fn live_login_creates_and_syncs_the_private_repository() {
        let account = match authenticated_account()
            .await
            .expect("read Memory Hub account")
        {
            Some(account) => account,
            None => {
                let authorization = begin_login().await.expect("begin GitHub login");
                println!(
                    "Open {} and enter {}",
                    authorization.verification_uri, authorization.user_code
                );
                let cancelled = AtomicBool::new(false);
                complete_login(authorization, &cancelled)
                    .await
                    .expect("complete GitHub login")
            }
        };
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        activate_account(root, &account).expect("store Memory Hub account");
        download_project(root)
            .await
            .expect("download project memory");
        upload_project(root).await.expect("upload project memory");
        let client = github_client().expect("GitHub client");
        let user_response = github_get(
            &client,
            &account.access_token,
            "https://api.github.com/user",
        )
        .await
        .expect("read OAuth scopes");
        assert!(
            user_response
                .headers()
                .get("x-oauth-scopes")
                .and_then(|value| value.to_str().ok())
                .is_some_and(|scopes| scopes.split(',').any(|scope| scope.trim() == "repo"))
        );
        let project = "memory-hub-live-test";
        let name = "ROUNDTRIP.md";
        let contents = format!("{GENERATED_HEADER}\n\nMemory Hub live sync test.\n");
        write_remote_document(&client, &account, project, name, &contents)
            .await
            .expect("upload live test document");
        assert_eq!(
            read_remote_document(&client, &account, project, name)
                .await
                .expect("download live test document")
                .as_deref(),
            Some(contents.as_str())
        );
        let url = contents_url(&account, project, name);
        let remote = github_get(&client, &account.access_token, &url)
            .await
            .expect("read live test sha")
            .json::<RepositoryContent>()
            .await
            .expect("parse live test sha");
        let deleted = client
            .delete(url)
            .header("Accept", GITHUB_ACCEPT)
            .bearer_auth(&account.access_token)
            .json(&serde_json::json!({
                "message": "Remove Memory Hub live sync test",
                "sha": remote.sha
            }))
            .send()
            .await
            .expect("delete live test document");
        assert!(deleted.status().is_success());
        assert_eq!(
            account.repository,
            format!("{}/dvz-memory-hub", account.login)
        );
        println!("Connected as @{}", account.login);
    }
}
