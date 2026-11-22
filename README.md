<p align="center">
  <img src="assets/greenvita.svg" alt="GreenVita" width="220">
</p>

<h1 align="center">Turbo GreenVita</h1>

<p align="center">
  Xbox Cloud Gaming on PlayStation Vita.
  <br>
  A native Rust client with an egui/SDL2 interface and hardware H.264 decoding.
</p>

<p align="center">
  <img alt="Rust 2024" src="https://img.shields.io/badge/Rust-2024-f74c00?style=for-the-badge&logo=rust&logoColor=white">
  <img alt="PS Vita homebrew" src="https://img.shields.io/badge/PS%20Vita-homebrew-44aa00?style=for-the-badge">
  <img alt="Xbox Cloud Gaming" src="https://img.shields.io/badge/Xbox-Cloud%20Gaming-107c10?style=for-the-badge&logo=xbox&logoColor=white">
</p>


> [!NOTE]
> Local console streaming has not been tested because I do not have a modern
> Xbox console available for validation.

## What this fork changes

Turbo GreenVita is a fork of
[GreenVita](https://github.com/Day-OS/green-vita) at 0.1.8. Everything below is
what this fork adds on top; the core xCloud/WebRTC client, sign-in, and UI
remain the upstream project's work.

### Streaming quality and latency

Ported from [Vita Moonlight](https://github.com/xyzz/vita-moonlight), which
streams smoothly on the same hardware. The first release of this fork (0.1.8)
shipped the latency work; the changes after it target decoder throughput.

**Latency and stability (0.1.8):**

- **Low-delay H.264 decode mode.** The hardware decoder is configured for
  minimal internal reordering, so B-frame streams no longer buffer several
  access units before the first picture is output.
- **4 reference frames** (up from 1; **2 since 0.4.0** for faster decode),
  which removes the visible corruption (datamoshing) during motion in
  high-bitrate scenes.
- **Triple-buffered staging** (3 CDRAM slots) plus **triple-buffered
  display textures** (0.4.0). Decoded frames write into a staging slot so
  the decoder never stalls waiting for the previous frame to be displayed,
  and the renderer records the GXM scene per display texture so it only
  waits for the scene that sampled the texture being reused — the other
  in-flight scenes overlap CPU tessellation.
- **Decode-driven rendering.** The UI loop wakes as soon as a frame is
  decoded instead of polling on a fixed timer, cutting up to one render-tick
  of added latency per frame.
- **More tolerant decoder admission control.** A single slow decode no longer
  forces a frame drop and a keyframe-request storm; small backlogs are
  absorbed instead of amplified.
- **SDP ICE credential normalization**, fixing session-negotiation edge cases
  with the xCloud answer. The max-bitrate setting below also advertises its
  value in the client-capabilities message and SDP.

The low-delay function (`sceAvcdecSetLowDelayModeNongameapp`) is not exported
by the `SceVideodecUser` library that VitaSDK's stub imports from; it lives in
`SceVideodecLowDelayUser`. Calling the SDK stub jumps to a null pointer and
crashes the stream instantly. `build.rs` therefore assembles a correct import
stub (`vita-stubs/SceVideodecLowDelayUser.S`) at build time and links it under
a renamed symbol — the same approach vita-moonlight uses with a custom
`vita-libs-gen` stub.

**Throughput (after 0.2.0):**

On-device testing confirms that these changes deliver a **clear performance
improvement** over RGB565 hardware downscaling; PHYCONT-NC alone contributed
only a small gain, while moving conversion and scaling to the GPU produced
the larger improvement.

- **PHYCONT-NC decoder memory.** The hardware decoder's internal frame buffer
  is now allocated in physically-contiguous, non-cached main memory (falling
  back to CDRAM if PHYCONT is unavailable). The buffer is cached and reused
  across sessions, which also avoids the CDRAM fragmentation that the old
  reserve-then-reallocate scheme worked around. The stream debug HUD shows
  the active pool as `mem:PHYCONT-NC` or `mem:CDRAM`.
- **Full-resolution YUV with GPU color conversion.** AVCDEC decodes
  `SCE_AVCDEC_PIXELFORMAT_YUV420_PACKED_RASTER` at 1280×720; the Vita GXM
  renderer converts and scales it on the GPU (NV12 → `YVU420P2_CSC0`,
  matching Vita Moonlight). The decoder no longer performs RGB conversion
  plus a 720p→544p downscale per frame. `out:NV12` in the HUD confirms the
  path; upload and GPU-wait times are drawn to compare against the previous
  path.
- **Memory management for output.** NV12 staging buffers are reserved before
  SDL starts, because SDL consumes the available CDRAM on the tested Vita.
  Slots are lent to each stream session and returned by the final owner, so
  repeated starts do not reallocate backing memory.
- **GPU-aware texture reuse.** The renderer records the GXM context used by
  SDL and waits for its scene to finish before the next upload or before
  destroying the displayed texture.

**Present path (0.4.0):**

On-device profiling showed the old `SDL_UpdateNVTexture` CPU memcpy out of
uncached CDRAM ran at ~69 MB/s — **20.5 ms per 720p frame**, capping the
display at ~30 fps even though the decoder sustained 60. The fixes below
moved that to **1 ms** and took simple scenes from 60/30 to 60/58–60
presented/decoded.

- **DMA-based NV12 upload (0.4.0).** The upload now runs on the Vita DMA
  engine (`sceDmacMemcpy`, Y and UV planes) into the locked SDL streaming
  texture instead of a CPU memcpy from non-cached memory. This bypasses
  the CPU caches entirely: upload **20,481 µs → 1,057 µs** on device,
  loop total 27,912 µs → ~5,000 µs. (`up` in the HUD.)
- **Per-texture GXM scene tracking (0.4.0).** Three rotating display
  textures replace the single streaming texture; `sceGxmFinish` now only
  waits for the scene that sampled the texture being reused (3 frames old),
  so GPU work overlaps CPU instead of blocking it. GPU wait dropped to
  ~5 µs (`gpu` in the HUD).
- **Overlay throttling with cached replay (0.4.0).** While streaming, egui
  rebuilds at 10 Hz instead of every loop iteration; video still presents
  every iteration. The debug HUD replays its last tessellated geometry on
  presentation-only frames so it stays steady instead of strobing. The
  bright-green monospace chip replaced the white-on-transparent debug
  line, which was unreadable over game content.
- **Decode priority + 15 ms loop deadline (0.4.0).** The decode thread is
  raised to `SDL_THREAD_PRIORITY_HIGH` on its pinned core, and the loop
  deadline is 15 ms, so late-decode frames land closer to the vsync edge.
- **Decode-result queue + drain-after-receive (0.4.0).** Decode results
  move through a depth-3 queue instead of a single replaceable slot, and
  the RTC pump drains the queue *after* receiving RTP — previously a pump
  cycle stranded just-decoded frames for a full cycle (`repl` metric,
  11→2 in complex scenes; pipeline age halved).
- **TransportLayerNack loss recovery (0.4.0).** NACK feedback was
  negotiated in the SDP but never sent; every Wi-Fi loss became a keyframe
  skip. The reassembler now reports missing sequence numbers and the pump
  sends a 16-bit-bitmap `TransportLayerNack`. New `nack:` counter in the
  HUD (between `drop:` and `err:`).
- **HUD additions (0.4.0).** `pr` (blocking `present()` time), `loop`
  (loop-start to present-return), `slot` (staging-slot misses), and `nack`
  (NACKs sent) — the numbers used to find the upload bottleneck above.
- **Removing per-frame allocations (0.4.0).** `missing_sequences` uses a
  binary search instead of a `HashSet`, and the assembled AU moves as a
  `Vec<u8>` instead of being copied via `Bytes::to_vec()`.

### Max bitrate setting

A new **Max bitrate** row in Settings advertises a bitrate cap (2, 3, 4, 5, 6,
8, or 10 Mbps) to xCloud in the client-capabilities message and SDP, letting
you trade image quality for stability on weak or congested Wi-Fi. The default
is 2 Mbps, matching upstream behavior. Settings saved with older values
(15–50 Mbps) load as 10 Mbps.

### UI

- Full rebrand to **Turbo GreenVita** (`TURBOVITA` title ID): logo, LiveArea,
  localized strings, release workflow, and package name.
- Reworked game-profile menu and Select button mapping.
- **No CHOOSE MODE screen (0.4.0).** After sign-in you go straight into the
  Cloud title list. The Home (console) path is left compiled but
  unreachable; back-navigation now returns to the title list.

## Install

You need a homebrew-enabled PS Vita with VitaShell and an Xbox account that has
access to Xbox Cloud Gaming.

1. Download `turbo-green-vita.vpk` from the
   [latest release](https://github.com/PoissMi/turbo-green-vita/releases/latest).
2. Transfer the VPK to the Vita.
3. Install it with VitaShell.
4. Launch Turbo GreenVita and complete the device-code sign-in.

> [!IMPORTANT]
> Enable **Unsafe Homebrew** in HENkaku Settings. Turbo GreenVita needs access
> to the Vita hardware video-decoder module.

## Build

### 1. Install VitaSDK

Turbo GreenVita follows the official [VitaSDK/VDPM setup](https://github.com/vitasdk/vdpm).
On Arch Linux, install the host tools first:

```sh
sudo pacman -S --needed base-devel git cmake python wget patch p7zip tar pkgconf rustup
```

Then bootstrap VitaSDK and its port libraries:

```sh
git clone https://github.com/vitasdk/vdpm
cd vdpm
./bootstrap-vitasdk.sh

export VITASDK=/usr/local/vitasdk
export PATH="$VITASDK/bin:$PATH"

./install-all.sh
```

`install-all.sh` installs the Vita port libraries used by this project,
including SDL2 and Opus. The old vitaGL/vitaShaRK dependency list is not needed.

Add these exports to your shell profile so future terminals can find VitaSDK:

```sh
export VITASDK=/usr/local/vitasdk
export PATH="$VITASDK/bin:$PATH"
```

> [!TIP]
> On Windows, VitaSDK recommends following the Linux instructions through
> WSL2. MSYS2 is also supported by VDPM, but WSL2 is the simpler route.

### 2. Install the Rust tools

```sh
rustup toolchain install nightly
cargo +nightly install cargo-vita
```

See the official [`cargo-vita` documentation](https://github.com/vita-rust/cargo-vita)
for its complete command reference.

### 3. Build the VPK

From the Turbo GreenVita repository:

```sh
make vpk
```

The Makefile supplies the required Vita Rust flags. The resulting package is:

```text
target/armv7-sony-vita-newlibeabihf/release/turbo-green-vita.vpk
```

<details>
<summary>Building without Make</summary>

Unix-like shell:

```sh
RUSTFLAGS="-C target-feature=-neon" cargo +nightly vita build vpk --release
```

Windows PowerShell:

```powershell
$env:RUSTFLAGS = "-C target-feature=-neon"
cargo +nightly vita build vpk --release
```

The repository contains platform wrappers under [`tools/`](tools/) so Cargo can
find the Vita compiler, archiver, and pkg-config implementation on Unix and Windows.

</details>

## Develop On Hardware

For the commands below, install
[`vitacompanion`](https://github.com/devnoname120/vitacompanion) on the Vita and
leave its FTP server running.

### First installation

```sh
make upload-vpk VITA_IP=192.168.0.103
```

This uploads the package to `ux0:/data/turbo-green-vita.vpk`; it does not install it.
Open VitaShell and install that file once. To choose another upload directory:

```sh
make upload-vpk VITA_IP=192.168.0.103 VITA_UPLOAD_DIR=ux0:/downloads/
```

### Fast update and run

After the VPK is installed:

```sh
make update-run-vita VITA_IP=192.168.0.103
```

This rebuilds `eboot.bin`, replaces `ux0:/app/TURBOVITA/eboot.bin`, and starts
the application. `make run-vita VITA_IP=...` is an alias for the same command.

> [!CAUTION]
> The update command replaces only `eboot.bin`. Reinstall the complete VPK
> whenever package metadata or files under `static/` change.

<details>
<summary>FTP error 550: File not found</summary>

The destination directory must already exist. Install the VPK before using
`update-run-vita`, and make sure the upload directory passed through
`VITA_UPLOAD_DIR` exists on the Vita memory card.

</details>
## Credits

- [Greenlight](https://github.com/unknownskl/greenlight), an open-source xCloud
  and Xbox home-streaming client that served as a protocol and UX reference
- [xbox-xcloud-player](https://github.com/unknownskl/xbox-xcloud-player), the
  WebRTC streaming library used by Greenlight and a key reference for xCloud
  and xHome session handling
- [Vita Moonlight](https://github.com/xyzz/vita-moonlight), a major reference
  for low-latency streaming and hardware video decoding on the PS Vita; the
  decoder and buffering improvements described in
  [What this fork changes](#what-this-fork-changes) are ported from it
- PS Vita icon used in the Turbo GreenVita logo: "PS Vita" by Mark Davis from
  [The Noun Project](https://thenounproject.com/icon/ps-vita-203775/)
- [VitaSDK](https://vitasdk.org/) and the
  [vita-rust](https://github.com/vita-rust) ecosystem

Turbo GreenVita is an independent homebrew project and is not affiliated with
or endorsed by Microsoft, Xbox, Sony, or PlayStation.

## License

Turbo GreenVita is licensed under the [Mozilla Public License 2.0](LICENSE).
The upstream GreenVita license and copyright notices are preserved.
