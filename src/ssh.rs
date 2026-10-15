//! SSH / SFTP 连接管理与远程文件系统操作。
//!
//! 基于纯 Rust 的 russh / russh-sftp 实现，Windows 上无需额外安装
//! OpenSSL / libssh2 等 C 依赖。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::SystemTime;

use async_trait::async_trait;
use russh::client::{self, Handle};
use russh::keys::key::PublicKey;
use russh::Disconnect;
use russh_sftp::client::SftpSession;
use russh_sftp::protocol::OpenFlags;
use tokio::sync::Mutex;
use uuid::Uuid;

/// russh 客户端事件处理器（仅校验主机密钥，开发工具不做指纹强校验）。
pub struct SshHandler;

#[async_trait]
impl client::Handler for SshHandler {
    type Error = russh::Error;

    async fn check_server_key(&mut self, _server_public_key: &PublicKey) -> Result<bool, Self::Error> {
        Ok(true)
    }
}

/// 一个已建立的远程会话（SSH 会话 + SFTP 通道）。
pub struct Conn {
    pub handle: Handle<SshHandler>,
    pub sftp: SftpSession,
}

/// 会话管理器：负责建立、查询、断开远程会话。
#[derive(Clone)]
pub struct SshManager {
    conns: Arc<Mutex<HashMap<String, Conn>>>,
    /// 上传取消标志：传输循环每块检查，置位即中止（原子，无需锁）
    pub cancel_up: Arc<AtomicBool>,
    /// 下载取消标志：传输循环每块检查，置位即中止（原子，无需锁）
    pub cancel_down: Arc<AtomicBool>,
}

impl Default for SshManager {
    fn default() -> Self {
        Self {
            conns: Arc::new(Mutex::new(HashMap::new())),
            cancel_up: Arc::new(AtomicBool::new(false)),
            cancel_down: Arc::new(AtomicBool::new(false)),
        }
    }
}

/// 请求取消当前正在进行的传输（按方向置标志，传输循环检测后中止）。
pub fn cancel_transfer(ssh: &SshManager, up: bool) {
    let f = if up { &ssh.cancel_up } else { &ssh.cancel_down };
    f.store(true, Ordering::SeqCst);
}

/// 传输结束后清除对应方向的取消标志（避免残留影响下次传输）。
pub fn reset_cancel(ssh: &SshManager, up: bool) {
    let f = if up { &ssh.cancel_up } else { &ssh.cancel_down };
    f.store(false, Ordering::SeqCst);
}

/// 目录项（文件浏览器展示用）。
#[derive(Clone)]
pub struct FileEntry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub size: Option<u64>,
    pub perms: Option<String>,
    pub mtime: Option<String>,
}

