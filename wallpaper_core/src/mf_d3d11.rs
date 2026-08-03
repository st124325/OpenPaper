//! Native MP4 decode foundation: Media Foundation Source Reader + D3D11.
//!
//! This module deliberately owns the exact GPU objects required by the final
//! presenter.  MP4 decoding can therefore be validated independently before
//! it replaces the stable libVLC output path.

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering},
        mpsc, Arc, Mutex, OnceLock,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use windows::{
    core::{implement, IUnknown, Interface, HRESULT, HSTRING, PCWSTR},
    Win32::{
        Foundation::{
            CloseHandle, BOOL, HANDLE, HMODULE, HWND, RECT, WAIT_FAILED, WAIT_OBJECT_0,
            WAIT_TIMEOUT,
        },
        Graphics::{
            Direct3D::D3D_DRIVER_TYPE_HARDWARE,
            Direct3D11::{
                D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
                ID3D11VideoContext, ID3D11VideoDevice, ID3D11VideoProcessor,
                ID3D11VideoProcessorEnumerator, ID3D11VideoProcessorInputView,
                ID3D11VideoProcessorOutputView, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION, D3D11_TEX2D_VPIV,
                D3D11_TEX2D_VPOV, D3D11_VIDEO_COLOR, D3D11_VIDEO_COLOR_0, D3D11_VIDEO_COLOR_RGBA,
                D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_CONTENT_DESC,
                D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0,
                D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0,
                D3D11_VIDEO_PROCESSOR_STREAM, D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
                D3D11_VPIV_DIMENSION_TEXTURE2D, D3D11_VPOV_DIMENSION_TEXTURE2D,
            },
            Dxgi::{
                Common::{
                    DXGI_ALPHA_MODE_IGNORE, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_RATIONAL,
                    DXGI_SAMPLE_DESC,
                },
                IDXGIAdapter, IDXGIDevice, IDXGIFactory2, IDXGIOutput, IDXGISwapChain1,
                IDXGISwapChain2, IDXGISwapChain3, DXGI_ERROR_WAS_STILL_DRAWING,
                DXGI_PRESENT_DO_NOT_WAIT, DXGI_SCALING_STRETCH, DXGI_SWAP_CHAIN_DESC1,
                DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT,
                DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL, DXGI_USAGE_RENDER_TARGET_OUTPUT,
            },
        },
        Media::MediaFoundation::{
            IMFDXGIBuffer, IMFDXGIDeviceManager, IMFMediaEvent, IMFSample, IMFSourceReader,
            IMFSourceReaderCallback, IMFSourceReaderCallback_Impl, MFCreateAttributes,
            MFCreateDXGIDeviceManager, MFCreateMediaType, MFCreateSourceReaderFromURL,
            MFMediaType_Video, MFStartup, MFVideoFormat_NV12, MFSTARTUP_FULL, MF_MT_FRAME_SIZE,
            MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS,
            MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED, MF_SOURCE_READERF_ENDOFSTREAM,
            MF_SOURCE_READER_ASYNC_CALLBACK, MF_SOURCE_READER_D3D_MANAGER,
            MF_SOURCE_READER_DISABLE_DXVA, MF_SOURCE_READER_FIRST_VIDEO_STREAM, MF_VERSION,
        },
        System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED},
        System::Threading::WaitForSingleObject,
    },
};

static MEDIA_FOUNDATION_STARTED: OnceLock<bool> = OnceLock::new();
static HARDWARE_PIPELINE_AVAILABLE: OnceLock<bool> = OnceLock::new();

struct NativeMp4Pipeline {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    _manager: IMFDXGIDeviceManager,
    reader: Option<IMFSourceReader>,
    swap_chain: IDXGISwapChain1,
    swap_chain3: IDXGISwapChain3,
    output_width: u32,
    output_height: u32,
    buffer_count: u32,
    stretch_to_fill: bool,
    frame_latency: FrameLatencyHandle,
    processor: Option<VideoProcessorResources>,
}

struct FrameLatencyHandle(HANDLE);

impl Drop for FrameLatencyHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

impl FrameLatencyHandle {
    unsafe fn new(swap_chain: &IDXGISwapChain1) -> Result<Self, String> {
        let swap_chain2: IDXGISwapChain2 = swap_chain
            .cast()
            .map_err(|error| format!("Could not query IDXGISwapChain2: {error}"))?;
        swap_chain2
            .SetMaximumFrameLatency(1)
            .map_err(|error| format!("Could not set swap-chain frame latency: {error}"))?;
        let handle = swap_chain2.GetFrameLatencyWaitableObject();
        if handle.0.is_null() {
            return Err("DXGI returned no frame-latency waitable object.".into());
        }
        Ok(Self(handle))
    }

    unsafe fn wait(&self) -> Result<bool, String> {
        match WaitForSingleObject(self.0, 100) {
            status if status == WAIT_OBJECT_0 => Ok(true),
            status if status == WAIT_TIMEOUT => Ok(false),
            status if status == WAIT_FAILED => {
                Err("Waiting for the DXGI frame-latency object failed.".into())
            }
            status => Err(format!(
                "DXGI returned an unexpected wait status: 0x{:08X}.",
                status.0
            )),
        }
    }
}

