use crate::error::{Result, ScrcpyError};
use crate::scrcpy::control::ControlEvent;
use crate::utils::find_available_port;
use axum::{
    extract::ws::{WebSocket, WebSocketUpgrade, Message},
    response::IntoResponse,
    routing::get,
    Router,
};
use bytes::Bytes;
use tokio::sync::{broadcast, RwLock, mpsc};
use tracing::{info, warn, debug};
use std::net::SocketAddr;
use std::sync::Arc;

/// 视频配置信息
#[derive(Clone)]
pub struct VideoConfig {
    pub sps: Option<Bytes>,
    pub pps: Option<Bytes>,
    pub width: u32,           // 视频流分辨率（可能经过缩放）
    pub height: u32,          // 视频流分辨率（可能经过缩放）
    pub device_width: u32,    // 设备物理屏幕宽度（用于触控）
    pub device_height: u32,   // 设备物理屏幕高度（用于触控）
    pub is_landscape: bool,   // 是否为横屏模式（width > height）
}

/// WebSocket 服务器
pub struct WebSocketServer {
    port: u16,
    actual_port: u16,  // 实际使用的端口（可能与请求的端口不同）
    public: bool,      // 是否监听所有接口（局域网可访问）
    // 使用 broadcast channel 向所有连接的客户端广播视频帧
    tx: broadcast::Sender<Bytes>,
    // 使用 broadcast channel 向所有连接的客户端广播配置变化
    config_tx: broadcast::Sender<String>,
    // 缓存 SPS/PPS 配置帧
    video_config: Arc<RwLock<VideoConfig>>,
    // 用于请求IDR帧的通道
    idr_request_tx: mpsc::Sender<()>,
    // 用于发送控制事件的通道
    control_tx: mpsc::Sender<ControlEvent>,
}

impl WebSocketServer {
    /// 创建新的 WebSocket 服务器（自动寻找可用端口）
    ///
    /// # Arguments
    /// * `port` - 期望的端口号，如果被占用会自动向后寻找
    /// * `public` - 是否监听所有接口（true: 0.0.0.0，false: 127.0.0.1）
    pub fn new(port: u16, idr_request_tx: mpsc::Sender<()>, control_tx: mpsc::Sender<ControlEvent>, device_width: u32, device_height: u32, public: bool) -> Result<Self> {
        // 自动寻找可用端口
        let actual_port = find_available_port(port, 100)?;

        let (tx, _rx) = broadcast::channel(2); // 极小缓冲：只保留1-2帧，最小化延迟
        let (config_tx, _) = broadcast::channel(16); // 配置变化广播通道

        let video_config = Arc::new(RwLock::new(VideoConfig {
            sps: None,
            pps: None,
            width: device_width,   // 使用设备分辨率作为初始值
            height: device_height, // 使用设备分辨率作为初始值
            device_width,   // 设备物理屏幕尺寸
            device_height,  // 设备物理屏幕尺寸
            is_landscape: device_width > device_height,  // 初始横屏状态
        }));

        Ok(Self { port, actual_port, public, tx, config_tx, video_config, idr_request_tx, control_tx })
    }

    /// 获取实际使用的端口
    pub fn get_actual_port(&self) -> u16 {
        self.actual_port
    }

    /// 获取视频帧发送器的克隆
    pub fn get_sender(&self) -> broadcast::Sender<Bytes> {
        self.tx.clone()
    }

    /// 获取配置变化广播器的克隆
    pub fn get_config_sender(&self) -> broadcast::Sender<String> {
        self.config_tx.clone()
    }

    /// 获取视频配置的克隆
    pub fn get_video_config(&self) -> Arc<RwLock<VideoConfig>> {
        self.video_config.clone()
    }

    /// 启动 WebSocket 服务器
    pub async fn start(self) -> Result<()> {
        // 根据 public 参数选择监听地址
        let bind_addr: [u8; 4] = if self.public {
            [0, 0, 0, 0]      // 监听所有接口，局域网可访问
        } else {
            [127, 0, 0, 1]    // 仅本地访问
        };
        let addr = SocketAddr::from((bind_addr, self.actual_port));
        info!("🌐 Starting WebSocket server on {}", addr);

        let tx = self.tx.clone();
        let config_tx = self.config_tx.clone();
        let video_config = self.video_config.clone();
        let idr_request_tx = self.idr_request_tx.clone();
        let control_tx = self.control_tx.clone();

        // 创建 Axum 路由
        let app = Router::new()
            .route("/ws", get({
                let tx = tx.clone();
                let config_tx = config_tx.clone();
                let video_config = video_config.clone();
                let idr_request_tx = idr_request_tx.clone();
                let control_tx = control_tx.clone();
                move |ws| handle_socket(ws, tx, config_tx, video_config, idr_request_tx, control_tx)
            }))
            .route("/", get(serve_html))
            .route("/decoder/Decoder.min.js", get(serve_broadway_decoder))
            .route("/decoder/jmuxer.min.js", get(serve_jmuxer));

        // 启动服务器
        let listener = tokio::net::TcpListener::bind(&addr)
            .await
            .map_err(|e| ScrcpyError::Network(format!("Failed to bind: {}", e)))?;

        info!("✅ WebSocket server ready at ws://{}/ws", addr);
        info!("📱 Open http://{} in your browser", addr);

        axum::serve(listener, app)
            .await
            .map_err(|e| ScrcpyError::Network(format!("Server error: {}", e)))?;

        Ok(())
    }
}

/// 处理 WebSocket 连接
async fn handle_socket(
    ws: WebSocketUpgrade,
    tx: broadcast::Sender<Bytes>,
    config_tx: broadcast::Sender<String>,
    video_config: Arc<RwLock<VideoConfig>>,
    idr_request_tx: mpsc::Sender<()>,
    control_tx: mpsc::Sender<ControlEvent>,
) -> impl IntoResponse {
    ws.on_upgrade(|socket| handle_client(socket, tx, config_tx, video_config, idr_request_tx, control_tx))
}

