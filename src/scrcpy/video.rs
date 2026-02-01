use crate::error::{Result, ScrcpyError};
use bytes::{Bytes, BytesMut};
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tracing::{debug, info, warn};

/// 在缓冲区中查找起始码 00 00 01 的位置
/// 返回起始码第一个字节的位置
fn find_start_code(buf: &[u8], start: usize) -> Option<usize> {
    if buf.len() < start + 3 {
        return None;
    }

    for i in start..buf.len() - 2 {
        if buf[i] == 0x00 && buf[i + 1] == 0x00 && buf[i + 2] == 0x01 {
            return Some(i);
        }
    }
    None
}

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
        // 批量读取缓冲区
        let mut read_buf = [0u8; 8192];

        loop {
            // 首先检查现有缓冲区中是否已有完整的 NAL 单元
            if let Some(nal) = self.try_extract_nal() {
                return Ok(Some(nal));
            }

            // 批量读取数据
            match self.stream.read(&mut read_buf).await {
                Ok(0) => {
                    debug!("Stream closed (EOF)");
                    return Ok(None);
                }
                Ok(n) => {
                    self.buffer.extend_from_slice(&read_buf[..n]);
                }
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    debug!("Stream closed (EOF)");
                    return Ok(None);
                }
                Err(e) => {
                    warn!("Failed to read: {}", e);
                    return Err(ScrcpyError::VideoStream(format!("Failed to read: {}", e)));
                }
            }

            // 检查缓冲区溢出
            if self.buffer.len() > 10 * 1024 * 1024 {
                warn!("Buffer overflow, clearing");
                self.buffer.clear();
                self.first_start_code_pos = None;
            }
        }
    }

    /// 尝试从缓冲区提取一个完整的 NAL 单元
    fn try_extract_nal(&mut self) -> Option<VideoFrame> {
        let buf = &self.buffer[..];
        let buf_len = buf.len();

        if buf_len < 4 {
            return None;
        }

        // 查找第一个起始码
        let first_pos = if self.first_start_code_pos.is_some() {
            self.first_start_code_pos.unwrap()
        } else {
            // 查找第一个 00 00 01 或 00 00 00 01
            let pos = find_start_code(buf, 0)?;
            self.first_start_code_pos = Some(pos);
            pos
        };

        // 从第一个起始码之后查找第二个起始码
        let search_start = first_pos + 3;
        if search_start >= buf_len {
            return None;
        }

        let second_pos = find_start_code(buf, search_start)?;

        // 提取 NAL 单元（不包含起始码）
        let nal_start = first_pos + 3;
        // 处理 4 字节起始码的情况
        let nal_start = if first_pos > 0 && buf[first_pos - 1] == 0x00 {
            first_pos + 3  // 已经跳过了 00 00 01
        } else {
            nal_start
        };

        let nal_end = if second_pos > 0 && buf[second_pos - 1] == 0x00 {
            second_pos - 1  // 4 字节起始码，回退一位
        } else {
            second_pos
        };

        if nal_start >= nal_end {
            // 两个起始码相邻，没有数据，移动到下一个
            self.buffer = BytesMut::from(&buf[second_pos..]);
            self.first_start_code_pos = Some(0);
            return None;
        }

        let nal_data = buf[nal_start..nal_end].to_vec();

        // 更新缓冲区，保留从第二个起始码开始的数据
        self.buffer = BytesMut::from(&buf[second_pos..]);
        self.first_start_code_pos = Some(0);

        // 解析 NAL 类型
        if nal_data.is_empty() {
            return None;
        }

        let nal_type = nal_data[0] & 0x1F;

        let frame_type = if matches!(nal_type, 7 | 8) {
            FrameType::Config
        } else {
            FrameType::Video
        };

        self.frame_count += 1;

        Some(VideoFrame::new(
            0,
            frame_type,
            Bytes::from(nal_data),
        ))
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

