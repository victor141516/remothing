// This device's microphone as the remote session's microphone. The browser
// records it (with its echo cancellation: the remote sound plays on the same
// page), an AudioWorklet hands over the samples, and they go to the server
// as Opus packets, or as plain 16 kHz samples where the browser cannot
// encode. The server gives the microphone to one client at a time and
// tells each one how it stands (S_MIC_INFO).

import { C_MIC } from './proto';

/** S_MIC_INFO. */
interface MicInfo {
  state: 'free' | 'busy' | 'active' | 'install' | 'installing' | 'restart' | 'unavailable';
  /** Where the server plays it. */
  sink?: string;
  /** What is missing on the server, when it can install it. */
  component?: { name: string; notice: string; restart: boolean };
  reason?: string;
  tried?: string[];
}

export interface MicActions {
  /** C_MIC_INFO: a request or an answer about this client's microphone. */
  tell(message: Record<string, unknown>): void;
  /** A complete C_MIC message. */
  send(message: Uint8Array): void;
  /** For the server's event log. */
  log(text: string): void;
  /** A yes/no question to the user. */
  ask(question: string): Promise<boolean>;
  /** `state` or `status` changed. */
  changed(): void;
}

/** One packet carries this much sound. */
const PACKET_MS = 20;
const OPUS: AudioEncoderConfig = { codec: 'opus', sampleRate: 48000, numberOfChannels: 1, bitrate: 32000 };
/** Sample rate of the plain fallback: enough for speech at a sixth of the bytes. */
const PLAIN_RATE = 16000;

/** Runs on the audio thread: passes each block of samples on. */
const WORKLET = `registerProcessor('remothing-mic', class extends AudioWorkletProcessor {
  process(inputs) {
    const samples = inputs[0][0];
    if (samples) this.port.postMessage(samples.slice());
    return true;
  }
});`;

export class Microphone {
  /** Browsers only give the microphone to secure pages. */
  readonly available = navigator.mediaDevices?.getUserMedia !== undefined;
  /** off: free to switch on; pending: asked; on: the server plays it; taken: another client talks. */
  state: 'off' | 'pending' | 'on' | 'taken' = 'off';
  /** For the button's tooltip. */
  status = 'off';
  private stream: MediaStream | null = null;
  private context: AudioContext | null = null;
  private encoder: AudioEncoder | null = null;
  /** Samples of the packet being filled. */
  private packet = new Float32Array(0);
  private filled = 0;
  private packets = 0;

  constructor(private readonly actions: MicActions) {}

  /** The user's switch. */
  async toggle(): Promise<void> {
    if (this.state === 'taken') return;
    if (this.state !== 'off') {
      this.actions.tell({ on: false });
      this.stop('off', 'off');
      return;
    }
    this.set('pending', 'waiting for the browser');
    try {
      const opus = typeof AudioEncoder !== 'undefined' && (await AudioEncoder.isConfigSupported(OPUS)).supported === true;
      const rate = opus ? OPUS.sampleRate : PLAIN_RATE;
      this.stream = await navigator.mediaDevices.getUserMedia({
        audio: { channelCount: 1, echoCancellation: true, noiseSuppression: true, autoGainControl: true },
      });
      // The context resamples the microphone to the rate we send.
      this.context = new AudioContext({ sampleRate: rate });
      const worklet = URL.createObjectURL(new Blob([WORKLET], { type: 'text/javascript' }));
      await this.context.audioWorklet.addModule(worklet);
      URL.revokeObjectURL(worklet);
      const node = new AudioWorkletNode(this.context, 'remothing-mic', { numberOfOutputs: 0, channelCount: 1, channelCountMode: 'explicit' });
      node.port.onmessage = (e: MessageEvent<Float32Array>) => this.samples(e.data);
      this.context.createMediaStreamSource(this.stream).connect(node);
      this.packet = new Float32Array((rate * PACKET_MS) / 1000);
      this.filled = 0;
      if (opus) {
        this.encoder = new AudioEncoder({
          output: (chunk) => {
            const message = new Uint8Array(1 + chunk.byteLength);
            message[0] = C_MIC;
            chunk.copyTo(message.subarray(1));
            this.actions.send(message);
          },
          error: (e) => {
            this.actions.tell({ on: false });
            this.stop('off', `stopped: encoder error (${e.message})`);
          },
        });
        this.encoder.configure(OPUS);
      }
      this.set('pending', 'asked the server');
      this.actions.tell({ on: true, codec: opus ? 'opus' : 'pcm', rate, channels: 1 });
    } catch (e) {
      this.stop('off', `unavailable: the browser gave no microphone (${e instanceof Error ? e.message : String(e)})`);
    }
  }

