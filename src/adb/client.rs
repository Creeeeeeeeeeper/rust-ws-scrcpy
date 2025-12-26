use crate::error::{Result, ScrcpyError};
use std::path::PathBuf;
use std::process::Stdio;
use tokio::process::Command;

pub struct AdbClient {
    pub adb_path: PathBuf,
}

impl AdbClient {
    pub fn new(adb_path: PathBuf) -> Self {
        Self { adb_path }
    }

    /// 执行ADB命令
    pub async fn execute(&self, args: &[&str]) -> Result<String> {
        let output = Command::new(&self.adb_path)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .map_err(|e| ScrcpyError::Adb(format!("Failed to execute ADB: {}", e)))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(ScrcpyError::Adb(format!("ADB command failed: {}", stderr)));
        }

        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    /// 获取已连接的设备列表
    pub async fn list_devices(&self) -> Result<Vec<String>> {
        let output = self.execute(&["devices"]).await?;

        let devices: Vec<String> = output
            .lines()
            .skip(1) // 跳过 "List of devices attached"
            .filter_map(|line| {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 2 && parts[1] == "device" {
                    Some(parts[0].to_string())
                } else {
                    None
                }
            })
            .collect();

        Ok(devices)
    }

    /// 推送文件到设备
    pub async fn push(&self, device_id: &str, local: &str, remote: &str) -> Result<()> {
        self.execute(&["-s", device_id, "push", local, remote]).await?;
        Ok(())
    }

    /// 执行shell命令
    pub async fn shell(&self, device_id: &str, command: &str) -> Result<String> {
        self.execute(&["-s", device_id, "shell", command]).await
    }

    /// 端口转发
    pub async fn forward(&self, device_id: &str, local_port: u16, remote: &str) -> Result<()> {
        let local = format!("tcp:{}", local_port);
        self.execute(&["-s", device_id, "forward", &local, remote]).await?;
        Ok(())
    }

    /// 移除端口转发
    pub async fn forward_remove(&self, device_id: &str, local_port: u16) -> Result<()> {
        let local = format!("tcp:{}", local_port);
        self.execute(&["-s", device_id, "forward", "--remove", &local]).await?;
        Ok(())
    }
}
