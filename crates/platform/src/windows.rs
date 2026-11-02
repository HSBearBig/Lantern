//! Windows 11 capture, H.264 encode, and input implementation.
//!
//! Capture uses Windows.Graphics.Capture instead of the legacy desktop duplication API. Frames
//! are copied to a CPU-readable D3D11 staging texture. The Media Foundation encoder accepts NV12,
//! so conversion and down-scaling happen in this adapter. This keeps the WebRTC-facing boundary
//! independent of D3D and makes the software encoder a reliable fallback.

use std::{
    collections::HashSet,
    mem::size_of,
    ptr,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use anyhow::{anyhow, bail, Context, Result};
use lantern_protocol::{ControlEvent, DisplayInfo, MouseButton};
use windows::{
    core::{Interface, HSTRING},
    Graphics::{
        Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession},
        DirectX::{Direct3D11::IDirect3DDevice, DirectXPixelFormat},
    },
    Win32::{
        Foundation::{HMODULE, POINT},
        Graphics::{
            Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL},
            Direct3D11::{
                D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Resource,
                ID3D11Texture2D, D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
                D3D11_USAGE_STAGING,
            },
            Dxgi::{
                Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC},
                IDXGIDevice,
            },
            Gdi::{MonitorFromPoint, MONITOR_DEFAULTTOPRIMARY},
        },
        Media::MediaFoundation::*,
        System::{
            Com::{CoInitializeEx, CoTaskMemFree, CoUninitialize, COINIT_MULTITHREADED},
            Variant::VARIANT,
            WinRT::{
                Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess},
                Graphics::Capture::IGraphicsCaptureItemInterop,
                RoGetActivationFactory, RoInitialize, RoUninitialize, RO_INIT_MULTITHREADED,
            },
        },
        UI::{HiDpi::GetDpiForSystem, Input::KeyboardAndMouse::*},
    },
};

use crate::{
    fit_dimensions, CaptureOptions, CaptureSource, EncodedFrame, InputInjector, NativeParts,
    RawFrame, VideoEncoder,
};

pub(crate) fn open(options: CaptureOptions) -> Result<NativeParts> {
    let runtime = Arc::new(WindowsRuntime::initialize()?);
    let capture = WindowsCapture::open(runtime.clone())?;
    let encoder = MfH264Encoder::new(options, runtime)?;
    Ok(NativeParts {
        capture: Box::new(capture),
        encoder: Box::new(encoder),
        input: Box::new(WindowsInput::default()),
    })
}

/// Owns the per-thread Windows runtimes used by capture and encoding.
///
/// `NativeParts` is created, pumped, and dropped on one dedicated blocking thread. Both capture
/// and encoder hold this guard so it outlives all COM/WinRT/MF objects regardless of field drop
/// order. Partial initialization also unwinds through this guard.
struct WindowsRuntime {
    owner: thread::ThreadId,
    com: bool,
    winrt: bool,
    media_foundation: bool,
}

impl WindowsRuntime {
    fn initialize() -> Result<Self> {
        let mut runtime = Self {
            owner: thread::current().id(),
            com: false,
            winrt: false,
            media_foundation: false,
        };
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED)
                .ok()
                .context("initialize COM")?;
            runtime.com = true;
            RoInitialize(RO_INIT_MULTITHREADED)
                .ok()
                .context("initialize WinRT")?;
            runtime.winrt = true;
            MFStartup(MF_VERSION, MFSTARTUP_FULL).context("start Media Foundation")?;
            runtime.media_foundation = true;
        }
        Ok(runtime)
    }
}

impl Drop for WindowsRuntime {
    fn drop(&mut self) {
        if thread::current().id() != self.owner {
            // Calling the apartment teardown functions from a different thread would corrupt COM
            // bookkeeping. Leaking the thread-scoped registrations is safer and makes violations
            // visible while the owner thread remains responsible for the process lifetime.
            tracing::error!("Windows native runtime dropped on a thread other than its owner");
            return;
        }
        unsafe {
            if self.media_foundation {
                MFShutdown().ok();
            }
            if self.winrt {
                RoUninitialize();
            }
            if self.com {
                CoUninitialize();
            }
        }
    }
}

struct WindowsCapture {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    pool: Direct3D11CaptureFramePool,
    _session: GraphicsCaptureSession,
    display: DisplayInfo,
    staging: Option<(u32, u32, ID3D11Texture2D)>,
    _runtime: Arc<WindowsRuntime>,
}

