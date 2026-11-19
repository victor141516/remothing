// Files from this computer to the remote one: dropped anywhere on the page,
// or pasted where the picture does not have the keyboard (after a click on
// the menu, say: in the picture Ctrl+V is the remote side's paste). They are
// saved in the Downloads folder of the remote session's user (files.rs), one
// at a time, with a note saying how far it got.

/** What the server answers for a saved file. */
interface Saved {
  name: string;
  dir: string;
}

export class Uploads {
  private readonly queue: File[] = [];
  private busy = false;
  private readonly overlay = document.createElement('div');
  /** dragenter/dragleave pairs: the overlay goes when the drag leaves the window. */
  private depth = 0;

  /** `url`: `files?token=…`; `note`: a line for the user, `sticky` until the next one. */
  constructor(
    private readonly url: string,
    private readonly note: (text: string, sticky: boolean) => void,
  ) {
    this.overlay.id = 'drop';
    this.overlay.hidden = true;
    this.overlay.textContent = 'Drop to save in Downloads on the remote computer';
    document.body.append(this.overlay);
    window.addEventListener('dragenter', (e) => {
      if (!carriesFiles(e)) return;
      e.preventDefault();
      this.depth++;
      this.overlay.hidden = false;
    });
    window.addEventListener('dragover', (e) => {
      if (!carriesFiles(e)) return;
      e.preventDefault();
      e.dataTransfer!.dropEffect = 'copy';
    });
    window.addEventListener('dragleave', (e) => {
      if (!carriesFiles(e)) return;
      this.depth = Math.max(0, this.depth - 1);
      if (this.depth === 0) this.overlay.hidden = true;
    });
    window.addEventListener('drop', (e) => {
      if (!carriesFiles(e)) return;
      e.preventDefault();
      this.depth = 0;
      this.overlay.hidden = true;
      this.add(e.dataTransfer!);
    });
    // Files pasted anywhere, a field of the menu included (text pastes stay theirs).
    document.addEventListener('paste', (e) => {
      if (!e.clipboardData?.files.length) return;
      e.preventDefault();
      this.add(e.clipboardData);
    });
  }

  private add(data: DataTransfer): void {
    // A folder arrives as a file that cannot be read: left out, and said so.
    const items = Array.from(data.items).filter((item) => item.kind === 'file');
    const folders = items.filter((item) => item.webkitGetAsEntry()?.isDirectory).length;
    const files = Array.from(data.files).filter((_, i) => !items[i]?.webkitGetAsEntry()?.isDirectory);
    if (folders > 0) this.note(`Folders are not sent (${folders} left out)`, false);
    this.queue.push(...files);
    if (!this.busy) this.next();
  }

  private next(): void {
    const file = this.queue.shift();
    if (file === undefined) {
      this.busy = false;
      return;
    }
    this.busy = true;
    const more = this.queue.length ? ` (${this.queue.length} more)` : '';
    const request = new XMLHttpRequest();
    request.open('POST', `${this.url}&name=${encodeURIComponent(file.name)}`);
    this.note(`Sending ${file.name}${more}…`, true);
    request.upload.onprogress = (e) => {
      if (e.lengthComputable) this.note(`Sending ${file.name}${more} · ${Math.floor((100 * e.loaded) / e.total)}%`, true);
    };
    request.onload = () => {
      if (request.status === 200) {
        const saved = JSON.parse(request.responseText) as Saved;
        this.note(`Saved in ${saved.dir}: ${saved.name}`, false);
      } else {
        this.note(`${file.name} was not saved: ${request.responseText || request.status}`, false);
      }
      this.next();
    };
    request.onerror = () => {
      this.note(`${file.name} was not saved: the connection broke off`, false);
      this.next();
    };
    request.send(file);
  }
}

function carriesFiles(e: DragEvent): boolean {
  return e.dataTransfer?.types.includes('Files') ?? false;
}
