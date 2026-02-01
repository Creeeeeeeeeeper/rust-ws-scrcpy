mod adb;
mod error;
mod scrcpy;
mod utils;
mod ws;

use adb::AdbClient;
use error::{Result, ScrcpyError};
use scrcpy::{ScrcpyServer, VideoStreamReader, ControlChannel};
use ws::WebSocketServer;
use std::path::PathBuf;
use tracing::{info, error, warn, debug, Level};
use tracing_subscriber;
use bytes::Bytes;
use clap::Parser;

/// Rust-scrcpy: Android screen mirroring over ADB with WebSocket broadcasting
///
/// Rust-scrcpy: 通过 ADB 实现 Android 屏幕镜像，并通过 WebSocket 广播到浏览器
#[derive(Parser, Debug)]
#[command(name = "Rust-ws-scrcpy")]
#[command(author = "zzzzyg")]
#[command(version = "2.2.0")]
#[command(about = "Stream Android device screen to web browsers via WebSocket", long_about = None)]
#[command(help_template = "{name} {version}\nAuthor: {author}\n\n{about}\n\n{usage-heading} {usage}\n\n{all-args}")]
struct Args {
    /// ADB executable path
    ///
    /// ADB 可执行文件路径
    #[arg(short, long, default_value = "../adb/adb.exe")]
    adb_path: PathBuf,

    /// scrcpy-server JAR file path
    ///
    /// scrcpy-server JAR 文件路径
    #[arg(short, long, default_value = "../scrcpy-server/scrcpy-server-v3.3.4")]
    server_path: PathBuf,

    /// List all connected devices and exit
    ///
    /// 列出所有已连接的设备并退出
    #[arg(long)]
    list: bool,

    /// Target device index or serial number (use first device if not specified)
    /// Use --list to see device indices
    ///
    /// 目标设备索引或序列号（不指定则使用第一个设备），使用 --list 查看设备索引
    #[arg(short, long)]
    device: Option<String>,

    /// Maximum video resolution (width or height, whichever is larger)
    ///
    /// 最大视频分辨率（宽或高的最大值）
    #[arg(short = 'm', long, default_value = "1920")]
    max_size: u32,

    /// Video bitrate in bits per second
    ///
    /// 视频比特率（每秒比特数）
    #[arg(short = 'b', long, default_value = "4000000")]
    bit_rate: u32,

    /// Maximum frames per second
    ///
    /// 最大帧率（每秒帧数）
    #[arg(short = 'f', long, default_value = "60")]
    max_fps: u32,

    /// WebSocket server port
    ///
    /// WebSocket 服务器端口
    #[arg(short = 'p', long, default_value = "8080")]
    ws_port: u16,

    /// Video port for scrcpy server
    ///
    /// scrcpy 服务器视频端口
    #[arg(long, default_value = "27183")]
    video_port: u16,

    /// Control port for scrcpy server
    ///
    /// scrcpy 服务器控制端口
    #[arg(long, default_value = "27184")]
    control_port: u16,

    /// Intra-refresh period in seconds (IDR frame interval)
    ///
    /// 帧内刷新周期（秒）- IDR 关键帧间隔
    #[arg(short = 'i', long, default_value = "1")]
    intra_refresh_period: u32,

    /// Video encoder name (e.g., OMX.google.h264.encoder, c2.android.avc.encoder)
    /// Use 'adb shell dumpsys media.codec' to list available encoders
    ///
    /// 视频编码器名称，留空则自动选择
    #[arg(short = 'e', long)]
    video_encoder: Option<String>,

    /// Log level (trace, debug, info, warn, error)
    ///
    /// 日志级别 (trace, debug, info, warn, error)
    #[arg(short = 'l', long, default_value = "info")]
    log_level: String,

    /// Listen on all network interfaces (0.0.0.0) for LAN access
    ///
    /// 监听所有网络接口（0.0.0.0），允许局域网访问
    #[arg(long)]
    public: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    // 解析命令行参数（这会自动处理 --help 和 --version）
    let args = Args::parse();

