# Virtual microphone: the client's microphone as a microphone on the server

- Status: under discussion
- Created: 2026-11-10

## Goal

Sound also in the other direction. What the user says into the client
device's microphone is captured in the browser, sent to the server, and
appears there as the input of a virtual microphone, so that programs on the
remote desktop (calls, recorders, dictation) hear it. It has to work on
Linux and on Windows.

## Open questions

1. **Windows: where does the virtual cable come from?** (see Windows below)
   - a) use one that is already installed, and say what to install when
     there is none;
   - b) also offer to install an open-source, already signed virtual audio
     driver from `remothing service install` (once, as administrator);
   - c) write and sign our own driver.
   Recommended: a) and b) together. c) is not worth it.
2. **Several clients with the microphone on**: one talker at a time (the
   last one to switch it on), or mix them? Recommended: one at a time.
3. **Transport**: WebSocket like the sound going the other way, or a WebRTC
   audio track? Recommended: WebSocket first (see Alternatives).
4. **Server switch**: a config key to forbid the microphone altogether
   (`input.microphone`), default on or off?
5. The user said they have ideas of their own about how to do this and held
   them back so as not to bias the proposal. Compare them with this before
   agreeing on anything.

## What exists today (the other direction)

Server sound to the client is done and is the model to mirror; see "Sound"
in `docs/ARCHITECTURE.md`.

- `crates/core/src/audio.rs`: one capture thread while somebody listens, one
  encoder for all listeners, packets on each client's WebSocket
  (`S_AUDIO_INFO`, `S_AUDIO`; the client asks with `C_AUDIO`).
- `AudioSource` trait in `crates/core/src/platform.rs` (48 kHz stereo
  16-bit), `AudioEncoder` in `crates/core/src/encode.rs` (libopus, AAC
  fallback) implemented in `crates/encoder/src/audio.rs`.
- Windows: WASAPI loopback (`crates/platform-windows/src/audio.rs`).
- Linux: helper programs writing samples to a pipe (`parec`, `pw-record`,
  `arecord`), and our own PulseAudio (or PipeWire) where no sound server
  runs (`crates/platform-linux/src/audio.rs`).
- Client: `client/src/audio.ts` (WebCodecs `AudioDecoder`, pieces scheduled
  on an `AudioContext`), `Sound` button in `client/src/ui.ts`.
- Everything is best effort with a `tried` list that reaches the server log,
  the admin status and the client. The microphone should behave the same.

## Proposed design

Reuse the sound path backwards. Only one part is hard: Windows has no way to
create a microphone without a driver.

### Client

- A `Mic` button, off by default. Switching it on asks the browser for the
  microphone (`getUserMedia`; needs HTTPS or localhost and the user's
  permission) with echo cancellation, noise suppression and automatic gain
  on. Echo cancellation matters: the remote sound plays in the same page.
- Encode with WebCodecs `AudioEncoder`: Opus, mono, 32-48 kbit/s, 20 ms
  packets. Send on the same WebSocket with two new messages: format / stop
  (`C_MIC_INFO`, JSON) and packet (`C_MIC`).
- Browser without `AudioEncoder`: degrade to 16 kHz 16-bit PCM from an
  `AudioWorklet` (256 kbit/s) and say so in the button's tooltip.
- The button shows the state the server reports (active, or unavailable and
  why), like `Sound` does.

### Server, common part

- An `AudioSink` trait per platform, the mirror of `AudioSource`:
  `Platform::open_microphone(tried) -> Box<dyn AudioSink>` and
  `write(&[i16])`, with the same `tried` reporting.
- Decode with FFmpeg's Opus decoder. It is FFmpeg's own (not libopus), so
  the builds need no change.
- A small jitter buffer, 40-60 ms: silence where a packet is missing, drop
  what arrives too late. Feed the sink at a steady rate.
- A status message to the client (`S_MIC_INFO`), and the config switch of
  open question 4.
- The sink is opened when a client switches its microphone on and closed
  when nobody talks.

### Linux

- Create a null sink `remothing-mic` and a virtual source over its monitor
  (`module-remap-source`), set as the default source. The remapped source is
  needed because some programs hide monitor sources from their device list.
- Feed it with a helper program (`pacat` into the null sink, or `pw-cat -p`
  on plain PipeWire), the same way capture uses `parec`.
- In our own sound server these are two more modules at start. In somebody
  else's server they are loaded with `pactl load-module` when the first
  client talks and unloaded on exit.
- ALSA only: the second end of an `snd-aloop` card, best effort.
- With PipeWire the same `pactl` commands work through pipewire-pulse; a
  native alternative is a node with `media.class=Audio/Source/Virtual`.

### Windows

There is no user-mode API to create a capture device. The server would play
into the "speaker" side of a virtual cable and programs would record from
its "microphone" side. Playing there with WASAPI (shared mode, render
client) is code much like the existing loopback capture. What has to be
decided is where the cable comes from:

| option | for | cost |
|---|---|---|
| a) Detect one already installed (VB-CABLE, Virtual Audio Cable, the Steam Streaming Microphone that Steam Remote Play installs) | nothing to install when it is there | depends on third-party software |
| b) Install an open-source, already signed virtual audio driver from `remothing service install` | works on any machine; once, as administrator | choose and audit the driver; its licence |
| c) Our own driver from Microsoft's SYSVAD sample | full control | EV certificate and Microsoft attestation signing; weeks |

- Without a cable the client sees "microphone unavailable: install ...", as
  with sound.
- Echo is not a problem by itself: the sound capture listens to the default
  playback device and the cable is another one. It becomes one if somebody
  makes the cable the default playback device; that can be detected and
  reported.
- Optionally make the cable's capture side the default recording device
  (only through the undocumented `IPolicyConfig`), or tell the user to pick
  it in the program they use.

## Latency

Estimated 100-150 ms from mouth to the remote program: capture 10-20 ms, one
20 ms packet, network, 40-60 ms jitter buffer, 10-20 ms device buffer.
Enough for calls. Not measured: nothing of this exists yet.

## Alternatives considered

- **WebRTC audio track** instead of WebCodecs over the WebSocket: the
  browser brings Opus, forward error correction, loss concealment and its
  own jitter handling, and it travels over UDP. It needs RTP media in the
  server's WebRTC side (`rtc.rs` uses str0m for data channels only today)
  plus depacketizing and decoding. Worth it later if the WebSocket's TCP
  gives dropouts on lossy links.
- **Raw PCM only**: simplest, but 768 kbit/s at 48 kHz mono; too much for a
  phone link next to the video. Kept only as the fallback at 16 kHz.
- **RDP-style audio input redirection** on Windows: only exists inside RDP
  sessions, not for the console session we stream.

## Testing

- Linux, in the container: play a tone into the client side (a synthetic
  `MediaStream` in headless Chromium, or Chromium's fake audio capture
  device), record from the virtual source with `parec`, compare length,
  pitch and level with what was sent. The sound tests of
  `docs/RESULTS.md` did the same in the other direction.
- Windows: record from the cable's microphone side with WASAPI and compare.
  Mind `AGENTS.md`: the user may be on this PC; no audible sound, private
  instance.
- Degraded cases: no cable on Windows, no sound server on Linux, browser
  without `AudioEncoder`, plain HTTP (no microphone permission).

## Not verified

Everything about Windows drivers and about the third-party cables comes from
general knowledge, not from trying them here: which open-source signed
driver to use, whether the Steam microphone can be fed by another program,
and how `IPolicyConfig` behaves on current Windows 11 all need checking
before option b) is promised.
