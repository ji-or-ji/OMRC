//! OMRC 核心。
//!
//! 这个 crate 里**不允许**出现任何平台相关的东西：没有 windows、没有 jni、
//! 没有 ndk、没有文件系统路径假设。它是纯逻辑，能在桌面机上用 `cargo test`
//! 同时跑起主机端和客户端，不需要真机。
//!
//! 这条纪律是整个架构的地基。一旦有平台依赖渗进来，双端同进程测试就崩了。

pub mod input;
pub mod proto;
pub mod session;
pub mod transport;
pub mod video;

/// 协议版本。两端握手时比对，不一致直接拒绝。
pub const PROTOCOL_VERSION: u16 = 1;