    // 根据参数设置日志级别
    let log_level = match args.log_level.to_lowercase().as_str() {
        "trace" => Level::TRACE,
        "debug" => Level::DEBUG,
        "info" => Level::INFO,
        "warn" => Level::WARN,
        "error" => Level::ERROR,
        _ => {
            eprintln!("⚠️  Invalid log level '{}', using 'info'", args.log_level);
            Level::INFO
        },
    };

    // 初始化日志
    tracing_subscriber::fmt()
        .with_max_level(log_level)
        .init();

    info!("🚀 Rust-Scrcpy starting...");
    info!("📋 Configuration:");
    info!("   ADB path: {:?}", args.adb_path);
    info!("   Server path: {:?}", args.server_path);
    if let Some(ref device) = args.device {
        info!("   Target device: {}", device);
    }
    info!("   Max size: {}p", args.max_size);
    info!("   Bitrate: {} Mbps", args.bit_rate / 1_000_000);
    info!("   Max FPS: {}", args.max_fps);
    info!("   WebSocket port: {}", args.ws_port);
    info!("   Video port: {}", args.video_port);
    info!("   Control port: {}", args.control_port);
    info!("   IDR interval: {}s", args.intra_refresh_period);
    info!("   Log level: {}", args.log_level);
    info!("   Public mode: {}", if args.public { "Yes (LAN accessible)" } else { "No (localhost only)" });

    // 获取ADB路径
    if !args.adb_path.exists() {
        eprintln!("❌ ADB not found at: {:?}", args.adb_path);
        eprintln!("Please specify correct ADB path with --adb-path option");
        return Ok(());
    }

    let adb = AdbClient::new(args.adb_path);

    // 列出已连接的设备
    info!("📱 Checking connected devices...");
    let devices = adb.list_devices().await?;

    if devices.is_empty() {
        eprintln!("❌ No devices connected");
        eprintln!("Please connect an Android device via USB or WiFi");
        return Ok(());
    }

    // 如果指定了 --list，列出设备并退出
    if args.list {
        println!("📱 Connected devices:");
        for (index, device) in devices.iter().enumerate() {
            println!("  [{}] {}", index, device);
        }
        println!("\nUse -d <index> or -d <serial> to select a device");
        return Ok(());
    }

    info!("✅ Found {} device(s):", devices.len());
    for (index, device) in devices.iter().enumerate() {
        info!("  [{}] {}", index, device);
    }

    // 选择设备（支持索引或序列号）
    let device_id = if let Some(device_arg) = args.device {
        // 尝试解析为索引
        if let Ok(index) = device_arg.parse::<usize>() {
            if index < devices.len() {
                devices[index].clone()
            } else {
                eprintln!("❌ Device index {} out of range (0-{})", index, devices.len() - 1);
                return Ok(());
            }
        } else {
            // 作为序列号处理
            if !devices.contains(&device_arg) {
                eprintln!("❌ Device '{}' not found in connected devices", device_arg);
                eprintln!("Use --list to see available devices");
                return Ok(());
            }
            device_arg
        }
    } else {
        devices[0].clone()
    };
    info!("🎯 Using device: {}", device_id);

    // 获取设备信息
    let model = adb.shell(&device_id, "getprop ro.product.model").await?;
    let android_version = adb.shell(&device_id, "getprop ro.build.version.release").await?;

    // 获取设备物理屏幕尺寸（用于触控坐标）
    let wm_size_output = adb.shell(&device_id, "wm size").await?;
    let (device_width, device_height) = parse_wm_size(&wm_size_output)?;

    info!("📱 Device Info:");
    info!("  Model: {}", model.trim());
    info!("  Android: {}", android_version.trim());
    info!("  Physical Screen: {}x{}", device_width, device_height);

    // 部署和启动scrcpy-server
    if !args.server_path.exists() {
        eprintln!("❌ scrcpy-server not found at: {:?}", args.server_path);
        eprintln!("Please specify correct server path with --server-path option");
        return Ok(());
    }

