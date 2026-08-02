//! Capability discovery for the direct hardware-decoder backend.
//!
//! This deliberately does not route playback yet: an MFT consumes elementary
//! H.264/HEVC samples, while MP4 demuxing is the next separate pipeline layer.

use windows::{
    core::{Interface, HSTRING, PCWSTR},
    Win32::{
        Foundation::BOOL,
        Media::MediaFoundation::{
            IMFMediaSource, IMFTransform, MFCreateSourceResolver, MFMediaType_Video, MFTEnumEx,
            MFVideoFormat_H264, MFVideoFormat_HEVC, MFT_CATEGORY_VIDEO_DECODER, MFT_ENUM_FLAG,
            MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER, MFT_REGISTER_TYPE_INFO,
            MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_OBJECT_TYPE, MF_RESOLUTION_MEDIASOURCE,
            MF_SA_D3D11_AWARE,
        },
        System::Com::CoTaskMemFree,
        UI::Shell::PropertiesSystem::IPropertyStore,
    },
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DirectCodec {
    H264,
    Hevc,
}

/// Reads MP4 metadata through IMFMediaSource, keeping demuxing separate from
/// the decoder MFT. It deliberately does not use IMFSourceReader.
pub fn probe_mp4_demuxer(path: &str) -> Result<DirectCodec, String> {
    unsafe {
        let resolver = MFCreateSourceResolver().map_err(|e| format!("MP4 resolver: {e}"))?;
        let url = HSTRING::from(path);
        let mut object_type = MF_OBJECT_TYPE(0);
        let mut object = None;
        resolver
            .CreateObjectFromURL(
                PCWSTR(url.as_ptr()),
                MF_RESOLUTION_MEDIASOURCE.0 as u32,
                None::<&IPropertyStore>,
                &mut object_type,
                &mut object,
            )
            .map_err(|e| format!("Could not open MP4 media source: {e}"))?;
        let source: IMFMediaSource = object
            .ok_or_else(|| "MP4 resolver returned no object.".to_string())?
            .cast()
            .map_err(|e| format!("Resolved object is not media source: {e}"))?;
        let descriptor = source
            .CreatePresentationDescriptor()
            .map_err(|e| format!("MP4 presentation descriptor: {e}"))?;
        for index in 0..descriptor
            .GetStreamDescriptorCount()
            .map_err(|e| format!("MP4 streams: {e}"))?
        {
            let mut selected = BOOL(0);
            let mut stream = None;
            descriptor
                .GetStreamDescriptorByIndex(index, &mut selected, &mut stream)
                .map_err(|e| format!("MP4 stream {index}: {e}"))?;
            let Some(stream) = stream else { continue };
            let media_type = stream
                .GetMediaTypeHandler()
                .and_then(|h| h.GetCurrentMediaType())
                .map_err(|e| format!("MP4 stream type: {e}"))?;
            if media_type.GetGUID(&MF_MT_MAJOR_TYPE).ok() != Some(MFMediaType_Video) {
                continue;
            }
            let subtype = media_type
                .GetGUID(&MF_MT_SUBTYPE)
                .map_err(|_| "MP4 video stream has no subtype.".to_string())?;
            if subtype == MFVideoFormat_H264 {
                return Ok(DirectCodec::H264);
            }
            if subtype == MFVideoFormat_HEVC {
                return Ok(DirectCodec::Hevc);
            }
            return Err(format!("Unsupported MP4 video subtype {subtype:?}."));
        }
        Err("MP4 has no video stream.".into())
    }
}

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