/// Objects whose creation is expensive. They are retained for every frame
/// with the same source and output dimensions.
struct VideoProcessorResources {
    input_width: u32,
    input_height: u32,
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    _enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    output_views: Vec<ID3D11VideoProcessorOutputView>,
    input_views: HashMap<(usize, u32), CachedInputView>,
    input_view_epoch: u64,
}

struct CachedInputView {
    // Keeping the texture alive makes its COM identity a safe cache key and
    // prevents another decoder surface from reusing the same pointer value.
    _texture: ID3D11Texture2D,
    view: ID3D11VideoProcessorInputView,
    last_used: u64,
}

const MAX_CACHED_INPUT_VIEWS: usize = 16;

/// Owns the native render thread. Dropping it requests a clean stop and joins
/// the thread, so a stopped wallpaper never leaves a D3D swap chain behind.
pub struct NativeMp4Renderer {
    stop: Arc<AtomicBool>,
    failed: Arc<AtomicBool>,
    frames_presented: Arc<AtomicU64>,
    callbacks_received: Arc<AtomicU64>,
    last_callback_status: Arc<AtomicI32>,
    last_callback_flags: Arc<AtomicU32>,
    last_request_status: Arc<AtomicI32>,
    finished: Arc<AtomicBool>,
    started_at: Instant,
    worker: JoinHandle<()>,
}

enum DecodeEvent {
    Sample(IMFSample, i64),
    FormatChanged,
    EndOfStream,
    Error,
}

/// Source Reader invokes this COM callback on Media Foundation worker threads.
/// It never renders itself: it only moves GPU-backed samples into the bounded
/// render queue and requests the next sample. The bounded queue provides
/// back-pressure instead of letting decoded DXVA surfaces grow unbounded.
#[implement(IMFSourceReaderCallback)]
struct AsyncReaderCallback {
    state: Arc<AsyncCallbackState>,
}

struct AsyncCallbackState {
    events: mpsc::SyncSender<DecodeEvent>,
    reader: Mutex<Option<IMFSourceReader>>,
    stop: Arc<AtomicBool>,
    callbacks_received: Arc<AtomicU64>,
    last_callback_status: Arc<AtomicI32>,
    last_callback_flags: Arc<AtomicU32>,
    last_request_status: Arc<AtomicI32>,
}

// Media Foundation documents that Source Reader callbacks may arrive on any
// thread. The only COM reader access in this state is serialized by `reader`.
unsafe impl Send for AsyncCallbackState {}
unsafe impl Sync for AsyncCallbackState {}

impl AsyncCallbackState {
    fn attach_reader(&self, reader: &IMFSourceReader) {
        if let Ok(mut slot) = self.reader.lock() {
            *slot = Some(reader.clone());
        }
    }

    fn detach_reader(&self) {
        if let Ok(mut slot) = self.reader.lock() {
            *slot = None;
        }
    }

    fn request_next(&self) -> windows::core::Result<()> {
        if self.stop.load(Ordering::Acquire) {
            return Ok(());
        }
        let reader = self.reader.lock().ok().and_then(|slot| slot.clone());
        let Some(reader) = reader else {
            return Ok(());
        };
        // In asynchronous Source Reader mode every out argument must be NULL.
        let result = unsafe {
            reader.ReadSample(
                MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32,
                0,
                None,
                None,
                None,
                None,
            )
        };
        self.last_request_status.store(
            result.as_ref().err().map_or(0, |error| error.code().0),
            Ordering::Release,
        );
        result
    }
}

impl IMFSourceReaderCallback_Impl for AsyncReaderCallback_Impl {
    fn OnReadSample(
        &self,
        hrstatus: HRESULT,
        _stream_index: u32,
        flags: u32,
        timestamp_100ns: i64,
        sample: Option<&IMFSample>,
    ) -> windows::core::Result<()> {
        self.state
            .callbacks_received
            .fetch_add(1, Ordering::Release);
        self.state
            .last_callback_status
            .store(hrstatus.0, Ordering::Release);
        self.state
            .last_callback_flags
            .store(flags, Ordering::Release);
        if self.state.stop.load(Ordering::Acquire) {
            return Ok(());
        }
        if hrstatus.is_err() {
            let _ = self.state.events.try_send(DecodeEvent::Error);
            return Ok(());
        }
        if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
            let _ = self.state.events.try_send(DecodeEvent::EndOfStream);
            return Ok(());
        }
        if let Some(sample) = sample {
            if self
                .state
                .events
                .try_send(DecodeEvent::Sample(sample.clone(), timestamp_100ns))
                .is_err()
            {
                // A single read is outstanding at a time, therefore a full
                // queue means the renderer is no longer making progress.
                let _ = self.state.events.try_send(DecodeEvent::Error);
                return Ok(());
            }
            // The render thread requests the next sample after it has
            // consumed this one. Never block a Media Foundation callback.
            return Ok(());
        }
        // Keep Source Reader calls on its callback MTA. It is not safe to
        // query the media type on the render thread and then issue ReadSample
        // from a different thread while a callback is in flight.
        if flags & MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED.0 as u32 != 0 {
            let reader = self.state.reader.lock().ok().and_then(|slot| slot.clone());
            let Some(reader) = reader else {
                let _ = self.state.events.try_send(DecodeEvent::Error);
                return Ok(());
            };
            if unsafe { inspect_renegotiated_video_format(&reader) }.is_err() {
                let _ = self.state.events.try_send(DecodeEvent::Error);
                return Ok(());
            }
            if unsafe { configure_dxva_nv12_output(&reader) }.is_err() {
                let _ = self.state.events.try_send(DecodeEvent::Error);
                return Ok(());
            }
            let _ = self.state.events.try_send(DecodeEvent::FormatChanged);
            self.state.request_next()?;
        } else {
            let _ = self.state.events.try_send(DecodeEvent::Error);
        }
        Ok(())
    }

    fn OnFlush(&self, _stream_index: u32) -> windows::core::Result<()> {
        Ok(())
    }

    fn OnEvent(
        &self,
        _stream_index: u32,
        _event: Option<&IMFMediaEvent>,
    ) -> windows::core::Result<()> {
        Ok(())
    }
}

