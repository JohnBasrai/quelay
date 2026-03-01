// ---------------------------------------------------------------------------
// Subcommand: multi-file
// ---------------------------------------------------------------------------

use clap::Args;

use crate::*;

#[derive(Debug, Args)]
pub struct MultiFileArgs {
    // ---
    /// Transfer 3 large files: 30 MiB, 2 MiB, 500 KiB.
    #[arg(long, conflicts_with_all = ["small", "size_mb", "duration_secs"])]
    large: bool,

    /// Transfer 4 boundary-condition sizes: 9000B, 1024B, 512B, 1B.
    /// Agents are reconfigured to 1 KiB chunk size so multi-block boundaries
    /// are exercised (at 16 KiB default, all four files are sub-chunk).
    #[arg(long, conflicts_with_all = ["large", "size_mb", "duration_secs"])]
    small: bool,

    /// Transfer a single file of N MiB.
    #[arg(long, conflicts_with_all = ["large", "small", "duration_secs"])]
    size_mb: Option<usize>,

    /// Derive file size from agent BW cap × N seconds.
    #[arg(long, conflicts_with_all = ["large", "small", "size_mb"])]
    duration_secs: Option<u64>,

    /// Number of files to transfer (default: 2). Ignored when --large or --small is given.
    #[arg(long, default_value_t = 2)]
    count: usize,

    /// Send files in both directions (sender→receiver and receiver→sender).
    #[arg(long, default_value_t = false)]
    bidirectional: bool,

    /// Simulate a recoverable link outage mid-transfer. Queues a second file
    /// while the link is down to verify pending queue handling across reconnect.
    /// All timing derived from the agent's configured BW cap.
    #[arg(long, conflicts_with = "link_fail")]
    link_outage: bool,

    /// Simulate a permanent link failure. Asserts the in-flight file receives
    /// a stream_failed callback and LinkState reaches Failed.
    #[arg(long, conflicts_with = "link_outage")]
    link_fail: bool,
}

// ---

pub async fn cmd_multi_file(
    sender_c2i: SocketAddr,
    receiver_c2i: SocketAddr,
    args: &MultiFileArgs,
) -> anyhow::Result<()> {
    // ---

    println!("=== multi-file ===");

    ensure_agent_running(sender_c2i)?;
    ensure_agent_running(receiver_c2i)?;

    let cap_mbps = query_cap(sender_c2i).context("query_cap(sender_c2i) failed")?;

    let file_sizes: Vec<usize> = if args.large {
        vec![
            30 * 1024 * 1024, //  30 MiB
            2 * 1024 * 1024,  //   2 MiB
            512 * 1024,       // 512 KiB
        ]
    } else if args.small {
        vec![9_000, 1_024, 512, 1]
    } else if let Some(mb) = args.size_mb {
        std::iter::repeat_n(mb * 1024 * 1024, args.count).collect()
    } else if let Some(secs) = args.duration_secs {
        let bytes = cap_mbps
            .map(|c| c as usize * 1_000_000 / 8 * secs as usize)
            .unwrap_or(32 * 1024 * 1024);
        std::iter::repeat_n(bytes, args.count).collect()
    } else {
        let bytes = cap_mbps
            .map(|c| c as usize * 1_000_000 / 8 * 10)
            .unwrap_or(32 * 1024 * 1024);
        std::iter::repeat_n(bytes, args.count).collect::<Vec<usize>>()
    };

    if args.link_outage {
        run_multi_file_link_outage(sender_c2i, receiver_c2i, &file_sizes, cap_mbps).await?;
    } else if args.link_fail {
        run_multi_file_link_fail(sender_c2i, receiver_c2i).await?;
    } else {
        for (i, &sz) in file_sizes.iter().enumerate() {
            run_single_transfer(
                sender_c2i,
                receiver_c2i,
                sz,
                &format!("multi-file-{i}"),
                cap_mbps,
            )
            .await?;

            if args.bidirectional {
                run_single_transfer(
                    receiver_c2i,
                    sender_c2i,
                    sz,
                    &format!("multi-file-{i}-reverse"),
                    cap_mbps,
                )
                .await?;
            }
        }
    }

    println!("  multi-file PASSED ✓");
    println!();
    Ok(())
}
