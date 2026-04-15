use chrono::{DateTime, Utc};

pub trait CameraHw {
    type Frame;

    fn recv_frame(&mut self) -> Option<(DateTime<Utc>, Self::Frame)>;
}

pub mod csi_color;
