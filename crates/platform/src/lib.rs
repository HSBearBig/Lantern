use anyhow::Result;
use lantern_protocol::{ControlEvent, DisplayInfo};
use std::time::{Duration, Instant};

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

#[derive(Clone, Copy, Debug)]
pub struct CaptureOptions {
    pub max_width: u32,
    pub max_height: u32,
    pub fps: u32,
    pub bitrate: u32,
    pub software: bool,
}
impl Default for CaptureOptions {
    fn default() -> Self {
        Self {
            max_width: 1920,
            max_height: 1080,
            fps: 30,
            bitrate: 4_000_000,
            software: false,
        }
    }
}

pub struct RawFrame {
    pub bgra: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub stride: usize,
    pub captured_at: Instant,
}
pub struct EncodedFrame {
    /// Annex-B H.264; IDRs include SPS and PPS.
    pub data: Vec<u8>,
    pub duration: Duration,
    pub keyframe: bool,
}
pub trait CaptureSource: Send {
    fn display(&self) -> DisplayInfo;
    fn next_frame(&mut self, timeout: Duration) -> Result<Option<RawFrame>>;
}
pub trait VideoEncoder: Send {
    fn encode(&mut self, frame: RawFrame, force_keyframe: bool) -> Result<Vec<EncodedFrame>>;
    fn mode(&self) -> &'static str;
}
pub trait InputInjector: Send {
    fn inject(&mut self, event: &ControlEvent) -> Result<()>;
    fn release_all(&mut self) -> Result<()>;
}
pub struct NativeParts {
    pub capture: Box<dyn CaptureSource>,
    pub encoder: Box<dyn VideoEncoder>,
    pub input: Box<dyn InputInjector>,
}
pub fn open(options: CaptureOptions) -> Result<NativeParts> {
    #[cfg(target_os = "macos")]
    return macos::open(options);
    #[cfg(target_os = "windows")]
    return windows::open(options);
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = options;
        anyhow::bail!("Native capture is supported on macOS and Windows only")
    }
}

/// Largest even-sized frame within the cap, preserving aspect ratio.
pub fn fit_dimensions(width: u32, height: u32, max_width: u32, max_height: u32) -> (u32, u32) {
    let ratio = (max_width as f64 / width.max(1) as f64)
        .min(max_height as f64 / height.max(1) as f64)
        .min(1.0);
    (
        ((width as f64 * ratio) as u32 & !1).max(2),
        ((height as f64 * ratio) as u32 & !1).max(2),
    )
}
#[cfg(test)]
mod tests {
    #[test]
    fn keeps_aspect_ratio_and_even_dimensions() {
        assert_eq!(super::fit_dimensions(3024, 1964, 1920, 1080), (1662, 1080));
        assert_eq!(super::fit_dimensions(3840, 2160, 1920, 1080), (1920, 1080));
        assert_eq!(super::fit_dimensions(1280, 720, 1920, 1080), (1280, 720));
    }
}
