**Progress tracker — updated:**

| # | Test | Status |
|---|---|---|
| 1 | `c2i_drains_before_bulk` | ✅ passing |
| 2 | `bulk_streams_share_budget` | ✅ passing |
| 3 | `idle_stream_does_not_accumulate_deficit` | ✅ passing |
| 4 | `deregister_removes_stream` | ✅ passing |
| 5 | `schedule_never_exceeds_budget` | ✅ passing |
| 6 | `c2i_does_not_starve_when_bulk_present` | ✅ passing |
| 7a | `token_bucket_caps_throughput` | ✅ passing |
| 7b | `mock_stream_bw_cap_enforced` | ✅ passing |
| 8 | Concurrent files / pending queue integration test | 🔒 blocked — data pump stub |
| 9 | Large bulk + C2I latency (DRR wired E2E) | 🔒 blocked — data pump stub |
| 10 | Throughput measurement vs. BW cap | 🔒 blocked — data pump stub |
