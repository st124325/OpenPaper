//! Native MP4 decode foundation: Media Foundation Source Reader + D3D11.
//!
//! This module deliberately owns the exact GPU objects required by the final
//! presenter.  MP4 decoding can therefore be validated independently before
//! it replaces the stable libVLC output path.

use std::sync::OnceLock;
use windows::{
    core::{HSTRING, Interface, PCWSTR},
    Win32::{
        Foundation::{HMODULE, HWND},
        Graphics::{
            Direct3D::D3D_DRIVER_TYPE_HARDWARE,
            Direct3D11::{
                D3D11CreateDevice, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION, ID3D11Device,
                ID3D11DeviceContext,
            },
            Dxgi::{
                Common::{DXGI_ALPHA_MODE_IGNORE, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC},
                DXGI_SCALING_STRETCH, DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_EFFECT_FLIP_DISCARD,
                DXGI_USAGE_RENDER_TARGET_OUTPUT, IDXGIAdapter, IDXGIDevice, IDXGIFactory2,
                IDXGIOutput, IDXGISwapChain1,
            },
        },
        Media::MediaFoundation::{
            MFCreateAttributes, MFCreateDXGIDeviceManager, MFCreateMediaType,
            MFCreateSourceReaderFromURL, MFStartup, IMFSourceReader, IMFDXGIDeviceManager,
            MFMediaType_Video, MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, MF_SOURCE_READER_D3D_MANAGER,
            MF_SOURCE_READER_DISABLE_DXVA, MF_SOURCE_READER_FIRST_VIDEO_STREAM, MF_MT_MAJOR_TYPE,
            MF_MT_SUBTYPE, MF_VERSION, MFVideoFormat_NV12, MFSTARTUP_FULL,
        },
    },
};

static MEDIA_FOUNDATION_STARTED: OnceLock<bool> = OnceLock::new();
static HARDWARE_PIPELINE_AVAILABLE: OnceLock<bool> = OnceLock::new();

struct NativeMp4Pipeline {
    _device: ID3D11Device,
    _context: ID3D11DeviceContext,
    _manager: IMFDXGIDeviceManager,
    _reader: IMFSourceReader,
    _swap_chain: IDXGISwapChain1,
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

/// Opens an MP4 with a hardware-enabled Source Reader and creates the D3D11
/// swap chain for the wallpaper host. The pipeline is currently used as a
/// preflight while the next substage adds texture conversion and Present.
pub fn probe_mp4(path: &str, host: HWND) -> Result<(), String> {
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

        // Read one decoded sample: this validates the Source Reader setup
        // before the renderer is allowed to replace the stable libVLC path.
        let mut sample = None;
        reader
            .ReadSample(video_stream, 0, None, None, None, Some(&mut sample))
            .map_err(|error| format!("Could not decode the first native frame: {error}"))?;
        if sample.is_none() {
            return Err("The native decoder returned no video frame.".into());
        }

        let _pipeline = NativeMp4Pipeline {
            _device: device,
            _context: context,
            _manager: manager,
            _reader: reader,
            _swap_chain: swap_chain,
        };
        Ok(())
    }
}
