use crate::error::{Result, ScrcpyError};
use crate::scrcpy::control::TouchEvent;
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
}

/// WebSocket 服务器
pub struct WebSocketServer {
    port: u16,
    actual_port: u16,  // 实际使用的端口（可能与请求的端口不同）
    // 使用 broadcast channel 向所有连接的客户端广播视频帧
    tx: broadcast::Sender<Bytes>,
    // 缓存 SPS/PPS 配置帧
    video_config: Arc<RwLock<VideoConfig>>,
    // 用于请求IDR帧的通道
    idr_request_tx: mpsc::Sender<()>,
    // 用于发送触控事件的通道
    control_tx: mpsc::Sender<TouchEvent>,
}

impl WebSocketServer {
    /// 创建新的 WebSocket 服务器（自动寻找可用端口）
    ///
    /// # Arguments
    /// * `port` - 期望的端口号，如果被占用会自动向后寻找
    /// * `max_port_attempts` - 端口搜索的最大尝试次数
    pub fn new(port: u16, idr_request_tx: mpsc::Sender<()>, control_tx: mpsc::Sender<TouchEvent>, device_width: u32, device_height: u32) -> Result<Self> {
        // 自动寻找可用端口
        let actual_port = find_available_port(port, 100)?;

        let (tx, _rx) = broadcast::channel(2); // 极小缓冲：只保留1-2帧，最小化延迟

        let video_config = Arc::new(RwLock::new(VideoConfig {
            sps: None,
            pps: None,
            width: device_width,   // 使用设备分辨率作为初始值
            height: device_height, // 使用设备分辨率作为初始值
            device_width,   // 设备物理屏幕尺寸
            device_height,  // 设备物理屏幕尺寸
        }));

        Ok(Self { port, actual_port, tx, video_config, idr_request_tx, control_tx })
    }

    /// 获取实际使用的端口
    pub fn get_actual_port(&self) -> u16 {
        self.actual_port
    }

    /// 获取视频帧发送器的克隆
    pub fn get_sender(&self) -> broadcast::Sender<Bytes> {
        self.tx.clone()
    }

    /// 获取视频配置的克隆
    pub fn get_video_config(&self) -> Arc<RwLock<VideoConfig>> {
        self.video_config.clone()
    }

