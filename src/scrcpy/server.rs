use crate::adb::AdbClient;
use crate::error::{Result, ScrcpyError};
use crate::scrcpy::video::CodecInfo;
use crate::utils::find_available_port;
use std::path::PathBuf;
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use std::process::Stdio;
use tracing::{debug, info, warn};

const DEVICE_SERVER_PATH: &str = "/data/local/tmp/scrcpy-server.jar";
const SOCKET_NAME: &str = "scrcpy";

/// scrcpy 3.3.4 的 codec_meta JSON 格式
#[derive(Debug, serde::Deserialize)]
struct CodecMeta {
    codec: String,
    width: u32,
    height: u32,
    #[serde(rename = "csd-0")]
    csd_0: Option<String>,  // SPS (base64)
    #[serde(rename = "csd-1")]
    csd_1: Option<String>,  // PPS (base64)
}


pub struct ScrcpyServer {
    adb: AdbClient,
    device_id: String,
    server_path: PathBuf,
    video_port: u16,
    actual_video_port: u16,    // 实际使用的视频端口
    control_port: u16,
    actual_control_port: u16,  // 实际使用的控制端口
    max_size: u32,
    bit_rate: u32,
    max_fps: u32,
    intra_refresh_period: u32,  // 强制IDR帧间隔（秒）
    server_process: Option<Child>,
}

impl ScrcpyServer {
    pub fn new(adb: AdbClient, device_id: String, server_path: PathBuf) -> Result<Self> {
        // 自动寻找可用端口
        let actual_video_port = find_available_port(27183, 100)?;
        let actual_control_port = find_available_port(actual_video_port + 1, 100)?;

        Ok(Self {
            adb,
            device_id,
            server_path,
            video_port: 27183,
            actual_video_port,
            control_port: 27184,
            actual_control_port,
            max_size: 1920,       // 最大分辨率
            bit_rate: 16_000_000, // 16Mbps - 提高码率改善画质
            max_fps: 60,
            intra_refresh_period: 1,  // 每1秒强制一个IDR帧
            server_process: None,
        })
    }

    /// 创建带自定义配置的服务器（自动寻找可用端口）
    pub fn with_config(
        adb: AdbClient,
        device_id: String,
        server_path: PathBuf,
        max_size: u32,
        bit_rate: u32,
        max_fps: u32,
        video_port: u16,
        control_port: u16,
        intra_refresh_period: u32,
    ) -> Result<Self> {
        // 自动寻找可用端口
        let actual_video_port = find_available_port(video_port, 100)?;
        // 控制端口从视频端口+1开始搜索，避免冲突
        let actual_control_port = find_available_port(
            if control_port <= actual_video_port { actual_video_port + 1 } else { control_port },
            100
        )?;

        Ok(Self {
            adb,
            device_id,
            server_path,
            video_port,
            actual_video_port,
            control_port,
            actual_control_port,
            max_size,
            bit_rate,
            max_fps,
            intra_refresh_period,
            server_process: None,
        })
    }

    /// 获取实际使用的视频端口
    pub fn get_actual_video_port(&self) -> u16 {
        self.actual_video_port
    }

    /// 获取实际使用的控制端口
    pub fn get_actual_control_port(&self) -> u16 {
        self.actual_control_port
    }

