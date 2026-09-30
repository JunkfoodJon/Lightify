# Security policy

Lightify signs in to your Spotify account, keeps that sign-in on your PC, and parses data
from Spotify, Beatport and the network. Security reports are welcome and taken seriously.

## Reporting a vulnerability

Please **don't open a public issue** for a security problem. Use GitHub's private
reporting instead: on this repository's **Security** tab, choose **Report a
vulnerability**. Only the maintainer can see the report.

Please include:
- what an attacker can do, and what they need first (a local user on the same PC? on the
  same network? able to serve a crafted response?);
- steps or a proof of concept;
- the version or commit you tested.

You'll get an acknowledgement as soon as the maintainer can manage. This is a free,
one-person project, so there is no bug bounty. Once a fix ships, reporters are credited
unless they'd rather not be.

## In scope

- Anything that lets untrusted input run code or crash the app. Untrusted input includes
  responses from Spotify and Beatport, images, playlist and track names, and requests
  to the sign-in callback listener on `127.0.0.1:8901`.
- A way for another user or program to get Lightify's saved Spotify sign-in, or to make
  it act on your account in a way you didn't choose.
- Any path by which the app sends data somewhere beyond what [PRIVACY.md](PRIVACY.md)
  lists.
- DLL search-order or file-replacement problems in how the app or its installer loads
  code.

## Out of scope

- An attacker who already controls your Windows account or has Administrator on the PC.
  They can already read everything that account can.
- Problems in Spotify's or Beatport's own services. Please report those to them.
- Bugs that aren't security problems. Please open a normal issue for those.

## Verifying a download

Every release on [lightify.stream](https://lightify.stream) lists SHA-256 checksums in
[SHA256SUMS.txt](https://lightify.stream/downloads/SHA256SUMS.txt). Check a download in
PowerShell:

```powershell
Get-FileHash .\Lightify_2.2.3_x64-setup.exe -Algorithm SHA256
```

The result must match the line for that file. The installer isn't code-signed yet, so
Windows SmartScreen may say the publisher is unknown. If you'd rather not trust a
prebuilt file, build it yourself; see the [README](README.md#build-from-source).
