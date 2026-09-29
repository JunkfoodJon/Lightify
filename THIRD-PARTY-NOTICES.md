# Third-party notices

Lightify's own code is licensed under the PolyForm Noncommercial License 1.0.0 (see
[LICENSE.md](LICENSE.md)). It is built on open-source components that keep their own
licenses. Their full license texts ship with each crate's source on
[crates.io](https://crates.io); `cargo metadata` lists the exact versions used, pinned in
`lightify-shell/Cargo.lock`.

## Notable components

| Component | Used for | License |
|---|---|---|
| [Slint](https://slint.dev) | User interface | Used under the **Slint Royalty-free Desktop, Mobile, and Web Applications License 2.0**; attribution below |
| [librespot](https://github.com/librespot-org/librespot) (`librespot-core`, `-connect`, `-playback`, `-metadata`, `-oauth`) | Spotify Connect playback | MIT |
| [Symphonia](https://github.com/pdeljanov/Symphonia) | Audio decoding (via librespot) | MPL-2.0 (used unmodified) |
| [tokio](https://tokio.rs), [reqwest](https://github.com/seanmonstar/reqwest), [serde](https://serde.rs) | Async runtime, HTTP, JSON | MIT / MIT OR Apache-2.0 |
| [wreq](https://github.com/0x676e67/wreq), `btls` / `btls-sys` (BoringSSL) | HTTP client for Beatport charts | Apache-2.0; `btls-sys` MIT; BoringSSL's own code under its ISC-style license |
| [image](https://github.com/image-rs/image), [png](https://github.com/image-rs/image-png) | Cover art decoding | MIT OR Apache-2.0 |
| [cpal](https://github.com/RustAudio/cpal), [rodio](https://github.com/RustAudio/rodio) | Audio output | Apache-2.0 / MIT OR Apache-2.0 |
| [tray-icon](https://github.com/tauri-apps/tray-icon), [windows-rs](https://github.com/microsoft/windows-rs) | Tray icon, Windows APIs | MIT OR Apache-2.0 |
| [winit](https://github.com/rust-windowing/winit) | Windowing | Apache-2.0 |
| [Inno Setup](https://jrsoftware.org/isinfo.php) | Building the installer (not shipped in it) | Inno Setup License |

The dependency tree contains no GPL-only code. Where a crate offers a choice of licenses
(for example `MIT OR Apache-2.0`, or `LGPL-3.0-or-later OR MPL-2.0`), Lightify uses it under
the permissive option.

## Slint attribution

Lightify uses Slint under the Slint Royalty-free License 2.0, which requires this
attribution:

<a href="https://slint.dev"><img src="https://raw.githubusercontent.com/slint-ui/slint/master/logo/MadeWithSlint-logo-whitebg.png" alt="Made with Slint" height="60"></a>

## Trademarks

Spotify is a trademark of Spotify AB. Beatport is a trademark of Beatport LLC. Lightify is
not affiliated with, endorsed by, or connected to either company.
