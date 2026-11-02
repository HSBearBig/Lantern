use crate::{
    fit_dimensions, CaptureOptions, CaptureSource, EncodedFrame, InputInjector, NativeParts,
    RawFrame, VideoEncoder,
};
use anyhow::{anyhow, bail, Context, Result};
use block2::RcBlock;
use lantern_protocol::{ControlEvent, DisplayInfo, MouseButton};
use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{define_class, msg_send, rc::Retained, AnyThread, DefinedClass};
use objc2_core_foundation::{
    CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType, CGPoint,
};
use objc2_core_graphics::{
    CGDisplayBounds, CGEvent, CGEventFlags, CGEventTapLocation, CGEventType, CGMainDisplayID,
    CGMouseButton, CGPreflightScreenCaptureAccess, CGScrollEventUnit,
};
use objc2_core_media::{
    kCMVideoCodecType_H264, CMFormatDescription, CMSampleBuffer, CMTime,
    CMVideoFormatDescriptionGetH264ParameterSetAtIndex,
};
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferCreate, CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow,
    CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
};
use objc2_foundation::{NSArray, NSError};
use objc2_screen_capture_kit::{
    SCContentFilter, SCDisplay, SCShareableContent, SCStream, SCStreamConfiguration,
    SCStreamDelegate, SCStreamFrameInfoStatus, SCStreamOutput, SCStreamOutputType,
};
use objc2_video_toolbox::{
    kVTCompressionPropertyKey_AllowFrameReordering, kVTCompressionPropertyKey_AverageBitRate,
    kVTCompressionPropertyKey_ExpectedFrameRate, kVTCompressionPropertyKey_MaxFrameDelayCount,
    kVTCompressionPropertyKey_MaxKeyFrameInterval, kVTCompressionPropertyKey_ProfileLevel,
    kVTCompressionPropertyKey_RealTime, kVTEncodeFrameOptionKey_ForceKeyFrame,
    kVTProfileLevel_H264_Baseline_AutoLevel,
    kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder, VTCompressionSession,
    VTEncodeInfoFlags, VTSessionSetProperty,
};
use std::collections::HashSet;
use std::ffi::c_void;
use std::ptr::{self, NonNull};
use std::slice;
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

const PIXEL_FORMAT_BGRA: u32 = u32::from_be_bytes(*b"BGRA");

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrusted() -> bool;
}

pub(super) fn open(options: CaptureOptions) -> Result<NativeParts> {
    if !CGPreflightScreenCaptureAccess() {
        bail!("Screen Recording permission is required. Enable it in System Settings > Privacy & Security > Screen Recording, then restart Lantern");
    }
    // SAFETY: AXIsProcessTrusted has no arguments and returns a Boolean value.
    if !unsafe { AXIsProcessTrusted() } {
        bail!("Accessibility permission is required. Enable it in System Settings > Privacy & Security > Accessibility, then restart Lantern");
    }

    let capture = MacCapture::open(options)?;
    let encoder = VideoToolboxEncoder::new(
        capture.display.width,
        capture.display.height,
        options.fps,
        options.bitrate,
        options.software,
    )?;
    let input = MacInput::new(capture.display.clone())?;
    Ok(NativeParts {
        capture: Box::new(capture),
        encoder: Box::new(encoder),
        input: Box::new(input),
    })
}

struct MacCapture {
    stream: Retained<SCStream>,
    _delegate: Retained<CaptureDelegate>,
    mailbox: Arc<FrameMailbox>,
    display: DisplayInfo,
}

#[derive(Default)]
struct FrameMailbox {
    state: Mutex<FrameMailboxState>,
    changed: Condvar,
}

#[derive(Default)]
struct FrameMailboxState {
    /// Capacity-one mailbox. A producer replaces an unconsumed frame so latency stays bounded.
    latest: Option<Result<RawFrame, String>>,
    stopped: Option<String>,
}

impl FrameMailbox {
    fn publish(&self, frame: Result<RawFrame, String>) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.latest = Some(frame);
        self.changed.notify_one();
    }

    fn stop(&self, message: String) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.stopped = Some(message);
        self.changed.notify_all();
    }

    fn receive(&self, timeout: Duration) -> Result<Option<RawFrame>> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (mut state, wait) = self
            .changed
            .wait_timeout_while(state, timeout, |state| {
                state.latest.is_none() && state.stopped.is_none()
            })
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(frame) = state.latest.take() {
            return frame.map(Some).map_err(anyhow::Error::msg);
        }
        if let Some(message) = state.stopped.clone() {
            bail!(message);
        }
        debug_assert!(wait.timed_out());
        Ok(None)
    }
}

struct CaptureDelegateIvars {
    mailbox: Arc<FrameMailbox>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[ivars = CaptureDelegateIvars]
    struct CaptureDelegate;

    unsafe impl NSObjectProtocol for CaptureDelegate {}

