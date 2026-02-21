/// Generate and clean up nftables/iptables redirect rules.
///
/// On start: set up rules to redirect TCP traffic to the proxy.
/// On stop: clean up rules to restore normal traffic flow.
///
/// Uses full binary paths because procd/systemd may have minimal PATH.
use std::process::Command;

const NFT_TABLE: &str = "xr_proxy";
const IPT_CHAIN: &str = "XR_PROXY";

/// Common locations for nft on OpenWRT and regular Linux.
const NFT_PATHS: &[&str] = &["/usr/sbin/nft", "/sbin/nft", "/usr/bin/nft"];
/// Common locations for iptables.
const IPT_PATHS: &[&str] = &[
    "/usr/sbin/iptables",
    "/sbin/iptables",
    "/usr/bin/iptables",
];

/// Detect whether nftables or iptables is available.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FirewallBackend {
    Nftables,
    Iptables,
}

/// Find the full path to a binary by checking common locations.
fn find_binary(candidates: &[&str]) -> Option<String> {
    for path in candidates {
        if std::path::Path::new(path).exists() {
            return Some(path.to_string());
        }
    }
    // Fallback: try bare name (works if PATH is set correctly)
    if let Some(first) = candidates.first() {
        if let Some(bare_name) = first.rsplit('/').next() {
            if Command::new(bare_name)
                .arg("--version")
                .output()
                .is_ok()
            {
                return Some(bare_name.to_string());
            }
        }
    }
    None
}

fn find_nft() -> Option<String> {
    find_binary(NFT_PATHS)
}

fn find_iptables() -> Option<String> {
    find_binary(IPT_PATHS)
}

pub fn detect_backend() -> Option<FirewallBackend> {
    if find_nft().is_some() {
        Some(FirewallBackend::Nftables)
    } else if find_iptables().is_some() {
        Some(FirewallBackend::Iptables)
    } else {
        None
    }
}

/// Set up redirect rules. `server_ip` is excluded to avoid redirect loops.
pub fn setup_redirect(
    backend: FirewallBackend,
    listen_port: u16,
    server_ip: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    match backend {
        FirewallBackend::Nftables => setup_nftables(listen_port, server_ip),
        FirewallBackend::Iptables => setup_iptables(listen_port, server_ip),
    }
}

/// Remove redirect rules.
pub fn cleanup_redirect(backend: FirewallBackend) -> Result<(), Box<dyn std::error::Error>> {
    match backend {
        FirewallBackend::Nftables => cleanup_nftables(),
        FirewallBackend::Iptables => cleanup_iptables(),
    }
}

// ── nftables ─────────────────────────────────────────────────────────

fn setup_nftables(
    listen_port: u16,
    server_ip: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let nft = find_nft().ok_or("nft binary not found")?;

    // Clean up any existing rules first
    let _ = cleanup_nftables();

    let ruleset = format!(
        r#"
table ip {table} {{
    chain prerouting {{
        type nat hook prerouting priority dstnat; policy accept;
        ip daddr {server_ip} return
        ip daddr 10.0.0.0/8 return
        ip daddr 172.16.0.0/12 return
        ip daddr 192.168.0.0/16 return
        ip daddr 127.0.0.0/8 return
        tcp dport {{ 80, 443 }} redirect to :{listen_port}
    }}
}}
"#,
        table = NFT_TABLE,
        server_ip = server_ip,
        listen_port = listen_port,
    );

    // Use sh -c with pipe to feed ruleset via stdin
    let status = Command::new("/bin/sh")
        .arg("-c")
        .arg(format!(
            "echo '{}' | {} -f -",
            ruleset.replace('\'', "'\\''"),
            nft
        ))
        .status()?;

    if !status.success() {
        return Err(format!("nft command failed (binary: {})", nft).into());
    }

    tracing::info!("nftables redirect rules installed (table: {}, nft: {})", NFT_TABLE, nft);
    Ok(())
}

fn cleanup_nftables() -> Result<(), Box<dyn std::error::Error>> {
    let nft = match find_nft() {
        Some(n) => n,
        None => return Ok(()), // no nft binary — nothing to clean
    };

    let status = Command::new(&nft)
        .args(["delete", "table", "ip", NFT_TABLE])
        .status()?;
    if status.success() {
        tracing::info!("nftables rules cleaned up");
    }
    Ok(())
}

// ── iptables ─────────────────────────────────────────────────────────

fn setup_iptables(
    listen_port: u16,
    server_ip: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let _ = cleanup_iptables();

    // Create custom chain
    run_ipt(&["-t", "nat", "-N", IPT_CHAIN])?;

    // Skip server IP, private ranges
    run_ipt(&["-t", "nat", "-A", IPT_CHAIN, "-d", server_ip, "-j", "RETURN"])?;
    run_ipt(&["-t", "nat", "-A", IPT_CHAIN, "-d", "10.0.0.0/8", "-j", "RETURN"])?;
    run_ipt(&["-t", "nat", "-A", IPT_CHAIN, "-d", "172.16.0.0/12", "-j", "RETURN"])?;
    run_ipt(&["-t", "nat", "-A", IPT_CHAIN, "-d", "192.168.0.0/16", "-j", "RETURN"])?;
    run_ipt(&["-t", "nat", "-A", IPT_CHAIN, "-d", "127.0.0.0/8", "-j", "RETURN"])?;

    // Redirect HTTP/HTTPS
    let port_str = listen_port.to_string();
    run_ipt(&[
        "-t", "nat", "-A", IPT_CHAIN,
        "-p", "tcp", "-m", "multiport", "--dports", "80,443",
        "-j", "REDIRECT", "--to-ports", &port_str,
    ])?;

    // Hook into PREROUTING
    run_ipt(&["-t", "nat", "-A", "PREROUTING", "-j", IPT_CHAIN])?;

    let ipt = find_iptables().unwrap_or_else(|| "iptables".to_string());
    tracing::info!("iptables redirect rules installed (chain: {}, binary: {})", IPT_CHAIN, ipt);
    Ok(())
}

fn cleanup_iptables() -> Result<(), Box<dyn std::error::Error>> {
    // Remove from PREROUTING
    let _ = run_ipt(&["-t", "nat", "-D", "PREROUTING", "-j", IPT_CHAIN]);
    // Flush and delete chain
    let _ = run_ipt(&["-t", "nat", "-F", IPT_CHAIN]);
    let _ = run_ipt(&["-t", "nat", "-X", IPT_CHAIN]);
    tracing::info!("iptables rules cleaned up");
    Ok(())
}

fn run_ipt(args: &[&str]) -> Result<(), Box<dyn std::error::Error>> {
    let ipt = find_iptables().ok_or("iptables binary not found")?;
    let status = Command::new(&ipt).args(args).status()?;
    if !status.success() {
        return Err(format!("{} {:?} failed", ipt, args).into());
    }
    Ok(())
}
