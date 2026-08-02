//! Native MP4 decode foundation: Media Foundation Source Reader + D3D11.
//!
//! This module deliberately owns the exact GPU objects required by the final
//! presenter.  MP4 decoding can therefore be validated independently before
//! it replaces the stable libVLC output path.

use std::{
    sync::{atomic::{AtomicBool, Ordering}, mpsc, Arc, OnceLock},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use windows::{
    core::{HSTRING, Interface, PCWSTR},
    Win32::{
        Foundation::{BOOL, HMODULE, HWND, RECT},
        Graphics::{
            Direct3D::D3D_DRIVER_TYPE_HARDWARE,
            Direct3D11::{
                D3D11CreateDevice, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION, ID3D11Device,
                D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                D3D11_VIDEO_PROCESSOR_CONTENT_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC,
                D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC,
                D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_STREAM,
                D3D11_VIDEO_USAGE_PLAYBACK_NORMAL, D3D11_VPIV_DIMENSION_TEXTURE2D,
                D3D11_VPOV_DIMENSION_TEXTURE2D, ID3D11DeviceContext, ID3D11Texture2D,
                ID3D11VideoContext, ID3D11VideoDevice,
            },
            Dxgi::{
                Common::{DXGI_ALPHA_MODE_IGNORE, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_RATIONAL, DXGI_SAMPLE_DESC},
                DXGI_SCALING_STRETCH, DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_EFFECT_FLIP_DISCARD,
                DXGI_PRESENT, DXGI_USAGE_RENDER_TARGET_OUTPUT, IDXGIAdapter, IDXGIDevice, IDXGIFactory2,
                IDXGIOutput, IDXGISwapChain1,
            },
        },
        Media::MediaFoundation::{
            MFCreateAttributes, MFCreateDXGIDeviceManager, MFCreateMediaType,
            MFCreateSourceReaderFromURL, MFStartup, IMFDXGIBuffer, IMFSourceReader, IMFDXGIDeviceManager,
            MFMediaType_Video, MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, MF_SOURCE_READER_D3D_MANAGER,
            MF_SOURCE_READER_DISABLE_DXVA, MF_SOURCE_READER_FIRST_VIDEO_STREAM, MF_MT_MAJOR_TYPE,
            MF_MT_SUBTYPE, MF_SOURCE_READERF_ENDOFSTREAM, MF_VERSION, MFVideoFormat_NV12, MFSTARTUP_FULL,
        },
        System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED},
    },
};

static MEDIA_FOUNDATION_STARTED: OnceLock<bool> = OnceLock::new();
static HARDWARE_PIPELINE_AVAILABLE: OnceLock<bool> = OnceLock::new();

struct NativeMp4Pipeline {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    _manager: IMFDXGIDeviceManager,
    reader: IMFSourceReader,
    swap_chain: IDXGISwapChain1,
}

/// Owns the native render thread. Dropping it requests a clean stop and joins
/// the thread, so a stopped wallpaper never leaves a D3D swap chain behind.
pub struct NativeMp4Renderer {
    stop: Arc<AtomicBool>,
    worker: JoinHandle<()>,
}

impl NativeMp4Renderer {
    pub fn start(path: String, host: HWND) -> Result<Self, String> {
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let host_value = host.0 as isize;
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let worker = thread::spawn(move || unsafe {
            let host = HWND(host_value as _);
            let initialized = CoInitializeEx(None, COINIT_MULTITHREADED).is_ok();
            let mut pipeline = match create_pipeline(&path, host) {
                Ok(pipeline) => pipeline,
                Err(error) => {
                    let _ = ready_tx.send(Err(error));
                    if initialized { CoUninitialize(); }
                    return;
                }
            };
            let _ = ready_tx.send(Ok(()));
            loop {
                if render_loop(&mut pipeline, &worker_stop).is_err() { break; }
                if worker_stop.load(Ordering::Acquire) { break; }
                // End-of-file is a normal wallpaper event. Recreating the
                // Source Reader also flushes all hardware decoder surfaces.
                pipeline = match create_pipeline(&path, host) {
                    Ok(pipeline) => pipeline,
                    Err(_) => break,
                };
            }
            if initialized { CoUninitialize(); }
        });
        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(Self { stop, worker }),
            Ok(Err(error)) => { let _ = worker.join(); Err(error) }
            Err(_) => { stop.store(true, Ordering::Release); let _ = worker.join(); Err("Native renderer initialization timed out.".into()) }
        }
    }

    pub fn stop(self) {
        self.stop.store(true, Ordering::Release);
        let _ = self.worker.join();
    }
}

