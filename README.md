其他依赖：**adb**（任意版本），**scrcpy-server**（v3.3.4，只能这个版本，其他版本协议不对）。*adb和scrcpy-server注意路径，--help中会有提示*

<p align="center">
  <img width="220"
       src="https://github.com/user-attachments/assets/4f6ab07c-c84e-43bc-a889-0d07eb22db18" />
</p>
<p align="center">
  <sub>🦀 && 🤖</sub><br/>
</p>



## 目录

1. [系统概述](#1-系统概述)
2. [整体架构](#2-整体架构)
3. [启动流程](#3-启动流程)
4. [ADB通信层](#4-adb通信层)
5. [Scrcpy Server协议](#5-scrcpy-server协议)
6. [视频流处理](#6-视频流处理)
7. [控制流处理](#7-控制流处理)
   - [7.7 文本注入协议](#77-文本注入协议)
   - [7.8 剪贴板设置协议](#78-剪贴板设置协议)
   - [7.9 统一控制事件类型](#79-统一控制事件类型)
   - [7.10 键盘输入支持](#710-键盘输入支持)
   - [7.11 剪贴板粘贴功能](#711-剪贴板粘贴功能)
   - [7.12 鼠标滚轮支持](#712-鼠标滚轮支持)
   - [7.5 屏幕旋转自动适配](#75-屏幕旋转自动适配)
8. [WebSocket通信](#8-websocket通信)
9. [前端解码与渲染](#9-前端解码与渲染)
10. [数据流转图](#10-数据流转图)
11. [关键技术细节](#11-关键技术细节)
12. [配置参数说明](#12-配置参数说明)
13. [错误处理](#13-错误处理)
14. [端口自动寻找机制](#14-端口自动寻找机制)

---

## 1. 系统概述

Rust-Scrcpy 是一个用 Rust 实现的 Android 屏幕镜像系统，通过 ADB 与设备通信，使用 scrcpy-server 捕获屏幕，并通过 WebSocket 将 H.264 视频流广播到浏览器客户端。同时支持双向控制，允许用户通过浏览器触控/鼠标操作远程控制 Android 设备。

### 核心特性

- **实时屏幕镜像**: 低延迟 H.264 视频流传输
- **多解码器支持**: WebCodecs（硬件加速）、JMuxer（MSE）、Broadway（软解码）自动降级
- **双向控制**: 支持触摸、鼠标、按键事件（仅单点控制）
- **键盘输入**: 支持字母、数字、功能键、方向键等
- **剪贴板粘贴**: 支持 Ctrl+V 快速粘贴文本到手机
- **鼠标滚轮**: 支持滚轮滚动，方便浏览网页和列表
- **屏幕旋转适配**: 自动检测横竖屏切换并调整显示
- **Web 客户端**: 多解码器自动降级，兼容所有现代浏览器
- **多客户端支持**: 使用 broadcast channel 同时向多个客户端推流
- **自动 IDR 帧请求**: 新客户端连接时自动获取关键帧，提高画面响应速度
- **自动端口**：自动跳过占用的端口，使用未被占用的端口

### 技术栈

| 组件           | 技术                                                  |
| -------------- | ----------------------------------------------------- |
| 后端运行时     | Tokio (异步)                                          |
| HTTP/WebSocket | Axum                                                  |
| 视频编码       | H.264 (Android MediaCodec)                            |
| 前端解码       | WebCodecs / JMuxer (MSE) / Broadway.js（自动降级）    |
| 进程通信       | ADB forward + TCP                                     |

---

## 2. 整体架构

```
┌─────────────────────────────────────────────────────────────────────────┐
│                              Rust-Scrcpy 系统架构                        │
├─────────────────────────────────────────────────────────────────────────┤
│                                                                         │
│  ┌──────────────┐     ADB      ┌──────────────────────────────────────┐ │
│  │              │◄────────────►│            Android Device            │ │
│  │   AdbClient  │   (USB/WiFi) │  ┌─────────────────────────────────┐ │ │
│  │              │              │  │       scrcpy-server.jar         │ │ │
│  └──────┬───────┘              │  │  ┌───────────┐  ┌────────────┐  │ │ │
│         │                      │  │  │  Video    │  │  Control   │  │ │ │
│         │                      │  │  │  Encoder  │  │  Handler   │  │ │ │
│         │                      │  │  └─────┬─────┘  └──────┬─────┘  │ │ │
│         │                      │  └────────┼───────────────┼────────┘ │ │
│         │                      └───────────┼───────────────┼──────────┘ │
│         │                                  │               │            │
│         │ adb forward                      │               │            │
│         │ tcp:27183 → localabstract:scrcpy │               │            │
│         │ tcp:27184 → localabstract:scrcpy │               │            │
│         │                                  │               │            │
│  ┌──────▼───────┐              ┌───────────▼───────────────▼──────────┐ │
│  │              │              │                                      │ │
│  │ ScrcpyServer │◄────────────►│            TCP Streams               │ │
│  │              │   TCP:27183  │  ┌────────────┐    ┌──────────────┐  │ │
│  └──────────────┘   TCP:27184  │  │ Video Port │    │ Control Port │  │ │
│                                │  │   27183    │    │    27184     │  │ │
│                                │  └──────┬─────┘    └──────┬───────┘  │ │
│                                └─────────┼─────────────────┼──────────┘ │
│                                          │                 │            │
│                                          ▼                 ▼            │
│                           ┌────────────────────┐  ┌──────────────────┐  │
│                           │ VideoStreamReader  │  │  ControlChannel  │  │
│                           │ (NAL 解析器)        │  │  (事件发送器)    │  │
│                           └─────────┬──────────┘  └────────┬─────────┘  │
│                                     │                      │            │
│                                     │ Bytes (NAL Units)    │ TouchEvent │
│                                     ▼                      ▼            │
│                           ┌─────────────────────────────────────────┐   │
│                           │           Main Event Loop               │   │
│                           │  ┌─────────────────────────────────┐    │   │
│                           │  │     tokio::select! {            │    │   │
│                           │  │       video_frame => broadcast, │    │   │
│                           │  │       control_event => send,    │    │   │
│                           │  │       idr_request => cache_send │    │   │
│                           │  │     }                           │    │   │
│                           │  └─────────────────────────────────┘    │   │
│                           └──────────────────┬──────────────────────┘   │
│                                              │                          │
│                                              │ broadcast::Sender<Bytes> │
│                                              ▼                          │
│                           ┌─────────────────────────────────────────┐   │
│                           │          WebSocketServer                │   │
│                           │  ┌─────────────────────────────────┐    │   │
│                           │  │     HTTP: /     → HTML页面      │    │   │
│                           │  │     WS:   /ws   → 视频流+控制    │    │   │
│                           │  └─────────────────────────────────┘    │   │
│                           └──────────────────┬──────────────────────┘   │
│                                              │                          │
│                                              │ WebSocket (Binary+Text)  │
│                                              ▼                          │
│                           ┌─────────────────────────────────────────┐   │
│                           │            Browser Client               │   │
│                           │  ┌─────────────────────────────────┐    │   │
│                           │  │  WebCodecs VideoDecoder         │    │   │
│                           │  │  Canvas 2D Rendering            │    │   │
│                           │  │  Touch/Mouse Event Handlers     │    │   │
│                           │  └─────────────────────────────────┘    │   │
│                           └─────────────────────────────────────────┘   │
│                                                                         │
└─────────────────────────────────────────────────────────────────────────┘
```

---

## 3. 启动流程

### 3.1 完整启动时序图

```
┌────────┐     ┌─────────┐     ┌──────────────┐     ┌─────────────────┐     ┌────────┐
│  User  │     │  Main   │     │  AdbClient   │     │  ScrcpyServer   │     │ Device │
└───┬────┘     └────┬────┘     └──────┬───────┘     └────────┬────────┘     └───┬────┘
    │               │                 │                      │                  │
    │ cargo run     │                 │                      │                  │
    │──────────────>│                 │                      │                  │
    │               │                 │                      │                  │
    │               │ list_devices()  │                      │                  │
    │               │────────────────>│                      │                  │
    │               │                 │    adb devices       │                  │
    │               │                 │─────────────────────────────────────────>
    │               │                 │                      │                  │
    │               │                 │<─────────────────────────────────────────
    │               │<────────────────│                      │                  │
    │               │                 │                      │                  │
    │               │ shell("wm size")│                      │                  │
    │               │────────────────>│                      │                  │
    │               │                 │─────────────────────────────────────────>
    │               │<────────────────│   "Physical size: 1080x1920"            │
    │               │                 │                      │                  │
    │               │ deploy()        │                      │                  │
    │               │────────────────────────────────────────>                  │
    │               │                 │    adb push          │                  │
    │               │                 │────────────────────────────────────────>│
    │               │                 │                      │                  │
    │               │ start()         │                      │                  │
    │               │────────────────────────────────────────>                  │
    │               │                 │                      │                  │
    │               │                 │  adb forward tcp:27183→localabstract:scrcpy
    │               │                 │────────────────────────────────────────>│
    │               │                 │  adb forward tcp:27184→localabstract:scrcpy
    │               │                 │────────────────────────────────────────>│
    │               │                 │                      │                  │
    │               │                 │  adb shell CLASSPATH=... app_process ...│
    │               │                 │────────────────────────────────────────>│
    │               │                 │                      │    [server启动]   │
    │               │                 │                      │                  │
    │               │ connect_video() │                      │                  │
    │               │────────────────────────────────────────>                  │
    │               │                 │  TCP connect 127.0.0.1:27183            │
    │               │                 │<───────────────────────────────────────>│
    │               │                 │                      │                  │
    │               │ connect_control()                      │                  │
    │               │────────────────────────────────────────>                  │
    │               │                 │  TCP connect 127.0.0.1:27184            │
    │               │                 │<───────────────────────────────────────>│
    │               │                 │                      │                  │
    │               │ read_video_header()                    │                  │
    │               │────────────────────────────────────────>                  │
    │               │                 │                      │    dummy byte    │
    │               │                 │<────────────────────────────────────────│
    │               │                 │                      │                  │
    │               │ [进入主事件循环]  │                      │                  │
    │               │                 │                      │   H.264 NAL流    │
    │               │                 │<────────────────────────────────────────│
    │               │                 │                      │                  │
```

### 3.2 启动代码流程

```rust
// src/main.rs - 简化的启动流程
#[tokio::main]
async fn main() -> Result<()> {
    // 1. 解析命令行参数
    let args = Args::parse();

    // 2. 初始化日志系统
    tracing_subscriber::fmt().with_max_level(log_level).init();

    // 3. 创建 ADB 客户端
    let adb = AdbClient::new(args.adb_path);

    // 4. 获取设备列表并选择设备
    let devices = adb.list_devices().await?;
    let device_id = devices[0].clone();

    // 5. 获取设备物理屏幕尺寸
    let wm_size_output = adb.shell(&device_id, "wm size").await?;
    let (device_width, device_height) = parse_wm_size(&wm_size_output)?;

    // 6. 创建并配置 ScrcpyServer
    let mut server = ScrcpyServer::with_config(adb, device_id, ...);

    // 7. 部署 server 到设备
    server.deploy().await?;

    // 8. 启动 server (设置端口转发并执行)
    server.start().await?;

    // 9. 连接视频流和控制流
    let video_stream = server.connect_video().await?;
    let control_stream = server.connect_control().await?;

    // 10. 读取协议头 (dummy byte)
    let codec_info = ScrcpyServer::read_video_header(&mut video_stream).await?;

    // 11. 创建通道
    let (idr_request_tx, idr_request_rx) = mpsc::channel(10);
    let (control_tx, control_rx) = mpsc::channel(100);

    // 12. 创建并启动 WebSocket 服务器
    let ws_server = WebSocketServer::new(ws_port, idr_request_tx, control_tx, ...);
    tokio::spawn(async move { ws_server.start().await });

    // 13. 进入主事件循环
    loop {
        tokio::select! {
            Some(control_event) = control_rx.recv() => { /* 处理控制事件 */ }
            Some(_) = idr_request_rx.recv() => { /* 处理IDR请求 */ }
            frame_result = reader.read_frame(false) => { /* 处理视频帧 */ }
        }
    }
}
```

---

### 3.3 WiFi启动

步骤：

1.**首次设置（需要 USB）**

先用 USB 连接手机，然后开启 TCP/IP 模式

```bash
adb tcpip 5555
```

查看手机 IP 地址（在手机 设置 > 关于手机 > 状态 中查看）
或者用命令：

```bash
adb shell ip route | findstr wlan     // powershell
```

2.**WiFi 连接**

拔掉 USB，通过 WiFi 连接
```bash
adb connect 192.168.1.xxx:5555
```

确认连接成功

```bash
adb devices
```
3.**运行 rust-scrcpy**

直接运行，会自动识别 WiFi 连接的设备，注意使用 `--public` 参数
```bash    
rust-ws-scrcpy.exe --public
```

或者指定设备

```bash
rust-ws-scrcpy.exe -d 192.168.1.xxx:5555 --public
```
**注意事项：**
  - 手机和电脑需要在同一局域网
  - WiFi 连接延迟会比 USB 稍高（通常增加 20-50ms）
  - 某些手机重启后需要重新开启 adb tcpip 5555
  - 部分 Android 11+ 设备支持无线调试，可在开发者选项中直接开启，无需 USB

## 4. ADB通信层

### 4.1 AdbClient 实现

```rust
// src/adb/client.rs
pub struct AdbClient {
    pub adb_path: PathBuf,  // ADB 可执行文件路径
}

impl AdbClient {
    /// 执行 ADB 命令并返回输出
    pub async fn execute(&self, args: &[&str]) -> Result<String>;

    /// 获取已连接的设备列表
    pub async fn list_devices(&self) -> Result<Vec<String>>;

    /// 推送文件到设备
    pub async fn push(&self, device_id: &str, local: &str, remote: &str) -> Result<()>;

    /// 执行 shell 命令
    pub async fn shell(&self, device_id: &str, command: &str) -> Result<String>;

    /// 设置端口转发
    pub async fn forward(&self, device_id: &str, local_port: u16, remote: &str) -> Result<()>;

    /// 移除端口转发
    pub async fn forward_remove(&self, device_id: &str, local_port: u16) -> Result<()>;
}
```

### 4.2 关键 ADB 命令

| 命令          | 用途            | 示例                                                |
| ------------- | --------------- | --------------------------------------------------- |
| `adb devices` | 列出已连接设备  | `adb devices`                                       |
| `adb push`    | 推送文件到设备  | `adb -s xxx push server.jar /data/local/tmp/`       |
| `adb shell`   | 执行 shell 命令 | `adb -s xxx shell wm size`                          |
| `adb forward` | 端口转发        | `adb -s xxx forward tcp:27183 localabstract:scrcpy` |

### 4.3 端口转发机制

```
PC 端                                    Android 设备端
┌─────────────────┐                     ┌─────────────────────────────┐
│                 │                     │                             │
│  127.0.0.1:27183├────── USB/WiFi ────►│ localabstract:scrcpy        │
│  (视频流)        │      ADB Forward    │ (Unix Abstract Socket)       │
│                 │                     │                             │
│  127.0.0.1:27184├────── USB/WiFi ────►│ localabstract:scrcpy        │
│  (控制流)        │      ADB Forward    │ (同一个Socket,不同连接)       │
│                 │                     │                             │
└─────────────────┘                     └─────────────────────────────┘
```

**重要说明**: 视频流和控制流使用同一个 `localabstract:scrcpy` socket，但是是两个独立的 TCP 连接。scrcpy-server 会根据连接顺序区分：第一个连接是视频流，第二个连接是控制流。

---

## 5. Scrcpy Server协议

### 5.1 Server 启动参数

```bash
CLASSPATH=/data/local/tmp/scrcpy-server.jar \
app_process / com.genymobile.scrcpy.Server 3.3.4 \
    log_level=info \
    max_size=1920 \           # 最大分辨率
    video_bit_rate=4000000 \  # 视频码率 (默认：4Mbps)
    max_fps=60 \              # 最大帧率
    video_codec_options=i-frame-interval=1 \  # IDR帧间隔(默认：1秒)
    tunnel_forward=true \     # 使用端口转发模式
    send_device_meta=false \  # 不发送设备元数据
    send_frame_meta=false \   # 不发送帧元数据
    send_dummy_byte=true \    # 发送 dummy byte
    send_codec_meta=false \   # 不发送编解码器元数据
    raw_stream=true \         # 原始 NAL 流模式
    audio=false \             # 禁用音频
    control=true \            # 启用控制
    cleanup=true              # 退出时清理
```

### 5.2 raw_stream 模式协议

当 `raw_stream=true` 时，视频流格式非常简单：

```
┌─────────────────────────────────────────────────────────────┐
│                    Video Stream Format                      │
├─────────────────────────────────────────────────────────────┤
│                                                             │
│  ┌──────────┐                                               │
│  │ Dummy    │  1 byte (0x00)                                │
│  │ Byte     │  表示连接建立成功                              │
│  └──────────┘                                               │
│       │                                                     │
│       ▼                                                     │
│  ┌──────────────────────────────────────────────────────┐   │
│  │              Annex-B H.264 NAL Stream                │   │
│  │                                                      │   │
│  │  ┌────────────┬─────────────────────────────────┐    │   │
│  │  │ Start Code │         NAL Unit Data           │    │   │
│  │  │ 00 00 01   │  [NAL Header][RBSP Payload]     │    │   │
│  │  └────────────┴─────────────────────────────────┘    │   │
│  │                        │                             │   │
│  │  ┌────────────┬────────▼────────────────────────┐    │   │
│  │  │ Start Code │         NAL Unit Data           │    │   │
│  │  │ 00 00 00 01│  [NAL Header][RBSP Payload]     │    │   │
│  │  └────────────┴─────────────────────────────────┘    │   │
│  │                        │                             │   │
│  │                       ...                            │   │
│  └──────────────────────────────────────────────────────┘   │
│                                                             │
└─────────────────────────────────────────────────────────────┘
```

### 5.3 双连接模式

scrcpy 3.x 在 `control=true` 模式下需要两个连接：

```
连接顺序:
1. 第一个 TCP 连接 → 视频流 (Video Socket)
2. 第二个 TCP 连接 → 控制流 (Control Socket)

重要: Server 会等待两个连接都建立后才开始发送数据！
```

---

## 6. 视频流处理

### 6.1 H.264 NAL 单元类型

| NAL Type | 名称          | 说明                |
| -------- | ------------- | ------------------- |
| 1        | Non-IDR Slice | P/B 帧 (需要参考帧) |
| 5        | IDR Slice     | 关键帧 (独立解码)   |
| 6        | SEI           | 补充增强信息        |
| 7        | SPS           | 序列参数集          |
| 8        | PPS           | 图像参数集          |

### 6.2 VideoStreamReader 实现

```rust
// src/scrcpy/video.rs
pub struct VideoStreamReader {
    stream: TcpStream,
    buffer: BytesMut,          // 1MB 读取缓冲区
    frame_count: u64,
    first_start_code_pos: Option<usize>,  // 第一个起始码位置
}

impl VideoStreamReader {
    /// 读取下一个 NAL 单元
    ///
    /// 解析逻辑:
    /// 1. 逐字节读取数据到缓冲区
    /// 2. 检测 00 00 01 起始码
    /// 3. 第一个起始码标记 NAL 开始
    /// 4. 第二个起始码标记 NAL 结束
    /// 5. 提取中间的 NAL 数据并返回
    pub async fn read_frame(&mut self, _with_meta: bool) -> Result<Option<VideoFrame>> {
        loop {
            // 逐字节读取
            let mut byte = [0u8; 1];
            self.stream.read_exact(&mut byte).await?;
            self.buffer.extend_from_slice(&byte);

            // 检查 3-byte 起始码 00 00 01
            let buf_len = self.buffer.len();
            if buf_len >= 3 {
                let last_3 = &self.buffer[buf_len - 3..];

                if last_3 == [0x00, 0x00, 0x01] {
                    if self.first_start_code_pos.is_none() {
                        // 记录第一个起始码位置
                        self.first_start_code_pos = Some(buf_len - 3);
                    } else {
                        // 提取 NAL 单元
                        let start = self.first_start_code_pos.unwrap() + 3;
                        let end = buf_len - 3;
                        let nal_data = self.buffer[start..end].to_vec();

                        // 解析 NAL 类型
                        let nal_type = nal_data[0] & 0x1F;
                        let frame_type = match nal_type {
                            7 | 8 => FrameType::Config,  // SPS/PPS
                            _ => FrameType::Video,
                        };

                        return Ok(Some(VideoFrame::new(0, frame_type, Bytes::from(nal_data))));
                    }
                }
            }
        }
    }
}
```

### 6.3 NAL 单元解析流程图

```
输入数据流:
... 00 00 01 [SPS数据] 00 00 01 [PPS数据] 00 00 01 [IDR数据] 00 00 01 ...
    ↑        ↑         ↑        ↑         ↑        ↑         ↑
    │        │         │        │         │        │         │
    │        │         │        │         │        │         │
    起始码1   NAL1结束   起始码2   NAL2结束   起始码3   NAL3结束   起始码4
             NAL1提取            NAL2提取            NAL3提取

解析状态机:
┌─────────────┐    检测到 00 00 01    ┌─────────────┐
│  等待第一个   │─────────────────────►│  等待第二个  │
│   起始码     │                      │   起始码     │
└─────────────┘                      └──────┬──────┘
                                             │
                                             │ 检测到 00 00 01
                                             ▼
                                      ┌─────────────┐
                                      │  提取NAL     │
                                      │  返回帧      │
                                      └──────┬──────┘
                                             │
                                             │ 循环
                                             ▼
                                      ┌─────────────┐
                                      │  等待下一个   │
                                      │   起始码     │
                                      └─────────────┘
```

### 6.4 SPS 解析获取分辨率

```rust
// src/main.rs - SPS 解析器
struct BitReader<'a> {
    data: &'a [u8],
    byte_offset: usize,
    bit_offset: u8,
}

impl BitReader {
    /// 读取 Exp-Golomb 编码的无符号整数 (ue(v))
    fn read_ue(&mut self) -> Option<u32>;

    /// 读取 Exp-Golomb 编码的有符号整数 (se(v))
    fn read_se(&mut self) -> Option<i32>;
}

fn parse_sps_resolution(sps_data: &[u8]) -> Option<(u32, u32)> {
    // SPS 结构 (简化):
    // - NAL header (1 byte)
    // - profile_idc (8 bits)
    // - constraint_flags (8 bits)
    // - level_idc (8 bits)
    // - seq_parameter_set_id (ue(v))
    // - [High Profile specific data]
    // - log2_max_frame_num_minus4 (ue(v))
    // - pic_order_cnt_type (ue(v))
    // - ...
    // - pic_width_in_mbs_minus1 (ue(v))      ← 宽度
    // - pic_height_in_map_units_minus1 (ue(v)) ← 高度
    // - frame_mbs_only_flag (1 bit)
    // - frame_cropping_flag (1 bit)
    // - [cropping offsets]

    // 计算分辨率:
    // width = (pic_width_in_mbs_minus1 + 1) * 16 - crop_left - crop_right
    // height = (pic_height_in_map_units_minus1 + 1) * 16 * (2 - frame_mbs_only_flag)
    //          - crop_top - crop_bottom
}
```

---

## 7. 控制流处理

### 7.1 控制消息类型

```rust
// src/scrcpy/control.rs
#[repr(u8)]
pub enum ControlMessageType {
    InjectKeycode = 0,            // 按键事件
    InjectText = 1,               // 文本输入
    InjectTouch = 2,              // 触摸事件
    InjectScroll = 3,             // 滚动事件
    SetScreenPowerMode = 4,       // 屏幕电源控制
    ExpandNotificationPanel = 5,  // 展开通知栏
    CollapseNotificationPanel = 6,// 收起通知栏
    GetClipboard = 7,             // 获取剪贴板
    SetClipboard = 8,             // 设置剪贴板
    RotateDevice = 10,            // 旋转设备
    // ... 更多类型可以翻翻源码
}
```

### 7.2 触摸事件协议 (32 字节)

```
┌─────────────────────────────────────────────────────────────────────────┐
│                    Touch Event Message Format (32 bytes)                │
├─────────────────────────────────────────────────────────────────────────┤
│                                                                         │
│  Offset │ Size │ Field         │ Type      │ Description                │
│  ───────┼──────┼───────────────┼───────────┼─────────────────────────── │
│    0    │  1   │ type          │ u8        │ = 2 (InjectTouch)          │
│    1    │  1   │ action        │ u8        │ 0=Down, 1=Up, 2=Move       │
│    2    │  8   │ pointer_id    │ i64 BE    │ -1=鼠标, >=0=触摸           │
│   10    │  4   │ x             │ u32 BE    │ 像素坐标                    │
│   14    │  4   │ y             │ u32 BE    │ 像素坐标                    │
│   18    │  2   │ width         │ u16 BE    │ 屏幕宽度                    │
│   20    │  2   │ height        │ u16 BE    │ 屏幕高度                    │
│   22    │  2   │ pressure      │ u16 BE    │ 0x0000-0xFFFF (0.0-1.0)    │
│   24    │  4   │ action_button │ u32 BE    │ 鼠标=1, 触摸=0              │
│   28    │  4   │ buttons       │ u32 BE    │ 按钮状态                    │
│  ───────┴──────┴───────────────┴───────────┴─────────────────────────── │
│                                                                         │
│  示例 (鼠标点击 DOWN):                                                    │
│  [02, 00, ff, ff, ff, ff, ff, ff, ff, ff,    ← type=2, action=0, id=-1  │
│   00, 00, 01, 2c, 00, 00, 02, 58,            ← x=300, y=600             │
│   04, 38, 07, 80,                            ← width=1080, height=1920  │
│   ff, ff,                                    ← pressure=1.0             │
│   00, 00, 00, 01,                            ← action_button=1          │
│   00, 00, 00, 01]                            ← buttons=1                │
│                                                                         │
└─────────────────────────────────────────────────────────────────────────┘
```

### 7.3 触摸动作枚举

```rust
#[repr(u8)]
pub enum AndroidMotionEventAction {
    Down = 0,        // ACTION_DOWN - 第一个手指按下
    Up = 1,          // ACTION_UP - 最后一个手指抬起
    Move = 2,        // ACTION_MOVE - 手指移动
    Cancel = 3,      // ACTION_CANCEL - 事件取消
    PointerDown = 5, // ACTION_POINTER_DOWN - 非第一个手指按下
    PointerUp = 6,   // ACTION_POINTER_UP - 非最后一个手指抬起
    HoverMove = 7,   // ACTION_HOVER_MOVE - 鼠标悬停移动
    HoverEnter = 9,  // ACTION_HOVER_ENTER - 鼠标进入
    HoverExit = 10,  // ACTION_HOVER_EXIT - 鼠标离开
}
```

### 7.4 按键事件协议 (14 字节)

```
┌─────────────────────────────────────────────────────────────────────────┐
│                    Key Event Message Format (14 bytes)                  │
├─────────────────────────────────────────────────────────────────────────┤
│                                                                         │
│  Offset │ Size │ Field     │ Type    │ Description                      │
│  ───────┼──────┼───────────┼─────────┼───────────────────────────────── │
│    0    │  1   │ type      │ u8      │ = 0 (InjectKeycode)              │
│    1    │  1   │ action    │ u8      │ 0=Down, 1=Up                     │
│    2    │  4   │ keycode   │ u32 BE  │ Android KeyCode                  │
│    6    │  4   │ repeat    │ u32 BE  │ 重复次数                          │
│   10    │  4   │ metastate │ u32 BE  │ 修饰键状态 (Shift/Ctrl/Alt)        │
│                                                                         │
│  常用 Android KeyCode:                                                   │
│    KEYCODE_BACK = 4       ← 返回键                                       │
│    KEYCODE_HOME = 3       ← Home键                                      │
│    KEYCODE_VOLUME_UP = 24                                               │
│    KEYCODE_VOLUME_DOWN = 25                                             │
│    KEYCODE_POWER = 26                                                   │
│                                                                         │
└─────────────────────────────────────────────────────────────────────────┘
```

### 7.5 滚动事件协议 (21 字节)

```
┌─────────────────────────────────────────────────────────────────────────┐
│                   Scroll Event Message Format (21 bytes)                │
├─────────────────────────────────────────────────────────────────────────┤
│                                                                         │
│  Offset │ Size │ Field   │ Type    │ Description                        │
│  ───────┼──────┼─────────┼─────────┼─────────────────────────────────── │
│    0    │  1   │ type    │ u8      │ = 3 (InjectScroll)                 │
│    1    │  4   │ x       │ i32 BE  │ 像素坐标                            │
│    5    │  4   │ y       │ i32 BE  │ 像素坐标                            │
│    9    │  2   │ width   │ u16 BE  │ 屏幕宽度                            │
│   11    │  2   │ height  │ u16 BE  │ 屏幕高度                            │
│   13    │  2   │ hscroll │ i16 BE  │ 水平滚动量 (×2048)                  │
│   15    │  2   │ vscroll │ i16 BE  │ 垂直滚动量 (×2048)                  │
│   17    │  4   │ buttons │ u32 BE  │ 按钮状态                            │
│                                                                         │
│  滚动值编码 (根据官方 scrcpy 抓包分析):                                     │
│    向下滚动: vscroll = -2048 (0xf800)                                    │
│    向上滚动: vscroll = +2048 (0x0800)                                    │
│    前端传入 -1/0/1，后端需要乘以 2048 转换                                  │
│                                                                         │
│  示例 (向下滚动):                                                         │
│  [03, 00, 00, 04, 27, 00, 00, 03, 70,    ← type=3, x=1063, y=880        │
│   05, a0, 0b, 90,                        ← width=1440, height=2960      │
│   00, 00, f8, 00,                        ← hscroll=0, vscroll=-2048     │
│   00, 00, 00, 00]                        ← buttons=0                    │
│                                                                         │
└─────────────────────────────────────────────────────────────────────────┘
```

### 7.6 ControlChannel 实现

```rust
// src/scrcpy/control.rs
pub struct ControlChannel {
    stream: TcpStream,  // TCP 连接到 scrcpy-server
}

impl ControlChannel {
    /// 发送触摸事件
    pub async fn send_touch_event(&mut self, event: &TouchEvent) -> Result<()> {
        let mut msg = Vec::with_capacity(32);

        // 构建消息 (所有多字节字段使用大端序)
        msg.push(ControlMessageType::InjectTouch as u8);  // type
        msg.push(event.action as u8);                      // action
        msg.extend_from_slice(&event.pointer_id.to_be_bytes());  // pointer_id (i64)

        // 计算像素坐标 (归一化坐标 × 屏幕尺寸)
        let x_fixed = (event.x * event.width as f32) as u32;
        let y_fixed = (event.y * event.height as f32) as u32;
        msg.extend_from_slice(&x_fixed.to_be_bytes());
        msg.extend_from_slice(&y_fixed.to_be_bytes());

        msg.extend_from_slice(&(event.width as u16).to_be_bytes());
        msg.extend_from_slice(&(event.height as u16).to_be_bytes());

        // pressure: 16位定点数 (0.0 → 0x0000, 1.0 → 0xFFFF)
        let pressure_u16 = (event.pressure * 0xFFFF as f32) as u16;
        msg.extend_from_slice(&pressure_u16.to_be_bytes());

        // action_button 和 buttons 的特殊处理
        let action_button = if event.pointer_id == -1 { 1u32 } else { 0u32 };
        let buttons = match event.action {
            Up | PointerUp => 0u32,
            _ => event.buttons,
        };
        msg.extend_from_slice(&action_button.to_be_bytes());
        msg.extend_from_slice(&buttons.to_be_bytes());

        // 发送并刷新
        self.stream.write_all(&msg).await?;
        self.stream.flush().await?;

        Ok(())
    }
}
```

### 7.7 文本注入协议

```
┌─────────────────────────────────────────────────────────────────────────┐
│                    Text Inject Message Format                           │
├─────────────────────────────────────────────────────────────────────────┤
│                                                                         │
│  Offset │ Size │ Field   │ Type      │ Description                      │
│  ───────┼──────┼─────────┼───────────┼───────────────────────────────── │
│    0    │  1   │ type    │ u8        │ = 1 (InjectText)                 │
│    1    │  4   │ length  │ u32 BE    │ 文本字节长度                      │
│    5    │  N   │ text    │ [u8; N]   │ UTF-8 编码的文本内容              │
│                                                                         │
└─────────────────────────────────────────────────────────────────────────┘
```

### 7.8 剪贴板设置协议

```
┌─────────────────────────────────────────────────────────────────────────┐
│                   Set Clipboard Message Format                          │
├─────────────────────────────────────────────────────────────────────────┤
│                                                                         │
│  Offset │ Size │ Field    │ Type      │ Description                     │
│  ───────┼──────┼──────────┼───────────┼──────────────────────────────── │
│    0    │  1   │ type     │ u8        │ = 8 (SetClipboard)              │
│    1    │  8   │ sequence │ u64 BE    │ 同步序列号 (通常为0)             │
│    9    │  1   │ paste    │ u8        │ 0=仅设置, 1=设置并粘贴           │
│   10    │  4   │ length   │ u32 BE    │ 文本字节长度                     │
│   14    │  N   │ text     │ [u8; N]   │ UTF-8 编码的文本内容             │
│                                                                         │
└─────────────────────────────────────────────────────────────────────────┘
```

### 7.9 统一控制事件类型

为了支持多种控制事件，使用统一的枚举类型：

```rust
// src/scrcpy/control.rs

// 文本输入事件
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextEvent {
    pub text: String,
}

// 剪贴板事件
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClipboardEvent {
    pub text: String,
    pub paste: bool,  // 是否同时模拟粘贴操作
}

// 统一的控制事件类型
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ControlEvent {
    #[serde(rename = "touch")]
    Touch(TouchEvent),
    #[serde(rename = "key")]
    Key(KeyEvent),
    #[serde(rename = "text")]
    Text(TextEvent),
    #[serde(rename = "clipboard")]
    Clipboard(ClipboardEvent),
    #[serde(rename = "scroll")]
    Scroll(ScrollEvent),
}
```

### 7.10 键盘输入支持

前端 JavaScript 键盘映射表：

```javascript
const KEY_MAP = {
    // 字母键 A-Z (Android: KEYCODE_A=29 到 KEYCODE_Z=54)
    'KeyA': 29, 'KeyB': 30, 'KeyC': 31, /* ... */ 'KeyZ': 54,

    // 数字键 0-9 (Android: KEYCODE_0=7 到 KEYCODE_9=16)
    'Digit0': 7, 'Digit1': 8, /* ... */ 'Digit9': 16,

    // 功能键
    'Enter': 66,        // KEYCODE_ENTER
    'Backspace': 67,    // KEYCODE_DEL
    'Delete': 112,      // KEYCODE_FORWARD_DEL
    'Tab': 61,          // KEYCODE_TAB
    'Space': 62,        // KEYCODE_SPACE
    'Escape': 111,      // KEYCODE_ESCAPE

    // 方向键
    'ArrowUp': 19,      // KEYCODE_DPAD_UP
    'ArrowDown': 20,    // KEYCODE_DPAD_DOWN
    'ArrowLeft': 21,    // KEYCODE_DPAD_LEFT
    'ArrowRight': 22,   // KEYCODE_DPAD_RIGHT

    // 符号键
    'Comma': 55, 'Period': 56, 'Slash': 76, /* ... */
};

// 修饰键状态
const META_SHIFT = 1;
const META_CTRL = 4096;
const META_ALT = 2;
```

### 7.11 剪贴板粘贴功能

前端支持两种粘贴方式：

```javascript
// 方式1: Ctrl+V 快捷键
document.addEventListener('keydown', (e) => {
    if (e.ctrlKey && e.code === 'KeyV') {
        e.preventDefault();
        handlePaste();
    }
});

// 方式2: 粘贴事件
document.addEventListener('paste', (e) => {
    e.preventDefault();
    const text = e.clipboardData.getData('text');
    if (text) sendText(text);
});

// 粘贴处理函数
async function handlePaste() {
    const text = await navigator.clipboard.readText();
    if (text) {
        sendText(text);  // 使用 InjectText 直接输入
    }
}
```

### 7.12 鼠标滚轮支持

前端监听滚轮事件，将滚动量发送到后端：

```javascript
// 滚动事件结构
{
    type: 'scroll',
    x: 0.5,           // 归一化坐标 [0, 1]
    y: 0.3,           // 归一化坐标 [0, 1]
    width: 1920,      // 视频宽度
    height: 1080,     // 视频高度
    hscroll: 0,       // 水平滚动量 (-1, 0, 1)
    vscroll: -1       // 垂直滚动量 (-1=向下, 1=向上)
}

// 滚轮事件处理
function handleWheel(e) {
    e.preventDefault();

    const coords = normalizeCoords(e.clientX, e.clientY);

    // deltaY > 0 表示向下滚动，对应 vscroll < 0
    const vscroll = e.deltaY > 0 ? -1 : (e.deltaY < 0 ? 1 : 0);
    const hscroll = e.deltaX > 0 ? -1 : (e.deltaX < 0 ? 1 : 0);

    if (vscroll !== 0 || hscroll !== 0) {
        sendScrollEvent(coords.x, coords.y, hscroll, vscroll);
    }
}

canvas.addEventListener('wheel', handleWheel, { passive: false });
```

后端 ScrollEvent 结构：

```rust
// src/scrcpy/control.rs
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScrollEvent {
    pub x: f32,           // 归一化坐标 [0, 1]
    pub y: f32,           // 归一化坐标 [0, 1]
    pub width: u32,       // 视频宽度
    pub height: u32,      // 视频高度
    pub hscroll: i32,     // 水平滚动量
    pub vscroll: i32,     // 垂直滚动量
}
```

---

## 7.5 屏幕旋转自动适配

### 7.5.1 功能概述

当手机屏幕旋转（如播放视频全屏）时，系统会自动检测分辨率变化并通知前端调整 canvas 布局。

### 7.5.2 后端检测机制

```rust
// src/main.rs - SPS 解析时检测旋转
if let Some((width, height)) = parse_sps_resolution(&frame.data) {
    let new_is_landscape = width > height;
    let resolution_changed = config.width != width || config.height != height;
    let orientation_changed = config.is_landscape != new_is_landscape;

    if resolution_changed || orientation_changed {
        config.width = width;
        config.height = height;
        config.is_landscape = new_is_landscape;

        // 广播配置更新给所有客户端
        let config_msg = format!(
            r#"{{"type":"config","width":{},"height":{},"is_landscape":{}}}"#,
            width, height, new_is_landscape
        );
        config_sender.send(config_msg);
    }
}
```

### 7.5.3 VideoConfig 结构

```rust
// src/ws/server.rs
pub struct VideoConfig {
    pub sps: Option<Bytes>,
    pub pps: Option<Bytes>,
    pub width: u32,           // 视频流分辨率
    pub height: u32,
    pub device_width: u32,    // 设备物理分辨率
    pub device_height: u32,
    pub is_landscape: bool,   // 是否为横屏模式
}
```

### 7.5.4 前端自适应布局

```javascript
let isLandscape = false;

function resizeCanvas() {
    if (videoWidth > 0 && videoHeight > 0) {
        const videoRatio = videoWidth / videoHeight;
        const windowRatio = window.innerWidth / window.innerHeight;

        // 根据视频和窗口的宽高比来决定如何适配
        if (videoRatio > windowRatio) {
            // 横屏视频在窄窗口，按宽度填满
            canvas.style.width = '100vw';
            canvas.style.height = `${window.innerWidth / videoRatio}px`;
        } else {
            // 竖屏视频，按高度填满
            canvas.style.height = '100vh';
            canvas.style.width = `${window.innerHeight * videoRatio}px`;
        }
    }
}

// 处理配置消息
ws.onmessage = (event) => {
    if (typeof event.data === 'string') {
        const msg = JSON.parse(event.data);
        if (msg.type === 'config') {
            videoWidth = msg.width;
            videoHeight = msg.height;
            isLandscape = msg.is_landscape || false;

            canvas.width = msg.width;
            canvas.height = msg.height;
            resizeCanvas();  // 重新调整布局
        }
    }
};
```

### 7.5.5 配置变化广播机制

```rust
// src/ws/server.rs - 添加配置广播通道
pub struct WebSocketServer {
    tx: broadcast::Sender<Bytes>,           // 视频帧广播
    config_tx: broadcast::Sender<String>,   // 配置变化广播
    // ...
}

// handle_client 中监听配置变化
loop {
    tokio::select! {
        // 接收配置变化并发送给客户端
        config_result = config_rx.recv() => {
            if let Ok(config_msg) = config_result {
                socket.send(Message::Text(config_msg)).await;
            }
        }

        // 接收视频帧并发送
        frame_result = rx.recv() => { /* ... */ }

        // 监听客户端消息
        msg = socket.recv() => { /* ... */ }
    }
}
```

---

## 8. WebSocket通信

### 8.1 WebSocket 服务器架构

```rust
// src/ws/server.rs
pub struct WebSocketServer {
    addr: SocketAddr,
    tx: broadcast::Sender<Bytes>,           // 视频帧广播通道
    video_config: Arc<RwLock<VideoConfig>>, // SPS/PPS 缓存
    idr_request_tx: mpsc::Sender<()>,       // IDR 请求通道
    control_tx: mpsc::Sender<TouchEvent>,   // 控制事件通道
}

pub struct VideoConfig {
    pub sps: Option<Bytes>,       // 缓存的 SPS
    pub pps: Option<Bytes>,       // 缓存的 PPS
    pub width: u32,               // 视频流分辨率
    pub height: u32,
    pub device_width: u32,        // 设备物理分辨率 (用于触控)
    pub device_height: u32,
}
```

### 8.2 WebSocket 消息协议

```
┌─────────────────────────────────────────────────────────────────────────┐
│                     WebSocket Message Protocol                          │
├─────────────────────────────────────────────────────────────────────────┤
│                                                                         │
│  服务器 → 客户端:                                                        │
│  ────────────────                                                       │
│                                                                         │
│  1. 配置消息 (Text/JSON):                                                │
│     {                                                                   │
│       "type": "config",                                                 │
│       "width": 1920,          ← 视频流分辨率 (用于canvas)                │
│       "height": 1080,                                                   │
│       "device_width": 1080,   ← 设备物理分辨率 (用于触控)                 │
│       "device_height": 1920                                             │
│     }                                                                   │
│                                                                         │
│  2. 视频帧 (Binary):                                                    │
│     [00 00 00 01] [NAL Unit Data]                                       │
│     └───起始码───┘ └──H.264 数据──┘                                       │
│                                                                         │
│                                                                         │
│  客户端 → 服务器:                                                        │
│  ────────────────                                                       │
│                                                                         │
│  1. 触控事件 (Text/JSON):                                                │
│     {                                                                   │
│       "action": 0,            ← 0=Down, 1=Up, 2=Move                    │
│       "pointer_id": -1,       ← -1=鼠标, >=0=触摸                        │
│       "x": 0.5,               ← 归一化坐标 [0, 1]                        │
│       "y": 0.3,               ← 归一化坐标 [0, 1]                        │
│       "pressure": 1.0,        ← 压力 [0, 1]                             │
│       "width": 1920,          ← 视频流宽度                               │
│       "height": 1080,         ← 视频流高度                               │
│       "buttons": 1            ← 按钮状态                                 │
│     }                                                                   │
│                                                                         │
└─────────────────────────────────────────────────────────────────────────┘
```

### 8.3 客户端连接处理流程

```rust
async fn handle_client(
    mut socket: WebSocket,
    tx: broadcast::Sender<Bytes>,
    video_config: Arc<RwLock<VideoConfig>>,
    idr_request_tx: mpsc::Sender<()>,
    control_tx: mpsc::Sender<TouchEvent>,
) {
    // 1. 请求 IDR 帧 (确保新客户端能立即解码)
    idr_request_tx.send(()).await;

    // 2. 发送配置信息
    let config = video_config.read().await;
    let config_msg = format!(r#"{{"type":"config","width":{},"height":{},...}}"#, ...);
    socket.send(Message::Text(config_msg)).await;

    // 3. 发送缓存的 SPS/PPS
    if let Some(sps) = &config.sps {
        socket.send(Message::Binary(sps.to_vec())).await;
    }
    if let Some(pps) = &config.pps {
        socket.send(Message::Binary(pps.to_vec())).await;
    }

    // 4. 订阅视频帧广播
    let mut rx = tx.subscribe();

    // 5. 主循环: 转发视频帧 + 处理控制事件
    loop {
        tokio::select! {
            // 接收视频帧并转发
            frame_result = rx.recv() => {
                match frame_result {
                    Ok(frame) => socket.send(Message::Binary(frame.to_vec())).await,
                    Err(Lagged(_)) => {
                        // 追帧: 丢弃积压的旧帧,跳到最新
                        while let Ok(latest) = rx.try_recv() {
                            socket.send(Message::Binary(latest.to_vec())).await;
                        }
                    }
                }
            }

            // 接收客户端消息
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        // 解析触控事件并转发
                        let touch_event: TouchEvent = serde_json::from_str(&text)?;
                        control_tx.send(touch_event).await;
                    }
                    Some(Ok(Message::Close(_))) => break,
                }
            }
        }
    }
}
```

### 8.4 追帧策略

```
当客户端处理速度慢于视频帧率时,广播通道会积压帧:

┌─────────────────────────────────────────────────────────────────────────┐
│                         追帧 (Frame Catching Up)                        │
├─────────────────────────────────────────────────────────────────────────┤
│                                                                         │
│  广播通道:  [帧1] [帧2] [帧3] [帧4] [帧5] [帧6] ... [帧N]                 │
│              ↑                                        ↑                 │
│              │                                        │                 │
│           客户端位置                               最新帧                │
│           (滞后)                                                        │
│                                                                         │
│  当检测到 RecvError::Lagged 时:                                          │
│                                                                         │
│  1. 进入追帧模式                                                         │
│  2. 使用 try_recv() 快速消费所有积压帧                                    │
│  3. 只发送最新的几帧给客户端                                              │
│  4. 跳过中间的旧帧以减少延迟                                              │
│                                                                         │
│  效果: 牺牲流畅性换取低延迟                                               │
│                                                                         │
└─────────────────────────────────────────────────────────────────────────┘
```

---

## 9. 前端解码与渲染

### 9.0 多解码器架构

系统支持三种解码器，按优先级自动降级：

| 解码器     | 优先级 | 特点                     | 兼容性                    |
| ---------- | ------ | ------------------------ | ------------------------- |
| WebCodecs  | 1      | 硬件加速，CPU 占用极低   | Chrome 94+, Edge 94+      |
| JMuxer     | 2      | MSE，浏览器原生解码      | 支持 MSE 的现代浏览器     |
| Broadway   | 3      | 纯 JS 软解码，兼容性最好 | 几乎所有浏览器            |

用户可通过 URL 参数指定解码器：`?decoder=webcodecs` / `?decoder=jmuxer` / `?decoder=broadway`

```javascript
// 解码器管理器 - 自动检测和降级
const DecoderManager = {
    getBestDecoder() {
        if (WebCodecsDecoder.isSupported()) return 'webcodecs';
        if (JMuxerDecoder.isSupported()) return 'jmuxer';
        if (BroadwayDecoder.isSupported()) return 'broadway';
        return null;
    }
};
```

### 9.1 WebCodecs 解码流程

```javascript
// 初始化解码器
decoder = new VideoDecoder({
    output: (frame) => {
        // 绘制到 canvas
        ctx.drawImage(frame, 0, 0, canvas.width, canvas.height);
        frame.close();
    },
    error: (e) => console.error('Decoder error:', e)
});

// 配置解码器
decoder.configure({
    codec: 'avc1.42001E',          // H.264 Baseline Profile Level 3.0
    optimizeForLatency: true,       // 优化延迟
    hardwareAcceleration: 'prefer-hardware',  // 优先硬件加速
});
```

### 9.2 NAL 单元处理逻辑

```javascript
ws.onmessage = (event) => {
    if (event.data instanceof ArrayBuffer) {
        const data = new Uint8Array(event.data);

        // 解析 NAL 类型 (跳过 00 00 00 01 起始码)
        const nalType = data[4] & 0x1F;

        switch (nalType) {
            case 7:  // SPS
                cachedSPS = data;
                return;  // 不立即解码,等待 IDR

            case 8:  // PPS
                cachedPPS = data;
                return;  // 不立即解码,等待 IDR

            case 5:  // IDR (关键帧)
                // 合并 SPS + PPS + IDR
                const combined = new Uint8Array(
                    cachedSPS.length + cachedPPS.length + data.length
                );
                combined.set(cachedSPS, 0);
                combined.set(cachedPPS, cachedSPS.length);
                combined.set(data, cachedSPS.length + cachedPPS.length);

                // 作为关键帧解码
                decoder.decode(new EncodedVideoChunk({
                    type: 'key',
                    timestamp: performance.now() * 1000,
                    data: combined
                }));
                break;

            default:  // P/B 帧
                // 限制解码器队列,防止积压
                if (decoder.decodeQueueSize > 3) {
                    console.warn('Dropping P-frame');
                    return;
                }

                decoder.decode(new EncodedVideoChunk({
                    type: 'delta',
                    timestamp: performance.now() * 1000,
                    data: data
                }));
        }
    }
};
```

### 9.3 触控事件处理

```javascript
// 坐标转换: Canvas像素 → 归一化坐标 [0, 1]
function normalizeCoords(canvasX, canvasY) {
    const rect = canvas.getBoundingClientRect();
    const x = (canvasX - rect.left) / rect.width;
    const y = (canvasY - rect.top) / rect.height;
    return {
        x: Math.max(0, Math.min(1, x)),
        y: Math.max(0, Math.min(1, y))
    };
}

// 发送触控事件
function sendTouchEvent(action, pointerId, x, y, pressure = 1.0) {
    const event = {
        action: action,           // 0=Down, 1=Up, 2=Move
        pointer_id: pointerId,    // -1=鼠标, >=0=触摸
        x: x,                     // 归一化坐标
        y: y,
        pressure: pressure,
        width: videoWidth,        // 视频流分辨率
        height: videoHeight,
        buttons: action === 1 ? 0 : 1  // UP时buttons=0
    };

    ws.send(JSON.stringify(event));
}

// 鼠标事件 (PC)
canvas.addEventListener('mousedown', (e) => {
    const coords = normalizeCoords(e.clientX, e.clientY);
    sendTouchEvent(0, -1, coords.x, coords.y);  // ACTION_DOWN, MOUSE_ID=-1
});

// 触摸事件 (移动端)
canvas.addEventListener('touchstart', (e) => {
    for (let touch of e.changedTouches) {
        const coords = normalizeCoords(touch.clientX, touch.clientY);
        sendTouchEvent(0, touch.identifier, coords.x, coords.y, touch.force || 1.0);
    }
});
```

---

## 10. 数据流转图

### 10.1 视频流数据流转

```
┌─────────────────────────────────────────────────────────────────────────┐
│                           视频流数据流转                                  │
├─────────────────────────────────────────────────────────────────────────┤
│                                                                         │
│  Android 设备                                                            │
│  ┌─────────────────────────────────────────────────────────────────┐    │
│  │  Surface → MediaCodec (H.264 编码) → NAL Units                   │   │
│  └──────────────────────────────────┬──────────────────────────────┘   │
│                                     │                                  │
│                                     │ Annex-B NAL Stream               │
│                                     │ (00 00 00 01 + NAL Data)         │
│                                     ▼                                  │
│  ┌─────────────────────────────────────────────────────────────────┐   │
│  │  scrcpy-server → localabstract:scrcpy                           │   │
│  └──────────────────────────────────┬──────────────────────────────┘   │
│                                     │                                  │
│ ════════════════════════════════════╪═══════════════════════════════   │
│  ADB Forward (USB/WiFi)             │                                  │
│ ════════════════════════════════════╪═══════════════════════════════   │
│                                     │                                  │
│  PC 端                              ▼                                  │
│  ┌─────────────────────────────────────────────────────────────────┐   │
│  │  TCP:27183 → VideoStreamReader                                  │   │
│  │                                                                 │   │
│  │  ┌──────────────────────────────────────────────────────────┐   │   │
│  │  │  逐字节读取 → 检测起始码 → 提取NAL单元 → VideoFrame           │   │   │
│  │  └────────────────────────────┬─────────────────────────────┘   │   │
│  └───────────────────────────────┼─────────────────────────────────┘   │
│                                  │                                     │
│                                  │ VideoFrame { pts, frame_type, data }│
│                                  ▼                                     │
│  ┌─────────────────────────────────────────────────────────────────┐   │
│  │  Main Event Loop                                                │   │
│  │                                                                 │   │
│  │  if SPS/PPS → 缓存到 video_config                                │   │
│  │  添加起始码 [00 00 00 01] + NAL Data                              │   │
│  │  → broadcast::Sender<Bytes>                                     │   │
│  └───────────────────────────────┬─────────────────────────────────┘   │
│                                  │                                     │
│                                  │ Bytes (带起始码的NAL)                │
│                                  ▼                                     │
│  ┌─────────────────────────────────────────────────────────────────┐   │
│  │  WebSocketServer                                                │   │
│  │                                                                 │   │
│  │  broadcast::Receiver → Message::Binary                          │   │
│  │  → WebSocket 发送到所有客户端                                      │   │
│  └───────────────────────────────┬─────────────────────────────────┘   │
│                                  │                                     │
│ ═════════════════════════════════╪══════════════════════════════════   │
│  WebSocket (ws://)               │                                     │
│ ═════════════════════════════════╪══════════════════════════════════   │
│                                  │                                     │
│  浏览器                           ▼                                     │
│  ┌─────────────────────────────────────────────────────────────────┐   │
│  │  JavaScript WebCodecs                                           │   │
│  │                                                                 │   │
│  │  ArrayBuffer → 解析NAL类型 → 缓存SPS/PPS                          │   │
│  │  → IDR时合并(SPS+PPS+IDR) → EncodedVideoChunk                    │   │
│  │  → VideoDecoder.decode()                                        │   │
│  │  → VideoFrame → Canvas.drawImage()                              │   │
│  └─────────────────────────────────────────────────────────────────┘   │
│                                                                        │
└────────────────────────────────────────────────────────────────────────┘
```

### 10.2 控制流数据流转

```
┌─────────────────────────────────────────────────────────────────────────┐
│                           控制流数据流转                                  │
├─────────────────────────────────────────────────────────────────────────┤
│                                                                         │
│  浏览器                                                                  │
│  ┌─────────────────────────────────────────────────────────────────┐    │
│  │  用户操作 (点击/滑动)                                              │   │
│  │  │                                                              │   │
│  │  ▼                                                              │   │
│  │  normalizeCoords() → 归一化坐标 [0,1]                             │   │
│  │  │                                                              │   │
│  │  ▼                                                              │   │
│  │  JSON.stringify({action, pointer_id, x, y, pressure, ...})      │   │
│  │  │                                                              │   │
│  │  ▼                                                              │   │
│  │  WebSocket.send(text)                                           │   │
│  └───────────────────────────────┬─────────────────────────────────┘   │
│                                  │                                     │
│ ═════════════════════════════════╪══════════════════════════════════   │
│  WebSocket (ws://)               │  JSON Text                          │
│ ═════════════════════════════════╪══════════════════════════════════   │
│                                  │                                     │
│  PC 端                           ▼                                     │
│  ┌─────────────────────────────────────────────────────────────────┐   │
│  │  WebSocketServer::handle_client()                               │   │
│  │  │                                                              │   │
│  │  ▼                                                              │   │
│  │  Message::Text(json) → serde_json::from_str::<TouchEvent>()     │   │
│  │  │                                                              │   │
│  │  ▼                                                              │   │
│  │  control_tx.send(touch_event)                                   │   │
│  └───────────────────────────────┬─────────────────────────────────┘   │
│                                  │                                      │
│                                  │ mpsc::Sender<TouchEvent>             │
│                                  ▼                                     │
│  ┌─────────────────────────────────────────────────────────────────┐   │
│  │  Main Event Loop                                                │   │
│  │  │                                                              │   │
│  │  ▼                                                              │   │
│  │  control_rx.recv() → TouchEvent                                 │   │
│  │  │                                                              │   │
│  │  ▼                                                              │   │
│  │  control_channel.send_touch_event(&event)                       │   │
│  └───────────────────────────────┬─────────────────────────────────┘   │
│                                  │                                     │
│                                  │ TouchEvent                          │
│                                  ▼                                     │
│  ┌─────────────────────────────────────────────────────────────────┐   │
│  │  ControlChannel::send_touch_event()                             │   │
│  │  │                                                              │   │
│  │  ▼                                                              │   │
│  │  构建 32 字节二进制消息 (Big Endian)                               │   │
│  │  [type][action][pointer_id][x][y][w][h][pressure][ab][buttons]  │   │
│  │  │                                                              │   │
│  │  ▼                                                              │   │
│  │  TCP write_all() + flush()                                      │   │
│  └───────────────────────────────┬─────────────────────────────────┘   │
│                                  │                                     │
│ ═════════════════════════════════╪══════════════════════════════════   │
│  ADB Forward TCP:27184           │  Binary (32 bytes)                  │
│ ═════════════════════════════════╪══════════════════════════════════   │
│                                  │                                     │
│  Android 设备                    ▼                                      │
│  ┌─────────────────────────────────────────────────────────────────┐   │
│  │  scrcpy-server                                                  │   │
│  │  │                                                              │   │
│  │  ▼                                                              │   │
│  │  解析控制消息 → InputManager.injectInputEvent()                   │   │
│  │  │                                                              │   │
│  │  ▼                                                              │   │
│  │  Android 系统触摸事件 → 应用响应                                   │    │
│  └─────────────────────────────────────────────────────────────────┘   │
│                                                                        │
└────────────────────────────────────────────────────────────────────────┘
```

---

## 11. 关键技术细节

### 11.1 字节序处理

scrcpy 协议使用 **大端序 (Big Endian)**：

```rust
// Rust 中的大端序转换
let x: u32 = 300;
let bytes = x.to_be_bytes();  // [0x00, 0x00, 0x01, 0x2C]

let pointer_id: i64 = -1;
let bytes = pointer_id.to_be_bytes();  // [0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]
```

### 11.2 压力值与滚动值编码

**压力值**使用 16 位定点数：

```rust
// float [0.0, 1.0] → u16 [0x0000, 0xFFFF]
let pressure: f32 = 1.0;
let pressure_u16 = (pressure * 0xFFFF as f32) as u16;  // 0xFFFF

let pressure: f32 = 0.0;
let pressure_u16 = (pressure * 0xFFFF as f32) as u16;  // 0x0000

let pressure: f32 = 0.5;
let pressure_u16 = (pressure * 0xFFFF as f32) as u16;  // 0x7FFF
```

**滚动值**使用 2048 作为单位（根据官方 scrcpy 抓包分析）：

```rust
// 前端传入 -1, 0, 1 → 后端乘以 2048
// 向下滚动: -1 * 2048 = -2048 (0xf800)
// 向上滚动:  1 * 2048 =  2048 (0x0800)

let vscroll: i32 = -1;  // 向下滚动
let vscroll_i16 = (vscroll * 2048) as i16;  // -2048 = 0xf800

let vscroll: i32 = 1;   // 向上滚动
let vscroll_i16 = (vscroll * 2048) as i16;  // 2048 = 0x0800
```

### 11.3 坐标系转换

```
浏览器坐标 → 归一化坐标 → 像素坐标

┌──────────────────────────────────────────────────────────────────────────┐
│  浏览器 Canvas                    归一化                 Android 设备       │
│  ┌──────────────┐                ┌─────────┐            ┌────────────┐   │
│  │              │                │         │            │            │   │
│  │   (300,200)  │  ────────►     │(0.3,0.2)│ ────────►  │ (324,384)  │   │
│  │    ●         │  /rect.w,h     │   ●     │  *dev_w,h  │    ●       │   │
│  │              │                │         │            │            │   │
│  │  1000x1000   │                │ [0,1]   │            │ 1080x1920  │   │
│  └──────────────┘                └─────────┘            └────────────┘   │
│                                                                          │
│  计算过程:                                                                │
│  x_norm = (clientX - rect.left) / rect.width = 300/1000 = 0.3            │
│  y_norm = (clientY - rect.top) / rect.height = 200/1000 = 0.2            │
│  x_pixel = x_norm * video_width = 0.3 * 1080 = 324                       │
│  y_pixel = y_norm * video_height = 0.2 * 1920 = 384                      │
│                                                                          │
└──────────────────────────────────────────────────────────────────────────┘
```

### 11.4 IDR 帧请求机制

```
新客户端连接时的 IDR 帧获取流程:

┌──────────────┐     ┌──────────────┐     ┌──────────────┐
│  Browser     │     │  WS Server   │     │  Main Loop   │
└──────┬───────┘     └──────┬───────┘     └──────┬───────┘
       │                    │                    │
       │  WS Connect        │                    │
       │───────────────────>│                    │
       │                    │  idr_request_tx    │
       │                    │───────────────────>│
       │                    │                    │
       │  config JSON       │                    │
       │<───────────────────│                    │
       │                    │                    │
       │  cached SPS/PPS    │                    │
       │<───────────────────│                    │
       │                    │                    │
       │                    │                    │ 设置 pending_idr = true
       │                    │                    │
       │                    │                    │ 等待下一个 IDR 帧
       │                    │                    │ (通常在 1 秒内到达)
       │                    │                    │
       │                    │  broadcast IDR     │
       │<───────────────────│<───────────────────│
       │                    │                    │
       │  可以开始解码!     │                     │
       │                    │                    │
```

### 11.5 SPS/PPS 缓存策略

```rust
// 主循环中的 SPS/PPS 缓存
if frame.frame_type == FrameType::Config {
    let nal_type = frame.data[0] & 0x1F;

    if nal_type == 7 && !sps_cached {
        // SPS - 同时解析分辨率
        let mut nal_with_start_code = vec![0x00, 0x00, 0x00, 0x01];
        nal_with_start_code.extend_from_slice(&frame.data);

        let mut config = video_config.write().await;
        config.sps = Some(Bytes::from(nal_with_start_code));

        // 解析分辨率
        if let Some((w, h)) = parse_sps_resolution(&frame.data) {
            config.width = w;
            config.height = h;
        }
        sps_cached = true;
    } else if nal_type == 8 && !pps_cached {
        // PPS
        let mut nal_with_start_code = vec![0x00, 0x00, 0x00, 0x01];
        nal_with_start_code.extend_from_slice(&frame.data);

        let mut config = video_config.write().await;
        config.pps = Some(Bytes::from(nal_with_start_code));
        pps_cached = true;
    }
}
```

---

## 12. 配置参数说明

### 12.1 命令行参数

| 参数                     | 短选项 | 默认值                                  | 说明                         |
| ------------------------ | ------ | --------------------------------------- | ---------------------------- |
| `--adb-path`             | `-a`   | `../adb/adb.exe`                        | ADB 可执行文件路径           |
| `--server-path`          | `-s`   | `../scrcpy-server/scrcpy-server-v3.3.4` | scrcpy-server JAR 路径       |
| `--list`                 |        | (不启用)                                | 列出所有已连接设备并退出     |
| `--device`               | `-d`   | (自动选择)                              | 目标设备索引或序列号         |
| `--max-size`             | `-m`   | `1920`                                  | 最大视频分辨率               |
| `--bit-rate`             | `-b`   | `4000000`                               | 视频码率 (bps)               |
| `--max-fps`              | `-f`   | `60`                                    | 最大帧率                     |
| `--ws-port`              | `-p`   | `8080`                                  | WebSocket 端口               |
| `--video-port`           |        | `27183`                                 | 视频流端口                   |
| `--control-port`         |        | `27184`                                 | 控制流端口                   |
| `--intra-refresh-period` | `-i`   | `1`                                     | IDR 帧间隔 (秒)              |
| `--video-encoder`        | `-e`   | (自动选择)                              | 指定视频编码器名称           |
| `--log-level`            | `-l`   | `info`                                  | 日志级别                     |
| `--public`               |        | (不启用)                                | 启用局域网访问 (0.0.0.0)     |

### 12.2 性能调优建议

```
┌─────────────────────────────────────────────────────────────────────────┐
│                          性能调优参数建议                                  │
├─────────────────────────────────────────────────────────────────────────┤
│                                                                         │
│  场景              │ max_size │ bit_rate  │ max_fps │ idr_interval       │
│  ─────────────────┼──────────┼───────────┼─────────┼─────────────────── │
│  高画质            │  1920    │ 8000000   │   60    │      2             │
│  平衡模式          │  1280    │ 4000000   │   60    │      1             │
│  低延迟            │  1280    │ 2000000   │   60    │      1             │
│  流量节省          │   720    │ 1000000   │   30    │      2             │
│                                                                         │
│  说明:                                                                   │
│  - max_size 越小,编码越快,延迟越低                                         │
│  - bit_rate 越高,画质越好,带宽占用越大                                      │
│  - idr_interval 越小,新客户端启动越快,但压缩效率降低                          │
│                                                                         │
└─────────────────────────────────────────────────────────────────────────┘
```

### 12.3 模拟器兼容性说明

部分 Android 模拟器（如雷电模拟器）使用 x86 架构，其视频编码器采用 Baseline Profile，压缩效率比真机/ARM 模拟器的 High Profile 低约 30-40%。

**问题表现：**
- 画面卡顿、延迟高
- CPU 占用较高

**解决方案：**

1. **降低分辨率和码率（推荐）**
   ```bash
   rust-scrcpy.exe -m 1080 -b 2000000
   ```

2. **进一步降低参数**
   ```bash
   rust-scrcpy.exe -m 720 -b 1000000
   ```

3. **指定编码器**（查看可用编码器：`adb shell "dumpsys media.codec | grep -i avc"`）
   ```bash
   rust-scrcpy.exe -m 1080 -b 2000000 -e c2.android.avc.encoder
   ```

4. **降低帧率换取更高分辨率**
   ```bash
   rust-scrcpy.exe -m 1080 -b 2000000 -f 30
   ```

**技术细节：**
- 程序会根据 SPS 中的 profile_idc 自动配置 WebCodecs 解码器
- Baseline Profile (66): `avc1.42xxxx`
- High Profile (100): `avc1.64xxxx`

### 12.4 雷电模拟器适配技术细节

为支持雷电模拟器等 x86 架构模拟器，进行了以下技术改进：

#### 12.4.1 SPS 防竞争字节处理

H.264 SPS 数据中可能包含防竞争字节 (Emulation Prevention Bytes)，即 `0x00 0x00 0x03` 序列。雷电模拟器的 SPS 包含此类字节，导致解析偏移错误。

```rust
// src/main.rs - 移除防竞争字节
fn remove_emulation_prevention_bytes(data: &[u8]) -> Vec<u8> {
    let mut result = Vec::with_capacity(data.len());
    let mut i = 0;
    while i < data.len() {
        if i + 2 < data.len() && data[i] == 0x00 && data[i + 1] == 0x00 && data[i + 2] == 0x03 {
            result.push(0x00);
            result.push(0x00);
            i += 3; // 跳过 0x03
        } else {
            result.push(data[i]);
            i += 1;
        }
    }
    result
}
```

#### 12.4.2 动态 Codec 检测

前端 WebCodecs 解码器根据 SPS 中的 profile_idc 动态配置 codec string：

```javascript
// 从 SPS 解析 codec string
reconfigureFromSPS(spsData) {
    // SPS 格式: 00 00 00 01 [NAL header] [profile_idc] [constraint_flags] [level_idc]
    const profileIdc = spsData[5];
    const constraintFlags = spsData[6];
    const levelIdc = spsData[7];

    // 构建 codec string: avc1.XXYYZZ
    const codecString = `avc1.${profileIdc.toString(16).padStart(2, '0')}${constraintFlags.toString(16).padStart(2, '0')}${levelIdc.toString(16).padStart(2, '0')}`;

    this.decoder.configure({
        codec: codecString,
        optimizeForLatency: true,
        hardwareAcceleration: 'prefer-hardware',
    });
}
```

#### 12.4.3 IDR 帧缓存机制

新客户端连接时，由于 broadcast channel 缓冲有限，可能错过 IDR 帧导致画面无法显示。解决方案是缓存最后一个 IDR 帧：

```rust
// src/main.rs - 缓存 IDR 帧
if nal_type == 5 {
    let video_config_clone = video_config.clone();
    let idr_clone = nal_bytes.clone();
    tokio::spawn(async move {
        let mut config = video_config_clone.write().await;
        config.last_idr = Some(idr_clone);
    });
}

// src/ws/server.rs - 新客户端连接时发送缓存的 SPS + PPS + IDR
if let Some(idr) = &config.last_idr {
    socket.send(Message::Binary(idr.to_vec())).await;
}
```

#### 12.4.4 视频流批量读取优化

原实现逐字节读取视频流效率低下，改为批量读取：

```rust
// src/scrcpy/video.rs - 批量读取
pub async fn read_frame(&mut self, _with_meta: bool) -> Result<Option<VideoFrame>> {
    let mut read_buf = [0u8; 8192];  // 8KB 批量读取

    loop {
        if let Some(nal) = self.try_extract_nal() {
            return Ok(Some(nal));
        }

        match self.stream.read(&mut read_buf).await {
            Ok(0) => return Ok(None),
            Ok(n) => self.buffer.extend_from_slice(&read_buf[..n]),
            Err(e) => return Err(...),
        }
    }
}
```

#### 12.4.5 修改汇总

| 文件 | 修改内容 |
|------|----------|
| `src/main.rs` | 添加 `remove_emulation_prevention_bytes()` 函数处理 SPS |
| `src/main.rs` | 添加 `--video-encoder` 命令行参数 |
| `src/main.rs` | 缓存 IDR 帧用于新客户端快速显示 |
| `src/scrcpy/server.rs` | 添加 `video_encoder` 参数支持 |
| `src/scrcpy/video.rs` | 视频流读取从逐字节改为 8KB 批量读取 |
| `src/ws/server.rs` | `VideoConfig` 添加 `last_idr` 字段 |
| `src/ws/server.rs` | 新客户端连接时发送 SPS + PPS + IDR |
| `src/ws/server.rs` | broadcast channel 缓冲从 2 增加到 60 |
| `src/ws/server.rs` | 前端 WebCodecs 解码器动态配置 codec string |
| `src/ws/server.rs` | 导航按钮大小改为较短边的 0.1 倍 |

---

## 13. 错误处理

### 13.1 错误类型定义

```rust
// src/error.rs
#[derive(Error, Debug)]
pub enum ScrcpyError {
    #[error("ADB error: {0}")]
    Adb(String),              // ADB 命令执行失败

    #[error("Device not found")]
    DeviceNotFound,           // 设备未找到

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),  // IO 错误

    #[error("Network error: {0}")]
    Network(String),          // 网络连接错误

    #[error("Video stream error: {0}")]
    VideoStream(String),      // 视频流解析错误

    #[error("Parse error: {0}")]
    Parse(String),            // 数据解析错误
}

pub type Result<T> = std::result::Result<T, ScrcpyError>;
```

### 13.2 常见错误及解决方案

| 错误                                 | 原因                 | 解决方案                     |
| ------------------------------------ | -------------------- | ---------------------------- |
| `ADB not found`                      | ADB 路径错误         | 检查 `--adb-path` 参数       |
| `No devices connected`               | 设备未连接           | 检查 USB 连接或 WiFi 调试    |
| `Server file not found`              | server JAR 不存在    | 检查 `--server-path` 参数    |
| `Failed to connect after 5 attempts` | 端口转发失败         | 重启 ADB 服务                |
| `Buffer overflow`                    | 视频流积压           | 提高处理速度或降低画质       |
| `WebSocket send failed`              | 客户端断开           | 正常断开,无需处理            |
| `No available port found`            | 端口范围内无可用端口 | 释放占用的端口或调整起始端口 |

---

## 14. 端口自动寻找机制

### 14.1 功能概述

当指定的端口被占用时，程序会自动向后搜索可用端口，无需手动修改配置。这在以下场景特别有用：

- 同时运行多个 rust-scrcpy 实例
- 端口被其他程序占用
- 之前的实例未正确释放端口

### 14.2 端口工具模块

```rust
// src/utils/port.rs

/// 检查端口是否可用
pub fn is_port_available(port: u16) -> bool {
    TcpListener::bind(("127.0.0.1", port)).is_ok()
}

/// 从指定端口开始，寻找第一个可用端口
///
/// # Arguments
/// * `start_port` - 起始端口
/// * `max_attempts` - 最大尝试次数（向后搜索的范围）
///
/// # Returns
/// * `Ok(port)` - 找到的可用端口
/// * `Err` - 在范围内未找到可用端口
pub fn find_available_port(start_port: u16, max_attempts: u16) -> Result<u16> {
    let end_port = start_port.saturating_add(max_attempts);

    for port in start_port..=end_port {
        if is_port_available(port) {
            if port != start_port {
                info!("📌 Port {} is occupied, using port {} instead", start_port, port);
            }
            return Ok(port);
        }
    }

    Err(ScrcpyError::NoAvailablePort(start_port, end_port))
}

/// 寻找多个连续可用端口
pub fn find_available_ports(start_port: u16, count: usize, max_attempts: u16) -> Result<Vec<u16>>;
```

### 14.3 端口配置说明

| 端口类型       | 默认值 | 搜索范围        | 用途          |
| -------------- | ------ | --------------- | ------------- |
| Video Port     | 27183  | 27183-27283     | scrcpy 视频流 |
| Control Port   | 27184  | video_port+1 起 | scrcpy 控制流 |
| WebSocket Port | 8080   | 8080-8180       | 浏览器连接    |

### 14.4 工作流程

```
┌─────────────────────────────────────────────────────────────────────────┐
│                        端口自动寻找流程                                   │
├─────────────────────────────────────────────────────────────────────────┤
│                                                                         │
│  1. 用户配置端口                                                          │
│     --ws-port 8080 --video-port 27183 --control-port 27184              │
│                                                                         │
│  2. 检测视频端口                                                          │
│     ┌─────────────────────────────────────────────────────────┐         │
│     │  27183 被占用? ──Yes──> 27184 被占用? ──Yes──> 27185...   │         │
│     │       │                      │                          │         │
│     │      No                     No                          │         │
│     │       ▼                      ▼                          │         │
│     │  使用 27183              使用 27184                      │         │
│     └─────────────────────────────────────────────────────────┘         │
│                                                                         │
│  3. 检测控制端口 (从 video_port + 1 开始)                                  │
│     确保控制端口 > 视频端口，避免冲突                                        │
│                                                                         │
│  4. 检测 WebSocket 端口                                                  │
│     独立检测，与 scrcpy 端口无关                                           │
│                                                                         │
│  5. 日志输出实际使用的端口                                                  │
│     INFO: Video port: 27185 (requested: 27183)                          │
│     INFO: Control port: 27186 (requested: 27184)                        │
│     INFO: WebSocket server ready at ws://0.0.0.0:8080/ws                │
│                                                                         │
└─────────────────────────────────────────────────────────────────────────┘
```

### 14.5 实际运行示例

```
2025-12-26T11:02:05.703681Z  INFO rust_scrcpy::utils::port: 📌 Port 27183 is occupied, using port 27185 instead
2025-12-26T11:02:05.745795Z  INFO rust_scrcpy::scrcpy::server: 🚀 Starting scrcpy-server...
2025-12-26T11:02:05.745892Z  INFO rust_scrcpy::scrcpy::server:    Video port: 27185 (requested: 27183)
2025-12-26T11:02:05.745969Z  INFO rust_scrcpy::scrcpy::server:    Control port: 27186 (requested: 27184)
```

### 14.6 ScrcpyServer 端口管理

```rust
// src/scrcpy/server.rs

pub struct ScrcpyServer {
    video_port: u16,           // 用户请求的视频端口
    actual_video_port: u16,    // 实际使用的视频端口
    control_port: u16,         // 用户请求的控制端口
    actual_control_port: u16,  // 实际使用的控制端口
    // ...
}

impl ScrcpyServer {
    pub fn with_config(...) -> Result<Self> {
        // 自动寻找可用端口
        let actual_video_port = find_available_port(video_port, 100)?;

        // 控制端口从视频端口+1开始搜索，避免冲突
        let actual_control_port = find_available_port(
            if control_port <= actual_video_port {
                actual_video_port + 1
            } else {
                control_port
            },
            100
        )?;

        Ok(Self { ... })
    }

    /// 获取实际使用的视频端口
    pub fn get_actual_video_port(&self) -> u16 {
        self.actual_video_port
    }

    /// 获取实际使用的控制端口
    pub fn get_actual_control_port(&self) -> u16 {
        self.actual_control_port
    }
}
```

### 14.7 WebSocketServer 端口管理

```rust
// src/ws/server.rs

pub struct WebSocketServer {
    port: u16,          // 用户请求的端口
    actual_port: u16,   // 实际使用的端口
    // ...
}

impl WebSocketServer {
    pub fn new(port: u16, ...) -> Result<Self> {
        // 自动寻找可用端口
        let actual_port = find_available_port(port, 100)?;

        Ok(Self { port, actual_port, ... })
    }

    /// 获取实际使用的端口
    pub fn get_actual_port(&self) -> u16 {
        self.actual_port
    }
}
```

### 14.8 错误处理

当在搜索范围内找不到可用端口时，会返回 `NoAvailablePort` 错误：

```rust
// src/error.rs

#[derive(Error, Debug)]
pub enum ScrcpyError {
    // ...

    #[error("No available port found in range {0}-{1}")]
    NoAvailablePort(u16, u16),
}
```

---

## 附录

### A. 项目文件结构

```
rust-scrcpy/
├── Cargo.toml              # 项目配置和依赖
├── src/
│   ├── main.rs             # 主程序入口、事件循环、SPS解析
│   ├── error.rs            # 错误类型定义
│   ├── adb/
│   │   ├── mod.rs          # ADB 模块导出
│   │   ├── client.rs       # ADB 客户端实现
│   │   └── device.rs       # 设备信息 (coming soon)
│   ├── scrcpy/
│   │   ├── mod.rs          # scrcpy 模块导出
│   │   ├── server.rs       # ScrcpyServer 实现
│   │   ├── video.rs        # 视频流读取器
│   │   └── control.rs      # 控制通道实现
│   ├── utils/
│   │   ├── mod.rs          # 工具模块导出
│   │   └── port.rs         # 端口可用性检测和自动寻找
│   └── ws/
│       ├── mod.rs          # WebSocket 模块导出
│       └── server.rs       # WebSocket 服务器和 HTML 页面
└── sum2.0.md               # 本技术文档
```

### B. 依赖库说明

| 依赖      | 版本 | 用途                  |
| --------- | ---- | --------------------- |
| tokio     | 1.42 | 异步运行时            |
| axum      | 0.7  | HTTP/WebSocket 服务器 |
| serde     | 1.0  | JSON 序列化           |
| bytes     | 1.9  | 高效字节缓冲          |
| tracing   | 0.1  | 日志系统              |
| clap      | 4.5  | 命令行参数解析        |
| thiserror | 2.0  | 错误处理              |

### C. 参考资料

- [scrcpy 官方仓库](https://github.com/Genymobile/scrcpy)
- [WebCodecs API](https://developer.mozilla.org/en-US/docs/Web/API/WebCodecs_API)
- [Android InputManager](https://developer.android.com/reference/android/hardware/input/InputManager)

**💐感谢[Claude Opus 4.5](https://claude.ai)和[ChatGPT 5.2](https://chatgpt.com)帮我阅读scrcpy源码和分析scrcpy流量包💐**