    let mut server = ScrcpyServer::with_config(
        adb,
        device_id,
        args.server_path,
        args.max_size,
        args.bit_rate,
        args.max_fps,
        args.video_port,
        args.control_port,
        args.intra_refresh_period,
        args.video_encoder,
    )?;

    // 部署服务器
    if let Err(e) = server.deploy().await {
        error!("Failed to deploy server: {}", e);
        return Err(e);
    }

    // 启动服务器
    if let Err(e) = server.start().await {
        error!("Failed to start server: {}", e);
        return Err(e);
    }

    // 连接到视频流
    let mut video_stream = match server.connect_video().await {
        Ok(stream) => stream,
        Err(e) => {
            error!("Failed to connect to video stream: {}", e);
            return Err(e);
        }
    };

    // 当 control=true 时，scrcpy server 需要两个连接都建立后才会发送数据
    // 所以必须先连接控制流，再读取 video header
    info!("🎮 Connecting to control stream...");
    let control_stream = match server.connect_control().await {
        Ok(stream) => {
            info!("✅ Control stream connected");
            stream
        }
        Err(e) => {
            warn!("Failed to connect control stream: {}, continuing without control", e);
            return Err(e);
        }
    };
    let mut control_channel = ControlChannel::new(control_stream);

    // 两个连接都建立后，现在可以读取 video header 了
    let codec_info = scrcpy::ScrcpyServer::read_video_header(&mut video_stream).await?;

    info!("🎥 Video stream ready!");
    info!("   Resolution will be parsed from SPS in NAL stream");

    // 创建视频流读取器
    let mut reader = VideoStreamReader::new(video_stream);

    // 创建 IDR 请求通道
    let (idr_request_tx, mut idr_request_rx) = tokio::sync::mpsc::channel::<()>(10);

    // 创建控制事件通道
    let (control_tx, mut control_rx) = tokio::sync::mpsc::channel::<scrcpy::control::ControlEvent>(100);

    // 创建 WebSocket 服务器（自动寻找可用端口）
    let ws_server = WebSocketServer::new(args.ws_port, idr_request_tx, control_tx, device_width, device_height, args.public)?;
    let actual_ws_port = ws_server.get_actual_port();
    let frame_sender = ws_server.get_sender();
    let config_sender = ws_server.get_config_sender();
    let video_config = ws_server.get_video_config();

    // 显示实际使用的端口信息
    if actual_ws_port != args.ws_port {
        info!("📌 WebSocket port {} was occupied, using port {} instead", args.ws_port, actual_ws_port);
    }

    // raw_stream 模式：SPS/PPS 将在视频帧循环中从 NAL 流提取并缓存

    // 在后台启动 WebSocket 服务器
    tokio::spawn(async move {
        if let Err(e) = ws_server.start().await {
            error!("WebSocket server error: {}", e);
        }
    });

    info!("📺 Starting to receive and broadcast video frames...");
    info!("   Press Ctrl+C to stop");

    let mut keyframe_count = 0;
    let mut config_frame_count = 0;
    let mut frame_counter = 0;
    let mut sps_cached = false;
    let mut pps_cached = false;
    let mut pending_idr_request = false;
    let mut last_idr_cache: Option<Bytes> = None; // 本地缓存 IDR，减少锁竞争