/// Transfers one decoder-owned NV12 texture into the BGRA swap-chain back
/// buffer. The video processor keeps both resources on the GPU: no pixels are
/// read back into RAM or converted by the CPU.
unsafe fn present_nv12_sample(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    swap_chain: &IDXGISwapChain1,
    sample: &windows::Win32::Media::MediaFoundation::IMFSample,
) -> Result<(), String> {
    let buffer = sample.GetBufferByIndex(0)
        .map_err(|error| format!("Could not get the decoded video buffer: {error}"))?;
    let dxgi_buffer: IMFDXGIBuffer = buffer.cast()
        .map_err(|_| "The decoder returned a system-memory frame instead of an NV12 GPU texture.".to_string())?;

    let mut raw_texture = std::ptr::null_mut();
    dxgi_buffer.GetResource(&ID3D11Texture2D::IID, &mut raw_texture)
        .map_err(|error| format!("Could not acquire the decoded NV12 texture: {error}"))?;
    let input_texture = ID3D11Texture2D::from_raw(raw_texture);
    let input_subresource = dxgi_buffer.GetSubresourceIndex()
        .map_err(|error| format!("Could not get the NV12 texture subresource: {error}"))?;

    let mut input_size = Default::default();
    input_texture.GetDesc(&mut input_size);
    let output_texture: ID3D11Texture2D = swap_chain.GetBuffer(0)
        .map_err(|error| format!("Could not get the BGRA swap-chain back buffer: {error}"))?;
    let mut output_size = Default::default();
    output_texture.GetDesc(&mut output_size);
    let video_device: ID3D11VideoDevice = device.cast()
        .map_err(|error| format!("Could not query the D3D11 video device: {error}"))?;
    let video_context: ID3D11VideoContext = context.cast()
        .map_err(|error| format!("Could not query the D3D11 video context: {error}"))?;

    let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
        InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
        InputFrameRate: DXGI_RATIONAL { Numerator: 60, Denominator: 1 },
        InputWidth: input_size.Width,
        InputHeight: input_size.Height,
        OutputFrameRate: DXGI_RATIONAL { Numerator: 60, Denominator: 1 },
        OutputWidth: output_size.Width,
        OutputHeight: output_size.Height,
        Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
    };
    let enumerator = video_device.CreateVideoProcessorEnumerator(&content)
        .map_err(|error| format!("Could not create the D3D11 video processor: {error}"))?;
    let processor = video_device.CreateVideoProcessor(&enumerator, 0)
        .map_err(|error| format!("Could not create the D3D11 video processor instance: {error}"))?;

    let input_description = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
        FourCC: 0,
        ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
        Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
            Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: input_subresource },
        },
    };
    let mut input_view = None;
    video_device.CreateVideoProcessorInputView(&input_texture, &enumerator, &input_description, Some(&mut input_view))
        .map_err(|error| format!("Could not create the NV12 input view: {error}"))?;
    let input_view = input_view.ok_or_else(|| "D3D11 returned no NV12 input view.".to_string())?;

    let output_description = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
        ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
        Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
            Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
        },
    };
    let mut output_view = None;
    video_device.CreateVideoProcessorOutputView(&output_texture, &enumerator, &output_description, Some(&mut output_view))
        .map_err(|error| format!("Could not create the BGRA output view: {error}"))?;
    let output_view = output_view.ok_or_else(|| "D3D11 returned no BGRA output view.".to_string())?;

    let source = RECT { left: 0, top: 0, right: input_size.Width as i32, bottom: input_size.Height as i32 };
    let destination = RECT { left: 0, top: 0, right: output_size.Width as i32, bottom: output_size.Height as i32 };
    video_context.VideoProcessorSetStreamFrameFormat(&processor, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
    video_context.VideoProcessorSetStreamSourceRect(&processor, 0, BOOL(1), Some(&source));
    video_context.VideoProcessorSetStreamDestRect(&processor, 0, BOOL(1), Some(&destination));
    video_context.VideoProcessorSetOutputTargetRect(&processor, BOOL(1), Some(&destination));

    let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
        Enable: BOOL(1),
        pInputSurface: std::mem::ManuallyDrop::new(Some(input_view)),
        ..Default::default()
    };
    video_context.VideoProcessorBlt(&processor, &output_view, 0, std::slice::from_ref(&stream))
        .map_err(|error| format!("GPU NV12-to-BGRA conversion failed: {error}"))?;
    // windows-rs models the C union as ManuallyDrop; balance its COM reference.
    std::mem::ManuallyDrop::drop(&mut stream.pInputSurface);
    swap_chain.Present(0, DXGI_PRESENT(0)).ok()
        .map_err(|error| format!("Could not present the GPU-converted frame: {error}"))?;
    Ok(())
}

