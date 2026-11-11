# AGENTS.md

remothing streams a desktop (or one application) to a browser with low
latency: static areas pixel-exact, moving areas as H.264, plus sound, input
and clipboard. One native server binary per platform (Linux X11, Windows)
with the TypeScript client embedded in it. It uses no existing streaming
software; codec and compression libraries are fine.

This file holds only what the other documents do not. Read the relevant one
before changing a subsystem:

- `docs/ARCHITECTURE.md`: how each part works and why it is that way.
- `docs/RESULTS.md`: measurements, profiles, and how they were taken.
- `docs/REQUIREMENTS.md`: requirement → implementation → status.
- `README.md`: build, run, configuration, client usage.

## Working with the user

- The user writes in Spanish: answer in Spanish. Code, comments, commit
  messages, this file and task files are in English.
- The Windows demo streams this PC's real desktop, and the user is often
  working on it remotely while you run. Never type or click into the live
  session, leave the clipboard as you found it, play nothing audible. Test
  intrusive things on a private instance (other ports, e.g. 9180/9181, own
  config under `.tools/`) with inert input, or in the Linux container.
- Say what was measured, what was only reasoned, and what was not verified.

## Layout

| path | what |
|---|---|
| `crates/core` | Everything platform-independent: provider traits (`platform.rs`, `encode.rs`), tiles and policy (`shared.rs`, `pixels.rs`), capture loop (`capture.rs`), per-client link model (`pacer.rs`), sessions and transports (`stream.rs`, `rtc.rs`), sound hub (`audio.rs`), wire protocol (`proto.rs`), admin API, config, TLS |
| `crates/encoder` | FFmpeg: H.264 (NVENC, AMF, QuickSync, x264), Opus / AAC |
| `crates/platform-linux` | Xvfb and X11 capture, XTest input, X selections, sound helpers. Whole crate is `cfg(target_os = "linux")` |
| `crates/platform-windows` | DXGI duplication, SendInput, clipboard, WASAPI loopback, Windows service. Whole crate is `cfg(windows)` |
| `crates/gpu-interop` | GPU surface types shared by capture and encoder |
| `crates/server` | The `remothing` binary: CLI, config path, picks the platform |
| `testapp` | `rt-testapp`: the latency probe application |
| `client` | Browser client and admin UI (TypeScript, esbuild). `client/dist` is embedded into the binary at compile time |
| `Dockerfile`, `docker/` | Linux images: `remothing` (slim) and `remothing-desktop` (XFCE + Firefox) |
| `scripts/` | Windows build dependencies and environment |
| `tasks/` | Pending work, one file per task (see Tasks) |
| `.tools/` | Git-ignored: vcpkg, libclang, local deployment configs, tokens, logs |

- OS-specific code goes behind the traits in `crates/core/src/platform.rs`.
  Core has no platform `cfg` apart from a file mode in `tls.rs`.
- The wire protocol exists twice and changes together:
  `crates/core/src/proto.rs` and `client/src/proto.ts`.

## Build and check

```sh
# Client first: the Rust build embeds client/dist.
cd client && npm install && npm run build        # tsc --noEmit, then esbuild

# Windows (this machine). Once: scripts/windows-deps.ps1
powershell -NoProfile -ExecutionPolicy Bypass -Command ". scripts/windows-env.ps1; cargo build --release -p remothing -p rt-testapp --features remothing/static-ffmpeg"
powershell -NoProfile -ExecutionPolicy Bypass -Command ". scripts/windows-env.ps1; cargo clippy --release --workspace --all-targets --features remothing/static-ffmpeg"
powershell -NoProfile -ExecutionPolicy Bypass -Command ". scripts/windows-env.ps1; cargo test --release -p remothing-core"

# Linux: only through Docker here. This is also the compile check for
# crates/platform-linux, which the Windows clippy run does not see.
docker build -t remothing .
docker build --target desktop -t remothing-desktop .
```

- Clippy must be clean on Windows and the Docker build must pass before a
  change is done.
- cargo under PowerShell ends with exit code 1 and `NativeCommandError`
  because it writes progress to stderr. Read the output, not the exit code.
- Inside `powershell -Command "..."` started from a POSIX shell, `$_` and
  `$(...)` are expanded by the outer shell. Put such code in a `.ps1` file.
- After changing the vcpkg FFmpeg build: `cargo clean --release -p
  ffmpeg-sys-next -p remothing-encoder`. The static libraries are bundled
  into that crate's rlib, so a plain rebuild keeps linking the old ones.
- A client change needs the Rust binary rebuilt (and the Docker image) to
  be served.

## Verifying a change

- Run the thing: a real browser against a real server, on the platform the
  change touches. Linux behaviour is checked in a container, Windows
  behaviour with a private instance of `target/release/remothing.exe`.
- Latency and link behaviour: container with `-e MODE=bench -e
  "BENCH_ARGS=--mode box|scroll|full --fps 165" --cap-add NET_ADMIN`, link
  shaped with `tc qdisc ... netem` inside it, client opened with
  `?bench=10`; the report is in `window.__bench.result`. For WebTransport
  also publish `-p 127.0.0.1:18092:8081/udp -e
  WEBTRANSPORT_ADVERTISE=127.0.0.1:18092` (the host port the browser
  reaches). `?bench` sends no view, so there is no motion-mode scaling;
  `?codec=h264|hevc|av1` compares codecs. Profiles and reference numbers
  are in `docs/RESULTS.md`: compare against them, and add the new numbers
  there.