impl NativeMp4Renderer {
    pub fn start(path: String, host: HWND) -> Result<Self, String> {
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let failed = Arc::new(AtomicBool::new(false));
        let worker_failed = Arc::clone(&failed);
        let frames_presented = Arc::new(AtomicU64::new(0));
        let worker_frames_presented = Arc::clone(&frames_presented);
        let callbacks_received = Arc::new(AtomicU64::new(0));
        let worker_callbacks_received = Arc::clone(&callbacks_received);
        let last_callback_status = Arc::new(AtomicI32::new(0));
        let worker_last_callback_status = Arc::clone(&last_callback_status);
        let last_callback_flags = Arc::new(AtomicU32::new(0));
        let worker_last_callback_flags = Arc::clone(&last_callback_flags);
        let last_request_status = Arc::new(AtomicI32::new(0));
        let worker_last_request_status = Arc::clone(&last_request_status);
        let finished = Arc::new(AtomicBool::new(false));
        let worker_finished = Arc::clone(&finished);
        let host_value = host.0 as isize;
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let worker = thread::spawn(move || unsafe {
            let host = HWND(host_value as _);
            let initialized = CoInitializeEx(None, COINIT_MULTITHREADED).is_ok();
            let _ = run_async_renderer(
                &path,
                host,
                &worker_stop,
                &worker_frames_presented,
                &worker_callbacks_received,
                &worker_last_callback_status,
                &worker_last_callback_flags,
                &worker_last_request_status,
                ready_tx,
            );
            if !worker_stop.load(Ordering::Acquire) {
                worker_failed.store(true, Ordering::Release);
            }
            worker_finished.store(true, Ordering::Release);
            if initialized {
                CoUninitialize();
            }
        });
        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(Self {
                stop,
                failed,
                frames_presented,
                callbacks_received,
                last_callback_status,
                last_callback_flags,
                last_request_status,
                finished,
                started_at: Instant::now(),
                worker,
            }),
            Ok(Err(error)) => {
                let _ = worker.join();
                Err(error)
            }
            Err(_) => {
                stop.store(true, Ordering::Release);
                let _ = worker.join();
                Err("Native renderer initialization timed out.".into())
            }
        }
    }

    pub fn stop(self) {
        self.stop.store(true, Ordering::Release);
        // A buggy codec/driver can stop delivering callbacks entirely. Never
        // let that external failure freeze the UI or prevent libVLC fallback.
        // Completed workers are still joined; a stalled one is detached and
        // owns no Rust references back into the engine state.
        if self.finished.load(Ordering::Acquire) {
            let _ = self.worker.join();
        }
    }

    pub fn has_failed(&self) -> bool {
        self.failed.load(Ordering::Acquire)
            || (self.frames_presented.load(Ordering::Acquire) == 0
                && self.started_at.elapsed() >= Duration::from_secs(8))
    }

    pub fn frames_presented(&self) -> u64 {
        self.frames_presented.load(Ordering::Acquire)
    }

    pub fn callbacks_received(&self) -> u64 {
        self.callbacks_received.load(Ordering::Acquire)
    }

    pub fn last_callback_status(&self) -> i32 {
        self.last_callback_status.load(Ordering::Acquire)
    }

    pub fn last_callback_flags(&self) -> u32 {
        self.last_callback_flags.load(Ordering::Acquire)
    }

    pub fn last_request_status(&self) -> i32 {
        self.last_request_status.load(Ordering::Acquire)
    }
}

