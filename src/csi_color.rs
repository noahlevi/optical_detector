use chrono::{DateTime, Utc};

#[cfg(target_os = "linux")]
mod imp {
    use super::*;
    use std::fs::File;
    use std::os::fd::AsRawFd;
    use std::sync::mpsc::{self, Receiver};
    use std::thread;

    // -----------------------------------------------------------------------
    // FFI: thin C wrapper around libargus
    // -----------------------------------------------------------------------

    #[repr(C)]
    struct ArgusContext {
        _opaque: [u8; 0],
    }

    #[link(name = "argus_wrapper", kind = "static")]
    extern "C" {
        fn argus_create(
            sensor_id: u32,
            width: u32,
            height: u32,
            fps: u32,
        ) -> *mut ArgusContext;

        fn argus_acquire_frame(
            ctx: *mut ArgusContext,
            buffer: *mut u8,
            buffer_size: u32,
            timestamp_ns: *mut i64,
        ) -> i32;

        fn argus_destroy(ctx: *mut ArgusContext);
    }

    // SAFETY: Argus context is accessed from a single capture thread only.
    unsafe impl Send for ArgusContext {}

    // -----------------------------------------------------------------------
    // Public camera type
    // -----------------------------------------------------------------------

    pub struct CsiColorCamera {
        frame_rx: Receiver<(DateTime<Utc>, Vec<u8>)>,
        stop_tx: mpsc::SyncSender<()>,
        capture_thread: Option<thread::JoinHandle<()>>,
        width: u32,
        height: u32,
    }

    impl CsiColorCamera {
        pub fn new(
            sensor_id: u32,
            width: u32,
            height: u32,
            fps: u32,
            gpio_chip: &str,
            gpio_line: u32,
        ) -> Result<Self, Box<dyn std::error::Error>> {
            // GPIO SYNC listener
            let (sync_tx, sync_rx) = mpsc::channel::<i64>();
            let chip = gpio_chip.to_string();
            thread::Builder::new()
                .name("gpio-sync".into())
                .spawn(move || {
                    gpio_sync_listener(&chip, gpio_line, &sync_tx);
                    tracing::warn!("GPIO SYNC listener exited");
                })?;

            // Channel: capture thread → recv_frame()
            let (frame_tx, frame_rx) = mpsc::sync_channel::<(DateTime<Utc>, Vec<u8>)>(1);

            // Channel: stop signal
            let (stop_tx, stop_rx) = mpsc::sync_channel::<()>(1);

            let nv12_size = (width * height * 3 / 2) as usize;

            let capture_thread = thread::Builder::new()
                .name("argus-capture".into())
                .spawn(move || {
                    // Create Argus context on the capture thread (required by Argus).
                    let ctx = unsafe { argus_create(sensor_id, width, height, fps) };
                    if ctx.is_null() {
                        tracing::error!("argus_create failed");
                        return;
                    }
                    tracing::info!(
                        "Argus capture started: {}x{} @ {} fps (MAILBOX mode)",
                        width,
                        height,
                        fps
                    );

                    let mut nv12_buf = vec![0u8; nv12_size];

                    loop {
                        // Check for stop signal (non-blocking)
                        if stop_rx.try_recv().is_ok() {
                            break;
                        }

                        let mut argus_ts_ns: i64 = 0;
                        let written = unsafe {
                            argus_acquire_frame(
                                ctx,
                                nv12_buf.as_mut_ptr(),
                                nv12_buf.len() as u32,
                                &mut argus_ts_ns,
                            )
                        };

                        if written < 0 {
                            tracing::error!("argus_acquire_frame failed");
                            break;
                        }

                        // Convert Argus monotonic timestamp to wall-clock DateTime.
                        // We take both clocks at the same moment to compute the offset.
                        let frame_ts = mono_ns_to_utc(argus_ts_ns);

                        // Also consume GPIO SYNC if available (for cross-check).
                        if let Ok(sync_ns) = sync_rx.try_recv() {
                            let mut tp = libc::timespec { tv_sec: 0, tv_nsec: 0 };
                            unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut tp) };
                            let mono_now =
                                tp.tv_sec as i64 * 1_000_000_000 + tp.tv_nsec as i64;
                            let sync_age_us = (mono_now - sync_ns) / 1000;
                            tracing::debug!("GPIO SYNC age at frame delivery: {}us", sync_age_us);
                        }

                        let rgb = nv12_to_rgb(
                            &nv12_buf[..written as usize],
                            width as usize,
                            height as usize,
                        );
                        let _ = frame_tx.try_send((frame_ts, rgb));
                    }

                    unsafe { argus_destroy(ctx) };
                })?;