/// 处理单个客户端连接
async fn handle_client(
    mut socket: WebSocket,
    tx: broadcast::Sender<Bytes>,
    config_tx: broadcast::Sender<String>,
    video_config: Arc<RwLock<VideoConfig>>,
    idr_request_tx: mpsc::Sender<()>,
    control_tx: mpsc::Sender<ControlEvent>,
) {
    info!("📱 New WebSocket client connected");

    // 🔥 关键：新客户端连接时，立即请求IDR帧
    info!("🎬 Requesting IDR frame for new client...");
    if let Err(e) = idr_request_tx.send(()).await {
        warn!("Failed to request IDR frame: {}", e);
    }

    // 立即发送视频配置信息（视频流分辨率 + 设备物理分辨率 + 横屏状态）
    let config = video_config.read().await;
    let config_msg = format!("{{\"type\":\"config\",\"width\":{},\"height\":{},\"device_width\":{},\"device_height\":{},\"is_landscape\":{}}}",
        config.width, config.height, config.device_width, config.device_height, config.is_landscape);
    if socket.send(Message::Text(config_msg)).await.is_err() {
        warn!("Failed to send config to client");
        return;
    }

    // 立即发送缓存的 SPS/PPS 给新客户端
    if let Some(sps) = &config.sps {
        info!("📤 Sending cached SPS to new client ({} bytes)", sps.len());
        if socket.send(Message::Binary(sps.to_vec())).await.is_err() {
            warn!("Failed to send SPS to client");
            return;
        }
    } else {
        info!("⚠️  No SPS cached yet");
    }
    if let Some(pps) = &config.pps {
        info!("📤 Sending cached PPS to new client ({} bytes)", pps.len());
        if socket.send(Message::Binary(pps.to_vec())).await.is_err() {
            warn!("Failed to send PPS to client");
            return;
        }
    } else {
        info!("⚠️  No PPS cached yet");
    }

    drop(config); // 释放读锁

    // 订阅广播频道
    let mut rx = tx.subscribe();
    let mut config_rx = config_tx.subscribe();

    // 持续接收并转发视频帧，同时监听客户端消息和配置变化
    loop {
        tokio::select! {
            // 接收配置变化并发送给客户端
            config_result = config_rx.recv() => {
                match config_result {
                    Ok(config_msg) => {
                        info!("📤 Sending config update to client");
                        if socket.send(Message::Text(config_msg)).await.is_err() {
                            warn!("❌ Client disconnected (config send failed)");
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        // 跳过旧的配置消息
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        info!("📡 Config broadcast channel closed");
                        break;
                    }
                }
            }
            // 接收视频帧并发送
            frame_result = rx.recv() => {
                match frame_result {
                    Ok(frame_data) => {
                        // 发送二进制数据到客户端
                        if socket.send(Message::Binary(frame_data.to_vec())).await.is_err() {
                            warn!("❌ Client disconnected (send failed)");
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_skipped)) => {
                        // 🔥 追帧策略：清空积压的旧帧，直接跳到最新
                        loop {
                            match rx.try_recv() {
                                Ok(latest_frame) => {
                                    // 尝试发送最新帧
                                    if socket.send(Message::Binary(latest_frame.to_vec())).await.is_err() {
                                        warn!("❌ Client disconnected during flush");
                                        break;
                                    }
                                }
                                Err(broadcast::error::TryRecvError::Empty) => {
                                    // 队列已空，追上了
                                    break;
                                }
                                Err(broadcast::error::TryRecvError::Lagged(_)) => {
                                    // 继续追
                                    continue;
                                }
                                Err(broadcast::error::TryRecvError::Closed) => {
                                    info!("📡 Broadcast channel closed during flush");
                                    return;
                                }
                            }
                        }
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        info!("📡 Broadcast channel closed");
                        break;
                    }
                }
            }

            // 监听客户端消息（包括close消息和控制事件）
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        // 解析控制事件JSON
                        debug!("📥 Received control message: {}", text);
                        match serde_json::from_str::<ControlEvent>(&text) {
                            Ok(control_event) => {
                                debug!("✅ Parsed control event: {:?}", control_event);
                                if let Err(e) = control_tx.send(control_event).await {
                                    warn!("Failed to forward control event: {}", e);
                                }
                            }
                            Err(e) => {
                                warn!("Failed to parse control event '{}': {}", text, e);
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) => {
                        info!("👋 Client sent close message");
                        break;
                    }
                    Some(Ok(Message::Ping(_))) => {
                        // 自动回复pong（axum会处理）
                    }
                    Some(Err(e)) => {
                        warn!("❌ Client disconnected (recv error): {}", e);
                        break;
                    }
                    None => {
                        warn!("❌ Client disconnected (recv None)");
                        break;
                    }
                    _ => {
                        // 忽略其他消息类型
                    }
                }
            }
        }
    }

    info!("👋 WebSocket client disconnected");
}