    /// 启动 WebSocket 服务器
    pub async fn start(self) -> Result<()> {
        let addr = SocketAddr::from(([0, 0, 0, 0], self.actual_port));
        info!("🌐 Starting WebSocket server on {}", addr);

        let tx = self.tx.clone();
        let video_config = self.video_config.clone();
        let idr_request_tx = self.idr_request_tx.clone();
        let control_tx = self.control_tx.clone();

        // 创建 Axum 路由
        let app = Router::new()
            .route("/ws", get({
                let tx = tx.clone();
                let video_config = video_config.clone();
                let idr_request_tx = idr_request_tx.clone();
                let control_tx = control_tx.clone();
                move |ws| handle_socket(ws, tx, video_config, idr_request_tx, control_tx)
            }))
            .route("/", get(serve_html));

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
    video_config: Arc<RwLock<VideoConfig>>,
    idr_request_tx: mpsc::Sender<()>,
    control_tx: mpsc::Sender<TouchEvent>,
) -> impl IntoResponse {
    ws.on_upgrade(|socket| handle_client(socket, tx, video_config, idr_request_tx, control_tx))
}

/// 处理单个客户端连接
async fn handle_client(
    mut socket: WebSocket,
    tx: broadcast::Sender<Bytes>,
    video_config: Arc<RwLock<VideoConfig>>,
    idr_request_tx: mpsc::Sender<()>,
    control_tx: mpsc::Sender<TouchEvent>,
) {
    info!("📱 New WebSocket client connected");

    // 🔥 关键：新客户端连接时，立即请求IDR帧
    info!("🎬 Requesting IDR frame for new client...");
    if let Err(e) = idr_request_tx.send(()).await {
        warn!("Failed to request IDR frame: {}", e);
    }

    // 立即发送视频配置信息（视频流分辨率 + 设备物理分辨率）
    let config = video_config.read().await;
    let config_msg = format!("{{\"type\":\"config\",\"width\":{},\"height\":{},\"device_width\":{},\"device_height\":{}}}",
        config.width, config.height, config.device_width, config.device_height);
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

    // 持续接收并转发视频帧，同时监听客户端消息
    loop {
        tokio::select! {
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
                        match serde_json::from_str::<TouchEvent>(&text) {
                            Ok(touch_event) => {
                                debug!("✅ Parsed touch event: action={:?}, pointer_id={}, x={}, y={}",
                                    touch_event.action, touch_event.pointer_id, touch_event.x, touch_event.y);
                                if let Err(e) = control_tx.send(touch_event).await {
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

        <canvas id="videoCanvas" width="1920" height="1080"></canvas>

    <script>
        let ws = null;
        let decoder = null;
        let canvas = document.getElementById('videoCanvas');
        let ctx = canvas.getContext('2d');
        let decoderReady = false;
        let frameCount = 0;
        let cachedSPS = null;
        let cachedPPS = null;
        let videoWidth = 0;         // 视频流分辨率（用于canvas显示）
        let videoHeight = 0;
        let deviceWidth = 0;        // 设备物理分辨率（用于触控坐标）
        let deviceHeight = 0;

        // 调整 canvas 显示尺寸
        function resizeCanvas() {
            if (videoWidth > 0 && videoHeight > 0) {
                const videoRatio = videoWidth / videoHeight;
                const windowWidth = window.innerWidth;
                const windowHeight = window.innerHeight;

                // 计算按高度填满时的宽度
                const widthByHeight = windowHeight * videoRatio;

                // 如果按高度填满后宽度超出窗口，则按宽度填满
                if (widthByHeight > windowWidth) {
                    canvas.style.width = '100vw';
                    canvas.style.height = `calc(100vw / ${videoRatio})`;
                } else {
                    // 否则按高度填满
                    canvas.style.height = '100vh';
                    canvas.style.width = `calc(100vh * ${videoRatio})`;
                }
            }
        }

        // 监听窗口大小变化
        window.addEventListener('resize', resizeCanvas);

        // 简单的 H.264 解码（需要浏览器支持 WebCodecs API）
        async function initDecoder() {
            if (!('VideoDecoder' in window)) {
                console.error('WebCodecs API not supported');
                // updateStatus('error', 'Browser does not support WebCodecs API');
                return;
            }

            decoder = new VideoDecoder({
                output: (frame) => {
                    // 绘制帧到 canvas（保持 canvas 的实际分辨率和 CSS 显示尺寸）
                    // 不要在这里修改 canvas.width/height，因为已经在 config 消息中设置好了
                    ctx.drawImage(frame, 0, 0, canvas.width, canvas.height);
                    frame.close();

                    frameCount++;
                    if (frameCount === 1) {
                        // updateStatus('connected', 'Video streaming! ' + canvas.width + 'x' + canvas.height);
                    }
                },
                error: (e) => {
                    console.error('Decoder error:', e);
                    decoderReady = false;
                }
            });

            // 简单配置解码器 - 不使用 description，让解码器从帧中自动提取
            try {
                decoder.configure({
                    codec: 'avc1.42001E', // H.264 Baseline Profile Level 3.0
                    optimizeForLatency: true,
                    hardwareAcceleration: 'prefer-hardware',
                });
                decoderReady = true;
            } catch (e) {
                console.error('Failed to configure decoder:', e);
                // updateStatus('error', 'Failed to configure decoder');
            }
        }

        function connect() {
            // updateStatus('connecting', 'Connecting to server...');

            const protocol = window.location.protocol === 'https:' ? 'wss:' : 'ws:';
            const wsUrl = `${protocol}//${window.location.host}/ws`;

            ws = new WebSocket(wsUrl);
            ws.binaryType = 'arraybuffer';

            ws.onopen = () => {
                // updateStatus('connected', 'Connected! Receiving video stream...');
                initDecoder();
            };

            ws.onmessage = (event) => {
                // 处理文本消息（配置信息）
                if (typeof event.data === 'string') {
                    try {
                        const msg = JSON.parse(event.data);
                        if (msg.type === 'config') {
                            // 保存视频流分辨率（用于canvas显示）
                            videoWidth = msg.width;
                            videoHeight = msg.height;

                            // 保存设备物理分辨率（用于触控坐标）
                            deviceWidth = msg.device_width;
                            deviceHeight = msg.device_height;

                            console.log('📐 Video resolution:', videoWidth, 'x', videoHeight);
                            console.log('📱 Device resolution:', deviceWidth, 'x', deviceHeight);

                            // 设置 canvas 实际分辨率（解码尺寸）
                            canvas.width = msg.width;
                            canvas.height = msg.height;

                            // 调整显示尺寸
                            resizeCanvas();

                            // 重新配置解码器
                            if (decoder) {
                                decoder.close();
                            }
                            initDecoder();
                        }
                    } catch (e) {
                        console.error('Failed to parse config:', e);
                    }
                    return;
                }

                // 处理二进制消息（视频帧）
                if (event.data instanceof ArrayBuffer) {
                    const data = new Uint8Array(event.data);

                    // 检查 NAL 单元类型
                    let nalType = 0;
                    if (data.length > 4) {
                        // 跳过起始码 00 00 00 01
                        nalType = data[4] & 0x1F;
                    }

                    // 缓存 SPS/PPS，等待 IDR 帧
                    if (nalType === 7) {
                        cachedSPS = data;
                        return; // 不立即解码，等待 IDR
                    } else if (nalType === 8) {
                        cachedPPS = data;
                        return; // 不立即解码，等待 IDR
                    }

                    // 收到 IDR 帧时，合并 SPS + PPS + IDR 为一个完整的帧
                    if (nalType === 5) {
                        if (decoder && decoderReady) {
                            try {
                                // ===== IDR 关键帧优先：如果队列积压，先清空队列
                                if (decoder.decodeQueueSize > 0) {
                                    console.warn('Flushing ' + decoder.decodeQueueSize + ' queued frames before IDR');
                                    decoder.flush();
                                }

                                // 合并 SPS + PPS + IDR 成一个完整的 Annex-B 流
                                let combinedData;

                                if (cachedSPS && cachedPPS) {
                                    // 计算总长度
                                    const totalLength = cachedSPS.length + cachedPPS.length + data.length;
                                    combinedData = new Uint8Array(totalLength);

                                    // 拼接：SPS + PPS + IDR（每个都有自己的起始码）
                                    let offset = 0;
                                    combinedData.set(cachedSPS, offset);
                                    offset += cachedSPS.length;
                                    combinedData.set(cachedPPS, offset);
                                    offset += cachedPPS.length;
                                    combinedData.set(data, offset);
                                } else {
                                    // 如果没有缓存的 SPS/PPS，只发送 IDR
                                    combinedData = data;
                                }

                                // 发送合并后的完整关键帧
                                const keyChunk = new EncodedVideoChunk({
                                    type: 'key',
                                    timestamp: performance.now() * 1000,
                                    data: combinedData
                                });
                                decoder.decode(keyChunk);

                            } catch (e) {
                                console.error('Decode error:', e.message);
                            }
                        }
                        return;
                    }

                    // 其他帧（非 IDR）正常解码
                    if (decoder && decoderReady && frameCount > 0) {
                        try {
                            // ===== 限制解码器队列大小，防止积压延迟
                            // 如果队列 > 3 帧，且当前是 P-frame，则丢弃
                            if (decoder.decodeQueueSize > 3) {
                                console.warn('Decoder queue full (' + decoder.decodeQueueSize + '), dropping P-frame');
                                return;
                            }

                            const chunk = new EncodedVideoChunk({
                                type: 'delta',
                                timestamp: performance.now() * 1000,
                                data: data
                            });
                            decoder.decode(chunk);
                        } catch (e) {
                            console.error('Decode error:', e.message);
                        }
                    }
                }
            };

            ws.onerror = (error) => {
                // updateStatus('error', 'Connection error');
                console.error('WebSocket error:', error);
                clearCanvas();  // 连接错误时清空画布
            };

            ws.onclose = () => {
                // updateStatus('error', 'Disconnected from server');
                clearCanvas();  // 连接断开时清空画布
                if (decoder) {
                    decoder.close();
                    decoder = null;
                }
            };
        }

        // 清空画布（变黑）
        function clearCanvas() {
            ctx.fillStyle = '#000000';
            ctx.fillRect(0, 0, canvas.width, canvas.height);
        }

        function disconnect() {
            if (ws) {
                ws.close();
                ws = null;
            }
            if (decoder) {
                decoder.close();
                decoder = null;
            }
            decoderReady = false;
            frameCount = 0;
            cachedSPS = null;
            cachedPPS = null;
            clearCanvas();  // 断开连接时清空画布
            // updateStatus('error', 'Disconnected');
        }

        // function updateStatus(type, message) {
        //     const statusEl = document.getElementById('status');
        //     statusEl.className = type;
        //     statusEl.textContent = message;
        // }

        // 触控事件处理
        let activeTouches = new Map(); // 存储当前活动的触控点

        function setupTouchEvents() {
            // 阻止默认的触摸行为
            canvas.addEventListener('touchstart', handleTouchStart, { passive: false });
            canvas.addEventListener('touchmove', handleTouchMove, { passive: false });
            canvas.addEventListener('touchend', handleTouchEnd, { passive: false });
            canvas.addEventListener('touchcancel', handleTouchEnd, { passive: false });

            // 添加鼠标事件支持（PC测试）
            canvas.addEventListener('mousedown', handleMouseDown);
            canvas.addEventListener('mousemove', handleMouseMove);
            canvas.addEventListener('mouseup', handleMouseUp);
            canvas.addEventListener('mouseleave', handleMouseUp);
        }

        // 坐标转换：Canvas像素坐标 → 归一化坐标 [0, 1]
        function normalizeCoords(canvasX, canvasY) {
            const rect = canvas.getBoundingClientRect();
            // 计算相对于canvas的位置
            const x = (canvasX - rect.left) / rect.width;
            const y = (canvasY - rect.top) / rect.height;
            return { x: Math.max(0, Math.min(1, x)), y: Math.max(0, Math.min(1, y)) };
        }

        // 发送触控事件到服务器
        function sendTouchEvent(action, pointerId, x, y, pressure = 1.0) {
            if (!ws || ws.readyState !== WebSocket.OPEN) {
                console.warn('WebSocket not ready, cannot send touch event');
                return;
            }

            if (!deviceWidth || !deviceHeight) {
                console.warn('Device dimensions not set, cannot send touch event');
                return;
            }

            // 根据 action 设置正确的 buttons 和 pressure
            // 鼠标模式（官方scrcpy使用的模式）：
            // DOWN: buttons=1, pressure=1.0
            // UP:   buttons=0, pressure=0.0
            // MOVE: buttons=1, pressure=1.0
            let buttons = 0;
            let actualPressure = pressure;

            if (action === 0) {
                // DOWN: buttons=1, pressure=1.0
                buttons = 1;
                actualPressure = 1.0;
            } else if (action === 1) {
                // UP: buttons=0, pressure=0.0
                buttons = 0;
                actualPressure = 0.0;
            } else if (action === 2) {
                // MOVE: buttons=1, pressure=1.0
                buttons = 1;
                actualPressure = 1.0;
            }

            const event = {
                action: action,
                pointer_id: pointerId,
                x: x,
                y: y,
                pressure: actualPressure,
                width: videoWidth,   // 使用视频流分辨率（scrcpy server 期望的尺寸）
                height: videoHeight, // 使用视频流分辨率（scrcpy server 期望的尺寸）
                buttons: buttons
            };

            const jsonStr = JSON.stringify(event);
            ws.send(jsonStr);
        }

        // 触摸事件处理器
        function handleTouchStart(e) {
            e.preventDefault();
            for (let touch of e.changedTouches) {
                const coords = normalizeCoords(touch.clientX, touch.clientY);
                activeTouches.set(touch.identifier, coords);

                // 真实触摸事件使用正数ID (touch.identifier从0开始)
                // Android ACTION_DOWN (0) 或 ACTION_POINTER_DOWN (5)
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

                // Android ACTION_MOVE (2)
                sendTouchEvent(2, touch.identifier, coords.x, coords.y, touch.force || 1.0);
            }
        }

        function handleTouchEnd(e) {
            e.preventDefault();
            for (let touch of e.changedTouches) {
                if (!activeTouches.has(touch.identifier)) continue;

                const coords = activeTouches.get(touch.identifier);
                activeTouches.delete(touch.identifier);

                // Android ACTION_UP (1) 或 ACTION_POINTER_UP (6)
                const action = activeTouches.size === 0 ? 1 : 6;
                sendTouchEvent(action, touch.identifier, coords.x, coords.y, 1.0);
            }
        }

        // 鼠标事件处理器（用于PC测试）
        let mouseDown = false;
        // 使用官方scrcpy的鼠标ID: POINTER_ID_MOUSE = -1
        const MOUSE_POINTER_ID = -1;

        function handleMouseDown(e) {
            mouseDown = true;
            const coords = normalizeCoords(e.clientX, e.clientY);
            activeTouches.set(MOUSE_POINTER_ID, coords);
            sendTouchEvent(0, MOUSE_POINTER_ID, coords.x, coords.y, 1.0); // ACTION_DOWN
        }

        function handleMouseMove(e) {
            const coords = normalizeCoords(e.clientX, e.clientY);
            if (mouseDown) {
                // 按下鼠标移动：ACTION_MOVE (2)
                activeTouches.set(MOUSE_POINTER_ID, coords);
                sendTouchEvent(2, MOUSE_POINTER_ID, coords.x, coords.y, 1.0);
            }
            // 暂时禁用 HOVER_MOVE 以减少日志
            // else {
            //     // 未按下鼠标移动：ACTION_HOVER_MOVE (7)
            //     sendTouchEvent(7, MOUSE_POINTER_ID, coords.x, coords.y, 1.0);
            // }
        }

        function handleMouseUp(e) {
            if (!mouseDown) return;
            mouseDown = false;
            const coords = activeTouches.get(MOUSE_POINTER_ID) || normalizeCoords(e.clientX, e.clientY);
            activeTouches.delete(MOUSE_POINTER_ID);
            sendTouchEvent(1, MOUSE_POINTER_ID, coords.x, coords.y, 1.0); // ACTION_UP
        }

        // 在连接成功后设置触控事件
        setupTouchEvents();

        // 自动连接
        connect();
    </script>
</body>
</html>
    "#;

    ([("content-type", "text/html; charset=utf-8")], html)
}
