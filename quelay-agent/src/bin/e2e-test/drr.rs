// ---------------------------------------------------------------------------
// Subcommand: drr
// ---------------------------------------------------------------------------

use clap::Args;

use crate::*;

#[derive(Debug, Args)]
pub struct DrrArgs {
    // ---
    /// Number of priority-varied files to queue behind the anchor file (default: 3).
    #[arg(long, default_value_t = 3)]
    file_count: usize,
}

pub async fn cmd_drr(
    sender_c2i: SocketAddr,
    receiver_c2i: SocketAddr,
    args: &DrrArgs,
) -> anyhow::Result<()> {
    // ---

    println!("=== drr ===");

    ensure_agent_running(sender_c2i)?;

    let cap_mbps = query_cap(sender_c2i).context("query_cap(sender_c2i) failed")?;

    {
        let mut agent = connect_agent(sender_c2i).context("connect_agent(sender_c2i) failed")?;
        agent
            .set_max_concurrent(1)
            .context("set_max_concurrent(1) failed")?;
    }

    let anchor_bytes = cap_mbps
        .map(|c| c as usize * 1_000_000 / 8 * 3)
        .unwrap_or(4 * 1024 * 1024);

    let priorities: Vec<(i8, &str)> = vec![(10, "low"), (30, "high"), (20, "med")];
    let priorities = &priorities[..args.file_count.min(priorities.len())];

    let anchor_payload =
        tokio::task::spawn_blocking(move || generate_test_data(anchor_bytes)).await?;
    let anchor_uuid = Uuid::new_v4().to_string();
    let anchor_sha256 = sha256_hex(&anchor_payload);

    let anchor_cb = TestCallbackServer::bind()?;
    let receiver_cb = TestCallbackServer::bind()?;
    let timeout = transfer_timeout(anchor_bytes, cap_mbps);

    {
        let mut sender_agent = connect_agent(sender_c2i)?;
        let mut receiver_agent = connect_agent(receiver_c2i)?;
        let e = sender_agent.set_callback(anchor_cb.endpoint())?;
        anyhow::ensure!(e.is_empty(), "drr set_callback (sender): {e}");
        let e = receiver_agent.set_callback(receiver_cb.endpoint())?;
        anyhow::ensure!(e.is_empty(), "drr set_callback (receiver): {e}");

        let mut attrs = BTreeMap::new();
        attrs.insert("sha256".to_string(), anchor_sha256.clone());
        let result = sender_agent.stream_start(
            anchor_uuid.clone(),
            StreamInfo {
                size_bytes: Some(anchor_bytes as i64),
                attrs: Some(attrs),
            },
            0,
        )?;

        anyhow::ensure!(
            result.status == Some(Status::RUNNING),
            "stream_start failed: expected {:?}, got {:?}",
            Status::RUNNING,
            result.status
        );
        println!("  anchor queued (priority 0, {} KiB)", anchor_bytes / 1024);

        for (idx, (pri, label)) in priorities.iter().enumerate() {
            // ---
            let small = 4 * 1024usize;
            let mut attrs = BTreeMap::new();
            attrs.insert("label".to_string(), label.to_string());
            let result = sender_agent.stream_start(
                Uuid::new_v4().to_string(),
                StreamInfo {
                    size_bytes: Some(small as i64),
                    attrs: Some(attrs),
                },
                *pri,
            )?;

            // The anchor holds the single active slot; each subsequent file
            // lands in the pending queue at depth idx+1 (1-based).
            let expected_pos = (idx + 1) as i32;
            anyhow::ensure!(
                result.queue_position == Some(expected_pos),
                "stream_start queue_position mismatch: expected {:?}, got {:?}",
                expected_pos,
                result.queue_position
            );
            println!("  queued {label} (priority {pri})");

            anyhow::ensure!(
                result.status == Some(Status::PENDING),
                "stream_start failed: expected {:?}, got {:?}",
                Status::PENDING,
                result.status
            );
        }
    }

    let anchor_port = match anchor_cb.recv_event_for(&anchor_uuid, timeout)? {
        TestCallbackEvent::Started { port, .. } => port,
        other => anyhow::bail!("drr anchor: expected Started, got {other:?}"),
    };

    let write_task = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        use std::io::Write;
        let mut tcp = std::net::TcpStream::connect(format!("127.0.0.1:{anchor_port}"))?;
        tcp.write_all(&anchor_payload)?;
        Ok(())
    });

    let receiver_port = match receiver_cb.recv_event_for(&anchor_uuid, timeout)? {
        TestCallbackEvent::Started { port, .. } => port,
        other => anyhow::bail!("drr anchor receiver: expected Started, got {other:?}"),
    };
    let mut received = Vec::with_capacity(anchor_bytes);
    {
        use std::io::Read;
        let mut tcp = std::net::TcpStream::connect(format!("127.0.0.1:{receiver_port}"))?;
        tcp.set_read_timeout(Some(timeout))?;
        tcp.read_to_end(&mut received)?;
    }

    match receiver_cb.recv_event_for(&anchor_uuid, timeout)? {
        TestCallbackEvent::Done { .. } => {}
        TestCallbackEvent::Failed { reason, .. } => {
            anyhow::bail!("drr anchor receiver stream_failed: {reason}")
        }
        other => anyhow::bail!("drr anchor receiver: expected Done, got {other:?}"),
    }
    match anchor_cb.recv_event_for(&anchor_uuid, timeout)? {
        TestCallbackEvent::Done { .. } => {}
        TestCallbackEvent::Failed { reason, .. } => {
            anyhow::bail!("drr anchor sender stream_failed: {reason}")
        }
        other => anyhow::bail!("drr anchor sender: expected Done, got {other:?}"),
    }

    write_task.await??;

    let sha256_rcvd = sha256_hex(&received);
    anyhow::ensure!(anchor_sha256 == sha256_rcvd, "drr anchor sha256 MISMATCH");
    println!("  drr anchor sha256 ✓");

    {
        let mut agent = connect_agent(sender_c2i).context("connect_agent(sender_c2i) failed")?;
        agent.set_max_concurrent(0)?; // 0 = restore default
    }

    println!("  drr PASSED ✓");
    println!();
    Ok(())
}