/// Runs decode asynchronously. The startup channel is completed after the
/// first ReadSample request has been accepted, never after a decoded frame,
/// so applying a wallpaper cannot be held hostage by a decoder.
#[allow(clippy::too_many_arguments)]
unsafe fn run_async_renderer(
    path: &str,
    host: HWND,
    stop: &Arc<AtomicBool>,
    frames_presented: &AtomicU64,
    callbacks_received: &Arc<AtomicU64>,
    last_callback_status: &Arc<AtomicI32>,
    last_callback_flags: &Arc<AtomicU32>,
    last_request_status: &Arc<AtomicI32>,
    ready_tx: mpsc::SyncSender<Result<(), String>>,
) -> Result<(), String> {
    let mut ready_tx = Some(ready_tx);
    loop {
        let (events_tx, events_rx) = mpsc::sync_channel(2);
        let callback_state = Arc::new(AsyncCallbackState {
            events: events_tx,
            reader: Mutex::new(None),
            stop: Arc::clone(stop),
            callbacks_received: Arc::clone(callbacks_received),
            last_callback_status: Arc::clone(last_callback_status),
            last_callback_flags: Arc::clone(last_callback_flags),
            last_request_status: Arc::clone(last_request_status),
        });
        // Share the original stop flag with the callback through a cheap
        // polling bridge; callback invocations must not depend on the UI.
        let callback: IMFSourceReaderCallback = AsyncReaderCallback {
            state: Arc::clone(&callback_state),
        }
        .into();
        let mut pipeline = match create_pipeline(path, host, Some(&callback)) {
            Ok(pipeline) => pipeline,
            Err(error) => {
                if let Some(sender) = ready_tx.take() {
                    let _ = sender.send(Err(error.clone()));
                }
                return Err(error);
            }
        };
        callback_state.attach_reader(
            pipeline
                .reader
                .as_ref()
                .expect("source-reader pipeline always owns its reader"),
        );
        if let Err(error) = callback_state.request_next() {
            callback_state.detach_reader();
            if let Some(sender) = ready_tx.take() {
                let _ = sender.send(Err(format!(
                    "Could not request the first native frame: {error}"
                )));
            }
            return Err(format!("Could not request the first native frame: {error}"));
        }
        if let Some(sender) = ready_tx.take() {
            let _ = sender.send(Ok(()));
        }

        let origin = Instant::now();
        let mut reached_end = false;
        loop {
            if stop.load(Ordering::Acquire) {
                if let Some(reader) = pipeline.reader.as_ref() {
                    let _ = reader.Flush(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32);
                }
                break;
            }
            match events_rx.recv_timeout(Duration::from_millis(100)) {
                Ok(DecodeEvent::Sample(sample, timestamp_100ns)) => {
                    let due =
                        Duration::from_nanos((timestamp_100ns.max(0) as u64).saturating_mul(100));
                    while due > origin.elapsed() && !stop.load(Ordering::Acquire) {
                        thread::sleep((due - origin.elapsed()).min(Duration::from_millis(5)));
                    }
                    if stop.load(Ordering::Acquire) {
                        continue;
                    }
                    present_nv12_sample(&mut pipeline, &sample)?;
                    frames_presented.fetch_add(1, Ordering::Release);
                    callback_state.request_next().map_err(|error| {
                        format!("Could not request the next native frame: {error}")
                    })?;
                }
                Ok(DecodeEvent::FormatChanged) => {
                    // The callback already validated NV12 and requested the
                    // next sample. Drop size-dependent D3D11 state here.
                    pipeline.processor = None;
                }
                Ok(DecodeEvent::EndOfStream) => {
                    reached_end = true;
                    break;
                }
                Ok(DecodeEvent::Error) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                    callback_state.detach_reader();
                    return Err("The asynchronous native decoder failed.".into());
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
        }
        callback_state.detach_reader();
        if stop.load(Ordering::Acquire) {
            return Ok(());
        }
        if !reached_end {
            return Err("The asynchronous native decoder stopped unexpectedly.".into());
        }
        // EOF is normal: drop the reader and its decoder surfaces, then
        // construct a fresh async reader for an infinite wallpaper loop.
    }
}

/// Validates a format reported by MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED.
unsafe fn inspect_renegotiated_video_format(reader: &IMFSourceReader) -> Result<(), String> {
    let media_type = reader
        .GetCurrentMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32)
        .map_err(|error| format!("Could not read the renegotiated media type: {error}"))?;
    let major = media_type
        .GetGUID(&MF_MT_MAJOR_TYPE)
        .map_err(|error| format!("Renegotiated media type has no major type: {error}"))?;
    if major != MFMediaType_Video {
        return Err("Renegotiated Source Reader stream is not video.".into());
    }
    let subtype = media_type
        .GetGUID(&MF_MT_SUBTYPE)
        .map_err(|error| format!("Renegotiated video type has no subtype: {error}"))?;
    if subtype != MFVideoFormat_NV12 {
        return Err(format!(
            "Native decoder renegotiated unsupported GPU subtype {subtype:?}; expected NV12."
        ));
    }
    let frame_size = media_type
        .GetUINT64(&MF_MT_FRAME_SIZE)
        .map_err(|error| format!("Renegotiated video type has no frame size: {error}"))?;
    let width = (frame_size >> 32) as u32;
    let height = frame_size as u32;
    if width == 0 || height == 0 {
        return Err("Renegotiated video type has an invalid zero frame size.".into());
    }
    Ok(())
}

/// Re-applies the GPU-friendly output type after a decoder transform changes
/// its format. This is valid after OnReadSample because no sample request is
/// pending at that point.
unsafe fn configure_dxva_nv12_output(reader: &IMFSourceReader) -> Result<(), String> {
    let media_type = MFCreateMediaType()
        .map_err(|error| format!("Could not create a renegotiated NV12 media type: {error}"))?;
    media_type
        .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
        .map_err(|error| format!("Could not set renegotiated video major type: {error}"))?;
    media_type
        .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)
        .map_err(|error| format!("Could not set renegotiated NV12 subtype: {error}"))?;
    reader
        .SetCurrentMediaType(
            MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32,
            None,
            &media_type,
        )
        .map_err(|error| format!("Could not reconfigure the DXVA NV12 output: {error}"))
}

