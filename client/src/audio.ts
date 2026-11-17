// Sound of the remote session. Once asked, the server sends Opus (or AAC)
// packets on the WebSocket; WebCodecs decodes them and an AudioContext
// plays each piece right after the previous one, a little behind its
// arrival so that uneven delivery leaves no gaps. Browsers only let a page
// play after a click or a key, so the sound starts with the first one. Of
// several windows only the newest starts it.

/** S_AUDIO_INFO. */
interface StreamInfo {
  active: boolean;
  /** As WebCodecs names it. */
  codec?: string;
  rate?: number;
  channels?: number;
  /** Decoder configuration, hex (AAC). */
  description?: string;
  /** Where the server captures the sound. */
  source?: string;
  reason?: string;
  /** Every capture path and encoder the server tried. */
  tried?: string[];
}

export interface SoundActions {
  /** Asks the server for the sound, or to stop sending it. */
  request(on: boolean): void;
  /** For the server's event log. */
  log(text: string): void;
  /** `status` or `playing` changed. */
  changed(): void;
}

/** How far ahead of the clock sound is queued when it starts or the queue ran dry: room for late packets. */
const LEAD_S = 0.06;
/** More queued than this came in a burst after a stall: playing it all would stay that late for good. */
const MAX_AHEAD_S = 0.25;
/** Nominal packet spacing for the decoder's timestamps, µs (it only needs them to grow). */
const PACKET_US = 10_000;

export class Sound {
  /** Whether sound plays or why not, for the overlay and the button. */
  status = 'waits for a click or a key (browsers only play sound after one)';
  /** The server's sound is being decoded and played. */
  playing = false;
  private context: AudioContext | null = null;
  private decoder: AudioDecoder | null = null;
  private wanted = true;
  /** Context time at which the next piece starts. */
  private nextTime = 0;
  private packets = 0;

  constructor(private readonly actions: SoundActions) {
    const types = ['pointerdown', 'pointerup', 'keydown'] as const;
    const unlock = async () => {
      // One context at the stream's rate: pieces resampled one by one would click at their seams.
      this.context ??= new AudioContext({ sampleRate: 48000, latencyHint: 'interactive' });
      await this.context.resume().catch(() => {});
      if (this.context.state !== 'running') return;
      for (const type of types) window.removeEventListener(type, unlock, true);
      if (this.wanted) this.ask();
    };
    for (const type of types) window.addEventListener(type, unlock, true);
    // With several windows open the newest one plays the sound; the older
    // ones go quiet, and their Sound button turns it on there too.
    const windows = new BroadcastChannel('remothing-sound');
    windows.onmessage = () => {
      if (this.wanted) this.quiet();
    };
    windows.postMessage('opened');
  }

  /** Another window took the sound. */
  private quiet(): void {
    this.wanted = false;
    if (this.context?.state === 'running') this.actions.request(false);
    this.stop('off: a newer window plays it (Sound turns it on here too)');
  }

  /** The user's switch. */
  toggle(): void {
    this.wanted = !this.wanted;
    if (this.context?.state !== 'running') return;
    if (this.wanted) {
      this.ask();
    } else {
      this.actions.request(false);
      this.stop('off');
    }
  }

  /** The server describes its sound stream, or tells why there is none. */
  info(json: string): void {
    const info = JSON.parse(json) as StreamInfo;
    this.decoder?.close();
    this.decoder = null;
    if (!info.active || !info.codec) {
      const tried = info.tried?.length ? ` (tried: ${info.tried.join('; ')})` : '';
      this.stop(`unavailable: ${info.reason ?? 'no reason given'}${tried}`);
      return;
    }
    if (typeof AudioDecoder === 'undefined') {
      this.actions.request(false);
      this.stop('unavailable: this browser cannot decode it here (WebCodecs needs HTTPS or localhost)');
      return;
    }
    const config: AudioDecoderConfig = { codec: info.codec, sampleRate: info.rate ?? 48000, numberOfChannels: info.channels ?? 2 };
    const hex = info.description?.match(/../g);
    if (hex) config.description = Uint8Array.from(hex, (byte) => parseInt(byte, 16));
    const decoder = new AudioDecoder({
      output: (data) => this.play(data),
      error: (e) => {
        this.actions.request(false);
        this.stop(`stopped: decoder error (${e.message})`);
      },
    });
    this.decoder = decoder;
    void AudioDecoder.isConfigSupported(config).then(({ supported }) => {
      if (this.decoder !== decoder) return;
      if (!supported) {
        this.actions.request(false);
        this.stop(`unavailable: this browser does not decode ${info.codec}`);
        return;
      }
      decoder.configure(config);
      this.playing = true;
      this.set(`on: ${info.codec} from ${info.source ?? 'the server'}`);
    });
  }

  /** One encoded packet (S_AUDIO: tag byte, then the packet). */
  packet(buf: ArrayBuffer): void {
    if (this.decoder?.state !== 'configured') return;
    this.decoder.decode(new EncodedAudioChunk({ type: 'key', timestamp: this.packets++ * PACKET_US, data: new Uint8Array(buf, 1) }));
  }

  private ask(): void {
    this.set('asked the server');
    this.actions.request(true);
  }

  private stop(status: string): void {
    if (this.decoder?.state !== 'closed') this.decoder?.close();
    this.decoder = null;
    this.playing = false;
    this.nextTime = 0;
    this.set(status);
  }

  private set(status: string): void {
    if (status === this.status) return;
    this.status = status;
    console.info(`sound ${status}`);
    this.actions.log(`sound ${status}`);
    this.actions.changed();
  }

  private play(data: AudioData): void {
    const context = this.context;
    if (!context || !this.playing) {
      data.close();
      return;
    }
    const piece = context.createBuffer(data.numberOfChannels, data.numberOfFrames, data.sampleRate);
    for (let channel = 0; channel < data.numberOfChannels; channel++) {
      data.copyTo(piece.getChannelData(channel), { planeIndex: channel, format: 'f32-planar' });
    }
    data.close();
    const now = context.currentTime;
    if (this.nextTime < now) this.nextTime = now + LEAD_S;
    else if (this.nextTime > now + MAX_AHEAD_S) return;
    const source = context.createBufferSource();
    source.buffer = piece;
    source.connect(context.destination);
    source.start(this.nextTime);
    this.nextTime += piece.duration;
  }
}
