use chrono::{DateTime, Utc};
use gstreamer::prelude::*;
use gstreamer_app::AppSink;

pub struct CsiColorCamera {
    frame_rx: std::sync::mpsc::Receiver<(DateTime<Utc>, Vec<u8>)>,
    pipeline: gstreamer::Pipeline,
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
            "nvarguscamerasrc name=cam sensor-id={sensor_id} ! \
             video/x-raw(memory:NVMM),width={width},height={height},framerate={fps}/1 ! \
             nvvidconv ! \
             video/x-raw,format=NV12 ! \
             appsink name=sink sync=false",
        );

        let pipeline = gstreamer::parse::launch(&pipeline_str)?
            .downcast::<gstreamer::Pipeline>()
            .map_err(|_| "failed to downcast to Pipeline")?;

        let sink = pipeline
            .by_name("sink")
            .unwrap()
            .downcast::<AppSink>()
            .map_err(|_| "failed to downcast to AppSink")?;

        sink.set_property("max-buffers", 1u32);
        sink.set_property("drop", true);
        sink.set_property("wait-on-eos", false);

        let (sync_tx, sync_rx) = std::sync::mpsc::channel::<i64>();

        // Start GPIO SYNC listener
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

        pipeline.set_state(gstreamer::State::Playing)?;

        // Frame delivery channel
        let (frame_tx, frame_rx) = std::sync::mpsc::sync_channel(1);

        // AppSink callback — fires on GStreamer streaming thread
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

                    // Timestamp from GPIO SYNC (one per frame, FIFO)
                    let ts = match sync_rx.try_recv() {
                        Ok(mono_ns) => {
                            let mut tp = libc::timespec {
                                tv_sec: 0,
                                tv_nsec: 0,
                            };
                            unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut tp) };
                            let mono_now = tp.tv_sec as i64 * 1_000_000_000 + tp.tv_nsec as i64;
                            let age_ns = mono_now - mono_ns;
                            let ts = Utc::now() - chrono::Duration::nanoseconds(age_ns);
                            tracing::debug!("SYNC age: {}us", age_ns / 1000);
                            ts
                        }
                        Err(_) => {
                            tracing::error!("no SYNC timestamp, using Utc::now()");
                            Utc::now()
                        }
                    };

                    // Send raw BGRx data + timestamp; convert to RGB outside callback
                    let _ = frame_tx.try_send((ts, data.to_vec()));

                    Ok(gstreamer::FlowSuccess::Ok)
                })
                .build(),
        );

        Ok(Self {
            frame_rx,
            pipeline,
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

    /// Access nvarguscamerasrc element to change runtime properties.
    pub fn source_element(&self) -> Option<gstreamer::Element> {
        self.pipeline.by_name("cam")
    }

    pub fn recv_frame(&mut self) -> Option<(DateTime<Utc>, Vec<u8>)> {
        self.frame_rx.recv().ok()
    }

    /*pub fn autofocus_enabled(&self) -> bool {
        self.af.is_some()
    }

    pub fn set_autofocus(&mut self, enabled: bool) {
        if enabled && self.af.is_none() {
            let center = self.lens.focus();
            self.af = Some(AutoFocus::new(center, 0.005)); // ±5mm sweep
        } else if !enabled {
            self.af = None;
        }
    }*/
}

impl Drop for CsiColorCamera {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gstreamer::State::Null);
    }
}

/// Listens for rising edges on the camera SYNC GPIO pin using the
/// chardev interface (/dev/gpiochipN) and stores the kernel timestamp.
fn gpio_sync_listener(chip: &str, line: u32, ts: &std::sync::mpsc::Sender<i64>) {
    use std::fs::File;
    use std::os::unix::io::AsRawFd;

    let chip_path = format!("/dev/{chip}");
    let chip_fd = match File::open(&chip_path) {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!("GPIO SYNC not available ({chip_path}): {e}");
            return;
        }
    };

    // GPIO v2 line request for edge detection
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
        attrs: [u8; 240], // 10 × GpioV2LineConfigAttribute (24 bytes each)
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

    // _IOWR(0xB4, 0x07, struct gpio_v2_line_request) — size must be 592
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

/*
fn focal_length(&self) -> f64 {
    self.lens.focal_length()
}

fn set_focal_length(&mut self, meters: f64) {
    self.lens.set_focal_length(meters);
}

fn fov(&self) -> angular_units::Rad<f64> {
    self.lens.fov()
}

fn set_focus(&mut self, offset: f64, speed: Option<f64>) {
    self.lens.set_focus(offset, speed);
}

fn focus(&self) -> f64 {
    self.lens.focus()
}
*/
