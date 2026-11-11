# Native NVENC: fewer keyframes

- Status: under discussion (parked: the user may want it later, not now)
- Created: 2026-11-11

## Goal

Drive NVENC through NVIDIA's SDK directly instead of through FFmpeg, for
what FFmpeg does not let us do without restarting the encoder. The gain is
not a faster encode on average but fewer spikes: fewer keyframes, so fewer
stutters when a client connects, when the link changes and after a loss.

## Open questions

1. Whether to do it at all, and when (the user's call; parked on
   2026-11-11).
2. Measure first how much today's keyframes cost in real use (the event log
   lists every keyframe with its size; task 002's late-frame reports say
   when a frame was late): if they rarely matter, this is not worth it.

## Why (what it would change)

Today, through FFmpeg:

- **Every bitrate cap change is a keyframe.** FFmpeg's NVENC wrapper
  resets the encoder when the rate changes. The link model changes the cap
  2-3 times in the first second of every connection (slow start) and again
  whenever the estimate moves clearly (3 s hold, dead band). Measured on a
  50 Mbit/s profile: 69 KB, then 143 KB, then 387 KB keyframes; 387 KB is
  about 60 ms of that link.
- **An unrepaired loss is a keyframe.** Over WebTransport a frame whose
  pieces cannot be rebuilt or sent again is given up, and the client asks
  for a keyframe: a large frame exactly when the network is already in
  trouble.
- **Another encoder setup is another encoder.** A new encoded size (motion
  mode) or switching two-pass on or off for H.264 opens a second NVENC
  (about 250 ms, done beside the current one) and starts with a keyframe.

With the SDK:

1. **Bitrate change without a keyframe**: `nvEncReconfigureEncoder` with
   the new rate. That the SDK accepts it without a reset is NVIDIA's
   documentation; not tried.
2. **Reference frame invalidation**: `nvEncInvalidateRefFrames` for the
   lost frames; the next P-frame predicts from an older good reference.
   Sunshine keeps 5 references for that and falls back to a keyframe only
   when the loss is older than its buffer (`src/nvenc/nvenc_base.cpp`
   ~284-299, 799-834 at Sunshine `ec2676b`).
3. **Reconfiguration instead of a new encoder** where the SDK allows it
   (rate control, possibly multipass; a new size most likely still needs a
   keyframe).
4. Sunshine also has intra refresh instead of keyframes on request and a
   bounded asynchronous wait (100 ms) for the encoder.

## Costs and risks

- Our own SDK wrapper: unsafe FFI, two input paths (D3D11 textures on
  Windows, CUDA on Linux), three codecs (H.264, HEVC, AV1), split-frame
  and two-pass settings that FFmpeg handles today. Sunshine's is several
  thousand lines.
- Two encoding paths: AMF, QuickSync and x264 stay on FFmpeg.
- Following SDK versions against installed drivers (FFmpeg does it now).
- NVIDIA only.

## Testing

- Linux bench container: count keyframes and their sizes during slow start
  and on the shaped profiles of `docs/RESULTS.md`, with the 2 % loss
  WebTransport profile for invalidation; compare p99 and max of
  capture→draw before and after.
- Windows: `examples/d3d11_bench` for encode time per codec.