    // 持续接收并广播视频帧
    loop {
        tokio::select! {
            // 处理控制事件
            Some(control_event) = control_rx.recv() => {
                debug!("🎮 Received control event: {:?}", control_event);
                let result = match control_event {
                    scrcpy::control::ControlEvent::Touch(touch) => {
                        control_channel.send_touch_event(&touch).await
                    }
                    scrcpy::control::ControlEvent::Key(key) => {
                        control_channel.send_key_event(&key).await
                    }
                    scrcpy::control::ControlEvent::Text(text) => {
                        control_channel.send_text(&text.text).await
                    }
                    scrcpy::control::ControlEvent::Clipboard(clip) => {
                        control_channel.set_clipboard(&clip.text, clip.paste).await
                    }
                    scrcpy::control::ControlEvent::Scroll(scroll) => {
                        control_channel.send_scroll_event(
                            scroll.x, scroll.y,
                            scroll.width, scroll.height,
                            scroll.hscroll, scroll.vscroll
                        ).await
                    }
                };
                if let Err(e) = result {
                    error!("Failed to send control event to device: {}", e);
                } else {
                    debug!("✅ Control event sent successfully");
                }
            }

            // 处理IDR请求
            Some(_) = idr_request_rx.recv() => {
                debug!("🎬 Received IDR request from new client");
                pending_idr_request = true;

                // 立即重新发送缓存的SPS/PPS和IDR
                if sps_cached {
                    // 获取当前缓存的SPS/PPS并重新广播
                    let config = video_config.read().await;
                    if let Some(sps) = &config.sps {
                        let _ = frame_sender.send(sps.clone());
                    }
                    if let Some(pps) = &config.pps {
                        let _ = frame_sender.send(pps.clone());
                    }
                    drop(config);
                }
                // 使用本地缓存的 IDR 避免锁竞争
                if let Some(ref idr) = last_idr_cache {
                    let _ = frame_sender.send(idr.clone());
                }
            }

            // 处理视频帧
            frame_result = tokio::time::timeout(
                tokio::time::Duration::from_secs(10),
                reader.read_frame(false)
            ) => {
                match frame_result {
                    Ok(Ok(Some(frame))) => {
                        if frame.is_keyframe() {
                            keyframe_count += 1;

                            // 如果收到IDR帧并且有pending请求，清除标志
                            let nal_type = frame.data[0] & 0x1F;
                            if nal_type == 5 && pending_idr_request {
                                debug!("✅ Got requested IDR frame");
                                pending_idr_request = false;
                            }
                        }

                        if frame.frame_type == scrcpy::FrameType::Config {
                            config_frame_count += 1;

                            // 缓存 SPS/PPS
                            let nal_type = frame.data[0] & 0x1F;
                            if nal_type == 7 {
                                // SPS - 从中解析分辨率
                                let mut nal_with_start_code = vec![0x00, 0x00, 0x00, 0x01];
                                nal_with_start_code.extend_from_slice(&frame.data);

                                let mut config = video_config.write().await;
                                config.sps = Some(Bytes::from(nal_with_start_code.clone()));

                                // 解析 SPS 获取分辨率，检测横竖屏变化
                                let mut should_broadcast = false;
                                match parse_sps_resolution(&frame.data) {
                                    Some((width, height)) => {
                                        debug!("SPS parsed: {}x{}, current config: {}x{}",
                                            width, height, config.width, config.height);
                                        let new_is_landscape = width > height;
                                        let resolution_changed = config.width != width || config.height != height;
                                        let orientation_changed = config.is_landscape != new_is_landscape;

                                        if resolution_changed || orientation_changed {
                                            config.width = width;
                                            config.height = height;
                                            config.is_landscape = new_is_landscape;
                                            should_broadcast = true;
                                            info!("🔄 Resolution changed: {}x{}, Landscape: {}", width, height, new_is_landscape);
                                        } else {
                                            debug!("SPS resolution matches current config, no change needed");
                                        }
                                    }
                                    None => {
                                        // SPS 解析失败，使用设备物理分辨率作为后备
                                        warn!("⚠️ Failed to parse SPS resolution, using device resolution as fallback");
                                        if config.width == 0 || config.height == 0 {
                                            config.width = config.device_width;
                                            config.height = config.device_height;
                                            config.is_landscape = config.device_width > config.device_height;
                                            should_broadcast = true;
                                            info!("🔄 Using device resolution: {}x{}, Landscape: {}",
                                                config.width, config.height, config.is_landscape);
                                        }
                                    }
                                }

                                // 如果分辨率/方向变化，广播配置更新给所有客户端
                                if should_broadcast {
                                    let config_msg = format!(
                                        "{{\"type\":\"config\",\"width\":{},\"height\":{},\"device_width\":{},\"device_height\":{},\"is_landscape\":{}}}",
                                        config.width, config.height, config.device_width, config.device_height, config.is_landscape
                                    );
                                    let _ = config_sender.send(config_msg);
                                }

                                drop(config);

                                if !sps_cached {
                                    info!("✅ SPS cached ({} bytes)", nal_with_start_code.len());
                                    sps_cached = true;
                                }

                            } else if nal_type == 8 && !pps_cached {
                                // PPS
                                let mut nal_with_start_code = vec![0x00, 0x00, 0x00, 0x01];
                                nal_with_start_code.extend_from_slice(&frame.data);

                                let mut config = video_config.write().await;
                                config.pps = Some(Bytes::from(nal_with_start_code.clone()));
                                drop(config);

                                info!("✅ PPS cached ({} bytes)", nal_with_start_code.len());
                                pps_cached = true;
                            }
                        }

                        // 构建完整的 NAL 单元（包含起始码）
                        let mut nal_with_start_code = vec![0x00, 0x00, 0x00, 0x01];
                        nal_with_start_code.extend_from_slice(&frame.data);
                        let nal_bytes = Bytes::from(nal_with_start_code);

                        // 缓存 IDR 帧，用于新客户端连接时立即发送
                        let nal_type = frame.data[0] & 0x1F;
                        if nal_type == 5 {
                            // 先更新本地缓存
                            last_idr_cache = Some(nal_bytes.clone());
                            // 异步更新共享缓存（不阻塞主循环）
                            let video_config_clone = video_config.clone();
                            let idr_clone = nal_bytes.clone();
                            tokio::spawn(async move {
                                let mut config = video_config_clone.write().await;
                                config.last_idr = Some(idr_clone);
                            });
                        }

                        // 广播给所有连接的 WebSocket 客户端
                        let _ = frame_sender.send(nal_bytes);

                        frame_counter += 1;

                        // 注释掉帧统计日志以提升性能
                        // if frame_counter % 60 == 0 {
                        //     info!(
                        //         "  Frames: {}, Keyframes: {}, Config: {}, Subscribers: {}",
                        //         reader.frame_count(),
                        //         keyframe_count,
                        //         config_frame_count,
                        //         frame_sender.receiver_count()
                        //     );
                        // }
                    }
                    Ok(Ok(None)) => {
                        warn!("Stream ended, waiting for reconnect...");
                        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                        continue;
                    }
                    Ok(Err(e)) => {
                        error!("Error reading frame: {}, retrying...", e);
                        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                        continue;
                    }
                    Err(_) => {
                        warn!("Timeout waiting for frame, continuing...");
                        continue;
                    }
                }
            }
        }
    }