unsafe fn create_video_processor_resources(
    pipeline: &NativeMp4Pipeline,
    input_width: u32,
    input_height: u32,
) -> Result<VideoProcessorResources, String> {
    let video_device: ID3D11VideoDevice = pipeline
        .device
        .cast()
        .map_err(|error| format!("Could not query the D3D11 video device: {error}"))?;
    let video_context: ID3D11VideoContext = pipeline
        .context
        .cast()
        .map_err(|error| format!("Could not query the D3D11 video context: {error}"))?;
    let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
        InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
        InputFrameRate: DXGI_RATIONAL {
            Numerator: 60,
            Denominator: 1,
        },
        InputWidth: input_width,
        InputHeight: input_height,
        OutputFrameRate: DXGI_RATIONAL {
            Numerator: 60,
            Denominator: 1,
        },
        OutputWidth: pipeline.output_width,
        OutputHeight: pipeline.output_height,
        Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
    };
    let enumerator = video_device
        .CreateVideoProcessorEnumerator(&content)
        .map_err(|error| format!("Could not create the D3D11 video processor: {error}"))?;
    let processor = video_device
        .CreateVideoProcessor(&enumerator, 0)
        .map_err(|error| format!("Could not create the D3D11 video processor instance: {error}"))?;
    let output_description = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
        ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
        Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
            Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
        },
    };
    let mut output_views = Vec::with_capacity(pipeline.buffer_count as usize);
    for index in 0..pipeline.buffer_count {
        let texture: ID3D11Texture2D = pipeline
            .swap_chain
            .GetBuffer(index)
            .map_err(|error| format!("Could not get BGRA swap-chain buffer {index}: {error}"))?;
        let mut output_view = None;
        video_device
            .CreateVideoProcessorOutputView(
                &texture,
                &enumerator,
                &output_description,
                Some(&mut output_view),
            )
            .map_err(|error| format!("Could not create BGRA output view {index}: {error}"))?;
        output_views.push(
            output_view.ok_or_else(|| format!("D3D11 returned no BGRA output view {index}."))?,
        );
    }
    let source = RECT {
        left: 0,
        top: 0,
        right: input_width as i32,
        bottom: input_height as i32,
    };
    let destination = destination_rect(
        input_width,
        input_height,
        pipeline.output_width,
        pipeline.output_height,
        pipeline.stretch_to_fill,
    );
    let output_target = RECT {
        left: 0,
        top: 0,
        right: pipeline.output_width as i32,
        bottom: pipeline.output_height as i32,
    };
    video_context.VideoProcessorSetStreamFrameFormat(
        &processor,
        0,
        D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
    );
    video_context.VideoProcessorSetStreamSourceRect(&processor, 0, BOOL(1), Some(&source));
    video_context.VideoProcessorSetStreamDestRect(&processor, 0, BOOL(1), Some(&destination));
    video_context.VideoProcessorSetOutputTargetRect(&processor, BOOL(1), Some(&output_target));
    let black = D3D11_VIDEO_COLOR {
        Anonymous: D3D11_VIDEO_COLOR_0 {
            RGBA: D3D11_VIDEO_COLOR_RGBA {
                R: 0.0,
                G: 0.0,
                B: 0.0,
                A: 1.0,
            },
        },
    };
    video_context.VideoProcessorSetOutputBackgroundColor(&processor, BOOL(0), &black);
    Ok(VideoProcessorResources {
        input_width,
        input_height,
        video_device,
        video_context,
        _enumerator: enumerator,
        processor,
        output_views,
        input_views: HashMap::with_capacity(MAX_CACHED_INPUT_VIEWS),
        input_view_epoch: 0,
    })
}

fn destination_rect(
    input_width: u32,
    input_height: u32,
    output_width: u32,
    output_height: u32,
    stretch_to_fill: bool,
) -> RECT {
    if stretch_to_fill || input_width == 0 || input_height == 0 {
        return RECT {
            left: 0,
            top: 0,
            right: output_width as i32,
            bottom: output_height as i32,
        };
    }

    let (width, height) = if u64::from(output_width) * u64::from(input_height)
        <= u64::from(output_height) * u64::from(input_width)
    {
        let height =
            (u64::from(output_width) * u64::from(input_height) / u64::from(input_width)) as u32;
        (output_width, height.max(1))
    } else {
        let width =
            (u64::from(output_height) * u64::from(input_width) / u64::from(input_height)) as u32;
        (width.max(1), output_height)
    };
    let left = (output_width.saturating_sub(width) / 2) as i32;
    let top = (output_height.saturating_sub(height) / 2) as i32;
    RECT {
        left,
        top,
        right: left + width as i32,
        bottom: top + height as i32,
    }
}

