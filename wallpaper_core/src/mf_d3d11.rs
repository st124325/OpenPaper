//! Media Foundation + D3D11 bootstrap for the native MP4 renderer.
//!
//! The first milestone deliberately creates the same D3D11/DXGI bridge that
//! the Source Reader will receive in the next milestone.  A decoder that
//! supports DXVA can then decode directly into GPU-owned textures instead of
//! copying decoded frames through CPU memory.

use std::sync::OnceLock;
use windows::Win32::{
    Foundation::HMODULE,
    Graphics::{
        Direct3D::D3D_DRIVER_TYPE_HARDWARE,
        Direct3D11::{
            D3D11CreateDevice, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            D3D11_SDK_VERSION, ID3D11Device,
        },
        Dxgi::IDXGIAdapter,
    },
    Media::MediaFoundation::{MFCreateDXGIDeviceManager, MFStartup, MFSTARTUP_FULL, MF_VERSION},
};

static HARDWARE_PIPELINE_AVAILABLE: OnceLock<bool> = OnceLock::new();

/// Returns whether this Windows session can create the D3D11 device and the
/// Media Foundation DXGI device manager required by the native renderer.
pub fn hardware_pipeline_available() -> bool {
    *HARDWARE_PIPELINE_AVAILABLE.get_or_init(|| unsafe {
        if MFStartup(MF_VERSION, MFSTARTUP_FULL).is_err() {
            return false;
        }

        let mut device: Option<ID3D11Device> = None;
        if D3D11CreateDevice(
            None::<&IDXGIAdapter>,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        )
        .is_err()
        {
            return false;
        }
        let Some(device) = device else { return false; };

        let mut reset_token = 0u32;
        let mut manager = None;
        if MFCreateDXGIDeviceManager(&mut reset_token, &mut manager).is_err() {
            return false;
        }
        let Some(manager) = manager else { return false; };
        manager.ResetDevice(&device, reset_token).is_ok()
    })
}