    // 停止服务器
    server.stop().await?;

    info!("👋 Shutting down...");
    Ok(())
}

// 解析 wm size 输出获取屏幕尺寸
// 输出格式: "Physical size: 1440x2960"
fn parse_wm_size(output: &str) -> Result<(u32, u32)> {
    let trimmed = output.trim();

    // 查找 "Physical size: " 后面的部分
    if let Some(size_part) = trimmed.strip_prefix("Physical size: ") {
        // 分割 "1440x2960"
        let parts: Vec<&str> = size_part.split('x').collect();
        if parts.len() == 2 {
            let width = parts[0].trim().parse::<u32>()
                .map_err(|_| ScrcpyError::Parse("Invalid width".to_string()))?;
            let height = parts[1].trim().parse::<u32>()
                .map_err(|_| ScrcpyError::Parse("Invalid height".to_string()))?;
            return Ok((width, height));
        }
    }

    Err(ScrcpyError::Parse(format!("Failed to parse wm size output: {}", trimmed)))
}

/// H.264 SPS 解析器 - 用于提取视频分辨率
/// SPS 使用 Exp-Golomb 编码，需要按位读取
struct BitReader<'a> {
    data: &'a [u8],
    byte_offset: usize,
    bit_offset: u8,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, byte_offset: 0, bit_offset: 0 }
    }

    fn read_bit(&mut self) -> Option<u8> {
        if self.byte_offset >= self.data.len() {
            return None;
        }
        let bit = (self.data[self.byte_offset] >> (7 - self.bit_offset)) & 1;
        self.bit_offset += 1;
        if self.bit_offset == 8 {
            self.bit_offset = 0;
            self.byte_offset += 1;
        }
        Some(bit)
    }

    fn read_bits(&mut self, n: u8) -> Option<u32> {
        let mut result = 0u32;
        for _ in 0..n {
            result = (result << 1) | self.read_bit()? as u32;
        }
        Some(result)
    }

    /// 读取 Exp-Golomb 编码的无符号整数 (ue(v))
    fn read_ue(&mut self) -> Option<u32> {
        let mut leading_zeros = 0u8;
        while self.read_bit()? == 0 {
            leading_zeros += 1;
            if leading_zeros > 31 {
                return None;
            }
        }
        if leading_zeros == 0 {
            return Some(0);
        }
        let suffix = self.read_bits(leading_zeros)?;
        Some((1 << leading_zeros) - 1 + suffix)
    }

    /// 读取 Exp-Golomb 编码的有符号整数 (se(v))
    fn read_se(&mut self) -> Option<i32> {
        let ue = self.read_ue()?;
        let value = ((ue + 1) / 2) as i32;
        if ue % 2 == 0 {
            Some(-value)
        } else {
            Some(value)
        }
    }
}