impl WindowsCapture {
    fn open(runtime: Arc<WindowsRuntime>) -> Result<Self> {
        unsafe {
            let mut device = None;
            let mut context = None;
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None::<*mut D3D_FEATURE_LEVEL>,
                Some(&mut context),
            )
            .context("create D3D11 capture device")?;
            let device = device.context("D3D11 did not return a device")?;
            let context = context.context("D3D11 did not return an immediate context")?;

            let dxgi: IDXGIDevice = device.cast().context("cast D3D11 device to DXGI")?;
            let inspectable = CreateDirect3D11DeviceFromDXGIDevice(&dxgi)
                .context("create WinRT Direct3D device")?;
            let winrt_device: IDirect3DDevice =
                inspectable.cast().context("cast WinRT Direct3D device")?;

            let factory: IGraphicsCaptureItemInterop = RoGetActivationFactory(&HSTRING::from(
                "Windows.Graphics.Capture.GraphicsCaptureItem",
            ))
            .context("activate GraphicsCaptureItem factory")?;
            let monitor = MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
            let item: GraphicsCaptureItem = factory
                .CreateForMonitor(monitor)
                .context("create capture item for the primary monitor")?;
            let size = item.Size().context("read primary monitor size")?;
            if size.Width <= 0 || size.Height <= 0 {
                bail!("primary monitor is unavailable or the interactive desktop is locked");
            }
            let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
                &winrt_device,
                DirectXPixelFormat::B8G8R8A8UIntNormalized,
                2,
                size,
            )
            .context("create two-frame Windows Graphics Capture pool")?;
            let session = pool
                .CreateCaptureSession(&item)
                .context("create capture session")?;
            session
                .SetIsCursorCaptureEnabled(true)
                .context("enable cursor capture")?;
            session
                .StartCapture()
                .context("start primary-monitor capture")?;
            let dpi = GetDpiForSystem();
            Ok(Self {
                device,
                context,
                pool,
                _session: session,
                display: DisplayInfo {
                    id: "primary".into(),
                    width: size.Width as u32,
                    height: size.Height as u32,
                    scale: dpi as f64 / 96.0,
                },
                staging: None,
                _runtime: runtime,
            })
        }
    }

    fn copy_surface(
        &mut self,
        texture: &ID3D11Texture2D,
        width: u32,
        height: u32,
    ) -> Result<Vec<u8>> {
        unsafe {
            if !matches!(self.staging.as_ref(), Some((w, h, _)) if *w == width && *h == height) {
                let desc = D3D11_TEXTURE2D_DESC {
                    Width: width,
                    Height: height,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    Usage: D3D11_USAGE_STAGING,
                    BindFlags: 0,
                    CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                    MiscFlags: 0,
                };
                let mut staging = None;
                self.device
                    .CreateTexture2D(&desc, None, Some(&mut staging))
                    .context("create CPU-readable capture texture")?;
                self.staging = Some((
                    width,
                    height,
                    staging.context("D3D11 did not return the staging texture")?,
                ));
            }
            let staging = &self
                .staging
                .as_ref()
                .context("staging texture was not created")?
                .2;
            let dst: ID3D11Resource = staging.cast()?;
            let src: ID3D11Resource = texture.cast()?;
            self.context.CopyResource(&dst, &src);

            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context
                .Map(&dst, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .context("map captured desktop frame; the desktop may be locked")?;
            struct Unmap<'a>(&'a ID3D11DeviceContext, &'a ID3D11Resource);
            impl Drop for Unmap<'_> {
                fn drop(&mut self) {
                    unsafe { self.0.Unmap(self.1, 0) }
                }
            }
            let _unmap = Unmap(&self.context, &dst);

            let row_bytes = width as usize * 4;
            if (mapped.RowPitch as usize) < row_bytes || mapped.pData.is_null() {
                bail!("Windows capture returned an invalid BGRA surface");
            }
            let mut bgra = vec![0u8; row_bytes * height as usize];
            for row in 0..height as usize {
                ptr::copy_nonoverlapping(
                    (mapped.pData as *const u8).add(row * mapped.RowPitch as usize),
                    bgra.as_mut_ptr().add(row * row_bytes),
                    row_bytes,
                );
            }
            Ok(bgra)
        }
    }
}

impl CaptureSource for WindowsCapture {
    fn display(&self) -> DisplayInfo {
        self.display.clone()
    }