    unsafe impl SCStreamOutput for CaptureDelegate {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        #[allow(non_snake_case)]
        unsafe fn stream_didOutputSampleBuffer_ofType(
            &self,
            _stream: &SCStream,
            sample_buffer: &CMSampleBuffer,
            output_type: SCStreamOutputType,
        ) {
            if output_type != SCStreamOutputType::Screen {
                return;
            }
            // Idle/started samples are notifications, not image frames. Treating their
            // absent pixel buffers as errors would stop every static desktop stream.
            let status = unsafe { sample_status(sample_buffer) };
            if matches!(status, Some(2 | 3 | 5)) {
                self.ivars().mailbox.stop(
                    "Screen capture is blank, suspended or stopped; the desktop may be locked"
                        .into(),
                );
                return;
            }
            if matches!(status, Some(1 | 4)) || sample_buffer.image_buffer().is_none() {
                return;
            }
            // SAFETY: ScreenCaptureKit keeps the sample alive for this callback; the copy owns bytes.
            let frame =
                unsafe { copy_sample_buffer(sample_buffer) }.map_err(|error| error.to_string());
            self.ivars().mailbox.publish(frame);
        }
    }

    unsafe impl SCStreamDelegate for CaptureDelegate {
        #[unsafe(method(stream:didStopWithError:))]
        #[allow(non_snake_case)]
        unsafe fn stream_didStopWithError(&self, _stream: &SCStream, error: &NSError) {
            self.ivars().mailbox.stop(format!(
                "ScreenCaptureKit stream stopped: {}",
                error.localizedDescription()
            ));
        }
    }
);

impl CaptureDelegate {
    fn new(mailbox: Arc<FrameMailbox>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(CaptureDelegateIvars { mailbox });
        // SAFETY: NSObject's initializer is valid for this subclass and ivars are initialized.
        unsafe { msg_send![super(this), init] }
    }
}

impl MacCapture {
    fn open(options: CaptureOptions) -> Result<Self> {
        let (tx, rx) = mpsc::sync_channel(1);
        let callback = RcBlock::new(
            move |content: *mut SCShareableContent, error: *mut NSError| {
                let result = if !error.is_null() {
                    // SAFETY: ScreenCaptureKit keeps callback arguments alive for the callback.
                    Err(unsafe { (&*error).localizedDescription().to_string() })
                } else {
                    unsafe { Retained::retain(content) }
                        .ok_or_else(|| "ScreenCaptureKit returned no shareable content".to_owned())
                };
                let _ = tx.send(result);
            },
        );
        // SAFETY: The copied block owns its sender and all callback arguments are used only during invocation.
        unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&callback) };
        let content = rx
            .recv_timeout(Duration::from_secs(10))
            .context("timed out while enumerating displays")?
            .map_err(|message| anyhow!("could not enumerate displays: {message}"))?;

        let main_id = CGMainDisplayID();
        // SAFETY: The returned NSArray and its display objects are retained for this scope.
        let displays = unsafe { content.displays() };
        let sc_display: Retained<SCDisplay> = displays
            .iter()
            .find(|display| unsafe { display.displayID() } == main_id)
            .ok_or_else(|| {
                anyhow!(
                    "the primary display is not available for capture (the session may be locked)"
                )
            })?;

        let source_width = unsafe { sc_display.width() }.max(1) as u32;
        let source_height = unsafe { sc_display.height() }.max(1) as u32;
        let (width, height) = fit_dimensions(
            source_width,
            source_height,
            options.max_width,
            options.max_height,
        );
        let empty_windows = NSArray::new();
        let filter = unsafe {
            SCContentFilter::initWithDisplay_excludingWindows(
                SCContentFilter::alloc(),
                &sc_display,
                &empty_windows,
            )
        };
        let config = unsafe { SCStreamConfiguration::new() };
        unsafe {
            config.setWidth(width as usize);
            config.setHeight(height as usize);
            config.setPixelFormat(PIXEL_FORMAT_BGRA);
            config.setShowsCursor(true);
            config.setScalesToFit(true);
            config.setPreservesAspectRatio(true);
            config.setQueueDepth(2);
            config.setMinimumFrameInterval(CMTime::new(1, options.fps.max(1) as i32));
        }
        let scale = unsafe { sc_display.width() as f64 / sc_display.frame().size.width.max(1.0) };
        let mailbox = Arc::new(FrameMailbox::default());
        let delegate = CaptureDelegate::new(Arc::clone(&mailbox));
        let stream_delegate = ProtocolObject::<dyn SCStreamDelegate>::from_ref(&*delegate);
        let output_delegate = ProtocolObject::<dyn SCStreamOutput>::from_ref(&*delegate);
        let stream = unsafe {
            SCStream::initWithFilter_configuration_delegate(
                SCStream::alloc(),
                &filter,
                &config,
                Some(stream_delegate),
            )
        };
        unsafe {
            stream
                .addStreamOutput_type_sampleHandlerQueue_error(
                    output_delegate,
                    SCStreamOutputType::Screen,
                    None,
                )
                .map_err(|error| {
                    anyhow!(
                        "could not add ScreenCaptureKit output: {}",
                        error.localizedDescription()
                    )
                })?;
        }

        let (start_tx, start_rx) = mpsc::sync_channel(1);
        let started = RcBlock::new(move |error: *mut NSError| {
            let result = if error.is_null() {
                Ok(())
            } else {
                // SAFETY: ScreenCaptureKit keeps the error alive for the callback.
                Err(unsafe { (&*error).localizedDescription().to_string() })
            };
            let _ = start_tx.send(result);
        });
        // SAFETY: SCStream copies the completion block for asynchronous invocation.
        unsafe { stream.startCaptureWithCompletionHandler(Some(&started)) };
        start_rx
            .recv_timeout(Duration::from_secs(10))
            .context("timed out while starting screen capture")?
            .map_err(|message| anyhow!("could not start screen capture: {message}"))?;

        Ok(Self {
            stream,
            _delegate: delegate,
            mailbox,
            display: DisplayInfo {
                id: main_id.to_string(),
                width,
                height,
                scale,
            },
        })
    }
}