/// 移除 H.264 NAL 单元中的防竞争字节 (Emulation Prevention Bytes)
/// H.264 规范中，为了防止在 NAL 数据中出现起始码 0x000001，
/// 编码器会在 0x000000、0x000001、0x000002、0x000003 前插入 0x03
/// 解析时需要移除这些 0x03 字节
fn remove_emulation_prevention_bytes(data: &[u8]) -> Vec<u8> {
    let mut result = Vec::with_capacity(data.len());
    let mut i = 0;

    while i < data.len() {
        // 检查是否是 0x00 0x00 0x03 序列
        if i + 2 < data.len() && data[i] == 0x00 && data[i + 1] == 0x00 && data[i + 2] == 0x03 {
            // 保留前两个 0x00，跳过 0x03
            result.push(0x00);
            result.push(0x00);
            i += 3; // 跳过 0x00 0x00 0x03
        } else {
            result.push(data[i]);
            i += 1;
        }
    }

    result
}

// 解析 H.264 SPS 获取分辨率
fn parse_sps_resolution(sps_data: &[u8]) -> Option<(u32, u32)> {
    if sps_data.len() < 4 {
        warn!("SPS too short: {} bytes", sps_data.len());
        return None;
    }

    // 移除防竞争字节后再解析
    let clean_data = remove_emulation_prevention_bytes(sps_data);

    // 打印 SPS 原始数据（前20字节）用于调试
    let preview_len = std::cmp::min(sps_data.len(), 20);
    debug!("SPS raw data ({} bytes): {:02x?}", sps_data.len(), &sps_data[..preview_len]);
    if clean_data.len() != sps_data.len() {
        debug!("SPS after removing EPB ({} bytes): {:02x?}", clean_data.len(), &clean_data[..std::cmp::min(clean_data.len(), 20)]);
    }

    let mut reader = BitReader::new(&clean_data);

    // NAL header (1 byte): forbidden_zero_bit(1) + nal_ref_idc(2) + nal_unit_type(5)
    let nal_header = reader.read_bits(8)?;
    let nal_type = nal_header & 0x1F;
    debug!("SPS NAL header: 0x{:02x}, type={}", nal_header, nal_type);

    // profile_idc (8 bits)
    let profile_idc = reader.read_bits(8)?;
    debug!("SPS profile_idc: {}", profile_idc);

    // constraint flags (8 bits)
    reader.read_bits(8)?;

    // level_idc (8 bits)
    reader.read_bits(8)?;

    // seq_parameter_set_id (ue(v))
    reader.read_ue()?;

    // 对于 High Profile 等，需要读取额外参数
    if profile_idc == 100 || profile_idc == 110 || profile_idc == 122 ||
       profile_idc == 244 || profile_idc == 44 || profile_idc == 83 ||
       profile_idc == 86 || profile_idc == 118 || profile_idc == 128 ||
       profile_idc == 138 || profile_idc == 139 || profile_idc == 134 ||
       profile_idc == 135 {
        // chroma_format_idc
        let chroma_format_idc = reader.read_ue()?;
        if chroma_format_idc == 3 {
            // separate_colour_plane_flag
            reader.read_bits(1)?;
        }
        // bit_depth_luma_minus8
        reader.read_ue()?;
        // bit_depth_chroma_minus8
        reader.read_ue()?;
        // qpprime_y_zero_transform_bypass_flag
        reader.read_bits(1)?;
        // seq_scaling_matrix_present_flag
        let scaling_matrix_present = reader.read_bits(1)?;
        if scaling_matrix_present == 1 {
            let count = if chroma_format_idc != 3 { 8 } else { 12 };
            for i in 0..count {
                let seq_scaling_list_present = reader.read_bits(1)?;
                if seq_scaling_list_present == 1 {
                    let size = if i < 6 { 16 } else { 64 };
                    let mut last_scale = 8i32;
                    let mut next_scale = 8i32;
                    for _ in 0..size {
                        if next_scale != 0 {
                            let delta_scale = reader.read_se()?;
                            next_scale = (last_scale + delta_scale + 256) % 256;
                        }
                        last_scale = if next_scale == 0 { last_scale } else { next_scale };
                    }
                }
            }
        }
    }

    // log2_max_frame_num_minus4
    reader.read_ue()?;

    // pic_order_cnt_type
    let pic_order_cnt_type = reader.read_ue()?;
    if pic_order_cnt_type == 0 {
        // log2_max_pic_order_cnt_lsb_minus4
        reader.read_ue()?;
    } else if pic_order_cnt_type == 1 {
        // delta_pic_order_always_zero_flag
        reader.read_bits(1)?;
        // offset_for_non_ref_pic
        reader.read_se()?;
        // offset_for_top_to_bottom_field
        reader.read_se()?;
        // num_ref_frames_in_pic_order_cnt_cycle
        let num_ref_frames = reader.read_ue()?;
        for _ in 0..num_ref_frames {
            reader.read_se()?;
        }
    }

    // max_num_ref_frames
    reader.read_ue()?;

    // gaps_in_frame_num_value_allowed_flag
    reader.read_bits(1)?;

    // pic_width_in_mbs_minus1
    let pic_width_in_mbs_minus1 = reader.read_ue()?;
    debug!("SPS pic_width_in_mbs_minus1: {}", pic_width_in_mbs_minus1);

    // pic_height_in_map_units_minus1
    let pic_height_in_map_units_minus1 = reader.read_ue()?;
    debug!("SPS pic_height_in_map_units_minus1: {}", pic_height_in_map_units_minus1);

    // frame_mbs_only_flag
    let frame_mbs_only_flag = reader.read_bits(1)?;
    debug!("SPS frame_mbs_only_flag: {}", frame_mbs_only_flag);

    // 计算实际分辨率
    let width = (pic_width_in_mbs_minus1 + 1) * 16;
    let height = (pic_height_in_map_units_minus1 + 1) * 16 * (2 - frame_mbs_only_flag);
    debug!("SPS calculated resolution: {}x{}", width, height);

    // 读取 frame_cropping_flag 来调整最终尺寸
    if frame_mbs_only_flag == 0 {
        // mb_adaptive_frame_field_flag
        reader.read_bits(1)?;
    }

    // direct_8x8_inference_flag
    reader.read_bits(1)?;

    // frame_cropping_flag
    let frame_cropping_flag = reader.read_bits(1)?;
    let (crop_left, crop_right, crop_top, crop_bottom) = if frame_cropping_flag == 1 {
        let left = reader.read_ue()? * 2;
        let right = reader.read_ue()? * 2;
        let top = reader.read_ue()? * 2;
        let bottom = reader.read_ue()? * 2;
        (left, right, top, bottom)
    } else {
        (0, 0, 0, 0)
    };

    let final_width = width - crop_left - crop_right;
    let final_height = height - crop_top - crop_bottom;

    info!("📐 SPS parsed resolution: {}x{} (before crop: {}x{})", final_width, final_height, width, height);

    Some((final_width, final_height))
}