    fn next_frame(&mut self, timeout: Duration) -> Result<Option<RawFrame>> {
        let deadline = Instant::now() + timeout;
        loop {
            match self.pool.TryGetNextFrame() {
                Ok(mut frame) => {
                    // The pool is intentionally tiny. If encoding briefly falls behind, discard
                    // queued older frames and read only the newest desktop state.
                    while let Ok(newer) = self.pool.TryGetNextFrame() {
                        frame.Close().ok();
                        frame = newer;
                    }
                    let size = frame
                        .ContentSize()
                        .context("read captured frame dimensions")?;
                    if size.Width <= 0 || size.Height <= 0 {
                        frame.Close().ok();
                        bail!("capture stopped because the desktop is locked or unavailable");
                    }
                    let resized = size.Width as u32 != self.display.width
                        || size.Height as u32 != self.display.height;
                    if resized {
                        self.display.width = size.Width as u32;
                        self.display.height = size.Height as u32;
                    }
                    let surface = frame.Surface().context("read captured D3D surface")?;
                    let access: IDirect3DDxgiInterfaceAccess = surface.cast()?;
                    let texture: ID3D11Texture2D = unsafe { access.GetInterface() }
                        .context("access captured D3D11 texture")?;
                    let captured_at = Instant::now();
                    let bgra =
                        self.copy_surface(&texture, size.Width as u32, size.Height as u32)?;
                    frame.Close().ok();
                    if resized {
                        // Recreate only after the outstanding frame is closed.
                        let dxgi: IDXGIDevice = self.device.cast()?;
                        let inspectable = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi) }?;
                        let winrt_device: IDirect3DDevice = inspectable.cast()?;
                        self.pool
                            .Recreate(
                                &winrt_device,
                                DirectXPixelFormat::B8G8R8A8UIntNormalized,
                                2,
                                size,
                            )
                            .context("resize capture frame pool")?;
                    }
                    return Ok(Some(RawFrame {
                        bgra,
                        width: size.Width as u32,
                        height: size.Height as u32,
                        stride: size.Width as usize * 4,
                        captured_at,
                    }));
                }
                Err(error) => {
                    // TryGetNextFrame reports E_FAIL when no frame is queued; other failures still
                    // get a short grace period because display-mode changes are transient.
                    if Instant::now() >= deadline {
                        if timeout.is_zero() {
                            return Ok(None);
                        }
                        tracing::debug!(?error, "capture frame unavailable before timeout");
                        return Ok(None);
                    }
                    thread::sleep(Duration::from_millis(2));
                }
            }
        }
    }
}

struct MfH264Encoder {
    options: CaptureOptions,
    transform: Option<IMFTransform>,
    event_generator: Option<IMFMediaEventGenerator>,
    async_mode: bool,
    pending_input: u32,
    pending_output: u32,
    width: u32,
    height: u32,
    frame_index: i64,
    mode: &'static str,
    sequence_header: Vec<u8>,
    tried_software_fallback: bool,
    _runtime: Arc<WindowsRuntime>,
}

// The encoder is created lazily on the thread that first calls `encode`. The public trait only
// exposes it through `&mut self`, so the COM transform never receives concurrent calls. Media
// Foundation MFTs are designed to run on an MTA worker thread and may be moved before first use.
unsafe impl Send for MfH264Encoder {}

impl MfH264Encoder {
    fn new(options: CaptureOptions, runtime: Arc<WindowsRuntime>) -> Result<Self> {
        Ok(Self {
            options,
            transform: None,
            event_generator: None,
            async_mode: false,
            pending_input: 0,
            pending_output: 0,
            width: 0,
            height: 0,
            frame_index: 0,
            mode: "uninitialized",
            sequence_header: Vec::new(),
            tried_software_fallback: options.software,
            _runtime: runtime,
        })
    }

    fn initialize(&mut self, source_width: u32, source_height: u32, software: bool) -> Result<()> {
        let (width, height) = fit_dimensions(
            source_width,
            source_height,
            self.options.max_width,
            self.options.max_height,
        );
        let transform = unsafe { activate_h264_encoder(!software)? };
        let async_mode = unsafe { configure_h264(&transform, width, height, &self.options)? };
        self.sequence_header = unsafe { read_sequence_header(&transform) };
        self.width = width;
        self.height = height;
        self.frame_index = 0;
        self.mode = if software {
            "software-h264"
        } else {
            "hardware-h264"
        };
        self.event_generator = if async_mode {
            Some(
                transform
                    .cast()
                    .context("asynchronous H.264 encoder has no media event generator")?,
            )
        } else {
            None
        };
        self.async_mode = async_mode;
        self.pending_input = 0;
        self.pending_output = 0;
        self.transform = Some(transform);
        Ok(())
    }

    fn poll_async_events(&mut self) -> Result<()> {
        let Some(generator) = self.event_generator.as_ref() else {
            return Ok(());
        };
        loop {
            match unsafe { generator.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(event) => {
                    unsafe { event.GetStatus()? }
                        .ok()
                        .context("asynchronous H.264 encoder event failed")?;
                    match unsafe { event.GetType()? } {
                        value if value == METransformNeedInput.0 as u32 => self.pending_input += 1,
                        value if value == METransformHaveOutput.0 as u32 => {
                            self.pending_output += 1
                        }
                        _ => {}
                    }
                }
                Err(error) if error.code() == MF_E_NO_EVENTS_AVAILABLE => return Ok(()),
                Err(error) => return Err(error).context("read asynchronous H.264 encoder event"),
            }
        }
    }

    fn wait_for_async_input(
        &mut self,
        transform: &IMFTransform,
        duration_100ns: i64,
        frames: &mut Vec<EncodedFrame>,
    ) -> Result<()> {
        let deadline = Instant::now() + Duration::from_millis(100);
        while self.pending_input == 0 {
            self.poll_async_events()?;
            if self.pending_output > 0 {
                frames.extend(unsafe { self.read_available_output(transform, duration_100ns)? });
            }
            if self.pending_input > 0 {
                break;
            }
            if Instant::now() >= deadline {
                bail!("hardware H.264 encoder did not request input in time");
            }
            thread::sleep(Duration::from_millis(1));
        }
        self.pending_input -= 1;
        Ok(())
    }