fn take_cached_input_view(
    resources: &mut VideoProcessorResources,
    key: (usize, u32),
) -> Option<ID3D11VideoProcessorInputView> {
    resources.input_view_epoch = resources.input_view_epoch.wrapping_add(1);
    let epoch = resources.input_view_epoch;
    let cached = resources.input_views.get_mut(&key)?;
    cached.last_used = epoch;
    Some(cached.view.clone())
}

unsafe fn create_cached_input_view(
    resources: &mut VideoProcessorResources,
    input_texture: &ID3D11Texture2D,
    input_subresource: u32,
) -> Result<ID3D11VideoProcessorInputView, String> {
    let input_description = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
        FourCC: 0,
        ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
        Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
            Texture2D: D3D11_TEX2D_VPIV {
                MipSlice: 0,
                ArraySlice: input_subresource,
            },
        },
    };
    let mut input_view = None;
    resources
        .video_device
        .CreateVideoProcessorInputView(
            input_texture,
            &resources._enumerator,
            &input_description,
            Some(&mut input_view),
        )
        .map_err(|error| format!("Could not create the NV12 input view: {error}"))?;
    let input_view = input_view.ok_or_else(|| "D3D11 returned no NV12 input view.".to_string())?;

    if resources.input_views.len() >= MAX_CACHED_INPUT_VIEWS {
        if let Some(stale_key) = resources
            .input_views
            .iter()
            .min_by_key(|(_, cached)| cached.last_used)
            .map(|(key, _)| *key)
        {
            resources.input_views.remove(&stale_key);
        }
    }
    resources.input_view_epoch = resources.input_view_epoch.wrapping_add(1);
    let key = (input_texture.as_raw() as usize, input_subresource);
    resources.input_views.insert(
        key,
        CachedInputView {
            _texture: input_texture.clone(),
            view: input_view.clone(),
            last_used: resources.input_view_epoch,
        },
    );
    Ok(input_view)
}

/// Transfers one decoder-owned NV12 texture into the current BGRA swap-chain
/// back buffer. The processor/output views are cached across frames, so no
/// pixels cross the CPU boundary and no expensive pipeline objects are rebuilt.
unsafe fn present_nv12_sample(
    pipeline: &mut NativeMp4Pipeline,
    sample: &windows::Win32::Media::MediaFoundation::IMFSample,
) -> Result<(), String> {
    // DXGI signals only when a back buffer is available. Waiting here avoids
    // waking the render loop just to discover that DWM is still presenting
    // the previous frame. A timeout drops this frame rather than stalling the
    // decoder or preventing shutdown.
    if !pipeline.frame_latency.wait()? {
        return Ok(());
    }
    let buffer = sample
        .GetBufferByIndex(0)
        .map_err(|error| format!("Could not get the decoded video buffer: {error}"))?;
    let dxgi_buffer: IMFDXGIBuffer = buffer.cast().map_err(|_| {
        "The decoder returned a system-memory frame instead of an NV12 GPU texture.".to_string()
    })?;

    let mut raw_texture = std::ptr::null_mut();
    dxgi_buffer
        .GetResource(&ID3D11Texture2D::IID, &mut raw_texture)
        .map_err(|error| format!("Could not acquire the decoded NV12 texture: {error}"))?;
    let input_texture = ID3D11Texture2D::from_raw(raw_texture);
    let input_subresource = dxgi_buffer
        .GetSubresourceIndex()
        .map_err(|error| format!("Could not get the NV12 texture subresource: {error}"))?;

    let input_key = (input_texture.as_raw() as usize, input_subresource);
    let cached_input_view = pipeline
        .processor
        .as_mut()
        .and_then(|resources| take_cached_input_view(resources, input_key));
    let input_view = if let Some(input_view) = cached_input_view {
        input_view
    } else {
        // Texture descriptions are invariant for a decoder surface. Query
        // only the first time a surface appears; format-change events clear
        // the whole processor/cache before the next frame arrives.
        let mut input_size = Default::default();
        input_texture.GetDesc(&mut input_size);
        let needs_rebuild = pipeline.processor.as_ref().is_none_or(|resources| {
            resources.input_width != input_size.Width || resources.input_height != input_size.Height
        });
        if needs_rebuild {
            pipeline.processor = Some(create_video_processor_resources(
                pipeline,
                input_size.Width,
                input_size.Height,
            )?);
        }
        create_cached_input_view(
            pipeline
                .processor
                .as_mut()
                .expect("processor resources were just created"),
            &input_texture,
            input_subresource,
        )?
    };

    let resources = pipeline
        .processor
        .as_ref()
        .expect("processor resources were just created");
    let current_buffer = pipeline.swap_chain3.GetCurrentBackBufferIndex() as usize;
    let output_view = resources.output_views.get(current_buffer).ok_or_else(|| {
        format!("Swap chain returned invalid back-buffer index {current_buffer}.")
    })?;

    let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
        Enable: BOOL(1),
        pInputSurface: std::mem::ManuallyDrop::new(Some(input_view)),
        ..Default::default()
    };
    resources
        .video_context
        .VideoProcessorBlt(
            &resources.processor,
            output_view,
            0,
            std::slice::from_ref(&stream),
        )
        .map_err(|error| format!("GPU NV12-to-BGRA conversion failed: {error}"))?;
    // windows-rs models the C union as ManuallyDrop; balance its COM reference.
    std::mem::ManuallyDrop::drop(&mut stream.pInputSurface);
    // The wallpaper must never stall its decoder waiting for desktop
    // composition. If DWM has not released a back buffer yet, drop this frame
    // and decode the next one; no pixels are copied to the CPU.
    let present_status = pipeline.swap_chain.Present(0, DXGI_PRESENT_DO_NOT_WAIT);
    if present_status.is_err() && present_status != DXGI_ERROR_WAS_STILL_DRAWING {
        return Err(format!(
            "Could not present the GPU-converted frame: {present_status}"
        ));
    }
    Ok(())
}