impl SshManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// 建立 SSH 连接并初始化 SFTP 通道，成功后返回会话 ID。
    pub async fn connect(
        &self,
        host: String,
        port: u16,
        user: String,
        password: Option<String>,
        key_path: Option<String>,
    ) -> Result<String, String> {
        let config = client::Config {
            // 自动保活：30s 无服务器数据则发 keepalive；3 次无响应判定连接失效
            inactivity_timeout: None,
            keepalive_interval: Some(std::time::Duration::from_secs(30)),
            keepalive_max: 3,
            ..<_>::default()
        };
        let mut session = client::connect(Arc::new(config), (host.as_str(), port), SshHandler)
            .await
            .map_err(|e| format!("无法连接到 {}:{}：{e}", host, port))?;

        let authed = if let Some(pwd) = password {
            session
                .authenticate_password(&user, pwd)
                .await
                .map_err(|e| format!("密码认证过程出错：{e}"))?
        } else if let Some(key) = key_path {
            let key_pair = russh::keys::load_secret_key(key, None)
                .map_err(|e| format!("读取私钥失败：{e}"))?;
            session
                .authenticate_publickey(&user, Arc::new(key_pair))
                .await
                .map_err(|e| format!("密钥认证过程出错：{e}"))?
        } else {
            return Err("请提供密码或私钥路径".to_string());
        };

        if !authed {
            return Err("认证失败：用户名 / 密码（或密钥）不正确".to_string());
        }

        let channel = session
            .channel_open_session()
            .await
            .map_err(|e| format!("打开会话通道失败：{e}"))?;
        channel
            .request_subsystem(true, "sftp")
            .await
            .map_err(|e| format!("请求 SFTP 子系统失败：{e}"))?;
        let sftp = SftpSession::new(channel.into_stream())
            .await
            .map_err(|e| format!("初始化 SFTP 通道失败：{e}"))?;

        let id = Uuid::new_v4().simple().to_string();
        let mut guard = self.conns.lock().await;
        guard.insert(
            id.clone(),
            Conn {
                handle: session,
                sftp,
            },
        );
        Ok(id)
    }

    /// 断开会话。
    pub async fn disconnect(&self, id: &str) -> Result<(), String> {
        let mut guard = self.conns.lock().await;
        if let Some(conn) = guard.remove(id) {
            let _ = conn
                .handle
                .disconnect(Disconnect::ByApplication, "", "en")
                .await
                .ok();
        }
        Ok(())
    }

    /// 列出目录内容。
    pub async fn list(&self, id: &str, path: &str) -> Result<Vec<FileEntry>, String> {
        let mut guard = self.conns.lock().await;
        let conn = guard.get_mut(id).ok_or("会话不存在或已断开")?;

        let canonical = conn.sftp.canonicalize(path).await.map_err(|e| format!("解析路径失败：{e}"))?;
        let mut out = Vec::new();
        let dir = conn.sftp.read_dir(&canonical).await.map_err(|e| format!("读取目录失败：{e}"))?;
        for entry in dir {
            let name = entry.file_name();
            let attr = entry.metadata();
            let full = if canonical == "/" {
                format!("/{}", name)
            } else {
                format!("{}/{}", canonical, name)
            };
            out.push(FileEntry {
                name,
                path: full,
                is_dir: attr.is_dir(),
                is_symlink: attr.is_symlink(),
                size: attr.size,
                perms: format_perms(attr.permissions),
                mtime: attr.mtime.map(fmt_unix_ts),
            });
        }
        // 目录在前、文件在后，各自按名称排序
        out.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
        Ok(out)
    }

    /// 创建目录。
    pub async fn mkdir(&self, id: &str, path: &str) -> Result<(), String> {
        let mut guard = self.conns.lock().await;
        let conn = guard.get_mut(id).ok_or("会话不存在或已断开")?;
        conn.sftp.create_dir(path).await.map_err(|e| format!("创建目录失败：{e}"))
    }

    /// 删除文件或目录（目录需为空）。
    /// 已由 [`remove_entry`] 取代（支持递归删除），保留该方法供内部复用。
    #[allow(dead_code)]
    pub async fn remove(&self, id: &str, path: &str) -> Result<(), String> {
        let mut guard = self.conns.lock().await;
        let conn = guard.get_mut(id).ok_or("会话不存在或已断开")?;
        let attr = conn.sftp.symlink_metadata(path).await.map_err(|e| format!("获取目标信息失败：{e}"))?;
        if attr.is_dir() {
            conn.sftp.remove_dir(path).await.map_err(|e| format!("删除目录失败（目录非空时请先清空）：{e}"))
        } else {
            conn.sftp.remove_file(path).await.map_err(|e| format!("删除文件失败：{e}"))
        }
    }

    /// 递归删除远端文件或目录（目录先清空子项再删除自身）。
    pub async fn remove_entry(&self, id: &str, path: &str) -> Result<(), String> {
        let mut guard = self.conns.lock().await;
        let conn = guard.get_mut(id).ok_or("会话不存在或已断开")?;
        let sftp = &mut conn.sftp;
        remove_recursive(sftp, path).await
    }

    /// 上传本地文件或目录（目录递归创建远端子目录并上传全部文件），返回传输字节数。
    /// `progress` 回调：单文件内按块上报 (已传字节, 该文件总字节)，已做节流。
    pub async fn upload_entry(
        &self,
        id: &str,
        local: &std::path::Path,
        remote_dir: &str,
        progress: &(dyn Fn(u64, u64) + Send + Sync),
    ) -> Result<u64, String> {
        let mut guard = self.conns.lock().await;
        let conn = guard.get_mut(id).ok_or("会话不存在或已断开")?;
        let sftp = &mut conn.sftp;

        let name = local
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| "无法识别文件名".to_string())?
            .to_string();
        let remote_path = if remote_dir == "/" {
            format!("/{name}")
        } else {
            format!("{remote_dir}/{name}")
        };
        upload_recursive(sftp, local, &remote_path, progress, &self.cancel_up).await
    }

    /// 下载远端文件或目录（目录递归下载全部子项到本地），返回传输字节数。
    /// `progress` 回调：单文件内按块上报 (已传字节, 该文件总字节)，已做节流。
    pub async fn download_entry(
        &self,
        id: &str,
        remote: &str,
        local: &std::path::Path,
        progress: &(dyn Fn(u64, u64) + Send + Sync),
    ) -> Result<u64, String> {
        let mut guard = self.conns.lock().await;
        let conn = guard.get_mut(id).ok_or("会话不存在或已断开")?;
        let sftp = &mut conn.sftp;

        download_recursive(sftp, remote, local, progress, &self.cancel_down).await
    }
}