    unsafe fn read_available_output(
        &mut self,
        transform: &IMFTransform,
        duration_100ns: i64,
    ) -> Result<Vec<EncodedFrame>> {
        let mut frames = Vec::new();
        loop {
            if self.async_mode {
                self.poll_async_events()?;
                if self.pending_output == 0 {
                    break;
                }
                self.pending_output -= 1;
            }
            let info = unsafe { transform.GetOutputStreamInfo(0)? };
            let supplied = info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 == 0;
            let output_sample = if supplied {
                let sample = unsafe { MFCreateSample()? };
                let size = info.cbSize.max((self.width * self.height) / 2).max(1);
                unsafe { sample.AddBuffer(&MFCreateMemoryBuffer(size)?)? };
                Some(sample)
            } else {
                None
            };
            let output = MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: std::mem::ManuallyDrop::new(output_sample.clone()),
                ..Default::default()
            };
            let mut status = 0;
            let mut outputs = [output];
            let outcome = unsafe { transform.ProcessOutput(0, &mut outputs, &mut status) };
            // These fields are ManuallyDrop because MFTs may replace them. Take ownership of
            // whatever the transform wrote on every path, including NEED_MORE_INPUT.
            let produced = unsafe { std::mem::ManuallyDrop::take(&mut outputs[0].pSample) };
            let events = unsafe { std::mem::ManuallyDrop::take(&mut outputs[0].pEvents) };
            drop(events);
            match outcome {
                Ok(()) => {
                    let Some(sample) = produced.or(output_sample) else {
                        break;
                    };
                    let clean = unsafe {
                        sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0
                    };
                    let buffer = unsafe { sample.ConvertToContiguousBuffer()? };
                    let len = unsafe { buffer.GetCurrentLength()? } as usize;
                    let mut ptr = ptr::null_mut();
                    unsafe { buffer.Lock(&mut ptr, None, None)? };
                    let bytes = unsafe { std::slice::from_raw_parts(ptr, len) }.to_vec();
                    unsafe { buffer.Unlock()? };
                    let mut data = normalize_annex_b(&bytes)?;
                    if self.sequence_header.is_empty() {
                        self.sequence_header = unsafe { read_sequence_header(transform) };
                    }
                    if clean && !self.sequence_header.is_empty() && !contains_parameter_sets(&data)
                    {
                        let mut with_header = self.sequence_header.clone();
                        with_header.extend_from_slice(&data);
                        data = with_header;
                    }
                    frames.push(EncodedFrame {
                        data,
                        duration: Duration::from_nanos((duration_100ns * 100) as u64),
                        keyframe: clean,
                    });
                }
                Err(error) if error.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => break,
                Err(error) => return Err(error).context("read H.264 encoder output"),
            }
        }
        Ok(frames)
    }

    fn encode_nv12(&mut self, nv12: &[u8], force_keyframe: bool) -> Result<Vec<EncodedFrame>> {
        let transform = self
            .transform
            .as_ref()
            .context("Media Foundation encoder is not initialized")?
            .clone();
        let duration_100ns = 10_000_000i64 / self.options.fps.max(1) as i64;
        let mut frames = Vec::new();
        if self.async_mode {
            self.poll_async_events()?;
            frames.extend(unsafe { self.read_available_output(&transform, duration_100ns)? });
            self.wait_for_async_input(&transform, duration_100ns, &mut frames)?;
        }
        unsafe {
            if force_keyframe {
                if let Ok(codec) = transform.cast::<ICodecAPI>() {
                    let value = VARIANT::from(true);
                    codec
                        .SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &value)
                        .ok();
                }
            }
            let buffer = MFCreateMemoryBuffer(nv12.len() as u32)?;
            let mut data = ptr::null_mut();
            buffer.Lock(&mut data, None, None)?;
            ptr::copy_nonoverlapping(nv12.as_ptr(), data, nv12.len());
            buffer.Unlock()?;
            buffer.SetCurrentLength(nv12.len() as u32)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(self.frame_index * duration_100ns)?;
            sample.SetSampleDuration(duration_100ns)?;
            transform
                .ProcessInput(0, &sample, 0)
                .context("submit NV12 frame to H.264 encoder")?;
            self.frame_index += 1;

            if self.async_mode {
                let deadline = Instant::now() + Duration::from_millis(8);
                while self.pending_output == 0 && Instant::now() < deadline {
                    self.poll_async_events()?;
                    if self.pending_output == 0 {
                        thread::sleep(Duration::from_millis(1));
                    }
                }
            }

            frames.extend(self.read_available_output(&transform, duration_100ns)?);
            Ok(frames)
        }
    }
}

