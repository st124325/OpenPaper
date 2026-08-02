//! Capability discovery for the direct hardware-decoder backend.
//!
//! This deliberately does not route playback yet: an MFT consumes elementary
//! H.264/HEVC samples, while MP4 demuxing is the next separate pipeline layer.

use windows::Win32::{
    Media::MediaFoundation::{
        IMFTransform, MFMediaType_Video, MFTEnumEx, MFVideoFormat_H264, MFVideoFormat_HEVC,
        MFT_CATEGORY_VIDEO_DECODER, MFT_ENUM_FLAG, MFT_ENUM_FLAG_HARDWARE,
        MFT_ENUM_FLAG_SORTANDFILTER, MFT_REGISTER_TYPE_INFO, MF_SA_D3D11_AWARE,
    },
    System::Com::CoTaskMemFree,
};

/// Returns whether Windows exposes a D3D11-aware hardware MFT for H.264 or
/// HEVC. The actual decoder is intentionally activated only after the MP4
/// demuxer has supplied its codec configuration and elementary samples.
pub fn has_d3d11_hardware_decoder() -> bool {
    unsafe {
        [MFVideoFormat_H264, MFVideoFormat_HEVC]
            .into_iter()
            .any(|subtype| has_decoder_for_subtype(subtype))
    }
}

unsafe fn has_decoder_for_subtype(subtype: windows::core::GUID) -> bool {
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: subtype,
    };
    let mut activations = std::ptr::null_mut();
    let mut count = 0u32;
    let flags = MFT_ENUM_FLAG(MFT_ENUM_FLAG_HARDWARE.0 | MFT_ENUM_FLAG_SORTANDFILTER.0);
    if MFTEnumEx(
        MFT_CATEGORY_VIDEO_DECODER,
        flags,
        Some(&input),
        None,
        &mut activations,
        &mut count,
    )
    .is_err()
        || activations.is_null()
    {
        return false;
    }
    let entries = std::slice::from_raw_parts(activations, count as usize);
    let supported = entries.iter().flatten().any(|activation| {
        activation
            .ActivateObject::<IMFTransform>()
            .ok()
            .and_then(|transform| transform.GetAttributes().ok())
            .and_then(|attributes| attributes.GetUINT32(&MF_SA_D3D11_AWARE).ok())
            .is_some_and(|aware| aware != 0)
    });
    CoTaskMemFree(Some(activations.cast()));
    supported
}
