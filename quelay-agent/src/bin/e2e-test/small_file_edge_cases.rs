// ---------------------------------------------------------------------------
// Subcommand: small-file-edge-cases
// ---------------------------------------------------------------------------

use clap::Args;

use crate::*;

#[derive(Debug, Args)]
pub struct SmallFileEdgeCasesArgs {
    // ---
    /// Test both transfer directions for each size.
    #[arg(long, default_value_t = false)]
    bidirectional: bool,
}

pub async fn cmd_small_file_edge_cases(
    ctx: &TestContext,
    args: &SmallFileEdgeCasesArgs,
) -> anyhow::Result<()> {
    // ---

    println!("=== small-file-edge-cases ===");

    ensure_agent_running(ctx.sender_c2i)?;
    ensure_agent_running(ctx.receiver_c2i)?;

    let cap_mbps = query_cap(ctx.sender_c2i).context("query_cap(sender_c2i) failed")?;

    {
        let mut s = connect_agent(ctx.sender_c2i)?;
        let mut r = connect_agent(ctx.receiver_c2i)?;
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
        run_single_transfer(ctx, *sz, label, cap_mbps).await?;
        if args.bidirectional {
            let reverse_ctx = TestContext {
                sender_c2i: ctx.receiver_c2i,
                receiver_c2i: ctx.sender_c2i,
                ..*ctx
            };
            run_single_transfer(&reverse_ctx, *sz, &format!("{label} (reverse)"), cap_mbps).await?;
        }
    }

    {
        let mut s = connect_agent(ctx.sender_c2i)?;
        let mut r = connect_agent(ctx.receiver_c2i)?;
        s.set_chunk_size_bytes(0)?; // 0 = restore default
        r.set_chunk_size_bytes(0)?;
    }

    println!("  small-file-edge-cases PASSED ✓");
    println!();
    Ok(())
}
