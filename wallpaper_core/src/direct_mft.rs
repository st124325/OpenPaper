//! Capability discovery for the direct hardware-decoder backend.
//!
//! This deliberately does not route playback yet: an MFT consumes elementary
//! H.264/HEVC samples, while MP4 demuxing is the next separate pipeline layer.

use std::{
    collections::VecDeque,
    mem::ManuallyDrop,
    thread,
    time::{Duration, Instant},
};

use windows::{
    core::{IUnknown, Interface, HSTRING, PCWSTR, PROPVARIANT},
    Win32::{
        Foundation::{BOOL, HMODULE, HWND},
        Graphics::{
            Direct3D::D3D_DRIVER_TYPE_HARDWARE,
            Direct3D10::ID3D10Multithread,
            Direct3D11::{
                D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext,
                D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                D3D11_SDK_VERSION,
            },
            Dxgi::IDXGIAdapter,
        },
        Media::MediaFoundation::{
            IMFDXGIDeviceManager, IMFMediaEventGenerator, IMFMediaSource, IMFMediaStream,
            IMFPresentationDescriptor, IMFShutdown, IMFTransform, MEMediaSample, MENewStream,
            MEStreamStarted, METransformHaveOutput, METransformNeedInput,
            MFCreateDXGIDeviceManager, MFCreateSourceResolver, MFMediaType_Video, MFTEnumEx,
            MFVideoFormat_H264, MFVideoFormat_HEVC, MFVideoFormat_NV12, MFT_CATEGORY_VIDEO_DECODER,
            MFT_ENUM_FLAG, MFT_ENUM_FLAG_ALL, MFT_ENUM_FLAG_SORTANDFILTER,
            MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_START_OF_STREAM,
            MFT_MESSAGE_SET_D3D_MANAGER, MFT_OUTPUT_DATA_BUFFER,
            MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MFT_REGISTER_TYPE_INFO, MF_EVENT_FLAG_NO_WAIT,
            MF_E_NOTACCEPTING, MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE,
            MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_OBJECT_TYPE, MF_RESOLUTION_MEDIASOURCE,
            MF_SA_D3D11_AWARE, MF_TRANSFORM_ASYNC, MF_TRANSFORM_ASYNC_UNLOCK,
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
        select_only_video_stream(&descriptor)?;
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

unsafe fn select_only_video_stream(descriptor: &IMFPresentationDescriptor) -> Result<(), String> {
    let mut video_found = false;
    for index in 0..descriptor
        .GetStreamDescriptorCount()
        .map_err(|e| format!("MP4 streams: {e}"))?
    {
        let mut selected = BOOL(0);
        let mut stream = None;
        descriptor
            .GetStreamDescriptorByIndex(index, &mut selected, &mut stream)
            .map_err(|e| format!("MP4 stream {index}: {e}"))?;
        let is_video = stream
            .and_then(|stream| stream.GetMediaTypeHandler().ok())
            .and_then(|handler| handler.GetCurrentMediaType().ok())
            .and_then(|media_type| media_type.GetGUID(&MF_MT_MAJOR_TYPE).ok())
            == Some(MFMediaType_Video);
        if is_video && !video_found {
            descriptor
                .SelectStream(index)
                .map_err(|e| format!("MP4 video selection: {e}"))?;
            video_found = true;
        } else {
            descriptor
                .DeselectStream(index)
                .map_err(|e| format!("MP4 stream deselection: {e}"))?;
        }
    }
    video_found
        .then_some(())
        .ok_or_else(|| "MP4 has no video stream.".into())
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
    let flags = decoder_enum_flags();
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
        let Ok(decoder) = activation.ActivateObject::<IMFTransform>() else {
            return false;
        };
        let accepted = decoder.SetInputType(0, media_type, 0).is_ok();
        if let Ok(shutdown) = decoder.cast::<IMFShutdown>() {
            let _ = shutdown.Shutdown();
        }
        accepted
    });
    CoTaskMemFree(Some(activations.cast()));
    accepted
}

/// Owns every COM/D3D object required by a hardware decoder. The fields are
/// intentionally kept together so the decoder can never outlive its device
/// manager or use a D3D device from a different pipeline.
pub struct DirectDecoderSession {
    pub decoder: IMFTransform,
    events: Option<IMFMediaEventGenerator>,
    pub(crate) device: ID3D11Device,
    pub(crate) context: ID3D11DeviceContext,
    pub(crate) manager: IMFDXGIDeviceManager,
}

impl DirectDecoderSession {
    /// Creates a zero-copy NV12 presenter on the same D3D11 device as the
    /// decoder. Using one device is mandatory for a GPU texture to be passed
    /// directly into the Video Processor.
    pub(crate) unsafe fn create_presenter(
        &self,
        host: HWND,
    ) -> Result<crate::mf_d3d11::ExternalNv12Presenter, String> {
        crate::mf_d3d11::ExternalNv12Presenter::new(
            self.device.clone(),
            self.context.clone(),
            self.manager.clone(),
            host,
        )
    }
}

impl Drop for DirectDecoderSession {
    fn drop(&mut self) {
        unsafe {
            if let Ok(shutdown) = self.decoder.cast::<IMFShutdown>() {
                let _ = shutdown.Shutdown();
            }
        }
    }
}

/// Reads one compressed video sample directly from the Media Source. It is
/// deliberately synchronous and bounded: source, stream, and sample remain
/// in one COM apartment until the sample is handed to the decoder.
pub fn read_first_direct_video_sample(
    path: &str,
    timeout: Duration,
) -> Result<windows::Win32::Media::MediaFoundation::IMFSample, String> {
    unsafe {
        let source = resolve_media_source(path)?;
        let descriptor = source
            .CreatePresentationDescriptor()
            .map_err(|e| format!("MP4 presentation descriptor: {e}"))?;
        select_only_video_stream(&descriptor)?;
        source
            .Start(&descriptor, std::ptr::null(), &PROPVARIANT::default())
            .map_err(|e| format!("MP4 source start: {e}"))?;
        let deadline = Instant::now() + timeout;
        let mut stream: Option<IMFMediaStream> = None;
        let mut sample = None;
        while Instant::now() < deadline && sample.is_none() {
            if let Some(active_stream) = stream.as_ref() {
                match active_stream.GetEvent(MF_EVENT_FLAG_NO_WAIT) {
                    Ok(event) if event.GetType().ok() == Some(MEStreamStarted.0 as u32) => {
                        active_stream
                            .RequestSample(None::<&IUnknown>)
                            .map_err(|e| format!("MP4 RequestSample: {e}"))?;
                    }
                    Ok(event) if event.GetType().ok() == Some(MEMediaSample.0 as u32) => {
                        let value = event
                            .GetValue()
                            .map_err(|e| format!("MP4 sample event: {e}"))?;
                        let object = IUnknown::try_from(&value)
                            .map_err(|e| format!("MP4 sample object: {e}"))?;
                        sample = Some(object.cast().map_err(|e| format!("MP4 sample cast: {e}"))?);
                    }
                    Ok(_) => {}
                    Err(_) => thread::sleep(Duration::from_millis(2)),
                }
            } else {
                match source.GetEvent(MF_EVENT_FLAG_NO_WAIT) {
                    Ok(event) if event.GetType().ok() == Some(MENewStream.0 as u32) => {
                        let value = event
                            .GetValue()
                            .map_err(|e| format!("MP4 stream event: {e}"))?;
                        let object = IUnknown::try_from(&value)
                            .map_err(|e| format!("MP4 stream object: {e}"))?;
                        stream = Some(object.cast().map_err(|e| format!("MP4 stream cast: {e}"))?);
                    }
                    Ok(_) => {}
                    Err(_) => thread::sleep(Duration::from_millis(2)),
                }
            }
        }
        let _ = source.Stop();
        let _ = source.Shutdown();
        sample.ok_or_else(|| {
            "Direct MP4 demuxer did not return a video sample before timeout.".into()
        })
    }
}

/// Executes the first native decoder operation against a real compressed MP4
/// sample. Output conversion is intentionally a separate stage so a failure
/// here can never leave user wallpaper playback without video.
pub fn can_process_first_direct_mp4_sample(path: &str) -> Result<bool, String> {
    let session = create_gpu_decoder_session_for_mp4(path)?;
    if session.events.is_some() {
        return Err("Hardware MFT is asynchronous; use the event-driven native smoke test.".into());
    }
    let sample = read_first_direct_video_sample(path, Duration::from_secs(3))?;
    unsafe {
        session
            .decoder
            .ProcessInput(0, &sample, 0)
            .map_err(|e| format!("Hardware MFT rejected the first MP4 sample: {e}"))?;
    }
    Ok(true)
}

/// Pulls an NV12 decoder-owned sample after input has been accepted. Hardware
/// decoders normally provide the output sample themselves; forcing a system
/// memory buffer here would defeat zero-copy presentation.
pub fn decode_first_direct_mp4_sample_to_nv12(
    path: &str,
) -> Result<windows::Win32::Media::MediaFoundation::IMFSample, String> {
    let session = create_gpu_decoder_session_for_mp4(path)?;
    let sample = read_first_direct_video_sample(path, Duration::from_secs(3))?;
    unsafe { process_sample_to_nv12(&session, &sample) }
}

pub fn decode_and_present_first_direct_mp4_sample(path: &str, host: HWND) -> Result<(), String> {
    let session = create_gpu_decoder_session_for_mp4(path)?;
    let mut presenter = unsafe { session.create_presenter(host)? };
    let sample = read_first_direct_video_sample(path, Duration::from_secs(3))?;
    let nv12 = unsafe { process_sample_to_nv12(&session, &sample)? };
    unsafe { presenter.present(&nv12) }
}

unsafe fn process_sample_to_nv12(
    session: &DirectDecoderSession,
    sample: &windows::Win32::Media::MediaFoundation::IMFSample,
) -> Result<windows::Win32::Media::MediaFoundation::IMFSample, String> {
    if session.events.is_some() {
        return Err("Hardware MFT is asynchronous; direct ProcessOutput is only valid after METransformHaveOutput.".into());
    }
    session
        .decoder
        .ProcessInput(0, sample, 0)
        .map_err(|e| format!("Hardware MFT rejected the first MP4 sample: {e}"))?;
    let info = session
        .decoder
        .GetOutputStreamInfo(0)
        .map_err(|e| format!("Hardware MFT output stream info: {e}"))?;
    if info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 == 0 {
        return Err("Hardware MFT requires an application-owned output buffer; GPU surface allocation is the next compatibility path.".into());
    }
    let output = MFT_OUTPUT_DATA_BUFFER {
        dwStreamID: 0,
        pSample: ManuallyDrop::new(None),
        dwStatus: 0,
        pEvents: ManuallyDrop::new(None),
    };
    let mut status = 0;
    let mut outputs = [output];
    let result = session.decoder.ProcessOutput(0, &mut outputs, &mut status);
    let mut output = outputs.into_iter().next().expect("one output buffer");
    let sample = ManuallyDrop::take(&mut output.pSample);
    ManuallyDrop::drop(&mut output.pEvents);
    result.map_err(|e| format!("Hardware MFT has no NV12 output yet: {e}"))?;
    sample.ok_or_else(|| "Hardware MFT returned success without an NV12 output sample.".into())
}

#[derive(Clone, Copy, Debug, Default)]
pub struct NativeSmokeStats {
    pub input_samples: u32,
    pub output_frames: u32,
}

/// Bounded end-to-end smoke test for the direct backend. It continuously
/// requests MP4 video samples, feeds the hardware MFT, keeps a three-frame
/// GPU queue for back-pressure, and presents every decoded NV12 frame.
pub fn run_native_mp4_smoke_test(
    path: &str,
    host: HWND,
    timeout: Duration,
) -> Result<NativeSmokeStats, String> {
    unsafe {
        let session = create_gpu_decoder_session_for_mp4(path)?;
        let skip_present =
            std::env::var("OPENPAPER_NATIVE_SMOKE_DECODE_ONLY").as_deref() == Ok("1");
        let mut presenter = if skip_present {
            None
        } else {
            Some(session.create_presenter(host)?)
        };
        let source = resolve_media_source(path)?;
        let descriptor = source
            .CreatePresentationDescriptor()
            .map_err(|e| format!("MP4 presentation descriptor: {e}"))?;
        select_only_video_stream(&descriptor)?;
        source
            .Start(&descriptor, std::ptr::null(), &PROPVARIANT::default())
            .map_err(|e| format!("MP4 source start: {e}"))?;

        let deadline = Instant::now() + timeout;
        let mut stream: Option<IMFMediaStream> = None;
        let mut compressed = VecDeque::with_capacity(8);
        let mut queue = VecDeque::with_capacity(3);
        let mut pending_need_input = 0u32;
        let mut stream_started = false;
        let mut sample_request_pending = false;
        let mut stats = NativeSmokeStats::default();
        while Instant::now() < deadline && stats.input_samples < 180 {
            if let Some(events) = session.events.as_ref() {
                // An async decoder may continuously publish NeedInput events.
                // Bound each pump iteration so the wall-clock watchdog and
                // source/presenter queues always get CPU time.
                for _ in 0..64 {
                    let Ok(event) = events.GetEvent(MF_EVENT_FLAG_NO_WAIT) else {
                        break;
                    };
                    match event.GetType().unwrap_or_default() {
                        kind if kind == METransformNeedInput.0 as u32 => {
                            pending_need_input = pending_need_input.saturating_add(1);
                        }
                        kind if kind == METransformHaveOutput.0 as u32 => {
                            if let Some(frame) = try_process_output(&session)? {
                                if queue.len() == 3 {
                                    queue.pop_front();
                                }
                                queue.push_back(frame);
                            }
                        }
                        _ => {}
                    }
                }
                while pending_need_input > 0 {
                    let Some(sample) = compressed.pop_front() else {
                        break;
                    };
                    session
                        .decoder
                        .ProcessInput(0, &sample, 0)
                        .map_err(|e| format!("Async hardware MFT ProcessInput failed: {e}"))?;
                    pending_need_input -= 1;
                    stats.input_samples += 1;
                }
            }
            while let Some(frame) = queue.pop_front() {
                if let Some(presenter) = presenter.as_mut() {
                    presenter.present(&frame)?;
                }
                stats.output_frames += 1;
            }
            if let Some(active_stream) = stream.as_ref() {
                match active_stream.GetEvent(MF_EVENT_FLAG_NO_WAIT) {
                    Ok(event) if event.GetType().ok() == Some(MEStreamStarted.0 as u32) => {
                        stream_started = true;
                    }
                    Ok(event) if event.GetType().ok() == Some(MEMediaSample.0 as u32) => {
                        sample_request_pending = false;
                        let value = event
                            .GetValue()
                            .map_err(|e| format!("MP4 sample event: {e}"))?;
                        let object = IUnknown::try_from(&value)
                            .map_err(|e| format!("MP4 sample object: {e}"))?;
                        let sample = object.cast().map_err(|e| format!("MP4 sample cast: {e}"))?;
                        if session.events.is_some() {
                            compressed.push_back(sample);
                        } else {
                            feed_and_collect_output(&session, &sample, &mut queue)?;
                            stats.input_samples += 1;
                        }
                    }
                    Ok(_) => {}
                    Err(_) => {}
                }
                if stream_started
                    && !sample_request_pending
                    && compressed.len() < 8
                    && stats.input_samples < 180
                {
                    active_stream
                        .RequestSample(None::<&IUnknown>)
                        .map_err(|e| format!("MP4 RequestSample: {e}"))?;
                    sample_request_pending = true;
                }
                if session.events.is_none() {
                    while let Some(frame) = queue.pop_front() {
                        if let Some(presenter) = presenter.as_mut() {
                            presenter.present(&frame)?;
                        }
                        stats.output_frames += 1;
                    }
                }
            } else {
                match source.GetEvent(MF_EVENT_FLAG_NO_WAIT) {
                    Ok(event) if event.GetType().ok() == Some(MENewStream.0 as u32) => {
                        let value = event
                            .GetValue()
                            .map_err(|e| format!("MP4 stream event: {e}"))?;
                        let object = IUnknown::try_from(&value)
                            .map_err(|e| format!("MP4 stream object: {e}"))?;
                        stream = Some(object.cast().map_err(|e| format!("MP4 stream cast: {e}"))?);
                    }
                    Ok(_) => {}
                    Err(_) => thread::sleep(Duration::from_millis(2)),
                }
            }
            thread::sleep(Duration::from_millis(1));
        }
        let _ = source.Shutdown();
        if stats.output_frames == 0 {
            return Err(format!(
                "Native smoke test decoded no frames after {} input samples.",
                stats.input_samples
            ));
        }
        Ok(stats)
    }
}

unsafe fn feed_and_collect_output(
    session: &DirectDecoderSession,
    sample: &windows::Win32::Media::MediaFoundation::IMFSample,
    queue: &mut VecDeque<windows::Win32::Media::MediaFoundation::IMFSample>,
) -> Result<(), String> {
    match session.decoder.ProcessInput(0, sample, 0) {
        Ok(()) => {}
        Err(error) if error.code() == MF_E_NOTACCEPTING => drain_decoder_output(session, queue)?,
        Err(error) => return Err(format!("Hardware MFT ProcessInput failed: {error}")),
    }
    drain_decoder_output(session, queue)
}

unsafe fn drain_decoder_output(
    session: &DirectDecoderSession,
    queue: &mut VecDeque<windows::Win32::Media::MediaFoundation::IMFSample>,
) -> Result<(), String> {
    loop {
        match try_process_output(session)? {
            Some(frame) => {
                if queue.len() == 3 {
                    queue.pop_front();
                }
                queue.push_back(frame);
            }
            None => return Ok(()),
        }
    }
}

unsafe fn try_process_output(
    session: &DirectDecoderSession,
) -> Result<Option<windows::Win32::Media::MediaFoundation::IMFSample>, String> {
    let info = session
        .decoder
        .GetOutputStreamInfo(0)
        .map_err(|e| format!("Hardware MFT output stream info: {e}"))?;
    if info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 == 0 {
        return Err("Hardware MFT requires an application-owned output buffer; GPU surface allocation is the next compatibility path.".into());
    }
    for _ in 0..2 {
        let output = MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: ManuallyDrop::new(None),
            dwStatus: 0,
            pEvents: ManuallyDrop::new(None),
        };
        let mut status = 0;
        let mut outputs = [output];
        let result = session.decoder.ProcessOutput(0, &mut outputs, &mut status);
        let mut output = outputs.into_iter().next().expect("one output buffer");
        let sample = ManuallyDrop::take(&mut output.pSample);
        ManuallyDrop::drop(&mut output.pEvents);
        match result {
            Ok(()) => return Ok(sample),
            Err(error) if error.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(None),
            Err(error) if error.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                select_current_nv12_output_type(&session.decoder)?;
            }
            Err(error) => return Err(format!("Hardware MFT ProcessOutput failed: {error}")),
        }
    }
    Err("Hardware MFT repeatedly changed its output stream type.".into())
}