// ScreenCaptureKit objects are valid from arbitrary queues. This wrapper moves them as a unit and
// calls them serially through `&mut self`; objc2 cannot currently express that framework guarantee.
unsafe impl Send for MacCapture {}

impl CaptureSource for MacCapture {
    fn display(&self) -> DisplayInfo {
        self.display.clone()
    }

    fn next_frame(&mut self, timeout: Duration) -> Result<Option<RawFrame>> {
        self.mailbox.receive(timeout)
    }
}

impl Drop for MacCapture {
    fn drop(&mut self) {
        let (tx, rx) = mpsc::sync_channel(1);
        let stopped = RcBlock::new(move |error: *mut NSError| {
            let result = if error.is_null() {
                Ok(())
            } else {
                // SAFETY: ScreenCaptureKit keeps the error alive for the callback.
                Err(unsafe { (&*error).localizedDescription().to_string() })
            };
            let _ = tx.send(result);
        });
        // SAFETY: SCStream copies the completion block and the delegate remains retained during Drop.
        unsafe { self.stream.stopCaptureWithCompletionHandler(Some(&stopped)) };
        match rx.recv_timeout(Duration::from_secs(2)) {
            Ok(Ok(())) => {}
            Ok(Err(error)) => tracing::warn!(%error, "could not stop ScreenCaptureKit stream"),
            Err(error) => tracing::warn!(%error, "timed out stopping ScreenCaptureKit stream"),
        }
    }
}

unsafe fn sample_status(sample: &CMSampleBuffer) -> Option<isize> {
    let attachments = sample.sample_attachments_array(false)?;
    if attachments.is_empty() {
        return None;
    }
    // CoreMedia documents this array as CFMutableDictionary sample attachments.
    // The array retains them for the duration of this callback.
    let raw_attachment = attachments.value_at_index(0);
    if raw_attachment.is_null() {
        return None;
    }
    let attachment = &*raw_attachment.cast::<CFType>();
    let dictionary = attachment.downcast_ref::<CFDictionary>()?;
    let key = SCStreamFrameInfoStatus as *const _ as *const c_void;
    let raw = dictionary.value(key);
    if raw.is_null() {
        return None;
    }
    let value = &*raw.cast::<CFType>();
    value.downcast_ref::<CFNumber>()?.as_isize()
}

unsafe fn copy_sample_buffer(sample: &CMSampleBuffer) -> Result<RawFrame> {
    let pixel = sample
        .image_buffer()
        .ok_or_else(|| anyhow!("screen frame has no image buffer"))?;
    let lock = CVPixelBufferLockBaseAddress(&pixel, CVPixelBufferLockFlags::ReadOnly);
    if lock != 0 {
        bail!("could not lock screen pixel buffer ({lock})");
    }
    let width = objc2_core_video::CVPixelBufferGetWidth(&pixel) as u32;
    let height = objc2_core_video::CVPixelBufferGetHeight(&pixel) as u32;
    let stride = CVPixelBufferGetBytesPerRow(&pixel);
    let base = CVPixelBufferGetBaseAddress(&pixel) as *const u8;
    let data = if base.is_null() {
        Vec::new()
    } else {
        slice::from_raw_parts(base, stride.saturating_mul(height as usize)).to_vec()
    };
    let unlock = CVPixelBufferUnlockBaseAddress(&pixel, CVPixelBufferLockFlags::ReadOnly);
    if unlock != 0 {
        bail!("could not unlock screen pixel buffer ({unlock})");
    }
    if data.is_empty() {
        bail!("screen frame contains no pixel data");
    }
    Ok(RawFrame {
        bgra: data,
        width,
        height,
        stride,
        captured_at: Instant::now(),
    })
}

struct VideoToolboxEncoder {
    session: CFRetained<VTCompressionSession>,
    fps: u32,
    frame_index: i64,
    software_requested: bool,
}

// VideoToolbox compression sessions are thread-safe. The trait moves the session between workers,
// while `&mut self` guarantees this wrapper never submits frames concurrently.
unsafe impl Send for VideoToolboxEncoder {}

