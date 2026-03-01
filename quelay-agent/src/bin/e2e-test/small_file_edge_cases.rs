// ---------------------------------------------------------------------------
// Subcommand: small-file-edge-cases
// ---------------------------------------------------------------------------

use clap::Args;
use std::net::SocketAddr;

use crate::*;

#[derive(Debug, Args)]
pub struct SmallFileEdgeCasesArgs {
    // ---
    /// Test both transfer directions for each size.
    #[arg(long, default_value_t = false)]
    bidirectional: bool,
}

pub async fn cmd_small_file_edge_cases(
    sender_c2i: SocketAddr,
    receiver_c2i: SocketAddr,
    args: &SmallFileEdgeCasesArgs,
) -> anyhow::Result<()> {
    // ---

    println!("=== small-file-edge-cases ===");

    ensure_agent_running(sender_c2i)?;
    ensure_agent_running(receiver_c2i)?;

    let cap_mbps = query_cap(sender_c2i).context("query_cap(sender_c2i) failed")?;

    {
        let mut s = connect_agent(sender_c2i)?;
        let mut r = connect_agent(receiver_c2i)?;
        s.set_chunk_size_bytes(1024)?;
        r.set_chunk_size_bytes(1024)?;
    }

    let sizes = [
        (9_000usize, "9000B (8 chunks + fragment)"),
        (1_024, "1024B (exact single chunk)"),
        (512, "512B (half chunk)"),
        (1, "1B (minimum C2I stream)"),
    ];

    for (sz, label) in &sizes {
        println!("  [{label}]");
        run_single_transfer(sender_c2i, receiver_c2i, *sz, label, cap_mbps).await?;
        if args.bidirectional {
            run_single_transfer(
                receiver_c2i,
                sender_c2i,
                *sz,
                &format!("{label} (reverse)"),
                cap_mbps,
            )
            .await?;
        }
    }

    {
        let mut s = connect_agent(sender_c2i)?;
        let mut r = connect_agent(receiver_c2i)?;
        s.set_chunk_size_bytes(0)?; // 0 = restore default
        r.set_chunk_size_bytes(0)?;
    }

    println!("  small-file-edge-cases PASSED ✓");
    println!();
    Ok(())
}