unsafe fn select_current_nv12_output_type(decoder: &IMFTransform) -> Result<(), String> {
    for index in 0..32 {
        let candidate = match decoder.GetOutputAvailableType(0, index) {
            Ok(value) => value,
            Err(_) => break,
        };
        if candidate.GetGUID(&MF_MT_SUBTYPE).ok() == Some(MFVideoFormat_NV12) {
            return decoder
                .SetOutputType(0, &candidate, 0)
                .map_err(|e| format!("Could not renegotiate NV12 decoder output: {e}"));
        }
    }
    Err("Decoder stream changed but exposed no NV12 output type.".into())
}

/// Creates a D3D11 device and binds its Media Foundation device manager to an
/// MFT which accepts the compressed video type from this MP4. The session is
/// ready for ProcessInput/ProcessOutput; demuxed samples will be connected in
/// the next renderer step.
pub fn create_gpu_decoder_session_for_mp4(path: &str) -> Result<DirectDecoderSession, String> {
    unsafe {
        let source = resolve_media_source(path)?;
        let descriptor = source
            .CreatePresentationDescriptor()
            .map_err(|e| format!("MP4 presentation descriptor: {e}"))?;
        let mut video_type = None;
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
            if media_type.GetGUID(&MF_MT_MAJOR_TYPE).ok() == Some(MFMediaType_Video) {
                video_type = Some(media_type);
                break;
            }
        }
        let _ = source.Shutdown();
        let video_type = video_type.ok_or_else(|| "MP4 has no video stream.".to_string())?;
        let subtype = video_type
            .GetGUID(&MF_MT_SUBTYPE)
            .map_err(|_| "MP4 video stream has no subtype.".to_string())?;
        if subtype != MFVideoFormat_H264 && subtype != MFVideoFormat_HEVC {
            return Err("MP4 video codec is not H.264 or HEVC.".into());
        }
        let (device, context, manager) = create_decoder_device_manager()?;
        let decoder = activate_gpu_decoder(subtype, &video_type, &manager)?;
        let attributes = decoder
            .GetAttributes()
            .map_err(|e| format!("Hardware MFT attributes: {e}"))?;
        let events = if attributes.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) != 0 {
            Some(
                decoder
                    .cast()
                    .map_err(|e| format!("Async hardware MFT event generator: {e}"))?,
            )
        } else {
            None
        };
        Ok(DirectDecoderSession {
            decoder,
            events,
            device,
            context,
            manager,
        })
    }
}

