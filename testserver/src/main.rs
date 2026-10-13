//! Usage: testserver [port] [size in MiB]   (defaults: 8090, 256)

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let mut args = std::env::args().skip(1);
    let port: u16 = args.next().map_or(8090, |a| a.parse().expect("port"));
    let mib: usize = args.next().map_or(256, |a| a.parse().expect("size in MiB"));
    let server = testserver::start(([127, 0, 0, 1], port).into(), mib << 20).await?;
    println!("serving {mib} MiB at {}", server.url("/data"));
    std::future::pending().await
}
