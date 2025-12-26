use crate::error::{Result, ScrcpyError};
use bytes::{Bytes, BytesMut};
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tracing::{debug, info, warn};

/// 视频帧类型
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FrameType {
    Config,  // 配置帧（SPS/PPS）
    Video,   // 视频帧
}

/// 视频帧
#[derive(Debug, Clone)]
pub struct VideoFrame {
    pub pts: u64,           // 显示时间戳（微秒）
    pub frame_type: FrameType,
    pub data: Bytes,        // H.264 NAL单元数据
}

impl VideoFrame {
    pub fn new(pts: u64, frame_type: FrameType, data: Bytes) -> Self {
        Self {
            pts,
            frame_type,
            data,
        }
    }

    /// 是否为关键帧（IDR）
    pub fn is_keyframe(&self) -> bool {
        if self.data.is_empty() {
            return false;
        }

        // H.264 NAL单元类型在第一个字节的低5位
        let nal_type = self.data[0] & 0x1F;

        // NAL类型5是IDR帧，7是SPS，8是PPS
        matches!(nal_type, 5 | 7 | 8)
    }
}

/// 视频流读取器
pub struct VideoStreamReader {
    stream: TcpStream,
    buffer: BytesMut,
    frame_count: u64,
    first_read: bool,  // 标记是否是第一次读取
    first_start_code_pos: Option<usize>,  // 第一个起始码的位置
}

impl VideoStreamReader {
    pub fn new(stream: TcpStream) -> Self {
        Self {
            stream,
            buffer: BytesMut::with_capacity(1024 * 1024), // 1MB缓冲区
            frame_count: 0,
            first_read: true,
            first_start_code_pos: None,
        }
    }

    /// 读取下一个视频帧
    ///
    /// scrcpy 3.3.4 raw_stream=true 模式：
    /// 直接的 Annex-B H.264 NAL 流，使用 00 00 01 或 00 00 00 01 起始码分隔
    pub async fn read_frame(&mut self, _with_meta: bool) -> Result<Option<VideoFrame>> {
        loop {
            // 逐字节读取
            let mut byte = [0u8; 1];
            match self.stream.read_exact(&mut byte).await {
                Ok(_) => {
                    self.buffer.extend_from_slice(&byte);
                }
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    debug!("Stream closed (EOF)");
                    return Ok(None);
                }
                Err(e) => {
                    warn!("Failed to read byte: {}", e);
                    return Err(ScrcpyError::VideoStream(format!("Failed to read byte: {}", e)));
                }
            }

            // 检查缓冲区溢出
            if self.buffer.len() > 10 * 1024 * 1024 {
                warn!("Buffer overflow, clearing");
                self.buffer.clear();
                self.first_start_code_pos = None;
                continue;
            }

            // 查找 3-byte 起始码 00 00 01
            let buf_len = self.buffer.len();
            if buf_len >= 3 {
                let last_3 = &self.buffer[buf_len - 3..];

                if last_3 == [0x00, 0x00, 0x01] {
                    // 找到一个起始码

                    if self.first_start_code_pos.is_none() {
                        // 这是第一个起始码，记录位置
                        self.first_start_code_pos = Some(buf_len - 3);
                        continue;
                    } else {
                        // 这是第二个起始码，提取中间的NAL单元
                        let start_pos = self.first_start_code_pos.unwrap();

                        // NAL数据从第一个起始码之后开始，到第二个起始码之前结束
                        // 跳过起始码本身(3字节)，提取NAL数据
                        let nal_start = start_pos + 3;
                        let nal_end = buf_len - 3;

                        if nal_start >= nal_end {
                            // 两个起始码相邻，没有数据
                            self.first_start_code_pos = Some(buf_len - 3);
                            continue;
                        }

                        let nal_data = self.buffer[nal_start..nal_end].to_vec();

                        // 清除已处理的数据，保留第二个起始码
                        self.buffer = BytesMut::from(&self.buffer[buf_len - 3..]);
                        self.first_start_code_pos = Some(0);  // 新的起始码现在在位置0

                        // 解析 NAL 类型
                        let nal_type = nal_data[0] & 0x1F;

                        let frame_type = if matches!(nal_type, 7 | 8) {
                            FrameType::Config
                        } else {
                            FrameType::Video
                        };

                        self.frame_count += 1;

                        return Ok(Some(VideoFrame::new(
                            0, // raw_stream 模式没有 PTS
                            frame_type,
                            Bytes::from(nal_data),
                        )));
                    }
                }
            }
        }
    }

    /// 获取已接收的帧数
    pub fn frame_count(&self) -> u64 {
        self.frame_count
    }
}

/// 视频编解码器配置数据
#[derive(Debug, Clone)]
pub struct ConfigData {
    pub sps: Vec<u8>,
    pub pps: Vec<u8>,
}

/// 视频编解码器信息
#[derive(Debug, Clone)]
pub struct CodecInfo {
    pub codec_id: u32,
    pub width: u32,
    pub height: u32,
    pub config_data: Option<ConfigData>,
}

impl CodecInfo {
    /// 从流中读取编解码器信息
    ///
    /// scrcpy 3.x 格式（如果 send_codec_meta=true）：
    /// - 4字节 codec_id (big-endian u32)
    /// - 4字节 width (big-endian u32)
    /// - 4字节 height (big-endian u32)
    pub async fn read_from_stream(stream: &mut TcpStream) -> Result<Self> {
        let mut buf = [0u8; 12];

        match tokio::time::timeout(
            tokio::time::Duration::from_secs(3),
            stream.read_exact(&mut buf)
        ).await {
            Ok(Ok(_)) => {
                let codec_id = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
                let width = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
                let height = u32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]);

                info!("📹 Codec info: codec_id={}, {}x{}", codec_id, width, height);

                Ok(Self {
                    codec_id,
                    width,
                    height,
                    config_data: None,
                })
            }
            Ok(Err(e)) => {
                debug!("Could not read codec info: {}", e);
                // 返回默认值
                Ok(Self {
                    codec_id: 0x68323634, // "h264"
                    width: 0,
                    height: 0,
                    config_data: None,
                })
            }
            Err(_) => {
                debug!("Timeout reading codec info, using defaults");
                Ok(Self {
                    codec_id: 0x68323634,
                    width: 0,
                    height: 0,
                    config_data: None,
                })
            }
        }
    }
}

