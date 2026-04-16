use chrono::Utc;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            std::env::var("RUST_LOG").unwrap_or_else(|_| "info,optical_detector=debug".to_string()),
        )
        .init();

    let mut day_cam_hw =
        optical_detector::csi_color::CsiColorCamera::new(0, 1920, 1080, 30, "gpiochip0", 144)?;

    println!(
        "camera started: {}x{}, waiting for frames",
        day_cam_hw.width(),
        day_cam_hw.height()
    );

    // Number of frames to skip while auto-exposure stabilises.
    // Override with AE_WARMUP_FRAMES env var.
    let warmup: u64 = std::env::var("AE_WARMUP_FRAMES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(100);

    let mut frame_idx = 0_u64;
    let mut max_latency_ms = 0.0_f64;
    let mut last_frame: Vec<u8> = Vec::new();

    loop {
        let (frame_ts, frame) = match day_cam_hw.recv_frame() {
            Some(f) => f,
            None => {
                if !last_frame.is_empty() {
                    std::fs::write("frame_last.nv12", &last_frame)?;
                    println!("saved frame_last.nv12");
                }
                break;
            }
        };

        let latency_us = (Utc::now() - frame_ts)
            .num_microseconds()
            .ok_or("latency overflow")?;
        let latency_ms = latency_us as f64 / 1_000.0;

        if latency_ms > max_latency_ms {
            max_latency_ms = latency_ms;
        }

        frame_idx += 1;
        println!(
            "frame={frame_idx} bytes={} latency_ms={latency_ms:.3} max_latency_ms={max_latency_ms:.3}",
            frame.len(),
        );

        // Save first stable frame after AE warmup
        if frame_idx == warmup + 1 {
            std::fs::write("frame_first.nv12", &frame)?;
            println!("saved frame_first.nv12 (after {warmup} warmup frames)");
        }

        // Atomic overwrite every frame after warmup — survives Ctrl+C intact.
        // Write to tmp first, then rename (rename is atomic on Linux).
        if frame_idx > warmup {
            std::fs::write("frame_last.nv12.tmp", &frame)?;
            std::fs::rename("frame_last.nv12.tmp", "frame_last.nv12")?;
        }

        last_frame = frame;
    }

    Ok(())
}
