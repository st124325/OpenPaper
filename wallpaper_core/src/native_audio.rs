//! Native MP4 audio path: Media Foundation decoding into PCM and event-driven
//! WASAPI shared-mode rendering. All COM objects remain on one STA thread.

use crate::playback_clock::PlaybackClock;

use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering},
        mpsc, Arc,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use windows::{
    core::{HSTRING, PCWSTR},
    Win32::{
        Foundation::{CloseHandle, HANDLE, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT},
        Media::{
            Audio::{
                eMultimedia, eRender, IAudioClient, IAudioClock, IAudioRenderClient,
                IMMDeviceEnumerator, ISimpleAudioVolume, MMDeviceEnumerator,
                AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
                AUDCLNT_STREAMFLAGS_EVENTCALLBACK, WAVEFORMATEX,
            },
            MediaFoundation::{
                IMFMediaType, IMFSourceReader, MFAudioFormat_PCM, MFCreateMediaType,
                MFCreateSourceReaderFromURL, MFCreateWaveFormatExFromMFMediaType,
                MFMediaType_Audio, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE,
                MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED, MF_SOURCE_READERF_ENDOFSTREAM,
                MF_SOURCE_READER_ALL_STREAMS, MF_SOURCE_READER_FIRST_AUDIO_STREAM,
            },
        },
        System::Com::{
            CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
            COINIT_APARTMENTTHREADED,
        },
        System::Threading::{CreateEventW, WaitForSingleObject},
    },
};

pub struct NativeAudioRenderer {
    stop: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    activated: Arc<AtomicBool>,
    muted: Arc<AtomicBool>,
    volume: Arc<AtomicI32>,
    failed: Arc<AtomicBool>,
    frames_written: Arc<AtomicU64>,
    finished: Arc<AtomicBool>,
    worker: JoinHandle<()>,
}

impl NativeAudioRenderer {
    pub fn start(
        path: String,
        muted: bool,
        volume: i32,
        playback_clock: Arc<PlaybackClock>,
    ) -> Result<Self, String> {
        let stop = Arc::new(AtomicBool::new(false));
        let paused = Arc::new(AtomicBool::new(false));
        let activated = Arc::new(AtomicBool::new(false));
        let muted_state = Arc::new(AtomicBool::new(muted));
        let volume_state = Arc::new(AtomicI32::new(volume.clamp(0, 100)));
        let failed = Arc::new(AtomicBool::new(false));
        let frames_written = Arc::new(AtomicU64::new(0));
        let finished = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);