/// 递归删除：文件直接删；目录先递归删子项再 rmdir。
async fn remove_recursive(sftp: &mut SftpSession, path: &str) -> Result<(), String> {
        let attr = sftp
            .symlink_metadata(path)
            .await
            .map_err(|e| format!("获取目标信息失败：{e}"))?;
        if attr.is_dir() {
            let entries = sftp
            .read_dir(path)
            .await
            .map_err(|e| format!("读取目录失败：{e}"))?;
        for entry in entries {
            let child = if path == "/" {
                format!("/{}", entry.file_name())
            } else {
                format!("{}/{}", path, entry.file_name())
            };
            Box::pin(remove_recursive(sftp, &child)).await?;
        }
        sftp
            .remove_dir(path)
            .await
            .map_err(|e| format!("删除目录失败：{e}"))?;    } else {
        sftp
            .remove_file(path)
            .await
            .map_err(|e| format!("删除文件失败：{e}"))?;
    }
    Ok(())
}

/// 递归上传：本地目录 -> 远端 create_dir 后逐项处理；本地文件 -> 流式上传。
async fn upload_recursive(
    sftp: &mut SftpSession,
    local: &std::path::Path,
    remote_path: &str,
    progress: &(dyn Fn(u64, u64) + Send + Sync),
    cancel: &AtomicBool,
) -> Result<u64, String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    if cancel.load(Ordering::SeqCst) {
        return Err("传输已取消".to_string());
    }
    let meta = tokio::fs::metadata(local)
        .await
        .map_err(|e| format!("读取本地信息失败：{e}"))?;
    if meta.is_dir() {
        // 远端目录不存在时创建（已存在则跳过）
        if sftp.symlink_metadata(remote_path).await.is_err() {
            sftp.create_dir(remote_path)
                .await
                .map_err(|e| format!("创建远端目录失败：{e}"))?;
        }
        let mut total: u64 = 0;
        let mut rd = tokio::fs::read_dir(local)
            .await
            .map_err(|e| format!("读取本地目录失败：{e}"))?;
        while let Ok(Some(de)) = rd.next_entry().await {
            // 目录级取消检查：每个子项处理前检测，置位立即中止
            if cancel.load(Ordering::SeqCst) {
                return Err("传输已取消".to_string());
            }
            let child_local = de.path();
            let child_remote = if remote_path == "/" {
                format!("/{}", de.file_name().to_string_lossy())
            } else {
                format!("{remote_path}/{}", de.file_name().to_string_lossy())
            };
            total += Box::pin(upload_recursive(sftp, &child_local, &child_remote, progress, cancel)).await?;
        }
        Ok(total)
    } else {
        let size = meta.len();
        let mut file = tokio::fs::File::open(local)
            .await
            .map_err(|e| format!("打开本地文件失败：{e}"))?;
        let mut remote_file = sftp
            .open_with_flags(
                remote_path,
                OpenFlags::CREATE | OpenFlags::TRUNCATE | OpenFlags::WRITE,
            )
            .await
            .map_err(|e| format!("创建远端文件失败：{e}"))?;
        let mut total: u64 = 0;
        let mut last_sent: u64 = 0;
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            if cancel.load(Ordering::SeqCst) {
                return Err("传输已取消".to_string());
            }
            let n = file
                .read(&mut buf)
                .await
                .map_err(|e| format!("读取本地文件失败：{e}"))?;
            if n == 0 {
                break;
            }
            remote_file
                .write_all(&buf[..n])
                .await
                .map_err(|e| format!("写入远端文件失败：{e}"))?;
            total += n as u64;
            // 节流：每 512KB 或完成时上报一次进度
            if total - last_sent >= 512 * 1024 {
                progress(total, size);
                last_sent = total;
            }
        }
        if total != last_sent {
            progress(total, size);
        }
        remote_file
            .flush()
            .await
            .map_err(|e| format!("刷新远端文件失败：{e}"))?;
        Ok(total)
    }
}