            Ok(Self {
                frame_rx,
                stop_tx,
                capture_thread: Some(capture_thread),
                width,
                height,
            })
        }

        pub fn width(&self) -> u32 {
            self.width
        }

        pub fn height(&self) -> u32 {
            self.height
        }

        pub fn recv_frame(&mut self) -> Option<(DateTime<Utc>, Vec<u8>)> {
            self.frame_rx.recv().ok()
        }
    }

    impl Drop for CsiColorCamera {
        fn drop(&mut self) {
            let _ = self.stop_tx.try_send(());
            if let Some(handle) = self.capture_thread.take() {
                let _ = handle.join();
            }
        }
    }

    impl crate::CameraHw for CsiColorCamera {
        type Frame = Vec<u8>;

        fn recv_frame(&mut self) -> Option<(DateTime<Utc>, Vec<u8>)> {
            self.frame_rx.recv().ok()
        }
    }

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    /// Convert NV12 (full-range YUV 4:2:0) to packed RGB.
    /// Output: width × height × 3 bytes, row-major, R-G-B order.
    fn nv12_to_rgb(nv12: &[u8], width: usize, height: usize) -> Vec<u8> {
        let y_plane  = &nv12[..width * height];
        let uv_plane = &nv12[width * height..];
        let mut rgb  = vec![0u8; width * height * 3];

        for row in 0..height {
            for col in 0..width {
                let y  = y_plane[row * width + col] as i32;
                let uv = (row / 2) * width + (col & !1);
                let u  = uv_plane[uv]     as i32 - 128;
                let v  = uv_plane[uv + 1] as i32 - 128;

                let r = (y + 1402 * v / 1000).clamp(0, 255) as u8;
                let g = (y - 344  * u / 1000 - 714 * v / 1000).clamp(0, 255) as u8;
                let b = (y + 1772 * u / 1000).clamp(0, 255) as u8;

                let i = (row * width + col) * 3;
                rgb[i]     = r;
                rgb[i + 1] = g;
                rgb[i + 2] = b;
            }
        }
        rgb
    }

    /// Convert a CLOCK_MONOTONIC nanosecond timestamp (from Argus) to UTC.
    fn mono_ns_to_utc(mono_ns: i64) -> DateTime<Utc> {
        let mut tp = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut tp) };
        let mono_now_ns = tp.tv_sec as i64 * 1_000_000_000 + tp.tv_nsec as i64;
        let age_ns = mono_now_ns - mono_ns;
        Utc::now() - chrono::Duration::nanoseconds(age_ns)
    }

    /// Listens for rising edges on the camera SYNC GPIO pin.
    fn gpio_sync_listener(chip: &str, line: u32, ts: &mpsc::Sender<i64>) {
        let chip_path = format!("/dev/{chip}");
        let chip_fd = match File::open(&chip_path) {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!("GPIO SYNC not available ({chip_path}): {e}");
                return;
            }
        };

        #[repr(C)]
        struct GpioV2LineRequest {
            offsets: [u32; 64],
            consumer: [u8; 32],
            config: GpioV2LineConfig,
            num_lines: u32,
            event_buffer_size: u32,
            _padding: [u32; 5],
            fd: i32,
        }

        #[repr(C)]
        struct GpioV2LineConfig {
            flags: u64,
            num_attrs: u32,
            _padding: [u32; 5],
            attrs: [u8; 240],
        }

        #[repr(C)]
        struct GpioV2LineEvent {
            timestamp_ns: u64,
            id: u32,
            offset: u32,
            seqno: u32,
            line_seqno: u32,
            _padding: [u32; 6],
        }

        const GPIO_V2_LINE_FLAG_INPUT: u64 = 0x04;
        const GPIO_V2_LINE_FLAG_EDGE_RISING: u64 = 0x10;
        const GPIO_V2_GET_LINE_IOCTL: libc::c_ulong =
            ((3u64 << 30) | (592u64 << 16) | (0xB4u64 << 8) | 0x07u64) as libc::c_ulong;

        let mut req = unsafe { std::mem::zeroed::<GpioV2LineRequest>() };
        req.offsets[0] = line;
        req.num_lines = 1;
        req.config.flags = GPIO_V2_LINE_FLAG_INPUT | GPIO_V2_LINE_FLAG_EDGE_RISING;
        let label = b"optical_detector";
        req.consumer[..label.len()].copy_from_slice(label);

        let ret = unsafe { libc::ioctl(chip_fd.as_raw_fd(), GPIO_V2_GET_LINE_IOCTL, &mut req) };
        if ret < 0 || req.fd < 0 {
            tracing::warn!(
                "GPIO SYNC ioctl failed on {chip_path} line {line}: {}",
                std::io::Error::last_os_error()
            );
            return;
        }

        tracing::info!("GPIO SYNC listening on {chip_path} line {line}");

        let mut event = unsafe { std::mem::zeroed::<GpioV2LineEvent>() };
        let event_size = std::mem::size_of::<GpioV2LineEvent>();

        loop {
            let n = unsafe {
                libc::read(
                    req.fd,
                    &mut event as *mut _ as *mut libc::c_void,
                    event_size,
                )
            };
            if n == event_size as isize {
                let _ = ts.send(event.timestamp_ns as i64);
            }
        }
    }
}

#[cfg(target_os = "linux")]
pub use imp::CsiColorCamera;

// -----------------------------------------------------------------------
// Stub for non-Linux (dev machines)
// -----------------------------------------------------------------------

#[cfg(not(target_os = "linux"))]
pub struct CsiColorCamera;

#[cfg(not(target_os = "linux"))]
impl CsiColorCamera {
    pub fn new(
        _sensor_id: u32,
        _width: u32,
        _height: u32,
        _fps: u32,
        _gpio_chip: &str,
        _gpio_line: u32,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        Err("CsiColorCamera is only supported on Linux".into())
    }

    pub fn width(&self) -> u32 { 0 }
    pub fn height(&self) -> u32 { 0 }
    pub fn recv_frame(&mut self) -> Option<(DateTime<Utc>, Vec<u8>)> { None }
}

#[cfg(not(target_os = "linux"))]
impl crate::CameraHw for CsiColorCamera {
    type Frame = Vec<u8>;
    fn recv_frame(&mut self) -> Option<(DateTime<Utc>, Vec<u8>)> { None }
}