impl VideoEncoder for MfH264Encoder {
    fn encode(&mut self, frame: RawFrame, force_keyframe: bool) -> Result<Vec<EncodedFrame>> {
        let target = fit_dimensions(
            frame.width,
            frame.height,
            self.options.max_width,
            self.options.max_height,
        );
        if self.transform.is_none() || target != (self.width, self.height) {
            let prefer_software = self.options.software;
            if let Err(hardware_error) = self.initialize(frame.width, frame.height, prefer_software)
            {
                if prefer_software {
                    return Err(hardware_error);
                }
                tracing::warn!(
                    ?hardware_error,
                    "hardware H.264 unavailable; using Media Foundation software encoder"
                );
                self.tried_software_fallback = true;
                self.initialize(frame.width, frame.height, true)?;
            }
        }
        let nv12 = bgra_to_nv12_scaled(&frame, self.width, self.height)?;
        match self.encode_nv12(&nv12, force_keyframe) {
            Ok(frames) => Ok(frames),
            Err(hardware_error)
                if !self.tried_software_fallback && self.mode == "hardware-h264" =>
            {
                tracing::warn!(
                    ?hardware_error,
                    "hardware H.264 failed; retrying with software encoder"
                );
                self.tried_software_fallback = true;
                self.initialize(frame.width, frame.height, true)?;
                self.encode_nv12(&nv12, true)
            }
            Err(error) => Err(error),
        }
    }

    fn mode(&self) -> &'static str {
        self.mode
    }
}

impl Drop for MfH264Encoder {
    fn drop(&mut self) {
        if let Some(transform) = self.transform.take() {
            unsafe {
                transform
                    .ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0)
                    .ok();
                if let Ok(shutdown) = transform.cast::<IMFShutdown>() {
                    shutdown.Shutdown().ok();
                }
            }
        }
    }
}

unsafe fn activate_h264_encoder(hardware: bool) -> Result<IMFTransform> {
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_NV12,
    };
    let output = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    };
    let flags = if hardware {
        MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER
    } else {
        MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER
    };
    let mut raw: *mut Option<IMFActivate> = ptr::null_mut();
    let mut count = 0;
    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            flags,
            Some(&input),
            Some(&output),
            &mut raw,
            &mut count,
        )
    }
    .context("enumerate Media Foundation H.264 encoders")?;
    if raw.is_null() || count == 0 {
        bail!(
            "no {} Media Foundation H.264 encoder is installed",
            if hardware { "hardware" } else { "software" }
        );
    }
    // MFTEnumEx allocates both the array and one owned COM reference per element. Move each
    // Option out before freeing only the array storage so normal Rust drops release every ref.
    let activations: Vec<Option<IMFActivate>> = (0..count as usize)
        .map(|index| unsafe { ptr::read(raw.add(index)) })
        .collect();
    unsafe { CoTaskMemFree(Some(raw.cast())) };
    let mut last_error = None;
    for activation in activations.iter().flatten() {
        match unsafe { activation.ActivateObject::<IMFTransform>() } {
            Ok(transform) => return Ok(transform),
            Err(error) => last_error = Some(error),
        }
    }
    Err(anyhow!(
        last_error.unwrap_or_else(windows::core::Error::from_thread)
    ))
    .context("activate Media Foundation H.264 encoder")
}

unsafe fn configure_h264(
    transform: &IMFTransform,
    width: u32,
    height: u32,
    options: &CaptureOptions,
) -> Result<bool> {
    let attributes = unsafe { transform.GetAttributes().ok() };
    let async_mode = attributes.as_ref().is_some_and(|attributes| unsafe {
        attributes.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) != 0
    });
    if let Some(attributes) = attributes.as_ref().filter(|_| async_mode) {
        unsafe { attributes.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1) }
            .context("unlock asynchronous hardware H.264 encoder")?;
    }
    let output = unsafe { MFCreateMediaType()? };
    unsafe {
        output.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        output.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
        output.SetUINT64(&MF_MT_FRAME_SIZE, pack_ratio(width, height))?;
        output.SetUINT64(&MF_MT_FRAME_RATE, pack_ratio(options.fps.max(1), 1))?;
        output.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack_ratio(1, 1))?;
        output.SetUINT32(&MF_MT_AVG_BITRATE, options.bitrate.max(128_000))?;
        output.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        output.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Base.0 as u32)?;
        transform
            .SetOutputType(0, &output, 0)
            .context("configure H.264 output type")?;

        let input = MFCreateMediaType()?;
        input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
        input.SetUINT64(&MF_MT_FRAME_SIZE, pack_ratio(width, height))?;
        input.SetUINT64(&MF_MT_FRAME_RATE, pack_ratio(options.fps.max(1), 1))?;
        input.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack_ratio(1, 1))?;
        input.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        transform
            .SetInputType(0, &input, 0)
            .context("configure NV12 input type")?;

        if let Ok(codec) = transform.cast::<ICodecAPI>() {
            codec
                .SetValue(&CODECAPI_AVLowLatencyMode, &VARIANT::from(true))
                .ok();
        }
        transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
        transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
    }
    Ok(async_mode)
}