/// 递归下载：远端目录 -> 本地建目录后逐项；远端文件 -> 复制到本地。
async fn download_recursive(
    sftp: &mut SftpSession,
    remote: &str,
    local: &std::path::Path,
    progress: &(dyn Fn(u64, u64) + Send + Sync),
    cancel: &AtomicBool,
) -> Result<u64, String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    if cancel.load(Ordering::SeqCst) {
        return Err("传输已取消".to_string());
    }
    let attr = sftp
        .symlink_metadata(remote)
        .await
        .map_err(|e| format!("获取远端信息失败：{e}"))?;
    if attr.is_dir() {
        tokio::fs::create_dir_all(local)
            .await
            .map_err(|e| format!("创建本地目录失败：{e}"))?;
        let mut total: u64 = 0;
        let entries = sftp
            .read_dir(remote)
            .await
            .map_err(|e| format!("读取远端目录失败：{e}"))?;
        for entry in entries {
            // 目录级取消检查：每个子项处理前检测，置位立即中止
            if cancel.load(Ordering::SeqCst) {
                return Err("传输已取消".to_string());
            }
            let child_remote = if remote == "/" {
                format!("/{}", entry.file_name())
            } else {
                format!("{remote}/{}", entry.file_name())
            };
            let child_local = local.join(entry.file_name());
            total += Box::pin(download_recursive(sftp, &child_remote, &child_local, progress, cancel)).await?;
        }
        Ok(total)
    } else {
        let size = attr.len();
        let mut remote_file = sftp
            .open(remote)
            .await
            .map_err(|e| format!("打开远端文件失败：{e}"))?;
        let mut out = tokio::fs::File::create(local)
            .await
            .map_err(|e| format!("创建本地文件失败：{e}"))?;
        let mut total: u64 = 0;
        let mut last_sent: u64 = 0;
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            if cancel.load(Ordering::SeqCst) {
                return Err("传输已取消".to_string());
            }
            let n = remote_file
                .read(&mut buf)
                .await
                .map_err(|e| format!("读取远端文件失败：{e}"))?;
            if n == 0 {
                break;
            }
            out.write_all(&buf[..n])
                .await
                .map_err(|e| format!("写入本地文件失败：{e}"))?;
            total += n as u64;
            // 节流：每 512KB 或完成时上报一次进度
            if total - last_sent >= 512 * 1024 {
                progress(total, size);
                last_sent = total;
            }
        }
        if total != last_sent {
            progress(total, size);
        }
        out.flush()
            .await
            .map_err(|e| format!("刷新本地文件失败：{e}"))?;
        Ok(total)
    }
}

