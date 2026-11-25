// This computer's clipboard: text or a PNG picture, read and written with
// the async Clipboard API (HTTPS or localhost pages only). A copy offering
// text besides a picture is text, as on the server.

/** What a clipboard holds: text, or a PNG picture. */
export type Clip = { text: string } | { png: Blob };

/** Tells clipboard contents apart: the text, or a hash of the picture's file. */
export async function clipKey(clip: Clip): Promise<string> {
  if ('text' in clip) return `text:${clip.text}`;
  const hash = new Uint8Array(await crypto.subtle.digest('SHA-256', await clip.png.arrayBuffer()));
  return `png:${Array.from(hash, (b) => b.toString(16).padStart(2, '0')).join('')}`;
}

/** This computer's clipboard: its text, else its PNG picture (empty text:
 * neither). Text only where the browser reads nothing else (`read()`
 * missing, or failing for another reason than a refusal). Rejects when the
 * browser refuses. */
export async function readClip(): Promise<Clip> {
  if (typeof navigator.clipboard.read === 'function') {
    try {
      for (const item of await navigator.clipboard.read()) {
        if (item.types.includes('text/plain')) return { text: await (await item.getType('text/plain')).text() };
        if (item.types.includes('image/png')) return { png: await item.getType('image/png') };
      }
      return { text: '' };
    } catch (e) {
      if (e instanceof DOMException && e.name === 'NotAllowedError') throw e;
    }
  }
  return { text: await navigator.clipboard.readText() };
}

/** This computer's clipboard, read only where the browser already allows
 * it (no permission prompt out of the blue); null otherwise. */
export async function readQuietly(): Promise<Clip | null> {
  if (!window.isSecureContext) return null;
  try {
    const permission = await navigator.permissions.query({ name: 'clipboard-read' as PermissionName });
    if (permission.state !== 'granted') return null;
    return await readClip();
  } catch {
    // Firefox and Safari know no such permission.
    return null;
  }
}

/** Puts `clip` on this computer's clipboard. */
export async function writeClip(clip: Clip): Promise<void> {
  if ('text' in clip) return navigator.clipboard.writeText(clip.text);
  await navigator.clipboard.write([new ClipboardItem({ 'image/png': clip.png })]);
}
