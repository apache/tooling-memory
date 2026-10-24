//! The contract check in a microsandbox microVM with the network disabled (no NIC), the
//! fixture mounted at the Linux layout, and the egress socket mapped to vsock port 5000.
//! Args: FIX directory, image, then the command to run inside.
use microsandbox::Sandbox;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let fix = &args[1];
    let image = &args[2];
    let cmd = &args[3];
    let rest: Vec<String> = args[4..].to_vec();
    let marker = std::env::var("CHECK_HOST_MARKER").unwrap_or_default();
    let sb = Sandbox::builder(format!("contract-{}", std::process::id()))
        .image(image.as_str())
        .disable_network()
        .vsock(format!("{fix}/run/egress.sock"), 5000)
        .volume("/home/user", |m| m.bind(format!("{fix}/home")))
        .volume("/work", |m| m.bind(format!("{fix}/work")))
        .volume("/opt/memory", |m| m.bind(format!("{fix}/opt/memory")).readonly())
        .volume("/memory", |m| m.bind(format!("{fix}/projection")).readonly())
        .workdir("/work")
        .user("1000:1000")
        .env("HOME", "/home/user")
        .env("CHECK_SOCKET", "vsock:2:5000")
        .env("CHECK_HOST_MARKER", marker)
        .replace()
        .create()
        .await?;
    let out = sb.exec(cmd.as_str(), rest).await?;
    print!("{}", out.stdout()?);
    eprint!("{}", out.stderr()?);
    let code = out.status().code;
    sb.stop().await?;
    std::process::exit(code);
}