impl VideoToolboxEncoder {
    fn new(width: u32, height: u32, fps: u32, bitrate: u32, software: bool) -> Result<Self> {
        let software_specification = software.then(|| {
            CFDictionary::<CFString, CFBoolean>::from_slices(
                &[unsafe { kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder }],
                &[CFBoolean::new(false)],
            )
        });
        let encoder_specification = software_specification.as_deref().map(untyped_dictionary);
        let mut raw_session: *mut VTCompressionSession = ptr::null_mut();
        let status = unsafe {
            VTCompressionSession::create(
                None,
                width as i32,
                height as i32,
                kCMVideoCodecType_H264,
                encoder_specification,
                None,
                None,
                None,
                ptr::null_mut(),
                NonNull::new(&mut raw_session).expect("stack pointer is non-null"),
            )
        };
        status_ok(status, "create H.264 encoder")?;
        let session = unsafe {
            CFRetained::from_raw(
                NonNull::new(raw_session)
                    .ok_or_else(|| anyhow!("VideoToolbox returned no encoder"))?,
            )
        };

        set_vt_property(
            &session,
            unsafe { kVTCompressionPropertyKey_RealTime },
            CFBoolean::new(true),
        )?;
        set_vt_property(
            &session,
            unsafe { kVTCompressionPropertyKey_AllowFrameReordering },
            CFBoolean::new(false),
        )?;
        set_vt_property(
            &session,
            unsafe { kVTCompressionPropertyKey_ProfileLevel },
            unsafe { kVTProfileLevel_H264_Baseline_AutoLevel },
        )?;
        let average_bitrate = CFNumber::new_i32(bitrate.min(i32::MAX as u32) as i32);
        set_vt_property(
            &session,
            unsafe { kVTCompressionPropertyKey_AverageBitRate },
            &average_bitrate,
        )?;
        let frame_rate = CFNumber::new_i32(fps.max(1).min(i32::MAX as u32) as i32);
        set_vt_property(
            &session,
            unsafe { kVTCompressionPropertyKey_ExpectedFrameRate },
            &frame_rate,
        )?;
        let key_interval =
            CFNumber::new_i32(fps.saturating_mul(2).max(1).min(i32::MAX as u32) as i32);
        set_vt_property(
            &session,
            unsafe { kVTCompressionPropertyKey_MaxKeyFrameInterval },
            &key_interval,
        )?;
        let delay = CFNumber::new_i32(1);
        set_vt_property(
            &session,
            unsafe { kVTCompressionPropertyKey_MaxFrameDelayCount },
            &delay,
        )?;
        status_ok(
            unsafe { session.prepare_to_encode_frames() },
            "prepare H.264 encoder",
        )?;

        Ok(Self {
            session,
            fps: fps.max(1),
            frame_index: 0,
            software_requested: software,
        })
    }
}

impl Drop for VideoToolboxEncoder {
    fn drop(&mut self) {
        unsafe { self.session.invalidate() };
    }
}

impl VideoEncoder for VideoToolboxEncoder {
    fn encode(&mut self, frame: RawFrame, force_keyframe: bool) -> Result<Vec<EncodedFrame>> {
        let pixel = pixel_buffer_from_bgra(&frame)?;
        let frame_properties = force_keyframe.then(|| {
            CFDictionary::<CFString, CFBoolean>::from_slices(
                &[unsafe { kVTEncodeFrameOptionKey_ForceKeyFrame }],
                &[CFBoolean::new(true)],
            )
        });
        let (tx, rx) = mpsc::sync_channel(1);
        let output = RcBlock::new(
            move |status: i32, _flags: VTEncodeInfoFlags, sample: *mut CMSampleBuffer| {
                let result = if status != 0 {
                    Err(format!("VideoToolbox encode callback failed ({status})"))
                } else if sample.is_null() {
                    Err("VideoToolbox dropped the frame".to_owned())
                } else {
                    // SAFETY: VideoToolbox keeps the sample valid for this callback and we copy all bytes.
                    unsafe { sample_to_annex_b(&*sample) }.map_err(|error| error.to_string())
                };
                let _ = tx.send(result);
            },
        );
        let pts = unsafe { CMTime::new(self.frame_index, self.fps as i32) };
        let duration = unsafe { CMTime::new(1, self.fps as i32) };
        self.frame_index = self.frame_index.saturating_add(1);
        let frame_properties = frame_properties.as_deref().map(untyped_dictionary);
        let output_ref: &block2::DynBlock<dyn Fn(i32, VTEncodeInfoFlags, *mut CMSampleBuffer)> =
            &output;
        let status = unsafe {
            self.session.encode_frame_with_output_handler(
                &pixel,
                pts,
                duration,
                frame_properties,
                ptr::null_mut(),
                (output_ref as *const block2::DynBlock<_>).cast_mut(),
            )
        };
        status_ok(status, "submit frame to H.264 encoder")?;
        status_ok(
            unsafe { self.session.complete_frames(pts) },
            "flush H.264 frame",
        )?;
        let (data, keyframe) = rx
            .recv_timeout(Duration::from_secs(2))
            .context("timed out waiting for H.264 output")?
            .map_err(anyhow::Error::msg)?;
        Ok(vec![EncodedFrame {
            data,
            duration: Duration::from_secs_f64(1.0 / self.fps as f64),
            keyframe,
        }])
    }

    fn mode(&self) -> &'static str {
        if self.software_requested {
            "videotoolbox-software-preferred"
        } else {
            "videotoolbox-hardware-preferred"
        }
    }
}