impl SshManager {

    /// 重命名 / 移动。
    pub async fn rename(&self, id: &str, old_path: &str, new_path: &str) -> Result<(), String> {
        let mut guard = self.conns.lock().await;
        let conn = guard.get_mut(id).ok_or("会话不存在或已断开")?;
        conn.sftp.rename(old_path, new_path).await.map_err(|e| format!("重命名失败：{e}"))
    }

    /// 流式写入远程文件（大文件上传）。
    /// 已由 [`upload_entry`] 取代（自动识别文件/目录），保留该方法供内部复用。
    #[allow(dead_code)]
    pub async fn upload<R: tokio::io::AsyncRead + Unpin + Send>(
        &self,
        id: &str,
        remote_path: &str,
        reader: &mut R,
    ) -> Result<u64, String> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let mut guard = self.conns.lock().await;
        let conn = guard.get_mut(id).ok_or("会话不存在或已断开")?;
        let mut file = conn
            .sftp
            .open_with_flags(
                remote_path,
                OpenFlags::CREATE | OpenFlags::TRUNCATE | OpenFlags::WRITE,
            )
            .await
            .map_err(|e| format!("创建远程文件失败：{e}"))?;

        let mut total: u64 = 0;
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = reader.read(&mut buf).await.map_err(|e| format!("读取上传数据失败：{e}"))?;
            if n == 0 {
                break;
            }
            file.write_all(&buf[..n]).await.map_err(|e| format!("写入远程文件失败：{e}"))?;
            total += n as u64;
        }
        file.flush().await.map_err(|e| format!("刷新远程文件失败：{e}"))?;
        Ok(total)
    }

    /// 打开远程交互式 shell（带 PTY），返回通道句柄。
    ///
    /// 返回的 `Channel` 提供 `make_reader` / `make_writer`（AsyncRead/AsyncWrite）
    /// 与 `signal`（发送 SIGINT 等 SSH 信号），由调用方在单任务中独占使用。
    pub async fn open_shell(
        &self,
        id: &str,
        cols: u32,
        rows: u32,
    ) -> Result<russh::Channel<russh::client::Msg>, String> {
        let mut guard = self.conns.lock().await;
        let conn = guard.get_mut(id).ok_or("会话不存在或已断开")?;

        let channel = conn
            .handle
            .channel_open_session()
            .await
            .map_err(|e| format!("打开 shell 通道失败：{e}"))?;
        channel
            .request_pty(
                true,
                "xterm-256color",
                cols,
                rows,
                0,
                0,
                // 显式开启 ISIG / ICANON / ECHO：确保 tty 将 0x03 识别为中断字符
                // （Ctrl+C 通用中止依赖此标志，避免个别环境默认关闭）
                &[
                    (russh::Pty::ISIG, 1),
                    (russh::Pty::ICANON, 1),
                    (russh::Pty::ECHO, 1),
                ],
            )
            .await
            .map_err(|e| format!("请求 PTY 失败：{e}"))?;
        channel
            .request_shell(true)
            .await
            .map_err(|e| format!("启动远程 shell 失败：{e}"))?;
        Ok(channel)
    }

    /// 连接有效性探针：打开通道执行 `true` 并等待输出结束。
    /// 成功表示连接仍有效；失败（通道打不开 / 读错误）表示连接已失效。
    pub async fn ping(&self, id: &str) -> Result<(), String> {
        use tokio::io::AsyncReadExt;

        let mut guard = self.conns.lock().await;
        let conn = guard.get_mut(id).ok_or("会话不存在或已断开")?;
        let channel = conn
            .handle
            .channel_open_session()
            .await
            .map_err(|e| format!("连接已失效：{e}"))?;
        channel
            .exec(true, "true")
            .await
            .map_err(|e| format!("连接已失效：{e}"))?;
        let mut stream = channel.into_stream();
        drop(guard);

        let mut tmp = [0u8; 1024];
        let read_result = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                match stream.read(&mut tmp).await {
                    Ok(0) => break Ok(()),
                    Ok(_) => continue,
                    Err(e) => break Err(e),
                }
            }
        })
        .await;
        match read_result {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(format!("连接已失效：{e}")),
            Err(_) => Err("连接已失效：探针超时".to_string()),
        }
    }

    /// 获取远端系统信息（通过 exec 通道执行一组只读命令）。
    ///
    /// 返回键值对列表：主机名 / 系统 / 内核 / 架构 / 运行时间 / CPU / 内存 / 硬盘。
    pub async fn sysinfo(&self, id: &str) -> Result<Vec<(String, String)>, String> {
        use tokio::io::AsyncReadExt;

        let cmd = concat!(
            "echo '@@主机名'; hostname;",
            "echo '@@系统'; ( . /etc/os-release 2>/dev/null && echo \"$PRETTY_NAME\" );",
            "echo '@@内核'; uname -r;",
            "echo '@@架构'; uname -m;",
            "echo '@@运行时间'; ( uptime -p 2>/dev/null || uptime );",
            "echo '@@CPU'; ( top -bn1 2>/dev/null | grep -E '%Cpu|Cpu\\(s\\)' | head -1 );",
            "echo '@@内存'; ( free -m 2>/dev/null || free ) | grep -iE 'Mem:|内存:' | head -1;",
            "echo '@@硬盘'; df -h 2>/dev/null | grep -E '^/dev/|overlay' | grep -vE 'tmpfs|udev|loop' | head -1",
        );

        let mut guard = self.conns.lock().await;
        let conn = guard.get_mut(id).ok_or("会话不存在或已断开")?;
        let channel = conn
            .handle
            .channel_open_session()
            .await
            .map_err(|e| format!("打开信息通道失败：{e}"))?;
        channel
            .exec(true, cmd)
            .await
            .map_err(|e| format!("执行系统信息命令失败：{e}"))?;
        let mut stream = channel.into_stream();
        drop(guard); // 数据读取不依赖会话锁

        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        loop {
            let n = stream
                .read(&mut tmp)
                .await
                .map_err(|e| format!("读取系统信息失败：{e}"))?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n]);
        }
        Ok(parse_sysinfo(&String::from_utf8_lossy(&buf)))
    }

    /// 下载远程文件到本地路径，返回传输字节数。
    /// 已由 [`download_entry`] 取代（自动识别文件/目录），保留该方法供内部复用。
    #[allow(dead_code)]
    pub async fn download(&self, id: &str, remote_path: &str, local_path: &str) -> Result<u64, String> {        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let mut guard = self.conns.lock().await;
        let conn = guard.get_mut(id).ok_or("会话不存在或已断开")?;
        let mut file = conn
            .sftp
            .open(remote_path)
            .await
            .map_err(|e| format!("打开远程文件失败：{e}"))?;
        drop(guard); // 文件句柄持有独立的 SFTP 引用，释放会话锁后再做长传输

        let mut out = tokio::fs::File::create(local_path)
            .await
            .map_err(|e| format!("创建本地文件失败：{e}"))?;
        let mut total: u64 = 0;
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = file
                .read(&mut buf)
                .await
                .map_err(|e| format!("读取远程文件失败：{e}"))?;
            if n == 0 {
                break;
            }
            out.write_all(&buf[..n])
                .await
                .map_err(|e| format!("写入本地文件失败：{e}"))?;
            total += n as u64;
        }
        out.flush().await.map_err(|e| format!("刷新本地文件失败：{e}"))?;
        Ok(total)
    }
}

