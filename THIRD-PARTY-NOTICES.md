# Third-party notices

Lightify's own code is licensed under the PolyForm Noncommercial License 1.0.0 (see
[LICENSE.md](LICENSE.md)). It is built on open-source components that keep their own
licenses. Their full license texts ship with each crate's source on
[crates.io](https://crates.io); `cargo metadata` lists the exact versions used, pinned in
`lightify-shell/Cargo.lock` and `lightify-installer/Cargo.lock`.

## Notable components

| Component | Used for | License |
|---|---|---|
| [Slint](https://slint.dev) | User interface | Used under the **Slint Royalty-free Desktop, Mobile, and Web Applications License 2.0**; attribution below |
| [librespot](https://github.com/librespot-org/librespot) (`librespot-core`, `-connect`, `-playback`, `-metadata`, `-oauth`) | Spotify Connect playback | MIT |
| [Symphonia](https://github.com/pdeljanov/Symphonia) | Audio decoding (Spotify playback via librespot, and Local mode's files) | MPL-2.0 (used unmodified) |
| [rubato](https://github.com/HEnquist/rubato) | Resampling Local mode's audio to the output device's rate | MIT |
| [tokio](https://tokio.rs), [reqwest](https://github.com/seanmonstar/reqwest), [serde](https://serde.rs) | Async runtime, HTTP, JSON | MIT / MIT OR Apache-2.0 |
| [wreq](https://github.com/0x676e67/wreq), `btls` / `btls-sys` (BoringSSL) | HTTP client for Beatport charts | Apache-2.0; `btls-sys` MIT; BoringSSL's own code under its ISC-style license |
| [image](https://github.com/image-rs/image), [png](https://github.com/image-rs/image-png) | Cover art decoding | MIT OR Apache-2.0 |
| [cpal](https://github.com/RustAudio/cpal), [rodio](https://github.com/RustAudio/rodio) | Audio output | Apache-2.0 / MIT OR Apache-2.0 |
| [tray-icon](https://github.com/tauri-apps/tray-icon), [windows-rs](https://github.com/microsoft/windows-rs) | Tray icon, Windows APIs | MIT OR Apache-2.0 |
| [winit](https://github.com/rust-windowing/winit) | Windowing | Apache-2.0 |
| [ab_glyph](https://github.com/alexheretic/ab-glyph) | Drawing the ASCII lava's characters | Apache-2.0 |
| [arboard](https://github.com/1Password/arboard), [chrono](https://github.com/chronotope/chrono) | Clipboard, dates | MIT OR Apache-2.0 |
| [ed25519-dalek](https://github.com/dalek-cryptography/curve25519-dalek), [sha2](https://github.com/RustCrypto/hashes) | Checking (and, for releases, making) update signatures and checksums | BSD-3-Clause / MIT OR Apache-2.0 |
| [zstd](https://github.com/gyscos/zstd-rs) (`zstd-safe`, `zstd-sys`, Zstandard) | Compressing the installer's payload | MIT; `zstd-safe`, `zstd-sys` and Zstandard under BSD-3-Clause |
| [Space Grotesk](https://github.com/floriankarsten/space-grotesk), [JetBrains Mono](https://github.com/JetBrains/JetBrainsMono) | The app's and installer's typefaces (bundled) | SIL Open Font License 1.1; texts in `lightify-shell/ui/fonts/` |
| [Yellowtail](https://github.com/google/fonts/tree/main/apache/yellowtail) | The Lightify wordmark (bundled) | Apache-2.0; text in `lightify-shell/ui/fonts/` |

The dependency tree contains no GPL-only code. Where a crate offers a choice of licenses
(for example `MIT OR Apache-2.0`, or `LGPL-3.0-or-later OR MPL-2.0`), Lightify uses it under
the permissive option.

## Patched copies in `lightify-shell/vendor/`

Two crates are vendored with small changes and used through `[patch.crates-io]`. They
stay under their original licenses:

- `librespot-connect` 0.8.0, from [librespot](https://github.com/librespot-org/librespot),
  under the MIT License:

  > The MIT License (MIT)
  >
  > Copyright (c) 2015 Paul Lietar
  >
  > Permission is hereby granted, free of charge, to any person obtaining a copy
  > of this software and associated documentation files (the "Software"), to deal
  > in the Software without restriction, including without limitation the rights
  > to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
  > copies of the Software, and to permit persons to whom the Software is
  > furnished to do so, subject to the following conditions:
  >
  > The above copyright notice and this permission notice shall be included in
  > all copies or substantial portions of the Software.
  >
  > THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
  > IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
  > FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
  > AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
  > LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
  > OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN
  > THE SOFTWARE.

- `i-slint-renderer-software` 1.17.1, part of [Slint](https://slint.dev), used under the
  Slint Royalty-free License 2.0 like the rest of Slint; its license texts are in
  `lightify-shell/vendor/i-slint-renderer-software/LICENSES/`.

## Slint attribution

Lightify uses Slint under the Slint Royalty-free License 2.0, which requires this
attribution:

<a href="https://slint.dev"><img src="https://raw.githubusercontent.com/slint-ui/slint/master/logo/MadeWithSlint-logo-whitebg.png" alt="Made with Slint" height="60"></a>

## Trademarks

Spotify is a trademark of Spotify AB. Beatport is a trademark of Beatport LLC. Lightify is
not affiliated with, endorsed by, or connected to either company.