fn untyped_dictionary(dictionary: &CFDictionary<CFString, CFBoolean>) -> &CFDictionary {
    // SAFETY: Core Foundation dictionary generic parameters exist only to type-check Rust access;
    // the Objective-C APIs consume the same CFDictionaryRef representation.
    unsafe { &*(dictionary as *const CFDictionary<CFString, CFBoolean>).cast::<CFDictionary>() }
}

fn set_vt_property<T: AsRef<CFType> + ?Sized>(
    session: &VTCompressionSession,
    key: &objc2_core_foundation::CFString,
    value: &T,
) -> Result<()> {
    status_ok(
        unsafe { VTSessionSetProperty(session.as_ref(), key, Some(value.as_ref())) },
        "set VideoToolbox property",
    )
}

fn pixel_buffer_from_bgra(frame: &RawFrame) -> Result<CFRetained<CVPixelBuffer>> {
    if frame.stride < frame.width as usize * 4
        || frame.bgra.len() < frame.stride.saturating_mul(frame.height as usize)
    {
        bail!("invalid BGRA frame layout");
    }
    let mut raw: *mut CVPixelBuffer = ptr::null_mut();
    let status = unsafe {
        CVPixelBufferCreate(
            None,
            frame.width as usize,
            frame.height as usize,
            PIXEL_FORMAT_BGRA,
            None,
            NonNull::new(&mut raw).expect("stack pointer is non-null"),
        )
    };
    if status != 0 {
        bail!("could not allocate encoder pixel buffer ({status})");
    }
    let pixel = unsafe {
        CFRetained::from_raw(
            NonNull::new(raw).ok_or_else(|| anyhow!("CoreVideo returned no pixel buffer"))?,
        )
    };
    let lock = unsafe { CVPixelBufferLockBaseAddress(&pixel, CVPixelBufferLockFlags::empty()) };
    if lock != 0 {
        bail!("could not lock encoder pixel buffer ({lock})");
    }
    let dst = CVPixelBufferGetBaseAddress(&pixel) as *mut u8;
    let dst_stride = CVPixelBufferGetBytesPerRow(&pixel);
    if dst.is_null() || dst_stride < frame.width as usize * 4 {
        let _ = unsafe { CVPixelBufferUnlockBaseAddress(&pixel, CVPixelBufferLockFlags::empty()) };
        bail!("encoder pixel buffer has an invalid layout");
    }
    let row_len = frame.width as usize * 4;
    for row in 0..frame.height as usize {
        unsafe {
            ptr::copy_nonoverlapping(
                frame.bgra.as_ptr().add(row * frame.stride),
                dst.add(row * dst_stride),
                row_len,
            );
        }
    }
    let unlock = unsafe { CVPixelBufferUnlockBaseAddress(&pixel, CVPixelBufferLockFlags::empty()) };
    if unlock != 0 {
        bail!("could not unlock encoder pixel buffer ({unlock})");
    }
    Ok(pixel)
}

unsafe fn sample_to_annex_b(sample: &CMSampleBuffer) -> Result<(Vec<u8>, bool)> {
    let format: CFRetained<CMFormatDescription> = sample
        .format_description()
        .ok_or_else(|| anyhow!("encoded sample has no format description"))?;
    let buffer = sample
        .data_buffer()
        .ok_or_else(|| anyhow!("encoded sample has no data buffer"))?;
    let mut available = 0usize;
    let mut total = 0usize;
    let mut bytes: *mut i8 = ptr::null_mut();
    let pointer_status = buffer.data_pointer(0, &mut available, &mut total, &mut bytes);
    if pointer_status != 0 {
        bytes = ptr::null_mut();
        available = 0;
    }
    if total == 0 {
        total = unsafe { buffer.data_length() };
    }
    if total == 0 {
        bail!("encoded H.264 buffer is empty");
    }

    let mut parameter_count = 0usize;
    let mut nal_header_length = 0i32;
    status_ok(
        CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
            &format,
            0,
            ptr::null_mut(),
            ptr::null_mut(),
            &mut parameter_count,
            &mut nal_header_length,
        ),
        "read H.264 format description",
    )?;
    let copied;
    let avcc = if !bytes.is_null() && available >= total {
        slice::from_raw_parts(bytes.cast::<u8>(), total)
    } else {
        copied = {
            let mut data = vec![0_u8; total];
            status_ok(
                buffer.copy_data_bytes(
                    0,
                    total,
                    NonNull::new(data.as_mut_ptr().cast::<c_void>())
                        .expect("non-empty Vec has a non-null data pointer"),
                ),
                "copy non-contiguous H.264 buffer",
            )?;
            data
        };
        &copied
    };
    let (mut access_unit, keyframe) = avcc_to_annex_b(avcc, nal_header_length as usize)?;
    if keyframe {
        let mut prefixed = Vec::new();
        for index in 0..parameter_count {
            let mut parameter = ptr::null();
            let mut size = 0usize;
            status_ok(
                CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                    &format,
                    index,
                    &mut parameter,
                    &mut size,
                    ptr::null_mut(),
                    ptr::null_mut(),
                ),
                "read H.264 parameter set",
            )?;
            if !parameter.is_null() && size > 0 {
                prefixed.extend_from_slice(&[0, 0, 0, 1]);
                prefixed.extend_from_slice(slice::from_raw_parts(parameter, size));
            }
        }
        prefixed.append(&mut access_unit);
        access_unit = prefixed;
    }
    Ok((access_unit, keyframe))
}

