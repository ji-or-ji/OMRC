//! Windows 主机端入口。
//!
//! 现阶段只做启动自检。W1 的第一件事是接上 DXGI 桌面复制。

use omrc_core::PROTOCOL_VERSION;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        protocol = PROTOCOL_VERSION,
        os = std::env::consts::OS,
        arch = std::env::consts::ARCH,
        "omrc host 启动"
    );

    tracing::info!("MTU={} 包头={} 载荷上限={}",
        omrc_transport_lan::MTU,
        omrc_transport_lan::HEADER_LEN,
        omrc_transport_lan::MAX_PAYLOAD,
    );

    tracing::warn!("W1 目标：DXGI 抓屏 → Media Foundation 硬编 → UDP 发送");
}