fn ensure_media_foundation_started() -> bool {
    *MEDIA_FOUNDATION_STARTED.get_or_init(|| unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL).is_ok() })
}

unsafe fn create_device_and_manager() -> Result<(ID3D11Device, ID3D11DeviceContext, IMFDXGIDeviceManager), String> {
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
    let manager = manager.ok_or_else(|| "Media Foundation returned no DXGI manager.".to_string())?;
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
unsafe fn create_pipeline(path: &str, host: HWND) -> Result<NativeMp4Pipeline, String> {
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

        let source_path = HSTRING::from(path);
        let reader = MFCreateSourceReaderFromURL(PCWSTR(source_path.as_ptr()), &attributes)
            .map_err(|error| format!("Could not open MP4 with Media Foundation: {error}"))?;
        let video_stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
        reader
            .SetStreamSelection(video_stream, true)
            .map_err(|error| format!("Could not select the video stream: {error}"))?;

        let media_type = MFCreateMediaType().map_err(|error| format!("Could not create an NV12 media type: {error}"))?;
        media_type
            .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .map_err(|error| format!("Could not set video media type: {error}"))?;
        media_type
            .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)
            .map_err(|error| format!("Could not request NV12 output: {error}"))?;
        reader
            .SetCurrentMediaType(video_stream, None, &media_type)
            .map_err(|error| format!("The native decoder does not support NV12: {error}"))?;

        let dxgi_device: IDXGIDevice = device.cast().map_err(|error| format!("Could not query IDXGIDevice: {error}"))?;
        let adapter = dxgi_device.GetAdapter().map_err(|error| format!("Could not get the DXGI adapter: {error}"))?;
        let factory: IDXGIFactory2 = adapter.GetParent().map_err(|error| format!("Could not get the DXGI factory: {error}"))?;
        let swap_chain_description = DXGI_SWAP_CHAIN_DESC1 {
            Width: 0,
            Height: 0,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            Stereo: false.into(),
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            Scaling: DXGI_SCALING_STRETCH,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
            AlphaMode: DXGI_ALPHA_MODE_IGNORE,
            Flags: 0,
        };
        let swap_chain = factory
            .CreateSwapChainForHwnd(&device, host, &swap_chain_description, None, None::<&IDXGIOutput>)
            .map_err(|error| format!("Could not create the D3D11 swap chain: {error}"))?;

        Ok(NativeMp4Pipeline {
            device,
            context,
            _manager: manager,
            reader,
            swap_chain,
        })
    }
}

unsafe fn render_loop(pipeline: &mut NativeMp4Pipeline, stop: &AtomicBool) -> Result<(), ()> {
    let video_stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
    let origin = Instant::now();

    while !stop.load(Ordering::Acquire) {
        let mut flags = 0u32;
        let mut timestamp_100ns = 0i64;
        let mut sample = None;
        let result = pipeline.reader.ReadSample(
            video_stream, 0, None, Some(&mut flags), Some(&mut timestamp_100ns), Some(&mut sample),
        );
        if result.is_err() {
            return Err(());
        }
        if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
            // Reopen at EOF. This avoids PROPVARIANT seeking and handles MP4
            // decoders that retain queued DXVA surfaces after a seek.
            break;
        }
        let Some(sample) = sample else { continue };
        let due_100ns = timestamp_100ns.max(0) as u64;
        let due = Duration::from_nanos(due_100ns.saturating_mul(100));
        while due > origin.elapsed() && !stop.load(Ordering::Acquire) {
            thread::sleep((due - origin.elapsed()).min(Duration::from_millis(10)));
        }
        if stop.load(Ordering::Acquire) { break; }
        if present_nv12_sample(&pipeline.device, &pipeline.context, &pipeline.swap_chain, &sample).is_err() {
            return Err(());
        }
    }
    Ok(())
}

/// Decodes and presents a native frame for a diagnostics/preflight check.
pub fn probe_mp4(path: &str, host: HWND) -> Result<(), String> {
    unsafe {
        let pipeline = create_pipeline(path, host)?;
        let video_stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
        let mut flags = 0u32;
        let mut sample = None;
        // In synchronous mode both stream flags and sample are mandatory;
        // passing NULL for flags makes Media Foundation return E_POINTER.
        pipeline.reader.ReadSample(video_stream, 0, None, Some(&mut flags), None, Some(&mut sample))
            .map_err(|error| format!("Could not decode the first native frame: {error}"))?;
        if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
            return Err("The native decoder reached end-of-stream before its first frame.".into());
        }
        let sample = sample.ok_or_else(|| "The native decoder returned no video frame.".to_string())?;
        present_nv12_sample(&pipeline.device, &pipeline.context, &pipeline.swap_chain, &sample)
    }
}