fn avcc_to_annex_b(avcc: &[u8], length_size: usize) -> Result<(Vec<u8>, bool)> {
    if !(1..=4).contains(&length_size) {
        bail!("invalid H.264 NAL length size: {length_size}");
    }
    let mut offset = 0usize;
    let mut output = Vec::with_capacity(avcc.len() + 32);
    let mut keyframe = false;
    while offset < avcc.len() {
        if avcc.len() - offset < length_size {
            bail!("truncated H.264 NAL length");
        }
        let mut size = 0usize;
        for byte in &avcc[offset..offset + length_size] {
            size = (size << 8) | *byte as usize;
        }
        offset += length_size;
        if size == 0 || avcc.len() - offset < size {
            bail!("invalid H.264 NAL size");
        }
        let nal = &avcc[offset..offset + size];
        keyframe |= nal.first().is_some_and(|byte| byte & 0x1f == 5);
        output.extend_from_slice(&[0, 0, 0, 1]);
        output.extend_from_slice(nal);
        offset += size;
    }
    Ok((output, keyframe))
}

fn status_ok(status: i32, operation: &str) -> Result<()> {
    if status == 0 {
        Ok(())
    } else {
        bail!("could not {operation} (OSStatus {status})")
    }
}

struct MacInput {
    bounds: objc2_core_foundation::CGRect,
    cursor: CGPoint,
    held_buttons: HashSet<MouseButton>,
    held_keys: HashSet<u16>,
}

impl MacInput {
    fn new(_display: DisplayInfo) -> Result<Self> {
        let bounds = CGDisplayBounds(CGMainDisplayID());
        if bounds.size.width <= 0.0 || bounds.size.height <= 0.0 {
            bail!("primary display has invalid bounds");
        }
        Ok(Self {
            bounds,
            cursor: CGPoint {
                x: bounds.origin.x,
                y: bounds.origin.y,
            },
            held_buttons: HashSet::new(),
            held_keys: HashSet::new(),
        })
    }

    fn post_mouse(&self, event_type: CGEventType, button: CGMouseButton) -> Result<()> {
        let event =
            CGEvent::new_mouse_event(None, event_type, self.cursor, button).ok_or_else(|| {
                anyhow!(
                    "could not create mouse event; Accessibility permission may have been revoked"
                )
            })?;
        CGEvent::set_flags(Some(&event), modifier_flags(&self.held_keys));
        CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
        Ok(())
    }

    fn post_key(&self, keycode: u16, down: bool) -> Result<()> {
        let event = CGEvent::new_keyboard_event(None, keycode, down).ok_or_else(|| {
            anyhow!(
                "could not create keyboard event; Accessibility permission may have been revoked"
            )
        })?;
        CGEvent::set_flags(Some(&event), modifier_flags(&self.held_keys));
        CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
        Ok(())
    }
}