/// 将权限位格式化为 `rwxr-xr-x` 风格字符串。
fn format_perms(mode: Option<u32>) -> Option<String> {
    let mode = mode?;
    let mut s = String::with_capacity(10);
    let flags = [
        (0o400, 'r'), (0o200, 'w'), (0o100, 'x'),
        (0o040, 'r'), (0o020, 'w'), (0o010, 'x'),
        (0o004, 'r'), (0o002, 'w'), (0o001, 'x'),
    ];
    for (bit, ch) in flags {
        s.push(if mode & bit != 0 { ch } else { '-' });
    }
    Some(s)
}

/// 将 Unix 时间戳格式化为本地时间字符串。
fn fmt_unix_ts(ts: u32) -> String {
    let secs = i64::from(ts);
    let dt = chrono::DateTime::<chrono::Local>::from(
        SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs.max(0) as u64),
    );
    dt.format("%Y-%m-%d %H:%M:%S").to_string()
}

/// 解析 `@@标记` 分段的系统信息命令输出为键值对列表。
/// CPU / 内存 / 硬盘 段会被格式化为可读指标；其他段原样保留。
fn parse_sysinfo(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut current: Option<String> = None;
    for line in text.lines() {
        if let Some(key) = line.strip_prefix("@@") {
            current = Some(key.trim().to_string());
            continue;
        }
        let v = line.trim();
        if v.is_empty() {
            continue;
        }
        if let Some(key) = current.take() {
            let val = match key.as_str() {
                "CPU" => fmt_cpu(v),
                "内存" => fmt_mem(v),
                "硬盘" => fmt_disk(v),
                _ => v.to_string(),
            };
            out.push((key, val));
        }
    }
    out
}