- Picture quality: record the scene losslessly with `ffmpeg -f x11grab`
  in a throwaway container (`apt-get install ffmpeg`) and compare encodes
  with PSNR/SSIM. On Windows `examples/d3d11_bench --codec C --dump DIR`
  writes the encoded stream (there is no AV1 software decoder in the
  Windows FFmpeg; decode the dump with dav1d in a container).
- The admin API (`/api/status`, Bearer admin token) lists per-client events,
  errors and the sound status: the first place to look after a hiccup.
- Remote input can be checked without touching this PC: in a container,
  send `C_MOVE`/`C_MOVE_REL` over `/input` and read `xdotool
  getmouselocation` (`apt-get install xdotool`).
- Unit tests exist where logic can be judged without a machine
  (`pacer.rs` simulator, input mapping). Do not add tests that only pin
  wording or wiring.

## Local deployments (this machine)

- Windows demo: the `remothing` service (LocalSystem, auto start), running
  `.tools/win-demo/remothing.exe` with `.tools/win-demo/config.toml`; logs
  `remothing-service.log` and `remothing-session.log` next to the config.
  Update from an elevated shell (the user's UAC prompt; ask first):
  `remothing.exe service stop`, copy `target/release/remothing.exe` over
  `.tools/win-demo/remothing.exe`, `remothing.exe service start`.
  `remothing-stable.exe` there is the rollback.
- `\\.\DISPLAY5` (3960x1800), when it exists, is Sunshine's virtual display
  (SudoMaker IddCx driver), not a monitor: never change its settings. HDR
  tests touch the GPU of the user's live session: only when the user says
  nothing else is running.
- Ubuntu desktop: container `rt-desktop` from image `remothing-desktop`,
  recreated after a rebuild with
  `powershell -NoProfile -ExecutionPolicy Bypass -File .tools/redeploy-desktop.ps1`,
  which keeps the tokens and settings of the running container
  (`.tools/desktop-tokens.txt` has only the stream token) and runs it with
  `--gpus all --shm-size=1g --cap-add SYS_NICE -p 8080:8080 -p 8080:8080/udp
  -p 127.0.0.1:8081:8081 -p 8081:8081/udp` (WebTransport on UDP 8081,
  advertised as `100.100.74.85:8081,127.0.0.1:8081`).
- The router (Zyxel, 192.168.1.1) forwards UDP 9080 (WebRTC) and UDP 9081
  (WebTransport) to the Windows demo; the Windows firewall's private
  profile is off.
- Redeploy both after a finished change and tell the user to reload tabs.

## Conventions

- Finish it or leave it out: no stubs, TODOs, placeholders or
  compatibility shims. No one-line wrapper functions.
- Comments say why, in plain words; `// SAFETY:` on every `unsafe` block.
- TypeScript: no inline structural casts such as `(x as { a: number }).a`
  (declare an interface); static lookup tables are `Record` literals, not
  `Set` or `Map`.
- Optional features (sound, clipboard, WebRTC, hardware encoders) are best
  effort: when one fails the stream goes on, and what was tried and why it
  failed reaches the server log, the admin status and the client. See the
  `tried` lists in `crates/core/src/audio.rs`.
- A behaviour change updates the docs in the same commit: ARCHITECTURE (how
  and why), README (how to use it), RESULTS (measured numbers only).
- Commits: one per finished piece of work; subject names what changed, body
  goes area by area (see `git log`). The repository is local: add no remote
  and push nothing unless asked.

## Boundaries

- No tokens, passwords, certificates or public addresses of the user in
  tracked files. They live in `.tools/` and in the config files.
- Do not delete or rewrite things in `.tools/` that you did not create.
- Router, Tailscale, reverse proxy and firewall of this machine are outside
  the repository: ask before touching them.

## Tasks

`tasks/` holds work that is not done yet, one Markdown file per task,
`NNN-short-name.md`; `tasks/README.md` has the format. A task says whether
it is still under discussion or agreed. Do not start one that is under
discussion: settle the open questions with the user first. When a task is
done, delete its file in the commit that finishes it; what was decided
belongs in the docs by then.

## Keeping this file current

After every finished task or change to the project, before the final
answer, ask yourself whether this file should change: a command that
changed, a new directory or convention, a trap that cost time, a rule that
turned out wrong or is no longer needed. Then tell the user, every time:

- either the exact edit you propose (the lines to add, change or remove)
  and why,
- or that you propose no change, and why.

Never apply an edit to this file on your own: the user accepts it, rejects
it or changes it. A proposed edit stays open until then. If the user's next
message does not decide it, raise it again in each following answer until
they give a final decision.

Keep the file short. It is loaded into every session: add what an agent
could not find out from the code and docs and would otherwise get wrong,
link to the docs instead of repeating them, and propose removing lines that
stopped earning their place.