  /** How the server sees this client's microphone. */
  info(json: string): void {
    const info = JSON.parse(json) as MicInfo;
    switch (info.state) {
      case 'free':
        // Also the answer to a connection that was lost while talking.
        this.stop('off', 'off');
        break;
      case 'busy':
        this.stop('taken', 'another client is talking');
        break;
      case 'active':
        this.set('on', `on: ${info.sink ?? 'the server'}`);
        break;
      case 'install':
        void this.offerInstall(info.component);
        break;
      case 'installing':
        this.set('pending', 'installing on the server (it may be asking for permission on its own screen)');
        break;
      case 'restart':
        void this.offerRestart();
        break;
      case 'unavailable': {
        const tried = info.tried?.length ? ` (tried: ${info.tried.join('; ')})` : '';
        this.stop('off', `unavailable: ${info.reason ?? 'no reason given'}${tried}`);
        break;
      }
    }
  }

  private async offerInstall(component: MicInfo['component']): Promise<void> {
    const name = component?.name ?? 'a component';
    this.set('pending', `needs ${name} on the server`);
    const restart = component?.restart ? ' The computer may have to restart afterwards.' : '';
    const yes = await this.actions.ask(
      `The microphone needs ${name} installed on the server. ${component?.notice ?? ''} ` +
        `The server's system will ask for administrator permission on its own screen.${restart} Install it now?`,
    );
    if (this.state !== 'pending') return;
    if (yes) {
      this.actions.tell({ install: true });
    } else {
      this.actions.tell({ on: false });
      this.stop('off', `off: needs ${name} on the server`);
    }
  }

  private async offerRestart(): Promise<void> {
    this.set('pending', 'the server computer has to restart');
    const yes = await this.actions.ask(
      'What the microphone needs is installed, but it only works after the server computer restarts. ' +
        'Restart it now? The stream comes back once remothing runs there again.',
    );
    if (this.state !== 'pending') return;
    if (yes) this.actions.tell({ restart: true });
    this.actions.tell({ on: false });
    this.stop('off', yes ? 'off: the server computer is restarting' : 'off: the server computer has to restart first');
  }

  /** A block of samples from the worklet: packets go out as they fill. */
  private samples(block: Float32Array): void {
    if (this.state !== 'on') return;
    for (let at = 0; at < block.length; ) {
      const part = block.subarray(at, at + this.packet.length - this.filled);
      this.packet.set(part, this.filled);
      this.filled += part.length;
      at += part.length;
      if (this.filled < this.packet.length) break;
      this.filled = 0;
      if (this.encoder) {
        const data = new AudioData({
          format: 'f32',
          sampleRate: OPUS.sampleRate,
          numberOfFrames: this.packet.length,
          numberOfChannels: 1,
          timestamp: this.packets++ * PACKET_MS * 1000,
          data: this.packet,
        });
        this.encoder.encode(data);
        data.close();
      } else {
        const message = new Uint8Array(1 + 2 * this.packet.length);
        message[0] = C_MIC;
        const view = new DataView(message.buffer);
        this.packet.forEach((s, i) => view.setInt16(1 + 2 * i, Math.max(-1, Math.min(1, s)) * 32767, true));
        this.actions.send(message);
      }
    }
  }

  /** Lets go of the device's microphone. */
  private stop(state: Microphone['state'], status: string): void {
    this.stream?.getTracks().forEach((track) => track.stop());
    this.stream = null;
    void this.context?.close();
    this.context = null;
    if (this.encoder?.state !== 'closed') this.encoder?.close();
    this.encoder = null;
    this.set(state, status);
  }

  private set(state: Microphone['state'], status: string): void {
    if (state === this.state && status === this.status) return;
    // Other clients taking and leaving the microphone is not this one's news.
    const own = [this.state, state].some((s) => s === 'pending' || s === 'on');
    this.state = state;
    this.status = status;
    if (own) {
      console.info(`microphone ${status}`);
      this.actions.log(`microphone ${status}`);
    }
    this.actions.changed();
  }
}
