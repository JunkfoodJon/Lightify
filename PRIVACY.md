# Privacy

This page lists everything Lightify stores on your PC and everything it sends over the
network.

**Short version:** Lightify sends nothing about you to its author or to anyone else.
There is no telemetry, no analytics, no crash reporting, no ads and no Lightify account.
It talks to Spotify so it can play your music, and to Beatport only when you open a
Beatport chart. Everything it keeps stays in your own Windows profile.

## What goes over the network

| What | When | Who sees it |
|---|---|---|
| **Spotify sign-in.** The standard Spotify sign-in page, using your own Spotify developer app's Client ID. | The first time you use Lightify, and again only if Spotify asks you to sign in again. | Spotify (`accounts.spotify.com`). The sign-in page sends you back to Lightify through a listener on your own PC (`127.0.0.1:8901`) that only runs while you're signing in. |
| **Your library, search and playback control.** Playlists, Liked Songs, searches, the queue, and liking or adding tracks. | While Lightify is open and you use it. | Spotify's Web API (`api.spotify.com`), and `open.spotify.com` for some public playlists the Web API won't list. |
| **Music playback.** Lightify plays through Spotify's own streaming servers, as its own Spotify Connect speaker. | While music is playing. | Spotify's streaming and audio servers. |
| **Cover art.** The images for playlists, albums, artists and tracks. | When a list with covers is shown. Covers already downloaded are reused. | Spotify's image servers. |
| **Beatport charts.** A plain request for the chart page. | Only when you open a chart on the BEATPORT tab. | Beatport (`www.beatport.com`). The tracks are then looked up on Spotify like a search. |

Nothing else. There's no update check: new versions are announced on
[lightify.stream](https://lightify.stream), and Lightify never contacts that site itself.

The Downloads panel talks to an optional downloader bridge that is **not** part of this
repository. It runs on your own PC, and Lightify only talks to it over `127.0.0.1`.

## What is stored on your PC

Everything is in `%APPDATA%\Lightify`, inside your own Windows user profile. The app
itself doesn't write to the registry; the installer only adds the usual entry in
Settings → Apps.

| File or folder | What it holds |
|---|---|
| `lightify_config.json` | Your Spotify developer app's Client ID, your chosen audio output, and your download folder. |
| `.lightify_cache` | Your Spotify sign-in (access and refresh tokens), so you don't sign in on every launch. |
| `lightify-audio-cache\` | The playback engine's own saved Spotify sign-in and volume. No audio is stored. |
| `thumbs\` | Cover art, resized, so lists open quickly. |
| `lightify_shell_library.json`, `lightify_shell_queue.json`, `lightify_shell_resume.json`, `lightify_shell_recent_searches.json`, `lightify_shell_mini.txt` | Your library list, your queue, where you left off, your recent searches and your last mini-player size, so they're there next time. |
| `lightify-shell-audio.log`, `lightify-shell-media.log` | Diagnostic logs from the playback engine and Windows' media controls. They stay on your PC. |
| `lightify_beatport_cookie.json` | Only if you put one there: a Beatport session Lightify sends with chart requests. Lightify only reads it and never creates it. |

Lightify also shows the current track and its cover in Windows' own media controls and
on the lock screen, as any music player does.

## Removing your data

- **Sign-in:** delete `%APPDATA%\Lightify\.lightify_cache` and
  `%APPDATA%\Lightify\lightify-audio-cache`. To revoke Lightify's access from Spotify's
  side, remove its access under **Manage apps** at
  [spotify.com/account/apps](https://www.spotify.com/account/apps/).
- **Everything:** uninstall Lightify, then delete the `%APPDATA%\Lightify` folder. The
  uninstaller leaves that folder in place on purpose, so a reinstall keeps your sign-in
  and settings.
