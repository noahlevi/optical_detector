use chrono::{DateTime, Utc};

#[cfg(target_os = "linux")]
mod imp {
    use super::*;
    use std::fs::File;
    use std::os::fd::AsRawFd;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::{self, Receiver};
    use std::sync::Arc;
    use std::thread::{self, JoinHandle};
    use v4l::buffer::Type as BufferType;
    use v4l::io::traits::CaptureStream;
    use v4l::prelude::MmapStream;
    use v4l::video::Capture;
    use v4l::{control, Device, Format, FourCC};

    const DEFAULT_CAPTURE_BUFFERS: u32 = 4;
    const DEFAULT_EXPOSURE_US: i64 = 1_000;
    const CONTROL_GAIN: u32 = 0x009a2009;
    const CONTROL_EXPOSURE: u32 = 0x009a200a;
    const CONTROL_FRAME_RATE: u32 = 0x009a200b;
    const CONTROL_BYPASS_MODE: u32 = 0x009a2064;
    const CONTROL_OVERRIDE_ENABLE: u32 = 0x009a2065;
    const CONTROL_LOW_LATENCY_MODE: u32 = 0x009a206d;

    pub struct CsiColorCamera {
        frame_rx: Receiver<(DateTime<Utc>, Vec<u8>)>,
        stop: Arc<AtomicBool>,
        capture_thread: Option<JoinHandle<()>>,
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
            let device_path = camera_device_path(sensor_id);
            let mut device = Device::with_path(&device_path)?;

            configure_format(&mut device, width, height)?;
            configure_controls(&device, fps)?;

            let (sync_tx, sync_rx) = mpsc::channel::<i64>();
            let chip = gpio_chip.to_string();
            thread::Builder::new()
                .name("gpio-sync".into())
                .spawn(move || {
                    gpio_sync_listener(&chip, gpio_line, &sync_tx);
                    tracing::warn!("GPIO SYNC listener exited");
                })?;

            let (frame_tx, frame_rx) = mpsc::sync_channel(1);
            let stop = Arc::new(AtomicBool::new(false));
            let stop_capture = Arc::clone(&stop);
            let buffer_count = capture_buffers();

            let capture_thread =
                thread::Builder::new()
                    .name("v4l2-capture".into())
                    .spawn(move || {
                        let mut stream = match MmapStream::with_buffers(
                            &mut device,
                            BufferType::VideoCapture,
                            buffer_count,
                        ) {
                            Ok(stream) => stream,
                            Err(error) => {
                                tracing::error!("failed to create V4L2 mmap stream: {error}");
                                return;
                            }
                        };

                        tracing::info!(
                            "capturing from {} as RG10 {}x{} @ {} fps with {} buffers",
                            device_path,
                            width,
                            height,
                            fps,
                            buffer_count
                        );

                        while !stop_capture.load(Ordering::Relaxed) {
                            let (data, _meta) = match stream.next() {
                                Ok(frame) => frame,
                                Err(error) => {
                                    tracing::error!("V4L2 capture failed: {error}");
                                    break;
                                }
                            };

                            let ts = timestamp_from_sync(&sync_rx);
                            let _ = frame_tx.try_send((ts, data.to_vec()));
                        }
                    })?;

            Ok(Self {
                frame_rx,
                stop,
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
            self.stop.store(true, Ordering::Relaxed);
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

    fn configure_format(
        device: &mut Device,
        width: u32,
        height: u32,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let format = Format::new(width, height, FourCC::new(b"RG10"));
        let applied = device.set_format(&format)?;
        tracing::info!(
            "camera format={} {}x{} bytes_per_line={} size_image={}",
            applied.fourcc.str().unwrap_or("unknown"),
            applied.width,
            applied.height,
            applied.stride,
            applied.size
        );
        Ok(())
    }

    fn configure_controls(device: &Device, fps: u32) -> Result<(), Box<dyn std::error::Error>> {
        set_i64_control(device, CONTROL_BYPASS_MODE, 1, "bypass_mode")?;
        set_i64_control(device, CONTROL_OVERRIDE_ENABLE, 1, "override_enable")?;
        set_bool_control(device, CONTROL_LOW_LATENCY_MODE, true, "low_latency_mode")?;
        set_i64_control(
            device,
            CONTROL_FRAME_RATE,
            i64::from(fps) * 1_000_000,
            "frame_rate",
        )?;

        let exposure_us = std::env::var("CAM_EXPOSURE_US")
            .ok()
            .and_then(|value| value.parse::<i64>().ok())
            .unwrap_or(DEFAULT_EXPOSURE_US);
        set_i64_control(device, CONTROL_EXPOSURE, exposure_us, "exposure")?;

        if let Some(gain) = std::env::var("CAM_GAIN")
            .ok()
            .and_then(|value| value.parse::<i64>().ok())
        {
            set_i64_control(device, CONTROL_GAIN, gain, "gain")?;
        }

        Ok(())
    }

    fn set_i64_control(
        device: &Device,
        id: u32,
        value: i64,
        name: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        device.set_control(control::Control {
            id,
            value: control::Value::Integer(value),
        })?;
        tracing::info!("camera {name}={value}");
        Ok(())
    }

    fn set_bool_control(
        device: &Device,
        id: u32,
        value: bool,
        name: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        device.set_control(control::Control {
            id,
            value: control::Value::Boolean(value),
        })?;
        tracing::info!("camera {name}={value}");
        Ok(())
    }

    fn camera_device_path(sensor_id: u32) -> String {
        std::env::var("CAM_VIDEO_DEVICE").unwrap_or_else(|_| format!("/dev/video{sensor_id}"))
    }

    fn capture_buffers() -> u32 {
        std::env::var("CAM_CAPTURE_BUFFERS")
            .ok()
            .and_then(|value| value.parse::<u32>().ok())
            .filter(|buffers| *buffers > 1)
            .unwrap_or(DEFAULT_CAPTURE_BUFFERS)
    }

    fn timestamp_from_sync(sync_rx: &Receiver<i64>) -> DateTime<Utc> {
        match sync_rx.try_recv() {
            Ok(mono_ns) => {
                let mut tp = libc::timespec {
                    tv_sec: 0,
                    tv_nsec: 0,
                };
                unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut tp) };
                let mono_now = tp.tv_sec as i64 * 1_000_000_000 + tp.tv_nsec as i64;
                let age_ns = mono_now - mono_ns;
                tracing::debug!("SYNC age: {}us", age_ns / 1000);
                Utc::now() - chrono::Duration::nanoseconds(age_ns)
            }
            Err(_) => {
                tracing::error!("no SYNC timestamp, using Utc::now()");
                Utc::now()
            }
        }
    }

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

    pub fn width(&self) -> u32 {
        0
    }

    pub fn height(&self) -> u32 {
        0
    }

    pub fn recv_frame(&mut self) -> Option<(DateTime<Utc>, Vec<u8>)> {
        None
    }
}

#[cfg(not(target_os = "linux"))]
impl crate::CameraHw for CsiColorCamera {
    type Frame = Vec<u8>;

    fn recv_frame(&mut self) -> Option<(DateTime<Utc>, Vec<u8>)> {
        None
    }
}