unsafe fn read_sequence_header(transform: &IMFTransform) -> Vec<u8> {
    let Ok(media_type) = (unsafe { transform.GetOutputCurrentType(0) }) else {
        return Vec::new();
    };
    let Ok(size) = (unsafe { media_type.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER) }) else {
        return Vec::new();
    };
    let mut data = vec![0; size as usize];
    if unsafe { media_type.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut data, None) }.is_err() {
        return Vec::new();
    }
    parameter_sets_from_header(&data).unwrap_or_default()
}

const fn pack_ratio(numerator: u32, denominator: u32) -> u64 {
    ((numerator as u64) << 32) | denominator as u64
}

fn bgra_to_nv12_scaled(frame: &RawFrame, out_width: u32, out_height: u32) -> Result<Vec<u8>> {
    if frame.width == 0
        || frame.height == 0
        || out_width & 1 != 0
        || out_height & 1 != 0
        || frame.stride < frame.width as usize * 4
        || frame.bgra.len() < frame.stride * frame.height as usize
    {
        bail!("invalid BGRA frame dimensions or stride");
    }
    let y_len = out_width as usize * out_height as usize;
    let mut nv12 = vec![0u8; y_len + y_len / 2];
    for oy in 0..out_height {
        let sy = (oy as u64 * frame.height as u64 / out_height as u64) as usize;
        for ox in 0..out_width {
            let sx = (ox as u64 * frame.width as u64 / out_width as u64) as usize;
            let p = sy * frame.stride + sx * 4;
            let (b, g, r) = (
                frame.bgra[p] as i32,
                frame.bgra[p + 1] as i32,
                frame.bgra[p + 2] as i32,
            );
            nv12[(oy * out_width + ox) as usize] =
                (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16).clamp(0, 255) as u8;
        }
    }
    for oy in (0..out_height).step_by(2) {
        for ox in (0..out_width).step_by(2) {
            let mut r = 0i32;
            let mut g = 0i32;
            let mut b = 0i32;
            for dy in 0..2 {
                for dx in 0..2 {
                    let sy = ((oy + dy) as u64 * frame.height as u64 / out_height as u64) as usize;
                    let sx = ((ox + dx) as u64 * frame.width as u64 / out_width as u64) as usize;
                    let p = sy * frame.stride + sx * 4;
                    b += frame.bgra[p] as i32;
                    g += frame.bgra[p + 1] as i32;
                    r += frame.bgra[p + 2] as i32;
                }
            }
            r /= 4;
            g /= 4;
            b /= 4;
            let uv = y_len + (oy / 2 * out_width + ox) as usize;
            nv12[uv] = (((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128).clamp(0, 255) as u8;
            nv12[uv + 1] = (((112 * r - 94 * g - 18 * b + 128) >> 8) + 128).clamp(0, 255) as u8;
        }
    }
    Ok(nv12)
}

fn normalize_annex_b(data: &[u8]) -> Result<Vec<u8>> {
    if data.starts_with(&[0, 0, 1]) || data.starts_with(&[0, 0, 0, 1]) {
        return Ok(data.to_vec());
    }
    let mut out = Vec::with_capacity(data.len() + 32);
    let mut offset = 0usize;
    while offset < data.len() {
        if data.len() - offset < 4 {
            bail!("truncated AVCC NAL length");
        }
        let len = u32::from_be_bytes(data[offset..offset + 4].try_into().unwrap()) as usize;
        offset += 4;
        if len == 0 || data.len() - offset < len {
            bail!("invalid AVCC NAL length");
        }
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&data[offset..offset + len]);
        offset += len;
    }
    Ok(out)
}

fn parameter_sets_from_header(data: &[u8]) -> Result<Vec<u8>> {
    if data.starts_with(&[0, 0, 1]) || data.starts_with(&[0, 0, 0, 1]) {
        return Ok(data.to_vec());
    }
    if data.len() < 7 || data[0] != 1 {
        return normalize_annex_b(data);
    }
    let mut offset = 5;
    let sps_count = data[offset] & 0x1f;
    offset += 1;
    let mut out = Vec::new();
    for _ in 0..sps_count {
        copy_avcc_parameter_set(data, &mut offset, &mut out)?;
    }
    if offset >= data.len() {
        bail!("truncated AVC decoder configuration record");
    }
    let pps_count = data[offset];
    offset += 1;
    for _ in 0..pps_count {
        copy_avcc_parameter_set(data, &mut offset, &mut out)?;
    }
    Ok(out)
}

fn copy_avcc_parameter_set(data: &[u8], offset: &mut usize, out: &mut Vec<u8>) -> Result<()> {
    if data.len() - *offset < 2 {
        bail!("truncated AVC parameter-set length");
    }
    let len = u16::from_be_bytes(data[*offset..*offset + 2].try_into().unwrap()) as usize;
    *offset += 2;
    if len == 0 || data.len() - *offset < len {
        bail!("invalid AVC parameter-set length");
    }
    out.extend_from_slice(&[0, 0, 0, 1]);
    out.extend_from_slice(&data[*offset..*offset + len]);
    *offset += len;
    Ok(())
}

fn contains_parameter_sets(data: &[u8]) -> bool {
    data.windows(5)
        .any(|w| w[..4] == [0, 0, 0, 1] && matches!(w[4] & 0x1f, 7 | 8))
        || data
            .windows(4)
            .any(|w| w[..3] == [0, 0, 1] && matches!(w[3] & 0x1f, 7 | 8))
}

#[derive(Default)]
struct WindowsInput {
    held_keys: HashSet<u16>,
    held_buttons: HashSet<MouseButton>,
}

impl WindowsInput {
    fn send(input: INPUT) -> Result<()> {
        let count = unsafe { SendInput(&[input], size_of::<INPUT>() as i32) };
        if count != 1 {
            bail!("Windows rejected injected input (UIPI, a secure desktop, or a locked session may be active)");
        }
        Ok(())
    }

    fn key(vk: u16, down: bool) -> Result<()> {
        let mut flags = if down {
            KEYBD_EVENT_FLAGS(0)
        } else {
            KEYEVENTF_KEYUP
        };
        if is_extended_key(vk) {
            flags |= KEYEVENTF_EXTENDEDKEY;
        }
        Self::send(INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(vk),
                    dwFlags: flags,
                    ..Default::default()
                },
            },
        })
    }

    fn unicode(unit: u16, down: bool) -> Result<()> {
        Self::send(INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wScan: unit,
                    dwFlags: KEYEVENTF_UNICODE
                        | if down {
                            KEYBD_EVENT_FLAGS(0)
                        } else {
                            KEYEVENTF_KEYUP
                        },
                    ..Default::default()
                },
            },
        })
    }

    fn mouse(flags: MOUSE_EVENT_FLAGS, data: i32, x: i32, y: i32) -> Result<()> {
        Self::send(INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: x,
                    dy: y,
                    mouseData: data as u32,
                    dwFlags: flags,
                    ..Default::default()
                },
            },
        })
    }
}

