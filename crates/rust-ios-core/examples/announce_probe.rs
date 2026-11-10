//! 二次 announce 复现探针（遗留项：iOS 27.2 端口迁移行为验证）。
//!
//! 用法（需运行中的 userspace tunnel）：
//! ```text
//! cargo run -p rust-ios-core --features real-device --example announce_probe -- \
//!   127.0.0.1:53878 fdf2:1cb2:4601::1 58078
//! ```
//! 参数：`<userspace endpoint> <tunnel server IPv6> <rsd port>`。
//!
//! 行为：经数据面 20 字节握手连到 RSD 端口，执行一次**额外的** host
//! handshake（announce）。iOS 27.2 起每隧道只接受一次 announce——第二次会使
//! 设备把所有服务迁到新端口、旧端口随连接关闭失效。探针打印第二次握手看到的
//! 服务数（迁移前形状），随后退出关闭连接；之后可用 `tunnel refresh` 重建。

use std::str::FromStr;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let endpoint = args
        .next()
        .expect("usage: <endpoint> <server-ipv6> <rsd-port>");
    let server_ipv6 = args
        .next()
        .expect("usage: <endpoint> <server-ipv6> <rsd-port>");
    let rsd_port: u16 = args
        .next()
        .expect("usage: <endpoint> <server-ipv6> <rsd-port>")
        .parse()?;

    let address: std::net::Ipv6Addr = std::net::Ipv6Addr::from_str(&server_ipv6)?;
    let stream = tokio::net::TcpStream::connect(&endpoint).await?;
    let mut stream = stream;
    // 数据面 20 字节握手：16 字节目标 IPv6 + 4 字节 LE 端口。
    let mut handshake = Vec::with_capacity(20);
    handshake.extend_from_slice(&address.octets());
    handshake.extend_from_slice(&(u32::from(rsd_port)).to_le_bytes());
    use tokio::io::AsyncWriteExt;
    stream.write_all(&handshake).await?;
    stream.flush().await?;

    let mut client = idevice::xpc::RemoteXpcClient::new(stream)
        .await
        .map_err(|e| format!("new: {e}"))?;
    println!("[stage] h2 client ready");
    client
        .do_handshake()
        .await
        .map_err(|e| format!("do_handshake: {e}"))?;
    println!("[stage] 3-message bootstrap done");
    // 额外的 host announce：iOS 27.2 上这会让服务迁移端口。
    client
        .send_host_handshake(7, 0x0100_0000_0000_0006)
        .await
        .map_err(|e| format!("announce: {e}"))?;
    println!("[stage] second host announce sent");
    let services = client
        .recv_root()
        .await
        .map_err(|e| format!("recv_root: {e}"))?;
    let count = services
        .as_dictionary()
        .and_then(|d| d.get("Services"))
        .and_then(|s| s.as_dictionary())
        .map(|s| s.len())
        .unwrap_or(0);
    println!("second-announce handshake saw {count} services");
    println!(
        "probe done: the device has now seen a second host announce; expect old service ports to die. Run `tunnel refresh` to recover."
    );
    Ok(())
}