unsafe fn create_decoder_device_manager(
) -> Result<(ID3D11Device, ID3D11DeviceContext, IMFDXGIDeviceManager), String> {
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
    .map_err(|e| format!("D3D11 hardware device creation failed: {e}"))?;
    let device = device.ok_or_else(|| "D3D11 returned no device.".to_string())?;
    let context = context.ok_or_else(|| "D3D11 returned no context.".to_string())?;
    let multithread: ID3D10Multithread = device
        .cast()
        .map_err(|e| format!("D3D11 multithread protection is unavailable: {e}"))?;
    let _ = multithread.SetMultithreadProtected(true);
    let mut token = 0;
    let mut manager = None;
    MFCreateDXGIDeviceManager(&mut token, &mut manager)
        .map_err(|e| format!("DXGI device manager creation failed: {e}"))?;
    let manager =
        manager.ok_or_else(|| "Media Foundation returned no DXGI manager.".to_string())?;
    manager
        .ResetDevice(&device, token)
        .map_err(|e| format!("DXGI device manager reset failed: {e}"))?;
    Ok((device, context, manager))
}

unsafe fn activate_gpu_decoder(
    subtype: windows::core::GUID,
    media_type: &windows::Win32::Media::MediaFoundation::IMFMediaType,
    manager: &IMFDXGIDeviceManager,
) -> Result<IMFTransform, String> {
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: subtype,
    };
    let mut activations = std::ptr::null_mut();
    let mut count = 0u32;
    let flags = decoder_enum_flags();
    MFTEnumEx(
        MFT_CATEGORY_VIDEO_DECODER,
        flags,
        Some(&input),
        None,
        &mut activations,
        &mut count,
    )
    .map_err(|e| format!("Hardware decoder enumeration failed: {e}"))?;
    if activations.is_null() {
        return Err("No hardware decoder was found for this MP4 codec.".into());
    }
    let entries = std::slice::from_raw_parts(activations, count as usize);
    let decoder = entries.iter().flatten().find_map(|activation| {
        let decoder = activation.ActivateObject::<IMFTransform>().ok()?;
        let configured = (|| -> Option<()> {
            let attributes = decoder.GetAttributes().ok()?;
            if attributes.GetUINT32(&MF_SA_D3D11_AWARE).unwrap_or(0) == 0 {
                return None;
            }
            if attributes.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) != 0 {
                attributes.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1).ok()?;
            }
            decoder
                .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)
                .ok()?;
            decoder.SetInputType(0, media_type, 0).ok()?;
            let mut output_index = 0;
            let output_type = loop {
                let candidate = decoder.GetOutputAvailableType(0, output_index).ok()?;
                if candidate.GetGUID(&MF_MT_SUBTYPE).ok() == Some(MFVideoFormat_NV12) {
                    break candidate;
                }
                output_index += 1;
                if output_index >= 32 {
                    return None;
                }
            };
            decoder.SetOutputType(0, &output_type, 0).ok()?;
            decoder
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .ok()?;
            decoder
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .ok()?;
            Some(())
        })();
        if configured.is_some() {
            Some(decoder)
        } else {
            if let Ok(shutdown) = decoder.cast::<IMFShutdown>() {
                let _ = shutdown.Shutdown();
            }
            None
        }
    });
    CoTaskMemFree(Some(activations.cast()));
    decoder.ok_or_else(|| {
        "No hardware decoder accepted the D3D11 device manager and MP4 media type.".into()
    })
}

unsafe fn has_decoder_for_subtype(subtype: windows::core::GUID) -> bool {
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: subtype,
    };
    let mut activations = std::ptr::null_mut();
    let mut count = 0u32;
    let flags = decoder_enum_flags();
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
        let Ok(transform) = activation.ActivateObject::<IMFTransform>() else {
            return false;
        };
        let supported = transform
            .GetAttributes()
            .ok()
            .and_then(|attributes| attributes.GetUINT32(&MF_SA_D3D11_AWARE).ok())
            .is_some_and(|aware| aware != 0);
        if let Ok(shutdown) = transform.cast::<IMFShutdown>() {
            let _ = shutdown.Shutdown();
        }
        supported
    });
    CoTaskMemFree(Some(activations.cast()));
    supported
}

fn decoder_enum_flags() -> MFT_ENUM_FLAG {
    // Microsoft's H.264/HEVC decoder is commonly registered as a regular MFT
    // and activates DXVA only after receiving an IMFDXGIDeviceManager.
    MFT_ENUM_FLAG(MFT_ENUM_FLAG_ALL.0 | MFT_ENUM_FLAG_SORTANDFILTER.0)
}
