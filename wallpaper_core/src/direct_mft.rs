//! Capability discovery for the direct hardware-decoder backend.
//!
//! This deliberately does not route playback yet: an MFT consumes elementary
//! H.264/HEVC samples, while MP4 demuxing is the next separate pipeline layer.

use std::{
    thread,
    time::{Duration, Instant},
};

use windows::{
    core::{IUnknown, Interface, HSTRING, PCWSTR, PROPVARIANT},
    Win32::{
        Foundation::BOOL,
        Media::MediaFoundation::{
            IMFMediaSource, IMFMediaStream, IMFTransform, MEMediaSample, MENewStream,
            MEStreamStarted, MFCreateSourceResolver, MFMediaType_Video, MFTEnumEx,
            MFVideoFormat_H264, MFVideoFormat_HEVC, MFT_CATEGORY_VIDEO_DECODER, MFT_ENUM_FLAG,
            MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER, MFT_REGISTER_TYPE_INFO,
            MF_EVENT_FLAG_NO_WAIT, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_OBJECT_TYPE,
            MF_RESOLUTION_MEDIASOURCE, MF_SA_D3D11_AWARE,
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

/// Result of a bounded, asynchronous Media Source demuxing session. This is
/// intentionally a diagnostic primitive: the native player will use the same
/// event model before samples are sent into the hardware decoder MFT.
#[derive(Clone, Copy, Debug, Default)]
pub struct DirectMp4EventStats {
    pub source_events: u32,
    pub stream_events: u32,
    pub samples: u32,
    pub stream_started: bool,
}

/// Opens an MP4 using `IMFMediaSource`, starts it, and receives samples via
/// `IMFMediaStream::RequestSample` for a short bounded period. No frames are
/// rendered here; this is the safe demuxing milestone before MFT wiring.
pub fn probe_direct_mp4_event_loop(
    path: &str,
    timeout: Duration,
) -> Result<DirectMp4EventStats, String> {
    unsafe {
        let source = resolve_media_source(path)?;
        let descriptor = source
            .CreatePresentationDescriptor()
            .map_err(|e| format!("MP4 presentation descriptor: {e}"))?;
        source
            .Start(&descriptor, std::ptr::null(), &PROPVARIANT::default())
            .map_err(|e| format!("MP4 source start: {e}"))?;

        let deadline = Instant::now() + timeout;
        let mut source_events = 0;
        let mut stream_events = 0;
        let mut samples = 0;
        let mut stream_started = false;
        let mut stream: Option<IMFMediaStream> = None;
        while Instant::now() < deadline && samples == 0 {
            if let Some(active_stream) = stream.as_ref() {
                match active_stream.GetEvent(MF_EVENT_FLAG_NO_WAIT) {
                    Ok(event) => {
                        stream_events += 1;
                        match event.GetType().unwrap_or_default() {
                            kind if kind == MEStreamStarted.0 as u32 => {
                                stream_started = true;
                                active_stream
                                    .RequestSample(None::<&IUnknown>)
                                    .map_err(|e| format!("MP4 RequestSample: {e}"))?;
                            }
                            kind if kind == MEMediaSample.0 as u32 => {
                                let value = event
                                    .GetValue()
                                    .map_err(|e| format!("MP4 sample event: {e}"))?;
                                let object = IUnknown::try_from(&value)
                                    .map_err(|e| format!("MP4 sample object: {e}"))?;
                                let _: windows::Win32::Media::MediaFoundation::IMFSample =
                                    object.cast().map_err(|e| format!("MP4 sample cast: {e}"))?;
                                samples += 1;
                            }
                            _ => {}
                        }
                    }
                    Err(_) => thread::sleep(Duration::from_millis(2)),
                }
            } else {
                match source.GetEvent(MF_EVENT_FLAG_NO_WAIT) {
                    Ok(event) => {
                        source_events += 1;
                        if event.GetType().ok() == Some(MENewStream.0 as u32) {
                            let value = event
                                .GetValue()
                                .map_err(|e| format!("MP4 stream event: {e}"))?;
                            let object = IUnknown::try_from(&value)
                                .map_err(|e| format!("MP4 stream object: {e}"))?;
                            stream =
                                Some(object.cast().map_err(|e| format!("MP4 stream cast: {e}"))?);
                        }
                    }
                    Err(_) => thread::sleep(Duration::from_millis(2)),
                }
            }
        }
        let stats = DirectMp4EventStats {
            source_events,
            stream_events,
            samples,
            stream_started,
        };
        let _ = source.Shutdown();
        Ok(stats)
    }
}

unsafe fn resolve_media_source(path: &str) -> Result<IMFMediaSource, String> {
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
    object
        .ok_or_else(|| "MP4 resolver returned no object.".to_string())?
        .cast()
        .map_err(|e| format!("Resolved object is not media source: {e}"))
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

/// Activates a hardware decoder and gives it the exact compressed video media
/// type advertised by the MP4 Media Source. A positive result proves more
/// than codec enumeration: Windows accepted this file's H.264/HEVC stream at
/// the decoder input boundary.
pub fn can_configure_hardware_decoder_for_mp4(path: &str) -> Result<bool, String> {
    unsafe {
        let source = resolve_media_source(path)?;
        let descriptor = source
            .CreatePresentationDescriptor()
            .map_err(|e| format!("MP4 presentation descriptor: {e}"))?;
        let mut configured = false;
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
                .and_then(|handler| handler.GetCurrentMediaType())
                .map_err(|e| format!("MP4 stream type: {e}"))?;
            if media_type.GetGUID(&MF_MT_MAJOR_TYPE).ok() != Some(MFMediaType_Video) {
                continue;
            }
            let subtype = media_type
                .GetGUID(&MF_MT_SUBTYPE)
                .map_err(|_| "MP4 video stream has no subtype.".to_string())?;
            if subtype != MFVideoFormat_H264 && subtype != MFVideoFormat_HEVC {
                break;
            }
            configured = configure_decoder_input(subtype, &media_type);
            break;
        }
        let _ = source.Shutdown();
        Ok(configured)
    }
}

unsafe fn configure_decoder_input(
    subtype: windows::core::GUID,
    media_type: &windows::Win32::Media::MediaFoundation::IMFMediaType,
) -> bool {
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: subtype,
    };
    let mut activations = std::ptr::null_mut();
    let mut count = 0u32;
    let flags = MFT_ENUM_FLAG(MFT_ENUM_FLAG_HARDWARE.0 | MFT_ENUM_FLAG_SORTANDFILTER.0);
    let result = MFTEnumEx(
        MFT_CATEGORY_VIDEO_DECODER,
        flags,
        Some(&input),
        None,
        &mut activations,
        &mut count,
    );
    if result.is_err() || activations.is_null() {
        return false;
    }
    let entries = std::slice::from_raw_parts(activations, count as usize);
    let accepted = entries.iter().flatten().any(|activation| {
        activation
            .ActivateObject::<IMFTransform>()
            .and_then(|decoder| decoder.SetInputType(0, media_type, 0))
            .is_ok()
    });
    CoTaskMemFree(Some(activations.cast()));
    accepted
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