impl InputInjector for MacInput {
    fn inject(&mut self, event: &ControlEvent) -> Result<()> {
        if !event.validate() {
            bail!("rejected invalid control event");
        }
        match event {
            ControlEvent::Pointer { x, y, .. } => {
                self.cursor = CGPoint {
                    x: self.bounds.origin.x + self.bounds.size.width * x,
                    y: self.bounds.origin.y + self.bounds.size.height * y,
                };
                let (kind, button) = if self.held_buttons.contains(&MouseButton::Left) {
                    (CGEventType::LeftMouseDragged, CGMouseButton::Left)
                } else if self.held_buttons.contains(&MouseButton::Right) {
                    (CGEventType::RightMouseDragged, CGMouseButton::Right)
                } else {
                    (CGEventType::MouseMoved, CGMouseButton::Left)
                };
                self.post_mouse(kind, button)?;
            }
            ControlEvent::Button { button, down, .. } => {
                if *down {
                    self.held_buttons.insert(*button);
                } else {
                    self.held_buttons.remove(button);
                }
                let (kind, native) = match (button, down) {
                    (MouseButton::Left, true) => (CGEventType::LeftMouseDown, CGMouseButton::Left),
                    (MouseButton::Left, false) => (CGEventType::LeftMouseUp, CGMouseButton::Left),
                    (MouseButton::Right, true) => {
                        (CGEventType::RightMouseDown, CGMouseButton::Right)
                    }
                    (MouseButton::Right, false) => {
                        (CGEventType::RightMouseUp, CGMouseButton::Right)
                    }
                    (MouseButton::Middle, true) => {
                        (CGEventType::OtherMouseDown, CGMouseButton::Center)
                    }
                    (MouseButton::Middle, false) => {
                        (CGEventType::OtherMouseUp, CGMouseButton::Center)
                    }
                };
                self.post_mouse(kind, native)?;
            }
            ControlEvent::Wheel { dx, dy } => {
                let event = CGEvent::new_scroll_wheel_event2(None, CGScrollEventUnit::Pixel, 2, -dy.round() as i32, -dx.round() as i32, 0)
                    .ok_or_else(|| anyhow!("could not create scroll event; Accessibility permission may have been revoked"))?;
                CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
            }
            ControlEvent::Key { code, down } => {
                let Some(keycode) = keycode_for_dom_code(code) else {
                    return Ok(());
                };
                if *down {
                    self.held_keys.insert(keycode);
                } else {
                    self.held_keys.remove(&keycode);
                }
                self.post_key(keycode, *down)?;
            }
            ControlEvent::Text { text } => {
                for chunk in unicode_chunks(text) {
                    let down = CGEvent::new_keyboard_event(None, 0, true)
                        .ok_or_else(|| anyhow!("could not create Unicode input event"))?;
                    unsafe {
                        CGEvent::keyboard_set_unicode_string(
                            Some(&down),
                            chunk.len() as u64,
                            chunk.as_ptr(),
                        )
                    };
                    CGEvent::set_flags(Some(&down), CGEventFlags::empty());
                    CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&down));
                    let up = CGEvent::new_keyboard_event(None, 0, false)
                        .ok_or_else(|| anyhow!("could not create Unicode input event"))?;
                    unsafe {
                        CGEvent::keyboard_set_unicode_string(
                            Some(&up),
                            chunk.len() as u64,
                            chunk.as_ptr(),
                        )
                    };
                    CGEvent::set_flags(Some(&up), CGEventFlags::empty());
                    CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&up));
                }
            }
            ControlEvent::ReleaseAll => self.release_all()?,
        }
        Ok(())
    }

    fn release_all(&mut self) -> Result<()> {
        let keys: Vec<_> = self.held_keys.drain().collect();
        let buttons: Vec<_> = self.held_buttons.drain().collect();
        let mut first_error = None;
        for key in keys {
            if let Err(error) = self.post_key(key, false) {
                first_error.get_or_insert(error);
            }
        }
        for button in buttons {
            let (kind, native) = match button {
                MouseButton::Left => (CGEventType::LeftMouseUp, CGMouseButton::Left),
                MouseButton::Right => (CGEventType::RightMouseUp, CGMouseButton::Right),
                MouseButton::Middle => (CGEventType::OtherMouseUp, CGMouseButton::Center),
            };
            if let Err(error) = self.post_mouse(kind, native) {
                first_error.get_or_insert(error);
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

impl Drop for MacInput {
    fn drop(&mut self) {
        if let Err(error) = self.release_all() {
            tracing::warn!(%error, "could not release macOS input state");
        }
    }
}

fn modifier_flags(held: &HashSet<u16>) -> CGEventFlags {
    let mut flags = CGEventFlags::empty();
    for code in held {
        flags |= match code {
            0x36 | 0x37 => CGEventFlags::MaskCommand,
            0x38 | 0x3c => CGEventFlags::MaskShift,
            0x3a | 0x3d => CGEventFlags::MaskAlternate,
            0x3b | 0x3e => CGEventFlags::MaskControl,
            _ => CGEventFlags::empty(),
        };
    }
    flags
}

fn unicode_chunks(text: &str) -> Vec<Vec<u16>> {
    let mut chunks = Vec::new();
    let mut current = Vec::with_capacity(20);
    for character in text.chars() {
        let mut buffer = [0u16; 2];
        let encoded = character.encode_utf16(&mut buffer);
        if current.len() + encoded.len() > 20 {
            chunks.push(current);
            current = Vec::with_capacity(20);
        }
        current.extend_from_slice(encoded);
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

fn keycode_for_dom_code(code: &str) -> Option<u16> {
    Some(match code {
        "KeyA" => 0x00,
        "KeyS" => 0x01,
        "KeyD" => 0x02,
        "KeyF" => 0x03,
        "KeyH" => 0x04,
        "KeyG" => 0x05,
        "KeyZ" => 0x06,
        "KeyX" => 0x07,
        "KeyC" => 0x08,
        "KeyV" => 0x09,
        "KeyB" => 0x0B,
        "KeyQ" => 0x0C,
        "KeyW" => 0x0D,
        "KeyE" => 0x0E,
        "KeyR" => 0x0F,
        "KeyY" => 0x10,
        "KeyT" => 0x11,
        "Digit1" => 0x12,
        "Digit2" => 0x13,
        "Digit3" => 0x14,
        "Digit4" => 0x15,
        "Digit6" => 0x16,
        "Digit5" => 0x17,
        "Equal" => 0x18,
        "Digit9" => 0x19,
        "Digit7" => 0x1A,
        "Minus" => 0x1B,
        "Digit8" => 0x1C,
        "Digit0" => 0x1D,
        "BracketRight" => 0x1E,
        "KeyO" => 0x1F,
        "KeyU" => 0x20,
        "BracketLeft" => 0x21,
        "KeyI" => 0x22,
        "KeyP" => 0x23,
        "Enter" => 0x24,
        "KeyL" => 0x25,
        "KeyJ" => 0x26,
        "Quote" => 0x27,
        "KeyK" => 0x28,
        "Semicolon" => 0x29,
        "Backslash" => 0x2A,
        "Comma" => 0x2B,
        "Slash" => 0x2C,
        "KeyN" => 0x2D,
        "KeyM" => 0x2E,
        "Period" => 0x2F,
        "Tab" => 0x30,
        "Space" => 0x31,
        "Backquote" => 0x32,
        "Backspace" => 0x33,
        "Escape" => 0x35,
        "MetaRight" => 0x36,
        "MetaLeft" => 0x37,
        "ShiftLeft" => 0x38,
        "CapsLock" => 0x39,
        "AltLeft" => 0x3A,
        "ControlLeft" => 0x3B,
        "ShiftRight" => 0x3C,
        "AltRight" => 0x3D,
        "ControlRight" => 0x3E,
        "F17" => 0x40,
        "NumpadDecimal" => 0x41,
        "NumpadMultiply" => 0x43,
        "NumpadAdd" => 0x45,
        "NumLock" => 0x47,
        "NumpadDivide" => 0x4B,
        "NumpadEnter" => 0x4C,
        "NumpadSubtract" => 0x4E,
        "F18" => 0x4F,
        "F19" => 0x50,
        "NumpadEqual" => 0x51,
        "Numpad0" => 0x52,
        "Numpad1" => 0x53,
        "Numpad2" => 0x54,
        "Numpad3" => 0x55,
        "Numpad4" => 0x56,
        "Numpad5" => 0x57,
        "Numpad6" => 0x58,
        "Numpad7" => 0x59,
        "F20" => 0x5A,
        "Numpad8" => 0x5B,
        "Numpad9" => 0x5C,
        "F5" => 0x60,
        "F6" => 0x61,
        "F7" => 0x62,
        "F3" => 0x63,
        "F8" => 0x64,
        "F9" => 0x65,
        "F11" => 0x67,
        "F13" => 0x69,
        "F16" => 0x6A,
        "F14" => 0x6B,
        "F10" => 0x6D,
        "F12" => 0x6F,
        "F15" => 0x71,
        "Home" => 0x73,
        "PageUp" => 0x74,
        "Delete" => 0x75,
        "F4" => 0x76,
        "End" => 0x77,
        "F2" => 0x78,
        "PageDown" => 0x79,
        "F1" => 0x7A,
        "ArrowLeft" => 0x7B,
        "ArrowRight" => 0x7C,
        "ArrowDown" => 0x7D,
        "ArrowUp" => 0x7E,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn modifier_flags_follow_both_sides_and_clear_on_release() {
        let held = HashSet::from([0x37, 0x3c, 0x3e]);
        let flags = modifier_flags(&held);
        assert!(flags.contains(
            CGEventFlags::MaskCommand | CGEventFlags::MaskShift | CGEventFlags::MaskControl
        ));
        assert!(!flags.contains(CGEventFlags::MaskAlternate));
        assert!(modifier_flags(&HashSet::new()).is_empty());
    }
    #[test]
    fn unicode_batches_never_split_surrogate_pairs() {
        let text = format!("{}🦀繁體中文", "a".repeat(19));
        let chunks = unicode_chunks(&text);
        assert!(chunks
            .iter()
            .all(|chunk| chunk.len() <= 20 && String::from_utf16(chunk).is_ok()));
        assert_eq!(String::from_utf16(&chunks.concat()).unwrap(), text);
    }

    fn test_frame(width: u32) -> RawFrame {
        RawFrame {
            bgra: vec![0; width as usize * 4],
            width,
            height: 1,
            stride: width as usize * 4,
            captured_at: Instant::now(),
        }
    }

    #[test]
    fn mailbox_replaces_stale_frame() {
        let mailbox = FrameMailbox::default();
        mailbox.publish(Ok(test_frame(1)));
        mailbox.publish(Ok(test_frame(2)));
        assert_eq!(mailbox.receive(Duration::ZERO).unwrap().unwrap().width, 2);
        assert!(mailbox.receive(Duration::ZERO).unwrap().is_none());
    }

    #[test]
    fn mailbox_reports_stream_stop() {
        let mailbox = FrameMailbox::default();
        mailbox.stop("display locked".into());
        let error = match mailbox.receive(Duration::ZERO) {
            Err(error) => error,
            Ok(_) => panic!("stopped mailbox should return an error"),
        };
        assert!(error.to_string().contains("display locked"));
    }

    #[test]
    fn converts_avcc_and_detects_idr() {
        let input = [0, 0, 0, 2, 0x65, 0x11, 0, 0, 0, 2, 0x41, 0x22];
        let (output, keyframe) = avcc_to_annex_b(&input, 4).unwrap();
        assert!(keyframe);
        assert_eq!(output, [0, 0, 0, 1, 0x65, 0x11, 0, 0, 0, 1, 0x41, 0x22]);
    }

    #[test]
    fn rejects_truncated_avcc() {
        assert!(avcc_to_annex_b(&[0, 0, 0, 3, 0x65], 4).is_err());
    }

    #[test]
    fn maps_browser_modifier_keys() {
        assert_eq!(keycode_for_dom_code("MetaLeft"), Some(0x37));
        assert_eq!(keycode_for_dom_code("ControlRight"), Some(0x3E));
        assert_eq!(keycode_for_dom_code("Unknown"), None);
    }
}