fn ensure_media_foundation_started() -> bool {
    *MEDIA_FOUNDATION_STARTED
        .get_or_init(|| unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL).is_ok() })
}

unsafe fn create_device_and_manager(
) -> Result<(ID3D11Device, ID3D11DeviceContext, IMFDXGIDeviceManager), String> {
    if !ensure_media_foundation_started() {
        return Err("Media Foundation could not start.".into());
    }

    let mut device = None;
    let mut context = None;
    D3D11CreateDevice(
        None::<&IDXGIAdapter>,
        D3D_DRIVER_TYPE_HARDWARE,
        HMODULE::default(),
        D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
        None,
        D3D11_SDK_VERSION,
        Some(&mut device),
        None,
        Some(&mut context),
    )
    .map_err(|error| format!("D3D11 video device creation failed: {error}"))?;
    let device = device.ok_or_else(|| "D3D11 returned no device.".to_string())?;
    let context = context.ok_or_else(|| "D3D11 returned no context.".to_string())?;

    let mut reset_token = 0u32;
    let mut manager = None;
    MFCreateDXGIDeviceManager(&mut reset_token, &mut manager)
        .map_err(|error| format!("DXGI device manager creation failed: {error}"))?;
    let manager =
        manager.ok_or_else(|| "Media Foundation returned no DXGI manager.".to_string())?;
    manager
        .ResetDevice(&device, reset_token)
        .map_err(|error| format!("DXGI device manager reset failed: {error}"))?;

    Ok((device, context, manager))
}

/// Returns whether this Windows session can create the GPU decode foundation.
pub fn hardware_pipeline_available() -> bool {
    *HARDWARE_PIPELINE_AVAILABLE.get_or_init(|| unsafe { create_device_and_manager().is_ok() })
}

/// Creates one decoder/swap-chain set on the render thread.
unsafe fn create_pipeline(
    path: &str,
    host: HWND,
    async_callback: Option<&IMFSourceReaderCallback>,
) -> Result<NativeMp4Pipeline, String> {
    unsafe {
        let (device, context, manager) = create_device_and_manager()?;
        let attributes = {
            let mut attributes = None;
            MFCreateAttributes(&mut attributes, 4)
                .map_err(|error| format!("Media Foundation attributes failed: {error}"))?;
            attributes.ok_or_else(|| "Media Foundation returned no attributes.".to_string())?
        };
        attributes
            .SetUnknown(&MF_SOURCE_READER_D3D_MANAGER, &manager)
            .map_err(|error| format!("Could not attach the DXGI manager: {error}"))?;
        attributes
            .SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)
            .map_err(|error| format!("Could not enable hardware transforms: {error}"))?;
        attributes
            .SetUINT32(&MF_SOURCE_READER_DISABLE_DXVA, 0)
            .map_err(|error| format!("Could not enable DXVA: {error}"))?;
        if let Some(callback) = async_callback {
            let callback_unknown: IUnknown = callback
                .cast()
                .map_err(|error| format!("Could not expose async callback as IUnknown: {error}"))?;
            attributes
                .SetUnknown(&MF_SOURCE_READER_ASYNC_CALLBACK, &callback_unknown)
                .map_err(|error| {
                    format!("Could not configure the async Source Reader callback: {error}")
                })?;
        }

        let source_path = HSTRING::from(path);
        let reader = MFCreateSourceReaderFromURL(PCWSTR(source_path.as_ptr()), &attributes)
            .map_err(|error| format!("Could not open MP4 with Media Foundation: {error}"))?;
        let video_stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
        reader
            .SetStreamSelection(video_stream, true)
            .map_err(|error| format!("Could not select the video stream: {error}"))?;

        let media_type = MFCreateMediaType()
            .map_err(|error| format!("Could not create an NV12 media type: {error}"))?;
        media_type
            .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .map_err(|error| format!("Could not set video media type: {error}"))?;
        media_type
            .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)
            .map_err(|error| format!("Could not request NV12 output: {error}"))?;
        reader
            .SetCurrentMediaType(video_stream, None, &media_type)
            .map_err(|error| format!("The native decoder does not support NV12: {error}"))?;

        let dxgi_device: IDXGIDevice = device
            .cast()
            .map_err(|error| format!("Could not query IDXGIDevice: {error}"))?;
        let adapter = dxgi_device
            .GetAdapter()
            .map_err(|error| format!("Could not get the DXGI adapter: {error}"))?;
        let factory: IDXGIFactory2 = adapter
            .GetParent()
            .map_err(|error| format!("Could not get the DXGI factory: {error}"))?;
        let swap_chain_description = DXGI_SWAP_CHAIN_DESC1 {
            Width: 0,
            Height: 0,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            Stereo: false.into(),
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            Scaling: DXGI_SCALING_STRETCH,
            // FLIP_SEQUENTIAL preserves the mapping between an index and its
            // back buffer. That makes cached Video Processor output views
            // valid across Present calls; FLIP_DISCARD does not provide this
            // guarantee and failed on the second buffer in real testing.
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
            AlphaMode: DXGI_ALPHA_MODE_IGNORE,
            Flags: DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT.0 as u32,
        };
        let swap_chain = factory
            .CreateSwapChainForHwnd(
                &device,
                host,
                &swap_chain_description,
                None,
                None::<&IDXGIOutput>,
            )
            .map_err(|error| format!("Could not create the D3D11 swap chain: {error}"))?;
        let swap_chain3: IDXGISwapChain3 = swap_chain
            .cast()
            .map_err(|error| format!("Could not query IDXGISwapChain3: {error}"))?;
        let swap_chain_info = swap_chain
            .GetDesc1()
            .map_err(|error| format!("Could not read the D3D11 swap-chain description: {error}"))?;
        let frame_latency = FrameLatencyHandle::new(&swap_chain)?;

        Ok(NativeMp4Pipeline {
            device,
            context,
            _manager: manager,
            reader: Some(reader),
            swap_chain,
            swap_chain3,
            output_width: swap_chain_info.Width,
            output_height: swap_chain_info.Height,
            buffer_count: swap_chain_info.BufferCount,
            stretch_to_fill: true,
            frame_latency,
            processor: None,
        })
    }
}