/// 从 `top -bn1` 的 `%Cpu(s): ... id, ...` 行提取空闲率 -> 使用率。
/// 兼容两种列间距：`99.9 id,` 与 `0.0 ni,100.0 id,`（`ni,` 与数值间无空格时
/// 数值会与前一字段粘在一起，不能直接取前一个 token，改为从 `id` 向前截取数字）。
fn fmt_cpu(line: &str) -> String {
    // 优先匹配 "id,"；行尾恰好是 "id"（无逗号）时匹配 " id"
    let marker = if line.contains("id,") { "id," } else { " id" };
    if let Some(pos) = line.find(marker) {
        let before = &line[..pos];
        let bytes = before.as_bytes();
        let mut end = bytes.len();
        // 先跳过末尾分隔符（空格、逗号）
        while end > 0 && (bytes[end - 1] == b' ' || bytes[end - 1] == b',') {
            end -= 1;
        }
        // 再向前收集数字与小数点
        while end > 0 && (bytes[end - 1].is_ascii_digit() || bytes[end - 1] == b'.') {
            end -= 1;
        }
        if end < bytes.len() {
            if let Ok(id) = before[end..].trim().parse::<f32>() {
                let used = (100.0 - id).max(0.0);
                return format!("使用率 {used:.1}% / 空闲 {id:.1}%");
            }
        }
    }
    line.to_string()
}

/// 将 MB 数值格式化为可读容量（>=1024 显示为 G，否则 M）。
fn fmt_mb(mb: f64) -> String {
    if mb >= 1024.0 {
        format!("{:.1}G", mb / 1024.0)
    } else {
        format!("{mb:.0}M")
    }
}

/// 从 `free -m` 的 Mem 行提取 总量/已用 -> 使用率。
/// 行格式：Mem:  total used free shared buff/cache available
fn fmt_mem(line: &str) -> String {
    let toks: Vec<&str> = line.split_whitespace().collect();
    if toks.len() >= 7 {
        let total: f64 = toks[1].parse().unwrap_or(0.0);
        let used: f64 = toks[2].parse().unwrap_or(0.0);
        if total > 0.0 {
            let pct = used / total * 100.0;
            return format!("已用 {} / 共 {}（{pct:.0}%）", fmt_mb(used), fmt_mb(total));
        }
    }
    line.to_string()
}