/// 提供简单的 HTML 页面
async fn serve_html() -> impl IntoResponse {
    let html = r#"
<!DOCTYPE html>
<html>
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0, maximum-scale=1.0, user-scalable=no">
    <title>Rust-Scrcpy Web Viewer</title>
    <!-- Broadway.js H.264 解码器 (本地文件) -->
    <script src="/decoder/Decoder.min.js"></script>
    <!-- JMuxer MSE 播放器 (本地文件) -->
    <script src="/decoder/jmuxer.min.js"></script>
    <style>
        * {
            margin: 0;
            padding: 0;
            box-sizing: border-box;
        }

        html {
            width: 100%;
            height: 100%;
        }

        body {
            font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif;
            width: 100%;
            height: 100%;
            margin: 0;
            padding: 0;
            overflow: hidden;
            background: #fff;
            display: flex;
            justify-content: center;
            align-items: center;
        }

        #videoCanvas {
            display: block;
            background: #000;
            position: relative;
        }

        /* Canvas 容器，用于裁剪超出部分 */
        #canvasContainer {
            position: relative;
            overflow: hidden;
            display: flex;
            justify-content: center;
            align-items: center;
        }

        /* 解码器状态指示器 */
        #decoderStatus {
            position: absolute;
            top: 10px;
            right: 10px;
            padding: 8px 16px;
            border-radius: 20px;
            font-size: 12px;
            font-weight: 500;
            color: white;
            background: rgba(0, 0, 0, 0.7);
            backdrop-filter: blur(10px);
            z-index: 1000;
            display: flex;
            align-items: center;
            gap: 8px;
            transition: transform 0.3s ease, opacity 0.3s ease;
            cursor: grab;
            user-select: none;
            -webkit-user-select: none;
        }

        #decoderStatus:active {
            cursor: grabbing;
        }

        #decoderStatus:hover {
            background: rgba(0, 0, 0, 0.85);
        }

        /* 贴边隐藏状态 */
        #decoderStatus.docked-left {
            transform: translateX(-85%);
        }
        #decoderStatus.docked-right {
            transform: translateX(85%);
        }
        #decoderStatus.docked-left:hover,
        #decoderStatus.docked-right:hover {
            transform: translateX(0);
        }

        #decoderStatus .dot {
            width: 8px;
            height: 8px;
            border-radius: 50%;
            background: #4CAF50;
            flex-shrink: 0;
        }

        #decoderStatus.webcodecs .dot { background: #4CAF50; }
        #decoderStatus.broadway .dot { background: #2196F3; }
        #decoderStatus.jmuxer .dot { background: #FF9800; }
        #decoderStatus.error .dot { background: #F44336; }
        #decoderStatus.loading .dot {
            background: #FFC107;
            animation: pulse 1s infinite;
        }

        @keyframes pulse {
            0%, 100% { opacity: 1; }
            50% { opacity: 0.4; }
        }

        /* 解码器选择面板 */
        #decoderPanel {
            position: fixed;
            padding: 12px;
            border-radius: 12px;
            background: rgba(0, 0, 0, 0.85);
            backdrop-filter: blur(10px);
            z-index: 1001;
            display: none;
            flex-direction: column;
            gap: 8px;
            min-width: 200px;
            user-select: none;
            -webkit-user-select: none;
        }

        #decoderPanel.visible {
            display: flex;
        }

        #decoderPanel .option {
            padding: 10px 14px;
            border-radius: 8px;
            background: rgba(255, 255, 255, 0.1);
            color: white;
            cursor: pointer;
            display: flex;
            justify-content: space-between;
            align-items: center;
            transition: background 0.2s;
            user-select: none;
            -webkit-user-select: none;
        }

        #decoderPanel .option:hover {
            background: rgba(255, 255, 255, 0.2);
        }

        #decoderPanel .option.active {
            background: rgba(76, 175, 80, 0.3);
            border: 1px solid #4CAF50;
        }

        #decoderPanel .option.unavailable {
            opacity: 0.5;
            cursor: not-allowed;
        }

        #decoderPanel .option .name {
            font-weight: 500;
        }

        #decoderPanel .option .status {
            font-size: 11px;
            opacity: 0.7;
        }

        .controls {
            margin-top: 20px;
            display: flex;
            gap: 10px;
            justify-content: center;
        }
    </style>