/// Presenter shared by the direct Media Source/MFT backend. It accepts an
/// NV12 DXGI sample produced by any decoder bound to the same D3D11 device.
/// No CPU copy is introduced between decode and the desktop swap chain.
pub(crate) struct ExternalNv12Presenter {
    pipeline: NativeMp4Pipeline,
}

impl ExternalNv12Presenter {
    pub(crate) unsafe fn new(
        device: ID3D11Device,
        context: ID3D11DeviceContext,
        manager: IMFDXGIDeviceManager,
        host: HWND,
        stretch_to_fill: bool,
    ) -> Result<Self, String> {
        let dxgi_device: IDXGIDevice = device
            .cast()
            .map_err(|error| format!("Could not query IDXGIDevice: {error}"))?;
        let adapter = dxgi_device
            .GetAdapter()
            .map_err(|error| format!("Could not get the DXGI adapter: {error}"))?;
        let factory: IDXGIFactory2 = adapter
            .GetParent()
            .map_err(|error| format!("Could not get the DXGI factory: {error}"))?;
        let description = DXGI_SWAP_CHAIN_DESC1 {
            Width: 0,
            Height: 0,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            Stereo: false.into(),
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            Scaling: DXGI_SCALING_STRETCH,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
            AlphaMode: DXGI_ALPHA_MODE_IGNORE,
            Flags: DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT.0 as u32,
        };
        let swap_chain = factory
            .CreateSwapChainForHwnd(&device, host, &description, None, None::<&IDXGIOutput>)
            .map_err(|error| format!("Could not create direct D3D11 swap chain: {error}"))?;
        let swap_chain3: IDXGISwapChain3 = swap_chain
            .cast()
            .map_err(|error| format!("Could not query IDXGISwapChain3: {error}"))?;
        let swap_chain_info = swap_chain
            .GetDesc1()
            .map_err(|error| format!("Could not read the D3D11 swap-chain description: {error}"))?;
        let frame_latency = FrameLatencyHandle::new(&swap_chain)?;
        Ok(Self {
            pipeline: NativeMp4Pipeline {
                device,
                context,
                _manager: manager,
                reader: None,
                swap_chain,
                swap_chain3,
                output_width: swap_chain_info.Width,
                output_height: swap_chain_info.Height,
                buffer_count: swap_chain_info.BufferCount,
                stretch_to_fill,
                frame_latency,
                processor: None,
            },
        })
    }

    pub(crate) unsafe fn present(
        &mut self,
        sample: &windows::Win32::Media::MediaFoundation::IMFSample,
    ) -> Result<(), String> {
        present_nv12_sample(&mut self.pipeline, sample)
    }
}

#[cfg(test)]
mod tests {
    use super::destination_rect;

    #[test]
    fn stretch_uses_the_complete_output() {
        let rect = destination_rect(1_920, 1_080, 1_280, 1_024, true);
        assert_eq!(
            (rect.left, rect.top, rect.right, rect.bottom),
            (0, 0, 1_280, 1_024)
        );
    }

    #[test]
    fn preserve_aspect_centers_letterbox_bars() {
        let rect = destination_rect(1_920, 1_080, 1_280, 1_024, false);
        assert_eq!(
            (rect.left, rect.top, rect.right, rect.bottom),
            (0, 152, 1_280, 872)
        );
    }

    #[test]
    fn preserve_aspect_centers_pillarbox_bars() {
        let rect = destination_rect(1_080, 1_920, 1_920, 1_080, false);
        assert_eq!(
            (rect.left, rect.top, rect.right, rect.bottom),
            (656, 0, 1_263, 1_080)
        );
    }
}