        let worker_stop = Arc::clone(&stop);
        let worker_paused = Arc::clone(&paused);
        let worker_activated = Arc::clone(&activated);
        let worker_muted = Arc::clone(&muted_state);
        let worker_volume = Arc::clone(&volume_state);
        let worker_failed = Arc::clone(&failed);
        let worker_frames = Arc::clone(&frames_written);
        let worker_finished = Arc::clone(&finished);
        let worker_clock = Arc::clone(&playback_clock);
        let worker = thread::spawn(move || unsafe {
            let initialized = CoInitializeEx(None, COINIT_APARTMENTTHREADED).is_ok();
            let result = if initialized {
                run_audio_renderer(
                    &path,
                    &worker_stop,
                    &worker_paused,
                    &worker_activated,
                    &worker_muted,
                    &worker_volume,
                    &worker_frames,
                    &ready_tx,
                    &worker_clock,
                )
            } else {
                Err("Could not initialize the native audio COM apartment.".into())
            };
            if let Err(error) = result {
                let _ = ready_tx.try_send(Err(error));
                if !worker_stop.load(Ordering::Acquire) {
                    worker_failed.store(true, Ordering::Release);
                }
            }
            worker_clock.deactivate();
            worker_finished.store(true, Ordering::Release);
            if initialized {
                CoUninitialize();
            }
        });

        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(Self {
                stop,
                paused,
                activated,
                muted: muted_state,
                volume: volume_state,
                failed,
                frames_written,
                finished,
                worker,
            }),
            Ok(Err(error)) => {
                stop.store(true, Ordering::Release);
                let _ = worker.join();
                Err(error)
            }
            Err(_) => {
                stop.store(true, Ordering::Release);
                if finished.load(Ordering::Acquire) {
                    let _ = worker.join();
                }
                Err("Native audio did not initialize in time.".into())
            }
        }
    }

    pub fn activate(&self) {
        self.activated.store(true, Ordering::Release);
    }

    pub fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::Release);
    }

    pub fn set_muted(&self, muted: bool) {
        self.muted.store(muted, Ordering::Release);
    }

    pub fn set_volume(&self, volume: i32) {
        self.volume.store(volume.clamp(0, 100), Ordering::Release);
    }

    pub fn has_failed(&self) -> bool {
        self.failed.load(Ordering::Acquire)
    }

    pub fn frames_written(&self) -> u64 {
        self.frames_written.load(Ordering::Acquire)
    }

    pub fn stop(self) {
        self.stop.store(true, Ordering::Release);
        for _ in 0..20 {
            if self.finished.load(Ordering::Acquire) {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        if self.finished.load(Ordering::Acquire) {
            let _ = self.worker.join();
        }
    }
}

struct AudioEvent(HANDLE);

impl Drop for AudioEvent {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

struct PcmReader {
    reader: IMFSourceReader,
    media_type: IMFMediaType,
}

enum ReadResult {
    Data(Vec<u8>),
    Empty,
    EndOfStream,
}

struct PcmQueue {
    chunks: VecDeque<Vec<u8>>,
    front_offset: usize,
    byte_count: usize,
}

impl PcmQueue {
    fn new() -> Self {
        Self {
            chunks: VecDeque::with_capacity(32),
            front_offset: 0,
            byte_count: 0,
        }
    }

    fn len(&self) -> usize {
        self.byte_count
    }

    fn push(&mut self, data: Vec<u8>) {
        if !data.is_empty() {
            self.byte_count += data.len();
            self.chunks.push_back(data);
        }
    }

    unsafe fn copy_to(&mut self, target: *mut u8, byte_count: usize) {
        debug_assert!(byte_count <= self.byte_count);
        let mut written = 0usize;
        while written < byte_count {
            let chunk = self.chunks.front().expect("PCM queue length is consistent");
            let available = chunk.len() - self.front_offset;
            let count = available.min(byte_count - written);
            std::ptr::copy_nonoverlapping(
                chunk.as_ptr().add(self.front_offset),
                target.add(written),
                count,
            );
            written += count;
            self.front_offset += count;
            self.byte_count -= count;
            if self.front_offset == chunk.len() {
                self.chunks.pop_front();
                self.front_offset = 0;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
unsafe fn run_audio_renderer(
    path: &str,
    stop: &AtomicBool,
    paused: &AtomicBool,
    activated: &AtomicBool,
    muted: &AtomicBool,
    volume: &AtomicI32,
    frames_written: &AtomicU64,
    ready: &mpsc::SyncSender<Result<(), String>>,
    playback_clock: &PlaybackClock,
) -> Result<(), String> {
    let mut pcm_reader = create_pcm_reader(path, None)?;
    let mut wave_format = std::ptr::null_mut::<WAVEFORMATEX>();
    let mut wave_size = 0u32;
    MFCreateWaveFormatExFromMFMediaType(
        &pcm_reader.media_type,
        &mut wave_format,
        Some(&mut wave_size),
        0,
    )
    .map_err(|e| format!("Could not convert PCM media type to WAVEFORMATEX: {e}"))?;
    if wave_format.is_null() {
        return Err("Media Foundation returned no PCM wave format.".into());
    }
    let block_align = (*wave_format).nBlockAlign as usize;
    if block_align == 0 {
        CoTaskMemFree(Some(wave_format.cast()));
        return Err("Decoded PCM format has zero block alignment.".into());
    }

    let enumerator: IMMDeviceEnumerator =
        CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
            .map_err(|e| format!("Could not create the Windows audio device enumerator: {e}"))?;
    let device = enumerator
        .GetDefaultAudioEndpoint(eRender, eMultimedia)
        .map_err(|e| format!("Could not open the default Windows audio output: {e}"))?;
    let client: IAudioClient = device
        .Activate(CLSCTX_ALL, None)
        .map_err(|e| format!("Could not activate WASAPI: {e}"))?;
    let initialize_result = client.Initialize(
        AUDCLNT_SHAREMODE_SHARED,
        AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
        1_000_000,
        0,
        wave_format,
        None,
    );
    CoTaskMemFree(Some(wave_format.cast()));
    initialize_result.map_err(|e| format!("Could not initialize WASAPI shared mode: {e}"))?;

    let audio_event = AudioEvent(
        CreateEventW(None, false, false, None)
            .map_err(|e| format!("Could not create the WASAPI render event: {e}"))?,
    );
    client
        .SetEventHandle(audio_event.0)
        .map_err(|e| format!("Could not attach the WASAPI render event: {e}"))?;

    let buffer_frames = client
        .GetBufferSize()
        .map_err(|e| format!("Could not query the WASAPI buffer size: {e}"))?;
    let render: IAudioRenderClient = client
        .GetService()
        .map_err(|e| format!("Could not obtain IAudioRenderClient: {e}"))?;
    let session_volume: ISimpleAudioVolume = client
        .GetService()
        .map_err(|e| format!("Could not obtain native audio session volume: {e}"))?;
    let audio_clock: IAudioClock = client
        .GetService()
        .map_err(|e| format!("Could not obtain the WASAPI device clock: {e}"))?;
    let clock_frequency = audio_clock
        .GetFrequency()
        .map_err(|e| format!("Could not query the WASAPI clock frequency: {e}"))?;
    if clock_frequency == 0 {
        return Err("WASAPI returned a zero device-clock frequency.".into());
    }
    apply_volume(&session_volume, muted, volume)?;

    let mut queue = PcmQueue::new();
    fill_pcm_queue(
        path,
        &mut pcm_reader,
        &mut queue,
        buffer_frames as usize * block_align,
    )?;
    let initial_frames = (queue.len() / block_align).min(buffer_frames as usize) as u32;
    if initial_frames == 0 {
        return Err("The MP4 audio stream produced no decoded PCM samples.".into());
    }
    let initial_bytes = initial_frames as usize * block_align;
    let target = render
        .GetBuffer(initial_frames)
        .map_err(|e| format!("Could not acquire the initial WASAPI buffer: {e}"))?;
    queue.copy_to(target, initial_bytes);
    render
        .ReleaseBuffer(initial_frames, 0)
        .map_err(|e| format!("Could not submit the initial WASAPI buffer: {e}"))?;
    frames_written.fetch_add(initial_frames as u64, Ordering::Release);
    let _ = ready.try_send(Ok(()));

    while !stop.load(Ordering::Acquire) && !activated.load(Ordering::Acquire) {
        thread::sleep(Duration::from_millis(5));
    }
    if stop.load(Ordering::Acquire) {
        return Ok(());
    }
    client
        .Start()
        .map_err(|e| format!("Could not start WASAPI playback: {e}"))?;
    let mut clock_origin = 0u64;
    audio_clock
        .GetPosition(&mut clock_origin, None)
        .map_err(|e| format!("Could not read the initial WASAPI clock: {e}"))?;
    playback_clock.activate();
    let mut is_paused = false;
    let mut applied_muted = muted.load(Ordering::Acquire);
    let mut applied_volume = volume.load(Ordering::Acquire);

    while !stop.load(Ordering::Acquire) {
        let requested_pause = paused.load(Ordering::Acquire);
        if requested_pause != is_paused {
            if requested_pause {
                client
                    .Stop()
                    .map_err(|e| format!("Could not pause WASAPI playback: {e}"))?;
            } else {
                client
                    .Start()
                    .map_err(|e| format!("Could not resume WASAPI playback: {e}"))?;
            }
            is_paused = requested_pause;
        }
        let next_muted = muted.load(Ordering::Acquire);
        let next_volume = volume.load(Ordering::Acquire);
        if next_muted != applied_muted || next_volume != applied_volume {
            apply_volume(&session_volume, muted, volume)?;
            applied_muted = next_muted;
            applied_volume = next_volume;
        }
        if is_paused {
            thread::sleep(Duration::from_millis(10));
            continue;
        }

        let wait_result = WaitForSingleObject(audio_event.0, 50);
        if wait_result == WAIT_FAILED {
            return Err("Waiting for the WASAPI render event failed.".into());
        }
        if wait_result != WAIT_OBJECT_0 && wait_result != WAIT_TIMEOUT {
            return Err(format!(
                "WASAPI returned an unexpected wait status: 0x{:08X}.",
                wait_result.0
            ));
        }

        let mut device_position = 0u64;
        audio_clock
            .GetPosition(&mut device_position, None)
            .map_err(|e| format!("Could not read the WASAPI clock: {e}"))?;
        playback_clock.update_from_device(device_position, clock_origin, clock_frequency);

        fill_pcm_queue(
            path,
            &mut pcm_reader,
            &mut queue,
            buffer_frames as usize * block_align * 2,
        )?;
        let padding = client
            .GetCurrentPadding()
            .map_err(|e| format!("Could not query WASAPI padding: {e}"))?;
        let available = buffer_frames.saturating_sub(padding) as usize;
        let writable_frames = available.min(queue.len() / block_align) as u32;
        if writable_frames != 0 {
            let byte_count = writable_frames as usize * block_align;
            let target = render
                .GetBuffer(writable_frames)
                .map_err(|e| format!("Could not acquire a WASAPI render buffer: {e}"))?;
            queue.copy_to(target, byte_count);
            render
                .ReleaseBuffer(writable_frames, 0)
                .map_err(|e| format!("Could not submit a WASAPI render buffer: {e}"))?;
            frames_written.fetch_add(writable_frames as u64, Ordering::Release);
        }
    }
    let _ = client.Stop();
    playback_clock.deactivate();
    Ok(())
}

unsafe fn apply_volume(
    session: &ISimpleAudioVolume,
    muted: &AtomicBool,
    volume: &AtomicI32,
) -> Result<(), String> {
    session
        .SetMasterVolume(
            volume.load(Ordering::Acquire).clamp(0, 100) as f32 / 100.0,
            std::ptr::null(),
        )
        .map_err(|e| format!("Could not set native audio volume: {e}"))?;
    session
        .SetMute(muted.load(Ordering::Acquire), std::ptr::null())
        .map_err(|e| format!("Could not set native audio mute: {e}"))
}

unsafe fn create_pcm_reader(
    path: &str,
    desired_type: Option<&IMFMediaType>,
) -> Result<PcmReader, String> {
    let source_path = HSTRING::from(path);
    let reader = MFCreateSourceReaderFromURL(
        PCWSTR(source_path.as_ptr()),
        None::<&windows::Win32::Media::MediaFoundation::IMFAttributes>,
    )
    .map_err(|e| format!("Could not open the MP4 audio stream: {e}"))?;
    reader
        .SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false)
        .map_err(|e| format!("Could not deselect unused MP4 streams: {e}"))?;
    reader
        .SetStreamSelection(MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32, true)
        .map_err(|e| format!("MP4 has no selectable audio stream: {e}"))?;

    let media_type = if let Some(desired) = desired_type {
        desired.clone()
    } else {
        let partial = MFCreateMediaType().map_err(|e| format!("PCM media type: {e}"))?;
        partial
            .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)
            .map_err(|e| format!("PCM major type: {e}"))?;
        partial
            .SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM)
            .map_err(|e| format!("PCM subtype: {e}"))?;
        partial
    };
    reader
        .SetCurrentMediaType(
            MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32,
            None,
            &media_type,
        )
        .map_err(|e| format!("Could not configure the MP4 audio decoder for PCM: {e}"))?;
    let actual = reader
        .GetCurrentMediaType(MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32)
        .map_err(|e| format!("Could not query the decoded PCM format: {e}"))?;
    Ok(PcmReader {
        reader,
        media_type: actual,
    })
}

unsafe fn fill_pcm_queue(
    path: &str,
    reader: &mut PcmReader,
    queue: &mut PcmQueue,
    target_bytes: usize,
) -> Result<(), String> {
    for _ in 0..256 {
        if queue.len() >= target_bytes {
            break;
        }
        match read_pcm_sample(&reader.reader)? {
            ReadResult::Data(bytes) => queue.push(bytes),
            ReadResult::Empty => {}
            ReadResult::EndOfStream => {
                *reader = create_pcm_reader(path, Some(&reader.media_type))?;
            }
        }
    }
    Ok(())
}

unsafe fn read_pcm_sample(reader: &IMFSourceReader) -> Result<ReadResult, String> {
    let mut flags = 0u32;
    let mut sample = None;
    reader
        .ReadSample(
            MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32,
            0,
            None,
            Some(&mut flags),
            None,
            Some(&mut sample),
        )
        .map_err(|e| format!("Could not decode the next MP4 audio sample: {e}"))?;
    if flags & MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED.0 as u32 != 0 {
        return Err("The MP4 audio format changed during playback.".into());
    }
    if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
        return Ok(ReadResult::EndOfStream);
    }
    let Some(sample) = sample else {
        return Ok(ReadResult::Empty);
    };
    let buffer = sample
        .ConvertToContiguousBuffer()
        .map_err(|e| format!("Could not create a contiguous PCM buffer: {e}"))?;
    let mut data = std::ptr::null_mut();
    let mut length = 0u32;
    buffer
        .Lock(&mut data, None, Some(&mut length))
        .map_err(|e| format!("Could not lock the decoded PCM buffer: {e}"))?;
    let bytes = if data.is_null() || length == 0 {
        Vec::new()
    } else {
        std::slice::from_raw_parts(data, length as usize).to_vec()
    };
    buffer
        .Unlock()
        .map_err(|e| format!("Could not unlock the decoded PCM buffer: {e}"))?;
    if bytes.is_empty() {
        Ok(ReadResult::Empty)
    } else {
        Ok(ReadResult::Data(bytes))
    }
}
