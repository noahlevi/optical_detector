use chrono::{DateTime, Utc};

#[cfg(target_os = "linux")]
mod imp {
    use super::*;
    use gstreamer::prelude::*;
    use gstreamer::MessageView;
    use gstreamer_app::AppSink;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::thread::{self, JoinHandle};

    use crate::CameraHw;

    pub struct CsiColorCamera {
        frame_rx: std::sync::mpsc::Receiver<(DateTime<Utc>, Vec<u8>)>,
        pipeline: gstreamer::Pipeline,
        stop: Arc<AtomicBool>,
        bus_thread: Option<JoinHandle<()>>,
        width: u32,
        height: u32,
    }

    impl CsiColorCamera {
        /// `gpio_chip` / `gpio_line`: SYNC output from camera (e.g. "gpiochip0", 144).
        pub fn new(
            sensor_id: u32,
            width: u32,
            height: u32,
            fps: u32,
            gpio_chip: &str,
            gpio_line: u32,
        ) -> Result<Self, Box<dyn std::error::Error>> {
            gstreamer::init()?;

            let pipeline_str = format!(
                "nvarguscamerasrc name=cam sensor-id={sensor_id} \
                 wbmode=0 tnr-mode=0 ! \
                 video/x-raw(memory:NVMM),width={width},height={height},framerate={fps}/1,format=NV12 ! \
                 nvvidconv ! \
                 video/x-raw,format=NV12 ! \
                 appsink name=sink sync=false max-buffers=1 drop=true",
            );
            tracing::info!("camera pipeline: {pipeline_str}");

            let pipeline = gstreamer::parse::launch(&pipeline_str)?
                .downcast::<gstreamer::Pipeline>()
                .map_err(|_| "failed to downcast to Pipeline")?;

            let sink = pipeline
                .by_name("sink")
                .ok_or("missing sink element")?
                .downcast::<AppSink>()
                .map_err(|_| "failed to downcast to AppSink")?;

            let (sync_tx, sync_rx) = std::sync::mpsc::channel::<i64>();

            {
                let chip = gpio_chip.to_string();
                std::thread::Builder::new()
                    .name("gpio-sync".into())
                    .spawn(move || {
                        gpio_sync_listener(&chip, gpio_line, &sync_tx);
                        tracing::warn!("GPIO SYNC listener exited");
                    })
                    .expect("failed to spawn GPIO SYNC thread");
            }

            let (frame_tx, frame_rx) = std::sync::mpsc::sync_channel(1);
            let stop = Arc::new(AtomicBool::new(false));

            sink.set_callbacks(
                gstreamer_app::AppSinkCallbacks::builder()
                    .new_sample(move |appsink| {
                        let sample = appsink
                            .pull_sample()
                            .map_err(|_| gstreamer::FlowError::Eos)?;
                        let buffer = sample.buffer().ok_or(gstreamer::FlowError::Error)?;
                        let map = buffer
                            .map_readable()
                            .map_err(|_| gstreamer::FlowError::Error)?;
                        let data: &[u8] = map.as_slice();

                        let ts = timestamp_from_sync(&sync_rx);
                        let _ = frame_tx.try_send((ts, data.to_vec()));

                        Ok(gstreamer::FlowSuccess::Ok)
                    })
                    .build(),
            );

            pipeline.set_state(gstreamer::State::Playing)?;
            let bus = pipeline.bus().ok_or("missing pipeline bus")?;
            let bus_stop = Arc::clone(&stop);
            let bus_thread = thread::Builder::new()
                .name("gst-bus".into())
                .spawn(move || {
                    while !bus_stop.load(Ordering::Relaxed) {
                        let Some(message) =
                            bus.timed_pop(Some(gstreamer::ClockTime::from_mseconds(200)))
                        else {
                            continue;
                        };

                        match message.view() {
                            MessageView::Error(err) => {
                                let src = err.src().map(|s| s.path_string()).unwrap_or_default();
                                tracing::error!(
                                    "GStreamer error from {src}: {} ({:?})",
                                    err.error(),
                                    err.debug()
                                );
                            }
                            MessageView::Warning(warn) => {
                                let src = warn.src().map(|s| s.path_string()).unwrap_or_default();
                                tracing::warn!(
                                    "GStreamer warning from {src}: {} ({:?})",
                                    warn.error(),
                                    warn.debug()
                                );
                            }
                            MessageView::Eos(..) => {
                                tracing::warn!("GStreamer pipeline reached EOS");
                                break;
                            }
                            _ => {}
                        }
                    }
                })?;

            Ok(Self {
                frame_rx,
                pipeline,
                stop,
                bus_thread: Some(bus_thread),
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

        /// Access the source element to change runtime properties.
        pub fn source_element(&self) -> Option<gstreamer::Element> {
            self.pipeline.by_name("cam")
        }

        pub fn recv_frame(&mut self) -> Option<(DateTime<Utc>, Vec<u8>)> {
            self.frame_rx.recv().ok()
        }
    }

    impl Drop for CsiColorCamera {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            let _ = self.pipeline.set_state(gstreamer::State::Null);
            if let Some(handle) = self.bus_thread.take() {
                let _ = handle.join();
            }
        }
    }

    impl CameraHw for CsiColorCamera {
        type Frame = Vec<u8>;

        fn recv_frame(&mut self) -> Option<(DateTime<Utc>, Vec<u8>)> {
            self.frame_rx.recv().ok()
        }
    }

    fn timestamp_from_sync(sync_rx: &std::sync::mpsc::Receiver<i64>) -> DateTime<Utc> {
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

    /// Listens for rising edges on the camera SYNC GPIO pin using the
    /// chardev interface (/dev/gpiochipN) and stores the kernel timestamp.
    fn gpio_sync_listener(chip: &str, line: u32, ts: &std::sync::mpsc::Sender<i64>) {
        use std::fs::File;
        use std::os::fd::AsRawFd;

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
        debug_assert_eq!(std::mem::size_of::<GpioV2LineRequest>(), 592);
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
