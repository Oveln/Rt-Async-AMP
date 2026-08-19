//! 消息类型定义

use core::fmt;
use crate::PAYLOAD_SIZE;
use serde::Serialize;

/// 消息类型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MsgType {
    /// 简单的信号通知
    Notification = 0,
    /// 带有负载数据
    Data = 1,
    /// RPC 请求
    Request = 2,
    /// RPC 响应
    Response = 3,
}

impl TryFrom<u8> for MsgType {
    type Error = u8;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Notification),
            1 => Ok(Self::Data),
            2 => Ok(Self::Request),
            3 => Ok(Self::Response),
            e => Err(e),
        }
    }
}

/// 消息负载
pub type Payload = [u8; PAYLOAD_SIZE];

/// 消息结构体 - 按 MESSAGE_ALIGN 对齐
#[repr(C, align(256))]
#[derive(Clone, Copy, PartialEq)]
pub struct Message {
    kind: u8,
    payload: Payload,
}

impl Message {
    /// 创建一个空消息 (用于初始化)
    #[inline]
    pub const fn empty() -> Self {
        Self {
            kind: 0,
            payload: [0u8; PAYLOAD_SIZE],
        }
    }

    /// 创建一个新的通知消息
    #[inline]
    pub fn notification(id: u32) -> Self {
        let mut payload = [0u8; PAYLOAD_SIZE];
        payload[0..4].copy_from_slice(&id.to_le_bytes());
        Self {
            kind: MsgType::Notification as u8,
            payload,
        }
    }

    /// 创建一个新的数据消息
    #[inline]
    pub fn data(data: &[u8]) -> Self {
        let mut payload = [0u8; PAYLOAD_SIZE];
        let len = data.len().min(PAYLOAD_SIZE);
        payload[..len].copy_from_slice(&data[..len]);
        Self {
            kind: MsgType::Data as u8,
            payload,
        }
    }

    /// 创建一个 RPC 请求消息（序列化参数）
    ///
    /// 格式: request_id(u64 le) + method_id(u64 le) + serialized_args
    #[inline]
    pub fn request<T: Serialize>(request_id: u64, method_id: u64, args: &T) -> Result<Self, postcard::Error> {
        let mut payload = [0u8; PAYLOAD_SIZE];
        payload[0..8].copy_from_slice(&request_id.to_le_bytes());
        payload[8..16].copy_from_slice(&method_id.to_le_bytes());

        let _ = postcard::to_slice(args, &mut payload[16..])?;

        Ok(Self {
            kind: MsgType::Request as u8,
            payload,
        })
    }

    /// 创建一个 RPC 响应消息（序列化结果）
    ///
    /// 格式: request_id(u64 le) + serialized_result
    #[inline]
    pub fn response<T: Serialize>(request_id: u64, result: &T) -> Result<Self, postcard::Error> {
        let mut payload = [0u8; PAYLOAD_SIZE];
        payload[0..8].copy_from_slice(&request_id.to_le_bytes());

        let _ = postcard::to_slice(result, &mut payload[8..])?;

        Ok(Self {
            kind: MsgType::Response as u8,
            payload,
        })
    }

    /// Returns the raw payload bytes (for debugging).
    #[inline]
    pub fn payload_bytes(&self) -> &[u8] {
        &self.payload[..]
    }

    /// 获取消息类型
    #[inline]
    pub fn ty(&self) -> Option<MsgType> {
        self.kind.try_into().ok()
    }

    /// 获取通知ID
    #[inline]
    pub fn as_notification(&self) -> Option<u32> {
        (self.ty() == Some(MsgType::Notification)).then(|| {
            u32::from_le_bytes(self.payload[0..4].try_into().unwrap())
        })
    }

    /// 获取数据 (返回整个 payload 的引用)
    #[inline]
    pub fn as_data(&self) -> Option<&Payload> {
        (self.ty() == Some(MsgType::Data)).then(|| &self.payload)
    }

    /// 获取 RPC 请求的 method_id
    #[inline]
    pub fn method_id(&self) -> Option<u64> {
        if self.ty() != Some(MsgType::Request) {
            return None;
        }
        let data = &self.payload;
        if data.len() < 16 {
            return None;
        }
        Some(u64::from_le_bytes([
            data[8], data[9], data[10], data[11],
            data[12], data[13], data[14], data[15],
        ]))
    }

    /// 获取 RPC 请求或响应的 request_id
    #[inline]
    pub fn request_id(&self) -> Option<u64> {
        if self.ty() != Some(MsgType::Request) && self.ty() != Some(MsgType::Response) {
            return None;
        }
        let data = &self.payload;
        if data.len() < 8 {
            return None;
        }
        Some(u64::from_le_bytes([
            data[0], data[1], data[2], data[3],
            data[4], data[5], data[6], data[7],
        ]))
    }

    /// 获取 RPC 请求解析为元组 (request_id, method_id, args)
    #[inline]
    pub fn as_request<T: serde::de::DeserializeOwned>(&self) -> Option<(u64, u64, T)> {
        if self.ty() != Some(MsgType::Request) {
            return None;
        }
        let data = &self.payload;
        if data.len() < 16 {
            return None;
        }
        let request_id = u64::from_le_bytes([
            data[0], data[1], data[2], data[3],
            data[4], data[5], data[6], data[7],
        ]);
        let method_id = u64::from_le_bytes([
            data[8], data[9], data[10], data[11],
            data[12], data[13], data[14], data[15],
        ]);
        let args = postcard::from_bytes(&data[16..]).ok()?;
        Some((request_id, method_id, args))
    }

    /// 获取 RPC 响应解析为元组 (request_id, result)
    #[inline]
    pub fn as_response<T: serde::de::DeserializeOwned>(&self) -> Option<(u64, T)> {
        if self.ty() != Some(MsgType::Response) {
            return None;
        }
        let data = &self.payload;
        if data.len() < 8 {
            return None;
        }
        let request_id = u64::from_le_bytes([
            data[0], data[1], data[2], data[3],
            data[4], data[5], data[6], data[7],
        ]);
        let result = postcard::from_bytes(&data[8..]).ok()?;
        Some((request_id, result))
    }
}

impl fmt::Debug for Message {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Message")
            .field("kind", &self.ty())
            .finish()
    }
}

