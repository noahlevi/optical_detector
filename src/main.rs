use chrono::Utc;
use std::sync::{Arc, Mutex};

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

    // Shared slot for the latest frame — writer thread drains it asynchronously.
    let save_slot: Arc<Mutex<Option<Vec<u8>>>> = Arc::new(Mutex::new(None));
    let save_slot_writer = Arc::clone(&save_slot);

    std::thread::Builder::new()
        .name("frame-save".into())
        .spawn(move || loop {
            let data = save_slot_writer.lock().unwrap().take();
            if let Some(frame) = data {
                let _ = std::fs::write("frame_last.nv12.tmp", &frame);
                let _ = std::fs::rename("frame_last.nv12.tmp", "frame_last.nv12");
            }
            std::thread::sleep(std::time::Duration::from_millis(16));
        })?;

    let mut frame_idx = 0_u64;
    let mut max_latency_ms = 0.0_f64;

    loop {
        let (frame_ts, frame) = match day_cam_hw.recv_frame() {
            Some(f) => f,
            None => break,
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
            "frame={frame_idx} ts={} latency_ms={latency_ms:.3} max_latency_ms={max_latency_ms:.3}",
            frame_ts.format("%H:%M:%S%.6f"),
        );

        // Save first stable frame after AE warmup
        if frame_idx == warmup + 1 {
            std::fs::write("frame_first.nv12", &frame)?;
            println!("saved frame_first.nv12 (after {warmup} warmup frames)");
        }

        // Hand latest frame to save thread — non-blocking, no latency impact
        if frame_idx > warmup {
            *save_slot.lock().unwrap() = Some(frame);
        }
    }

    Ok(())
}
