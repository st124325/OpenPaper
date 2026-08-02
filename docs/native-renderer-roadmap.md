# Native MP4 renderer roadmap

OpenPaper is moving from libVLC window playback to an in-process Windows
Media Foundation and D3D11 renderer without disrupting GIF and WEBP support.

1. **Foundation (complete):** create a D3D11 device with video support and an
   `IMFDXGIDeviceManager`; expose a capability probe.
2. **Decode and swap chain (complete preflight):** create an `IMFSourceReader`
   for MP4, request NV12 GPU samples using `MF_SOURCE_READER_D3D_MANAGER` and
   DXVA, decode one frame, and create a flip-model DXGI swap chain for
   `WallpaperCoreHost`.
3. **Present (next):** retain those objects for playback, convert decoded NV12
   textures on the GPU and present at the source frame rate.
4. **Policy:** pause decode when full-screen apps run, apply the existing
   performance modes, handle device loss and retain libVLC as a safe fallback.
5. **Migration:** route supported H.264/HEVC MP4 files to the native renderer;
   GIF, WEBP and unsupported codecs remain on libVLC until dedicated paths exist.