/// 从 `df -h` 根分区行提取 容量/已用/使用率。
/// 行格式：Filesystem Size Used Avail Use% Mounted on
fn fmt_disk(line: &str) -> String {
    let toks: Vec<&str> = line.split_whitespace().collect();
    if toks.len() >= 6 {
        let mount = toks[5];
        return format!(
            "{} 已用 {} / 共 {}（{}）",
            mount, toks[2], toks[1], toks[4]
        );
    }
    line.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sysinfo_parsing() {
        let text = "@@主机名\nmy-server\n@@系统\nDebian GNU/Linux 12 (bookworm)\n@@内核\n6.1.0-18-amd64\n@@架构\nx86_64\n@@运行时间\nup 3 days, 2 hours\n@@CPU\n%Cpu(s):  2.3 us,  1.1 sy,  0.0 ni, 96.4 id,  0.2 wa,  0.0 hi,  0.0 si,  0.0 st\n@@内存\nMem:  15503 1023 1480 27 13000 14155\n@@硬盘\n/dev/sda1        40G  8.2G   30G  22% /\n";
        let info = parse_sysinfo(text);
        assert_eq!(info.len(), 8);
        assert_eq!(info[0], ("主机名".to_string(), "my-server".to_string()));
        assert_eq!(info[1], ("系统".to_string(), "Debian GNU/Linux 12 (bookworm)".to_string()));
        assert_eq!(info[2], ("内核".to_string(), "6.1.0-18-amd64".to_string()));
        assert_eq!(info[3], ("架构".to_string(), "x86_64".to_string()));
        assert_eq!(info[4], ("运行时间".to_string(), "up 3 days, 2 hours".to_string()));
        // CPU：空闲 96.4% -> 使用率 3.6%
        assert_eq!(info[5], ("CPU".to_string(), "使用率 3.6% / 空闲 96.4%".to_string()));
        // 内存：total 15503MB used 1023MB -> 15.1G / 1023M（7%）
        assert_eq!(info[6], ("内存".to_string(), "已用 1023M / 共 15.1G（7%）".to_string()));
        // 硬盘：根分区 / 40G 已用 8.2G 22%
        assert_eq!(info[7], ("硬盘".to_string(), "/ 已用 8.2G / 共 40G（22%）".to_string()));
    }

    #[test]
    fn sysinfo_metric_fallback() {
        // CPU/内存/硬盘命令输出异常时回退为原始行，不 panic
        let text = "@@CPU\n(no data)\n@@内存\nMem:  not-a-number\n@@硬盘\n";
        let info = parse_sysinfo(text);
        assert_eq!(info.len(), 2);
        assert_eq!(info[0], ("CPU".to_string(), "(no data)".to_string()));
        assert_eq!(info[1], ("内存".to_string(), "Mem:  not-a-number".to_string()));
    }

    #[test]
    fn sysinfo_cpu_no_space_before_id() {
        // 部分发行版 top 输出 `0.0 ni,100.0 id,`（ni 与数值间无空格），
        // 空闲值会与前一字段粘连，仍应正确解析
        let text = "@@CPU\n%Cpu(s):  0.0 us,  0.0 sy,  0.0 ni,100.0 id,  0.0 wa,  0.0 hi,  0.0 si,  0.0 st\n";
        let info = parse_sysinfo(text);
        assert_eq!(info.len(), 1);
        assert_eq!(info[0], ("CPU".to_string(), "使用率 0.0% / 空闲 100.0%".to_string()));
    }

    #[test]
    fn sysinfo_missing_fields() {
        // 某字段命令失败（无输出）时跳过，不影响其余字段
        let text = "@@主机名\ns\n@@系统\n\n@@内核\n5.15.0\n@@架构\n@@运行时间\nup 1 hour\n";
        let info = parse_sysinfo(text);
        assert_eq!(info.len(), 3);
        assert_eq!(info[0].0, "主机名");
        assert_eq!(info[1], ("内核".to_string(), "5.15.0".to_string()));
        assert_eq!(info[2].0, "运行时间");
    }
}