impl InputInjector for WindowsInput {
    fn inject(&mut self, event: &ControlEvent) -> Result<()> {
        if !event.validate() {
            bail!("rejected invalid remote input event");
        }
        match event {
            ControlEvent::Pointer { x, y, .. } => {
                Self::mouse(
                    MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE,
                    0,
                    (x * 65_535.0).round() as i32,
                    (y * 65_535.0).round() as i32,
                )?;
            }
            ControlEvent::Button { button, down, .. } => {
                let flags = match (button, down) {
                    (MouseButton::Left, true) => MOUSEEVENTF_LEFTDOWN,
                    (MouseButton::Left, false) => MOUSEEVENTF_LEFTUP,
                    (MouseButton::Right, true) => MOUSEEVENTF_RIGHTDOWN,
                    (MouseButton::Right, false) => MOUSEEVENTF_RIGHTUP,
                    (MouseButton::Middle, true) => MOUSEEVENTF_MIDDLEDOWN,
                    (MouseButton::Middle, false) => MOUSEEVENTF_MIDDLEUP,
                };
                Self::mouse(flags, 0, 0, 0)?;
                if *down {
                    self.held_buttons.insert(*button);
                } else {
                    self.held_buttons.remove(button);
                }
            }
            ControlEvent::Wheel { dx, dy } => {
                if *dy != 0.0 {
                    Self::mouse(
                        MOUSEEVENTF_WHEEL,
                        (-dy * 120.0 / 100.0).round() as i32,
                        0,
                        0,
                    )?;
                }
                if *dx != 0.0 {
                    Self::mouse(
                        MOUSEEVENTF_HWHEEL,
                        (dx * 120.0 / 100.0).round() as i32,
                        0,
                        0,
                    )?;
                }
            }
            ControlEvent::Key { code, down } => {
                let Some(vk) = virtual_key(code) else {
                    return Ok(());
                };
                Self::key(vk, *down)?;
                if *down {
                    self.held_keys.insert(vk);
                } else {
                    self.held_keys.remove(&vk);
                }
            }
            ControlEvent::Text { text } => {
                for unit in text.encode_utf16() {
                    Self::unicode(unit, true)?;
                    Self::unicode(unit, false)?;
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
            if let Err(error) = Self::key(key, false) {
                first_error.get_or_insert(error);
            }
        }
        for button in buttons {
            let flags = match button {
                MouseButton::Left => MOUSEEVENTF_LEFTUP,
                MouseButton::Right => MOUSEEVENTF_RIGHTUP,
                MouseButton::Middle => MOUSEEVENTF_MIDDLEUP,
            };
            if let Err(error) = Self::mouse(flags, 0, 0, 0) {
                first_error.get_or_insert(error);
            }
        }
        if let Some(error) = first_error {
            Err(error)
        } else {
            Ok(())
        }
    }
}

impl Drop for WindowsInput {
    fn drop(&mut self) {
        self.release_all().ok();
    }
}

fn virtual_key(code: &str) -> Option<u16> {
    if let Some(suffix) = code.strip_prefix("Key").filter(|suffix| suffix.len() == 1) {
        return suffix
            .bytes()
            .next()
            .map(|ch| ch.to_ascii_uppercase() as u16);
    }
    if let Some(suffix) = code
        .strip_prefix("Digit")
        .filter(|suffix| suffix.len() == 1)
    {
        return suffix.bytes().next().map(|ch| ch as u16);
    }
    if let Some(index) = code
        .strip_prefix('F')
        .and_then(|n| n.parse::<u16>().ok())
        .filter(|n| (1..=24).contains(n))
    {
        return Some(VK_F1.0 + index - 1);
    }
    Some(match code {
        "Backspace" => VK_BACK.0,
        "Tab" => VK_TAB.0,
        "Enter" | "NumpadEnter" => VK_RETURN.0,
        "ShiftLeft" => VK_LSHIFT.0,
        "ShiftRight" => VK_RSHIFT.0,
        "ControlLeft" => VK_LCONTROL.0,
        "ControlRight" => VK_RCONTROL.0,
        "AltLeft" => VK_LMENU.0,
        "AltRight" => VK_RMENU.0,
        "MetaLeft" => VK_LWIN.0,
        "MetaRight" => VK_RWIN.0,
        "Pause" => VK_PAUSE.0,
        "CapsLock" => VK_CAPITAL.0,
        "Escape" => VK_ESCAPE.0,
        "Space" => VK_SPACE.0,
        "PageUp" => VK_PRIOR.0,
        "PageDown" => VK_NEXT.0,
        "End" => VK_END.0,
        "Home" => VK_HOME.0,
        "ArrowLeft" => VK_LEFT.0,
        "ArrowUp" => VK_UP.0,
        "ArrowRight" => VK_RIGHT.0,
        "ArrowDown" => VK_DOWN.0,
        "Insert" => VK_INSERT.0,
        "Delete" => VK_DELETE.0,
        "Semicolon" => VK_OEM_1.0,
        "Equal" => VK_OEM_PLUS.0,
        "Comma" => VK_OEM_COMMA.0,
        "Minus" => VK_OEM_MINUS.0,
        "Period" => VK_OEM_PERIOD.0,
        "Slash" => VK_OEM_2.0,
        "Backquote" => VK_OEM_3.0,
        "BracketLeft" => VK_OEM_4.0,
        "Backslash" => VK_OEM_5.0,
        "BracketRight" => VK_OEM_6.0,
        "Quote" => VK_OEM_7.0,
        _ => return None,
    })
}

fn is_extended_key(vk: u16) -> bool {
    matches!(
        vk,
        v if v == VK_RCONTROL.0
            || v == VK_RMENU.0
            || v == VK_LWIN.0
            || v == VK_RWIN.0
            || v == VK_INSERT.0
            || v == VK_DELETE.0
            || v == VK_HOME.0
            || v == VK_END.0
            || v == VK_PRIOR.0
            || v == VK_NEXT.0
            || v == VK_LEFT.0
            || v == VK_RIGHT.0
            || v == VK_UP.0
            || v == VK_DOWN.0
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn converts_black_and_white_to_video_range_nv12() {
        let frame = RawFrame {
            bgra: vec![
                0, 0, 0, 255, 255, 255, 255, 255, 0, 0, 0, 255, 255, 255, 255, 255,
            ],
            width: 2,
            height: 2,
            stride: 8,
            captured_at: Instant::now(),
        };
        let nv12 = bgra_to_nv12_scaled(&frame, 2, 2).unwrap();
        assert_eq!(&nv12[..4], &[16, 235, 16, 235]);
        assert_eq!(nv12.len(), 6);
    }
    #[test]
    fn converts_avcc_nals_to_annex_b() {
        let data = [0, 0, 0, 2, 0x67, 1, 0, 0, 0, 2, 0x68, 2];
        assert_eq!(
            normalize_annex_b(&data).unwrap(),
            [0, 0, 0, 1, 0x67, 1, 0, 0, 0, 1, 0x68, 2]
        );
        assert!(normalize_annex_b(&[0, 0, 0, 9, 0x67]).is_err());
    }
    #[test]
    fn extracts_sps_and_pps_from_avc_configuration() {
        let avcc = [
            1, 0x42, 0, 0x1f, 0xff, 0xe1, 0, 2, 0x67, 0x42, 1, 0, 2, 0x68, 0xce,
        ];
        let annex_b = parameter_sets_from_header(&avcc).unwrap();
        assert_eq!(annex_b, [0, 0, 0, 1, 0x67, 0x42, 0, 0, 0, 1, 0x68, 0xce]);
        assert!(contains_parameter_sets(&annex_b));
    }
    #[test]
    fn maps_browser_key_codes() {
        assert_eq!(virtual_key("KeyA"), Some(b'A' as u16));
        assert_eq!(virtual_key("Digit7"), Some(b'7' as u16));
        assert_eq!(virtual_key("ArrowLeft"), Some(VK_LEFT.0));
        assert_eq!(virtual_key("Unknown"), None);
    }
}
