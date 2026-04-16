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

    let mut frame_idx = 0_u64;
    let mut max_latency_ms = 0.0_f64;
    let mut last_frame: Vec<u8> = Vec::new();

    loop {
        let (frame_ts, frame) = match day_cam_hw.recv_frame() {
            Some(f) => f,
            None => {
                // Camera stopped — save last frame
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

        if frame_idx == 1 {
            std::fs::write("frame_first.nv12", &frame)?;
            println!("saved frame_first.nv12");
        }

        last_frame = frame;
    }

    Ok(())
}