    /// 部署服务器到设备
    pub async fn deploy(&self) -> Result<()> {
        info!("📦 Deploying scrcpy-server to device...");

        // 检查本地服务器文件是否存在
        if !self.server_path.exists() {
            return Err(ScrcpyError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("Server file not found: {:?}", self.server_path),
            )));
        }

        // 推送服务器到设备
        let local_path = self.server_path.to_str().ok_or_else(|| {
            ScrcpyError::Parse("Invalid server path".to_string())
        })?;

        info!("  Pushing {} to device...", local_path);
        self.adb
            .push(&self.device_id, local_path, DEVICE_SERVER_PATH)
            .await?;

        info!("✅ Server deployed successfully");
        Ok(())
    }

    /// 启动scrcpy-server
    pub async fn start(&mut self) -> Result<()> {
        info!("🚀 Starting scrcpy-server...");
        info!("   Video port: {} (requested: {})", self.actual_video_port, self.video_port);
        info!("   Control port: {} (requested: {})", self.actual_control_port, self.control_port);

        // 设置端口转发 - 视频socket
        info!("  Setting up video port forwarding: localabstract:{}", SOCKET_NAME);
        self.adb
            .forward(
                &self.device_id,
                self.actual_video_port,
                &format!("localabstract:{}", SOCKET_NAME),
            )
            .await?;

        // 设置端口转发 - 控制socket (使用同一个 abstract socket，scrcpy 会区分连接)
        info!("  Setting up control port forwarding: localabstract:{}", SOCKET_NAME);
        self.adb
            .forward(
                &self.device_id,
                self.actual_control_port,
                &format!("localabstract:{}", SOCKET_NAME),
            )
            .await?;

        // 启动server的命令
        // scrcpy 3.x 必须明确指定参数来启用视频流
        // 使用 video_codec_options=i-frame-interval 来控制IDR帧间隔
        // i-frame-interval 单位是秒

        info!("  IDR frame interval: {}s", self.intra_refresh_period);

        // scrcpy v3.3.4 参数 (按照 SUMMARY.md 的工作配置)
        let server_args = format!(
            "CLASSPATH={} app_process / com.genymobile.scrcpy.Server 3.3.4 \
             log_level=info \
             max_size={} \
             video_bit_rate={} \
             max_fps={} \
             video_codec_options=i-frame-interval={} \
             tunnel_forward=true \
             send_device_meta=false \
             send_frame_meta=false \
             send_dummy_byte=true \
             send_codec_meta=false \
             raw_stream=true \
             audio=false \
             control=true \
             cleanup=true",
            DEVICE_SERVER_PATH,
            self.max_size,
            self.bit_rate,
            self.max_fps,
            self.intra_refresh_period
        );

        info!("  Executing: shell {}", server_args);

        // 使用ADB启动server（异步进程）
        // 注意：可能需要stdin来传递配置
        let adb_path = self.adb.adb_path.clone();
        let device_id = self.device_id.clone();

        let mut child = Command::new(&adb_path)
            .args(&["-s", &device_id, "shell", &server_args])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| ScrcpyError::Adb(format!("Failed to start server: {}", e)))?;

        // 先获取 stderr 用于后台监控
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                use tokio::io::{AsyncBufReadExt, BufReader};
                let mut reader = BufReader::new(stderr);
                let mut line = String::new();
                while let Ok(n) = reader.read_line(&mut line).await {
                    if n == 0 { break; }
                    warn!("  Server stderr: {}", line.trim());
                    line.clear();
                }
            });
        }

        // 读取server的stdout，等待它准备好
        let mut server_started = false;
        if let Some(stdout) = child.stdout.take() {
            use tokio::io::{AsyncBufReadExt, BufReader};
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();

            // 尝试读取第一行输出，确认服务器已启动
            tokio::select! {
                result = reader.read_line(&mut line) => {
                    match result {
                        Ok(n) if n > 0 => {
                            info!("  Server output: {}", line.trim());
                            server_started = true;
                        }
                        Ok(_) => {
                            warn!("  Server produced no output");
                        }
                        Err(e) => {
                            warn!("  Failed to read server output: {}", e);
                        }
                    }
                }
                _ = tokio::time::sleep(tokio::time::Duration::from_secs(3)) => {
                    warn!("  Timeout waiting for server output (might still be starting)");
                }
            }
        } else {
            warn!("  Could not capture server stdout");
        }

        self.server_process = Some(child);

        // 等待服务器启动 - 增加等待时间确保服务器完全就绪
        info!("  Waiting for server to initialize...");
        tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;

        info!("✅ Server started on port {}", self.actual_video_port);
        Ok(())
    }

    /// 连接到scrcpy-server的视频流
    pub async fn connect_video(&self) -> Result<TcpStream> {
        info!("🔌 Connecting to video stream...");

        let addr = format!("127.0.0.1:{}", self.actual_video_port);

        // 尝试连接，带重试机制
        let mut stream = None;
        for attempt in 1..=5 {
            info!("  Connection attempt {}/5...", attempt);
            match TcpStream::connect(&addr).await {
                Ok(s) => {
                    stream = Some(s);
                    break;
                }
                Err(e) if attempt < 5 => {
                    info!("  Connection failed: {}, retrying...", e);
                    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                }
                Err(e) => {
                    return Err(ScrcpyError::Network(format!("Failed to connect after 5 attempts: {}", e)));
                }
            }
        }

        let stream = stream.unwrap();

        // raw_stream=true + control=false 模式：
        // 不需要发送任何 marker，直接连接即可
        // 服务器会发送 dummy byte，然后是 NAL 流

        info!("✅ Connected to video stream");

        Ok(stream)
    }

    /// 连接到scrcpy-server的控制流
    /// 控制流使用独立的端口 (control_port)，通过 adb forward 映射到同一个 abstract socket
    pub async fn connect_control(&self) -> Result<TcpStream> {
        info!("🎮 Connecting to control stream...");

        // 使用实际的控制端口
        let addr = format!("127.0.0.1:{}", self.actual_control_port);

        // 连接到控制流
        let stream = TcpStream::connect(&addr).await
            .map_err(|e| ScrcpyError::Network(format!("Failed to connect control: {}", e)))?;

        info!("✅ Connected to control stream on port {}", self.actual_control_port);
        Ok(stream)
    }

    /// 从已连接的video stream读取scrcpy协议头
    pub async fn read_video_header(stream: &mut TcpStream) -> Result<CodecInfo> {
        info!("📖 Reading scrcpy protocol header...");

        // scrcpy 3.3.4 + raw_stream=true 模式：
        // 只有一个 dummy byte (0x00)，然后直接是 Annex-B NAL 流

        // 读取 dummy byte (1 byte)
        let mut dummy_byte = [0u8; 1];
        stream.read_exact(&mut dummy_byte).await
            .map_err(|e| ScrcpyError::Network(format!("Failed to read dummy byte: {}", e)))?;
        info!("  Dummy byte: 0x{:02x}", dummy_byte[0]);

        info!("✅ Protocol header read successfully");
        info!("ℹ️  SPS/PPS will be extracted from raw NAL stream");

        // 返回默认的 CodecInfo，SPS/PPS 将从视频流中提取
        Ok(CodecInfo {
            codec_id: 0,  // raw_stream 模式没有 codec_id
            width: 0,     // 将从 SPS 中解析
            height: 0,    // 将从 SPS 中解析
            config_data: None,  // SPS/PPS 将从 NAL 流中提取
        })
    }

    /// 停止服务器
    pub async fn stop(&mut self) -> Result<()> {
        info!("🛑 Stopping scrcpy-server...");

        // 杀死server进程
        if let Some(mut child) = self.server_process.take() {
            let _ = child.kill().await;
        }

        // 移除端口转发（使用实际端口）
        let _ = self.adb.forward_remove(&self.device_id, self.actual_video_port).await;
        let _ = self.adb.forward_remove(&self.device_id, self.actual_control_port).await;

        info!("✅ Server stopped");
        Ok(())
    }
}

impl Drop for ScrcpyServer {
    fn drop(&mut self) {
        if let Some(mut child) = self.server_process.take() {
            let _ = child.start_kill();
        }
    }
}
