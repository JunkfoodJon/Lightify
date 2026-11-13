# Privacy

This page lists everything Lightify stores on your PC and everything it sends over the
network.

**Short version:** Lightify sends nothing about you to its author or to anyone else.
There is no telemetry, no analytics, no crash reporting, no ads and no Lightify account.
It talks to Spotify so it can play your music, to Beatport only when you open a Beatport
chart, and to lightify.stream to check for updates. Everything it keeps stays in your own
Windows profile.

## What goes over the network

| What | When | Who sees it |
|---|---|---|
| **Spotify sign-in.** The standard Spotify sign-in page, using your own Spotify developer app's Client ID. | The first time you use Lightify, and again only if Spotify asks you to sign in again. | Spotify (`accounts.spotify.com`). The sign-in page sends you back to Lightify through a listener on your own PC (`127.0.0.1:8901`) that only runs while you're signing in. |
| **Your library, search and playback control.** Playlists, Liked Songs, searches, the queue, and liking or adding tracks. | While Lightify is open and you use it. | Spotify's Web API (`api.spotify.com`), and `open.spotify.com` for some public playlists the Web API won't list. |
| **Music playback.** Lightify plays through Spotify's own streaming servers, as its own Spotify Connect speaker. | While music is playing. | Spotify's streaming and audio servers. |
| **Cover art.** The images for playlists, albums, artists and tracks. | When a list with covers is shown. Covers already downloaded are reused. | Spotify's image servers. |
| **Beatport charts.** A request for the chart, with an anonymous Beatport token (no Beatport account). | Only when you open a chart on the BEATPORT tab. | Beatport (`api.beatport.com`, and `www.beatport.com` for the token). The tracks are then looked up on Spotify like a search. |
| **Update check.** A plain request for `downloads/latest.json`, which names the newest version. It carries Lightify's version in its user agent and nothing else. | About 8 seconds after Lightify starts, then every 6 hours, and when you press *Check now* in Settings. It checks even with *Install updates automatically* off; that setting only decides whether a new version downloads on its own. | `lightify.stream`, the project's own website. Like any website visit, its server sees your IP address and that request. |
| **Update download.** The new installer, checked against a signature and a SHA-256 before it is used. | When a newer version exists and either automatic updates are on or you press *Download*. It installs only when you press *Restart now*. | `lightify.stream`. |

Nothing else. Local mode reads the audio files in the folders you add and never goes
online.

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
| `lightify_shell_window.json`, `lightify_shell_mode.txt`, `lightify_shell_lava.txt`, `lightify_shell_autoupdate.txt`, `lightify_shell_update_seen.txt`, `lightify_shell_lang.txt` | Where the window sits and its size, whether you were in Local mode, your ASCII lava and update settings, and the language you picked. |
| `lightify_shell_history.json`, `lightify_shell_local_history.json` | The songs you played, for the Recent panel: one list for Spotify, one for Local mode. |
| `lightify_shell_local_folders.json`, `lightify_shell_local_index.json`, `lightify_shell_local_playlists.json`, `lightify_shell_local_filters.json`, `lightify_shell_local_volume.txt` | Local mode: the folders and files you added, the list of tracks found in them (each file's path, length and tags: title, artist, album, genre, track and disc number), your local playlists, your filters and Local mode's volume. |
| `downloader-failed.json` | Only if you use the optional downloader bridge: the downloads that failed (name, artist, link and the error), so you can retry them after a restart. |
| `lightify_bp_matches.json` | Which Spotify track matched each Beatport chart entry, so a chart doesn't have to be looked up again. |
| `lightify-shell-audio.log`, `lightify-shell-media.log` | Diagnostic logs from the playback engine and Windows' media controls. They stay on your PC. |
| `lightify_beatport_cookie.json` | Only if you put one there: a Beatport session Lightify sends with chart requests. Lightify only reads it and never creates it. |

Downloaded updates are kept in `%LOCALAPPDATA%\Lightify\updates` until they're installed.

Lightify also shows the current track and its cover in Windows' own media controls and
on the lock screen, as any music player does.

## Removing your data

- **Sign-in:** delete `%APPDATA%\Lightify\.lightify_cache` and
  `%APPDATA%\Lightify\lightify-audio-cache`. To revoke Lightify's access from Spotify's
  side, remove its access under **Manage apps** at
  [spotify.com/account/apps](https://www.spotify.com/account/apps/).
- **Everything:** uninstall Lightify, then delete the `%APPDATA%\Lightify` folder and
  `%LOCALAPPDATA%\Lightify\updates`. The uninstaller leaves your data in place on purpose,
  so a reinstall keeps your sign-in and settings.