</head>
<body>
    <!-- Canvas 容器，用于裁剪超出部分 -->
    <div id="canvasContainer">
        <canvas id="videoCanvas" width="1920" height="1080"></canvas>

        <!-- 解码器状态指示器 -->
        <div id="decoderStatus" class="loading">
            <span class="dot"></span>
            <span id="decoderName">初始化中...</span>
        </div>
    </div>

    <!-- 解码器选择面板 -->
    <div id="decoderPanel">
        <div class="option" data-decoder="webcodecs">
            <span class="name">WebCodecs</span>
            <span class="status" id="webcodecs-status">检测中...</span>
        </div>
        <div class="option" data-decoder="broadway">
            <span class="name">Broadway</span>
            <span class="status" id="broadway-status">检测中...</span>
        </div>
        <div class="option" data-decoder="jmuxer">
            <span class="name">JMuxer (MSE)</span>
            <span class="status" id="jmuxer-status">检测中...</span>
        </div>
    </div>

    <script>
        // ========== 全局变量 ==========
        let ws = null;
        let currentDecoder = null;
        let canvas = document.getElementById('videoCanvas');
        let ctx = canvas.getContext('2d');
        let frameCount = 0;
        let cachedSPS = null;
        let cachedPPS = null;
        let videoWidth = 0;
        let videoHeight = 0;
        let deviceWidth = 0;
        let deviceHeight = 0;
        let isLandscape = false;

        // 解码器可用性状态
        const decoderSupport = {
            webcodecs: false,
            broadway: false,
            jmuxer: false
        };

        // 当前使用的解码器类型
        let currentDecoderType = null;

        // ========== 解码器抽象接口 ==========
        class BaseDecoder {
            constructor(canvas) {
                this.canvas = canvas;
                this.ctx = canvas.getContext('2d');
                this.ready = false;
                this.frameCount = 0;
            }

            async init(width, height) {
                throw new Error('Not implemented');
            }

            decode(nalData, isKeyFrame) {
                throw new Error('Not implemented');
            }

            close() {
                this.ready = false;
            }

            static isSupported() {
                return false;
            }

            getName() {
                return 'Base';
            }
        }

        // ========== WebCodecs 解码器 ==========
        class WebCodecsDecoder extends BaseDecoder {
            constructor(canvas) {
                super(canvas);
                this.decoder = null;
            }

            static isSupported() {
                return 'VideoDecoder' in window;
            }

            getName() {
                return 'WebCodecs';
            }

            async init(width, height) {
                if (!WebCodecsDecoder.isSupported()) {
                    throw new Error('WebCodecs API not supported');
                }

                if (this.decoder) {
                    this.decoder.close();
                }

                this.decoder = new VideoDecoder({
                    output: (frame) => {
                        this.ctx.drawImage(frame, 0, 0, this.canvas.width, this.canvas.height);
                        frame.close();
                        this.frameCount++;
                    },
                    error: (e) => {
                        console.error('WebCodecs decoder error:', e);
                        this.ready = false;
                    }
                });

                this.decoder.configure({
                    codec: 'avc1.42001E',
                    optimizeForLatency: true,
                    hardwareAcceleration: 'prefer-hardware',
                });

                this.ready = true;
                console.log('✅ WebCodecs decoder initialized');
            }

            decode(nalData, isKeyFrame) {
                if (!this.decoder || !this.ready) return;

                try {
                    if (isKeyFrame && this.decoder.decodeQueueSize > 0) {
                        this.decoder.flush();
                    }

                    if (!isKeyFrame && this.decoder.decodeQueueSize > 3) {
                        console.warn('WebCodecs queue full, dropping P-frame');
                        return;
                    }

                    const chunk = new EncodedVideoChunk({
                        type: isKeyFrame ? 'key' : 'delta',
                        timestamp: performance.now() * 1000,
                        data: nalData
                    });
                    this.decoder.decode(chunk);
                } catch (e) {
                    console.error('WebCodecs decode error:', e);
                }
            }

            close() {
                if (this.decoder) {
                    this.decoder.close();
                    this.decoder = null;
                }
                super.close();
            }
        }

        // ========== Broadway.js 原生解码器 ==========
        class BroadwayDecoder extends BaseDecoder {
            constructor(canvas) {
                super(canvas);
                this.decoder = null;
                this.imageData = null;
            }

            static isSupported() {
                // Broadway Decoder.min.js 提供 Decoder 类
                return typeof Decoder !== 'undefined';
            }

            getName() {
                return 'Broadway';
            }

            async init(width, height) {
                try {
                    if (this.decoder) {
                        this.decoder = null;
                    }

                    const w = width || this.canvas.width;
                    const h = height || this.canvas.height;

                    // 创建 Broadway 解码器实例
                    // 使用 rgb: true 返回 RGBA 数据便于直接绘制到 canvas
                    this.decoder = new Decoder({
                        rgb: true
                    });

                    // 设置解码回调
                    this.decoder.onPictureDecoded = (buffer, decWidth, decHeight) => {
                        // buffer 是 Uint8Array，包含 RGBA 数据
                        this.renderRGB(buffer, decWidth, decHeight);
                        this.frameCount++;
                    };

                    this.ready = true;
                    console.log('✅ Broadway decoder initialized');
                } catch (e) {
                    console.error('Broadway init error:', e);
                    throw e;
                }
            }

            renderRGB(buffer, width, height) {
                // 确保 canvas 尺寸匹配
                if (this.canvas.width !== width || this.canvas.height !== height) {
                    // 不改变 canvas 尺寸，使用缩放绘制
                }

                // 创建或重用 ImageData
                if (!this.imageData || this.imageData.width !== width || this.imageData.height !== height) {
                    this.imageData = this.ctx.createImageData(width, height);
                }

                // 复制 RGBA 数据
                this.imageData.data.set(buffer);

                // 绘制到 canvas（如果尺寸不同，需要缩放）
                if (this.canvas.width === width && this.canvas.height === height) {
                    this.ctx.putImageData(this.imageData, 0, 0);
                } else {
                    // 创建临时 canvas 进行缩放
                    const tempCanvas = document.createElement('canvas');
                    tempCanvas.width = width;
                    tempCanvas.height = height;
                    const tempCtx = tempCanvas.getContext('2d');
                    tempCtx.putImageData(this.imageData, 0, 0);
                    this.ctx.drawImage(tempCanvas, 0, 0, this.canvas.width, this.canvas.height);
                }
            }

            decode(nalData, isKeyFrame) {
                if (!this.decoder || !this.ready) return;

                try {
                    this.decoder.decode(nalData);
                } catch (e) {
                    console.error('Broadway decode error:', e);
                }
            }

            close() {
                this.decoder = null;
                this.imageData = null;
                super.close();
            }
        }

        // ========== JMuxer MSE 解码器 ==========
        class JMuxerDecoder extends BaseDecoder {
            constructor(canvas) {
                super(canvas);
                this.player = null;
                this.video = null;
            }

            static isSupported() {
                // JMuxer 需要 MSE 支持
                return typeof JMuxer !== 'undefined' &&
                       typeof MediaSource !== 'undefined' &&
                       MediaSource.isTypeSupported('video/mp4; codecs="avc1.42E01E"');
            }

            getName() {
                return 'JMuxer (MSE)';
            }

            async init(width, height) {
                try {
                    if (this.player) {
                        this.player.destroy();
                    }
                    if (this.video) {
                        this.video.remove();
                    }

                    // 创建隐藏的 video 元素
                    const video = document.createElement('video');
                    video.style.cssText = 'position:absolute;top:-9999px;left:-9999px;';
                    video.muted = true;
                    video.autoplay = true;
                    video.playsInline = true;
                    document.body.appendChild(video);

                    this.player = new JMuxer({
                        node: video,
                        mode: 'video',
                        flushingTime: 1,  // 减少延迟
                        clearBuffer: true,
                        fps: 60,
                        debug: false,
                        onReady: () => {
                            console.log('✅ JMuxer ready');
                            video.play().catch(e => console.warn('Video play failed:', e));
                        },
                        onError: (e) => {
                            console.error('JMuxer error:', e);
                        }
                    });

                    this.video = video;

                    // 将视频帧绘制到 canvas
                    this.renderLoop();

                    this.ready = true;
                    console.log('✅ JMuxer decoder initialized');
                } catch (e) {
                    console.error('JMuxer init error:', e);
                    throw e;
                }
            }

            renderLoop() {
                const render = () => {
                    if (this.video && this.video.readyState >= 2) {
                        this.ctx.drawImage(this.video, 0, 0, this.canvas.width, this.canvas.height);
                        this.frameCount++;
                    }
                    if (this.ready) {
                        requestAnimationFrame(render);
                    }
                };
                requestAnimationFrame(render);
            }

            decode(nalData, isKeyFrame) {
                if (!this.player || !this.ready) return;

                try {
                    // JMuxer 需要特定的数据格式
                    this.player.feed({
                        video: nalData,
                        duration: 1000 / 60  // 假设 60fps
                    });
                } catch (e) {
                    console.error('JMuxer decode error:', e);
                }
            }

            close() {
                if (this.player) {
                    this.player.destroy();
                    this.player = null;
                }
                if (this.video) {
                    this.video.remove();
                    this.video = null;
                }
                super.close();
            }
        }

        // ========== 解码器管理器 ==========
        const DecoderManager = {
            decoders: {
                webcodecs: WebCodecsDecoder,
                broadway: BroadwayDecoder,
                jmuxer: JMuxerDecoder
            },

            // 检测所有解码器的可用性
            async detectSupport() {
                decoderSupport.webcodecs = WebCodecsDecoder.isSupported();
                decoderSupport.broadway = BroadwayDecoder.isSupported();
                decoderSupport.jmuxer = JMuxerDecoder.isSupported();

                // 更新 UI
                this.updateSupportUI();

                console.log('🔍 Decoder support:', decoderSupport);
                return decoderSupport;
            },

            updateSupportUI() {
                document.getElementById('webcodecs-status').textContent =
                    decoderSupport.webcodecs ? '✓ 可用 (硬件加速)' : '✗ 不支持';
                document.getElementById('broadway-status').textContent =
                    decoderSupport.broadway ? '✓ 可用 (软解码)' : '✗ 未加载';
                document.getElementById('jmuxer-status').textContent =
                    decoderSupport.jmuxer ? '✓ 可用 (MSE)' : '✗ 不支持';

                // 标记不可用的选项
                document.querySelectorAll('#decoderPanel .option').forEach(option => {
                    const decoder = option.dataset.decoder;
                    if (!decoderSupport[decoder]) {
                        option.classList.add('unavailable');
                    } else {
                        option.classList.remove('unavailable');
                    }
                });
            },

            // 获取最佳可用解码器
            getBestDecoder() {
                if (decoderSupport.webcodecs) return 'webcodecs';
                if (decoderSupport.jmuxer) return 'jmuxer';
                if (decoderSupport.broadway) return 'broadway';
                return null;
            },

            // 创建解码器实例
            async createDecoder(type, canvas) {
                const DecoderClass = this.decoders[type];
                if (!DecoderClass) {
                    throw new Error(`Unknown decoder type: ${type}`);
                }

                if (!decoderSupport[type]) {
                    throw new Error(`Decoder ${type} is not supported`);
                }

                return new DecoderClass(canvas);
            }
        };

        // ========== UI 控制函数 ==========
        function updateDecoderStatus(type, name) {
            const statusEl = document.getElementById('decoderStatus');
            const nameEl = document.getElementById('decoderName');

            // 保留 docked 类
            const dockedClass = statusEl.classList.contains('docked-left') ? 'docked-left' :
                               statusEl.classList.contains('docked-right') ? 'docked-right' : '';
            statusEl.className = type + (dockedClass ? ' ' + dockedClass : '');
            nameEl.textContent = name;

            // 更新选择面板中的激活状态
            document.querySelectorAll('#decoderPanel .option').forEach(option => {
                option.classList.remove('active');
                if (option.dataset.decoder === type) {
                    option.classList.add('active');
                }
            });
        }

        function toggleDecoderPanel() {
            const panel = document.getElementById('decoderPanel');
            const status = document.getElementById('decoderStatus');
            panel.classList.toggle('visible');

            // 定位面板到状态指示器下方
            if (panel.classList.contains('visible')) {
                const rect = status.getBoundingClientRect();
                const panelWidth = 200;

                // 计算面板位置
                let left = rect.left;
                let top = rect.bottom + 8;

                // 确保不超出右边界
                if (left + panelWidth > window.innerWidth) {
                    left = window.innerWidth - panelWidth - 10;
                }
                // 确保不超出左边界
                if (left < 10) {
                    left = 10;
                }

                panel.style.left = left + 'px';
                panel.style.top = top + 'px';
                panel.style.right = 'auto';
            }
        }

        async function switchDecoder(type) {
            if (!decoderSupport[type]) {
                console.warn(`Decoder ${type} is not supported`);
                return;
            }

            if (currentDecoderType === type) {
                toggleDecoderPanel();
                return;
            }

            console.log(`🔄 Switching to ${type} decoder...`);
            updateDecoderStatus('loading', `切换到 ${type}...`);

            try {
                // 关闭当前解码器
                if (currentDecoder) {
                    currentDecoder.close();
                }

                // 创建新解码器
                currentDecoder = await DecoderManager.createDecoder(type, canvas);
                await currentDecoder.init(videoWidth, videoHeight);

                currentDecoderType = type;
                frameCount = 0;

                updateDecoderStatus(type, currentDecoder.getName());
                console.log(`✅ Switched to ${type} decoder`);

                // 保存用户选择到 URL
                const url = new URL(window.location);
                url.searchParams.set('decoder', type);
                window.history.replaceState({}, '', url);

            } catch (e) {
                console.error(`Failed to switch to ${type}:`, e);
                updateDecoderStatus('error', `${type} 初始化失败`);

                // 尝试回退到其他解码器
                const fallback = DecoderManager.getBestDecoder();
                if (fallback && fallback !== type) {
                    console.log(`🔄 Falling back to ${fallback}...`);
                    await switchDecoder(fallback);
                }
            }

            toggleDecoderPanel();
        }

        // ========== 拖动、吸附、贴边隐藏 ==========
        const statusEl = document.getElementById('decoderStatus');
        const container = document.getElementById('canvasContainer');
        let isDragging = false;
        let dragStartX = 0;
        let dragStartY = 0;
        let elementStartX = 0;
        let elementStartY = 0;
        let hasMoved = false;
        const SNAP_THRESHOLD = 20;  // 吸附阈值
        const EDGE_THRESHOLD = 30;  // 贴边隐藏阈值

        function getContainerRect() {
            return container.getBoundingClientRect();
        }

        function getStatusPosition() {
            const rect = statusEl.getBoundingClientRect();
            const containerRect = getContainerRect();
            return {
                x: rect.left - containerRect.left,
                y: rect.top - containerRect.top,
                width: rect.width,
                height: rect.height
            };
        }

        function setStatusPosition(x, y) {
            statusEl.style.left = x + 'px';
            statusEl.style.top = y + 'px';
            statusEl.style.right = 'auto';
        }

        function handleDragStart(e) {
            if (e.target.closest('#decoderPanel')) return;

            isDragging = true;
            hasMoved = false;

            const pos = getStatusPosition();
            elementStartX = pos.x;
            elementStartY = pos.y;

            if (e.type === 'touchstart') {
                dragStartX = e.touches[0].clientX;
                dragStartY = e.touches[0].clientY;
            } else {
                dragStartX = e.clientX;
                dragStartY = e.clientY;
            }

            // 移除贴边状态以便拖动
            statusEl.classList.remove('docked-left', 'docked-right');
            statusEl.style.transition = 'none';
        }

        function handleDragMove(e) {
            if (!isDragging) return;

            let clientX, clientY;
            if (e.type === 'touchmove') {
                clientX = e.touches[0].clientX;
                clientY = e.touches[0].clientY;
                e.preventDefault();
            } else {
                clientX = e.clientX;
                clientY = e.clientY;
            }

            const deltaX = clientX - dragStartX;
            const deltaY = clientY - dragStartY;

            // 判断是否真的在移动
            if (Math.abs(deltaX) > 5 || Math.abs(deltaY) > 5) {
                hasMoved = true;
            }

            let newX = elementStartX + deltaX;
            let newY = elementStartY + deltaY;

            const pos = getStatusPosition();
            const containerRect = getContainerRect();
            const maxX = containerRect.width - pos.width;
            const maxY = containerRect.height - pos.height;

            // 边界限制（限制在容器内）
            newX = Math.max(0, Math.min(newX, maxX));
            newY = Math.max(0, Math.min(newY, maxY));

            // 边缘吸附
            if (newX < SNAP_THRESHOLD) newX = 0;
            if (newX > maxX - SNAP_THRESHOLD) newX = maxX;
            if (newY < SNAP_THRESHOLD) newY = 0;
            if (newY > maxY - SNAP_THRESHOLD) newY = maxY;

            setStatusPosition(newX, newY);
        }

        function handleDragEnd(e) {
            if (!isDragging) return;
            isDragging = false;

            statusEl.style.transition = 'transform 0.3s ease, opacity 0.3s ease';

            const pos = getStatusPosition();
            const containerRect = getContainerRect();
            const maxX = containerRect.width - pos.width;

            // 贴边隐藏判断
            if (pos.x <= EDGE_THRESHOLD) {
                setStatusPosition(0, pos.y);
                statusEl.classList.add('docked-left');
            } else if (pos.x >= maxX - EDGE_THRESHOLD) {
                setStatusPosition(maxX, pos.y);
                statusEl.classList.add('docked-right');
            }

            // 如果没有移动，则视为点击，切换面板
            if (!hasMoved) {
                toggleDecoderPanel();
            }

            // 保存位置到 localStorage
            saveStatusPosition();
        }

        function saveStatusPosition() {
            const pos = getStatusPosition();
            const containerRect = getContainerRect();
            // 保存相对位置（百分比）
            const relX = pos.x / containerRect.width;
            const relY = pos.y / containerRect.height;
            const docked = statusEl.classList.contains('docked-left') ? 'left' :
                          statusEl.classList.contains('docked-right') ? 'right' : '';
            localStorage.setItem('decoderStatusPos', JSON.stringify({
                relX: relX, relY: relY, docked: docked
            }));
        }

        function loadStatusPosition() {
            try {
                const saved = localStorage.getItem('decoderStatusPos');
                if (saved) {
                    const data = JSON.parse(saved);
                    const containerRect = getContainerRect();
                    const pos = getStatusPosition();

                    // 从相对位置恢复
                    let x = data.relX * containerRect.width;
                    let y = data.relY * containerRect.height;

                    // 确保不超出边界
                    const maxX = containerRect.width - pos.width;
                    const maxY = containerRect.height - pos.height;
                    x = Math.max(0, Math.min(x, maxX));
                    y = Math.max(0, Math.min(y, maxY));

                    setStatusPosition(x, y);

                    if (data.docked === 'left') {
                        setStatusPosition(0, y);
                        statusEl.classList.add('docked-left');
                    } else if (data.docked === 'right') {
                        setStatusPosition(maxX, y);
                        statusEl.classList.add('docked-right');
                    }
                }
            } catch (e) {
                console.warn('Failed to load status position:', e);
            }
        }

        // 绑定拖动事件
        statusEl.addEventListener('mousedown', handleDragStart);
        document.addEventListener('mousemove', handleDragMove);
        document.addEventListener('mouseup', handleDragEnd);

        statusEl.addEventListener('touchstart', handleDragStart, { passive: false });
        document.addEventListener('touchmove', handleDragMove, { passive: false });
        document.addEventListener('touchend', handleDragEnd);

        // 绑定解码器选项点击事件
        document.querySelectorAll('#decoderPanel .option').forEach(option => {
            option.addEventListener('click', () => {
                const decoder = option.dataset.decoder;
                if (decoder) switchDecoder(decoder);
            });
        });

        // 加载保存的位置
        loadStatusPosition();

        // ========== Canvas 尺寸管理 ==========
        function resizeCanvas() {
            if (videoWidth > 0 && videoHeight > 0) {
                const videoRatio = videoWidth / videoHeight;
                const windowWidth = window.innerWidth;
                const windowHeight = window.innerHeight;
                const windowRatio = windowWidth / windowHeight;

                let canvasStyleWidth, canvasStyleHeight;
                if (videoRatio > windowRatio) {
                    canvasStyleWidth = windowWidth;
                    canvasStyleHeight = windowWidth / videoRatio;
                } else {
                    canvasStyleHeight = windowHeight;
                    canvasStyleWidth = windowHeight * videoRatio;
                }

                canvas.style.width = canvasStyleWidth + 'px';
                canvas.style.height = canvasStyleHeight + 'px';

                // 同步设置容器尺寸
                container.style.width = canvasStyleWidth + 'px';
                container.style.height = canvasStyleHeight + 'px';

                // 重新加载位置以适应新尺寸
                loadStatusPosition();
            }
        }

        window.addEventListener('resize', resizeCanvas);

        // 点击其他区域关闭面板
        document.addEventListener('click', (e) => {
            const panel = document.getElementById('decoderPanel');
            const status = document.getElementById('decoderStatus');
            if (!panel.contains(e.target) && !status.contains(e.target)) {
                panel.classList.remove('visible');
            }
        });

        // ========== 解码处理 ==========
        function handleVideoFrame(data) {
            if (!currentDecoder || !currentDecoder.ready) return;

            // 检查 NAL 单元类型
            let nalType = 0;
            if (data.length > 4) {
                nalType = data[4] & 0x1F;
            }

            // 缓存 SPS/PPS
            if (nalType === 7) {
                cachedSPS = data;
                return;
            } else if (nalType === 8) {
                cachedPPS = data;
                return;
            }

            // IDR 帧处理
            if (nalType === 5) {
                let combinedData = data;

                if (cachedSPS && cachedPPS) {
                    const totalLength = cachedSPS.length + cachedPPS.length + data.length;
                    combinedData = new Uint8Array(totalLength);

                    let offset = 0;
                    combinedData.set(cachedSPS, offset);
                    offset += cachedSPS.length;
                    combinedData.set(cachedPPS, offset);
                    offset += cachedPPS.length;
                    combinedData.set(data, offset);
                }

                currentDecoder.decode(combinedData, true);
                frameCount++;
                return;
            }

            // P 帧处理
            if (frameCount > 0) {
                currentDecoder.decode(data, false);
            }
        }

        // ========== WebSocket 连接 ==========
        async function connect() {
            updateDecoderStatus('loading', '连接中...');

            const protocol = window.location.protocol === 'https:' ? 'wss:' : 'ws:';
            const wsUrl = `${protocol}//${window.location.host}/ws`;

            ws = new WebSocket(wsUrl);
            ws.binaryType = 'arraybuffer';

            ws.onopen = async () => {
                console.log('✅ WebSocket connected');

                // 检测解码器支持
                await DecoderManager.detectSupport();

                // 从 URL 参数获取指定的解码器
                const urlParams = new URLSearchParams(window.location.search);
                const requestedDecoder = urlParams.get('decoder');

                // 选择解码器
                let decoderToUse = null;

                if (requestedDecoder && decoderSupport[requestedDecoder]) {
                    decoderToUse = requestedDecoder;
                    console.log(`📋 Using requested decoder: ${requestedDecoder}`);
                } else {
                    decoderToUse = DecoderManager.getBestDecoder();
                    console.log(`🔍 Auto-selected decoder: ${decoderToUse}`);
                }

                if (!decoderToUse) {
                    updateDecoderStatus('error', '无可用解码器');
                    console.error('No decoder available!');
                    return;
                }

                // 初始化解码器
                try {
                    currentDecoder = await DecoderManager.createDecoder(decoderToUse, canvas);
                    await currentDecoder.init(videoWidth || 1920, videoHeight || 1080);
                    currentDecoderType = decoderToUse;
                    updateDecoderStatus(decoderToUse, currentDecoder.getName());
                } catch (e) {
                    console.error('Decoder init failed:', e);
                    updateDecoderStatus('error', '解码器初始化失败');
                }
            };

            ws.onmessage = (event) => {
                // 处理文本消息（配置信息）
                if (typeof event.data === 'string') {
                    try {
                        const msg = JSON.parse(event.data);
                        if (msg.type === 'config') {
                            videoWidth = msg.width;
                            videoHeight = msg.height;
                            deviceWidth = msg.device_width;
                            deviceHeight = msg.device_height;
                            isLandscape = msg.is_landscape || false;

                            console.log('📐 Video resolution:', videoWidth, 'x', videoHeight);
                            console.log('📱 Device resolution:', deviceWidth, 'x', deviceHeight);

                            canvas.width = msg.width;
                            canvas.height = msg.height;
                            resizeCanvas();

                            // 重新初始化解码器
                            if (currentDecoder) {
                                currentDecoder.init(videoWidth, videoHeight);
                            }
                        }
                    } catch (e) {
                        console.error('Failed to parse config:', e);
                    }
                    return;
                }

                // 处理二进制消息（视频帧）
                if (event.data instanceof ArrayBuffer) {
                    handleVideoFrame(new Uint8Array(event.data));
                }
            };

            ws.onerror = (error) => {
                console.error('WebSocket error:', error);
                updateDecoderStatus('error', '连接错误');
                clearCanvas();
            };

            ws.onclose = () => {
                console.log('WebSocket closed');
                updateDecoderStatus('error', '连接断开');
                clearCanvas();
                if (currentDecoder) {
                    currentDecoder.close();
                    currentDecoder = null;
                }
            };
        }

        function clearCanvas() {
            ctx.fillStyle = '#000000';
            ctx.fillRect(0, 0, canvas.width, canvas.height);
        }

        function disconnect() {
            if (ws) {
                ws.close();
                ws = null;
            }
            if (currentDecoder) {
                currentDecoder.close();
                currentDecoder = null;
            }
            frameCount = 0;
            cachedSPS = null;
            cachedPPS = null;
            clearCanvas();
        }

        // ========== 触控事件处理 ==========
        let activeTouches = new Map();

        function setupTouchEvents() {
            canvas.addEventListener('touchstart', handleTouchStart, { passive: false });
            canvas.addEventListener('touchmove', handleTouchMove, { passive: false });
            canvas.addEventListener('touchend', handleTouchEnd, { passive: false });
            canvas.addEventListener('touchcancel', handleTouchEnd, { passive: false });

            canvas.addEventListener('mousedown', handleMouseDown);
            canvas.addEventListener('mousemove', handleMouseMove);
            canvas.addEventListener('mouseup', handleMouseUp);
            canvas.addEventListener('mouseleave', handleMouseUp);
        }

        function normalizeCoords(canvasX, canvasY) {
            const rect = canvas.getBoundingClientRect();
            const x = (canvasX - rect.left) / rect.width;
            const y = (canvasY - rect.top) / rect.height;
            return { x: Math.max(0, Math.min(1, x)), y: Math.max(0, Math.min(1, y)) };
        }

        function sendTouchEvent(action, pointerId, x, y, pressure = 1.0) {
            if (!ws || ws.readyState !== WebSocket.OPEN) return;
            if (!deviceWidth || !deviceHeight) return;

            let buttons = 0;
            let actualPressure = pressure;

            if (action === 0) {
                buttons = 1;
                actualPressure = 1.0;
            } else if (action === 1) {
                buttons = 0;
                actualPressure = 0.0;
            } else if (action === 2) {
                buttons = 1;
                actualPressure = 1.0;
            }

            const event = {
                type: 'touch',
                action: action,
                pointer_id: pointerId,
                x: x,
                y: y,
                pressure: actualPressure,
                width: videoWidth,
                height: videoHeight,
                buttons: buttons
            };

            ws.send(JSON.stringify(event));
        }

        function handleTouchStart(e) {
            e.preventDefault();
            for (let touch of e.changedTouches) {
                const coords = normalizeCoords(touch.clientX, touch.clientY);
                activeTouches.set(touch.identifier, coords);
                const action = activeTouches.size === 1 ? 0 : 5;
                sendTouchEvent(action, touch.identifier, coords.x, coords.y, touch.force || 1.0);
            }
        }

        function handleTouchMove(e) {
            e.preventDefault();
            for (let touch of e.changedTouches) {
                if (!activeTouches.has(touch.identifier)) continue;
                const coords = normalizeCoords(touch.clientX, touch.clientY);
                activeTouches.set(touch.identifier, coords);
                sendTouchEvent(2, touch.identifier, coords.x, coords.y, touch.force || 1.0);
            }
        }

        function handleTouchEnd(e) {
            e.preventDefault();
            for (let touch of e.changedTouches) {
                if (!activeTouches.has(touch.identifier)) continue;
                const coords = activeTouches.get(touch.identifier);
                activeTouches.delete(touch.identifier);
                const action = activeTouches.size === 0 ? 1 : 6;
                sendTouchEvent(action, touch.identifier, coords.x, coords.y, 1.0);
            }
        }

        let mouseDown = false;
        const MOUSE_POINTER_ID = -1;

        function handleMouseDown(e) {
            mouseDown = true;
            const coords = normalizeCoords(e.clientX, e.clientY);
            activeTouches.set(MOUSE_POINTER_ID, coords);
            sendTouchEvent(0, MOUSE_POINTER_ID, coords.x, coords.y, 1.0);
        }

        function handleMouseMove(e) {
            const coords = normalizeCoords(e.clientX, e.clientY);
            if (mouseDown) {
                activeTouches.set(MOUSE_POINTER_ID, coords);
                sendTouchEvent(2, MOUSE_POINTER_ID, coords.x, coords.y, 1.0);
            }
        }

        function handleMouseUp(e) {
            if (!mouseDown) return;
            mouseDown = false;
            const coords = activeTouches.get(MOUSE_POINTER_ID) || normalizeCoords(e.clientX, e.clientY);
            activeTouches.delete(MOUSE_POINTER_ID);
            sendTouchEvent(1, MOUSE_POINTER_ID, coords.x, coords.y, 1.0);
        }

        // ========== 键盘事件处理 ==========
        const KEY_MAP = {
            'KeyA': 29, 'KeyB': 30, 'KeyC': 31, 'KeyD': 32, 'KeyE': 33,
            'KeyF': 34, 'KeyG': 35, 'KeyH': 36, 'KeyI': 37, 'KeyJ': 38,
            'KeyK': 39, 'KeyL': 40, 'KeyM': 41, 'KeyN': 42, 'KeyO': 43,
            'KeyP': 44, 'KeyQ': 45, 'KeyR': 46, 'KeyS': 47, 'KeyT': 48,
            'KeyU': 49, 'KeyV': 50, 'KeyW': 51, 'KeyX': 52, 'KeyY': 53, 'KeyZ': 54,
            'Digit0': 7, 'Digit1': 8, 'Digit2': 9, 'Digit3': 10, 'Digit4': 11,
            'Digit5': 12, 'Digit6': 13, 'Digit7': 14, 'Digit8': 15, 'Digit9': 16,
            'Enter': 66, 'Backspace': 67, 'Delete': 112, 'Tab': 61, 'Space': 62, 'Escape': 111,
            'ArrowUp': 19, 'ArrowDown': 20, 'ArrowLeft': 21, 'ArrowRight': 22,
            'Home': 3, 'End': 123, 'PageUp': 92, 'PageDown': 93,
            'Comma': 55, 'Period': 56, 'Slash': 76, 'Semicolon': 74, 'Quote': 75,
            'BracketLeft': 71, 'BracketRight': 72, 'Backslash': 73,
            'Minus': 69, 'Equal': 70, 'Backquote': 68,
        };

        const META_SHIFT = 1;
        const META_CTRL = 4096;
        const META_ALT = 2;

        function getMetaState(e) {
            let meta = 0;
            if (e.shiftKey) meta |= META_SHIFT;
            if (e.ctrlKey) meta |= META_CTRL;
            if (e.altKey) meta |= META_ALT;
            return meta;
        }

        function sendKeyEvent(action, keycode, metastate) {
            if (!ws || ws.readyState !== WebSocket.OPEN) return;
            ws.send(JSON.stringify({
                type: 'key',
                action: action,
                keycode: keycode,
                repeat: 0,
                metastate: metastate
            }));
        }

        function handleKeyDown(e) {
            if (e.ctrlKey && e.code === 'KeyV') {
                e.preventDefault();
                handlePaste();
                return;
            }
            const keycode = KEY_MAP[e.code];
            if (keycode !== undefined) {
                e.preventDefault();
                sendKeyEvent(0, keycode, getMetaState(e));
            }
        }

        function handleKeyUp(e) {
            const keycode = KEY_MAP[e.code];
            if (keycode !== undefined) {
                e.preventDefault();
                sendKeyEvent(1, keycode, getMetaState(e));
            }
        }

        // ========== 文本输入和粘贴 ==========
        function sendText(text) {
            if (!ws || ws.readyState !== WebSocket.OPEN) return;
            ws.send(JSON.stringify({ type: 'text', text: text }));
            console.log('📝 Sent text:', text.length, 'chars');
        }

        function setClipboard(text, paste) {
            if (!ws || ws.readyState !== WebSocket.OPEN) return;
            ws.send(JSON.stringify({ type: 'clipboard', text: text, paste: paste }));
        }

        async function handlePaste() {
            try {
                const text = await navigator.clipboard.readText();
                if (text) sendText(text);
            } catch (e) {
                console.error('Failed to read clipboard:', e);
            }
        }

        function setupKeyboardEvents() {
            document.addEventListener('keydown', handleKeyDown);
            document.addEventListener('keyup', handleKeyUp);
            document.addEventListener('paste', async (e) => {
                e.preventDefault();
                const text = e.clipboardData.getData('text');
                if (text) sendText(text);
            });
        }

        // ========== 滚轮滚动 ==========
        function sendScrollEvent(x, y, hscroll, vscroll) {
            if (!ws || ws.readyState !== WebSocket.OPEN) return;
            if (!videoWidth || !videoHeight) return;
            ws.send(JSON.stringify({
                type: 'scroll',
                x: x, y: y,
                width: videoWidth, height: videoHeight,
                hscroll: hscroll, vscroll: vscroll
            }));
        }

        function handleWheel(e) {
            e.preventDefault();
            const coords = normalizeCoords(e.clientX, e.clientY);
            const vscroll = e.deltaY > 0 ? -1 : (e.deltaY < 0 ? 1 : 0);
            const hscroll = e.deltaX > 0 ? -1 : (e.deltaX < 0 ? 1 : 0);
            if (vscroll !== 0 || hscroll !== 0) {
                sendScrollEvent(coords.x, coords.y, hscroll, vscroll);
            }
        }

        function setupScrollEvents() {
            canvas.addEventListener('wheel', handleWheel, { passive: false });
        }

        // ========== 初始化 ==========
        setupTouchEvents();
        setupKeyboardEvents();
        setupScrollEvents();
        connect();
    </script>
</body>
</html>
    "#;

    ([("content-type", "text/html; charset=utf-8")], html)
}

/// 提供 Broadway Decoder.min.js
async fn serve_broadway_decoder() -> impl IntoResponse {
    let js = include_str!("../decoder/Decoder.min.js");
    ([("content-type", "application/javascript; charset=utf-8")], js)
}

/// 提供 JMuxer jmuxer.min.js
async fn serve_jmuxer() -> impl IntoResponse {
    let js = include_str!("../decoder/jmuxer.min.js");
    ([("content-type", "application/javascript; charset=utf-8")], js)
}
