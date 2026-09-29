# Lightify — Setup Guide

For Lightify 2.1 and later. Setup takes about five minutes, and you only do it once.

## What you need

- **Windows 10 or Windows 11**, 64-bit
- **A Spotify Premium account.** Spotify only lets Premium accounts stream to apps like Lightify.
- **A web browser**, used once to sign in

Lightify doesn't need the Spotify app installed or open.

---

## Step 1: Create your Spotify app

Lightify signs in through your own free Spotify developer app. That keeps your login between you and Spotify.

1. Go to [developer.spotify.com/dashboard](https://developer.spotify.com/dashboard) and log in with your Spotify account.
2. Click **Create app** and fill in:
   - **App name:** `Lightify` (or anything you like)
   - **App description:** anything
   - **Redirect URI:** `http://127.0.0.1:8901/callback`, then click **Add**
   - **Which API/SDKs are you planning to use?** Tick **Web API**
3. Accept the terms and click **Save**.
4. Open the app's **Settings** and copy the **Client ID**. It's a 32-character code.

> The redirect URI must be exactly `http://127.0.0.1:8901/callback`, with no trailing slash and no `https`.

**Signing in with a different Spotify account?** New Spotify apps start in *development mode*, which only lets in accounts you list. Open your app, go to **User Management**, and add the name and email of each Spotify account that will use it. Your own account (the one that created the app) works without this.

---

## Step 2: Install Lightify

1. Download `Lightify_2.2.3_x64-setup.exe` from [lightify.stream](https://lightify.stream).
2. Run it. The installer isn't code-signed yet, so Windows may show **"Windows protected your PC"**. Click **More info**, then **Run anyway**.
3. Choose whether to install for just you or everyone on the PC, then finish the installer.

To make sure the file wasn't changed on the way to you, compare it with `SHA256SUMS.txt` on the download page:

```powershell
Get-FileHash .\Lightify_2.2.3_x64-setup.exe -Algorithm SHA256
```

---

## Step 3: Sign in

1. Open Lightify. The first time, it shows the Spotify sign-in page.
2. Paste your **Client ID** and click **Authorise**.
3. Your browser opens Spotify's login page. Log in and click **Agree**.
4. The browser shows **You're signed in**. Close that tab and go back to Lightify.

If the browser didn't open, or you closed it by mistake, click **Retry**.

## Step 4: Allow playback (first launch only)

Right after you sign in, Lightify's player asks Spotify for permission to stream, and your browser opens once more. Approve it. From then on, Lightify appears as its own speaker (**Lightify**) in Spotify Connect and plays on this PC.

You're done. Next time, Lightify opens straight to your library.

---

## Good to know

- **Where Lightify keeps its files:** `%APPDATA%\Lightify` (paste that into File Explorer's address bar). The sign-in lives in `.lightify_cache`, and settings in `lightify_config.json`.
- **Sign out or switch accounts:** close Lightify, delete `%APPDATA%\Lightify\.lightify_cache`, then open Lightify again.
- **Remove Lightify:** Windows Settings → Apps → Installed apps → Lightify → Uninstall. Your sign-in and settings stay in `%APPDATA%\Lightify`; delete that folder too for a clean removal.
- **Already used Lightify 2.0 or 2.1?** 2.2 picks up the same sign-in, so you won't be asked again.

## Troubleshooting

| What you see | What to do |
|---|---|
| "That doesn't look like a Client ID" | Copy the Client ID again from your app's Settings page. It's 32 letters and numbers, not the Client Secret. |
| Spotify shows "INVALID_CLIENT: Invalid redirect URI" | In your app's Settings, check that the redirect URI is exactly `http://127.0.0.1:8901/callback`. |
| "Spotify blocked this account for your app" | Add the account under **User Management** in your Spotify app (see Step 1). |
| "Couldn't open the sign-in port" | Another Lightify is probably mid-sign-in. Close all Lightify windows and try again. |
| Music doesn't start | Make sure you approved the playback permission in Step 4, and that the account is Premium. |

---

## Downloads

Spotify playback login and the downloader login are separate. The first time you download something, open **Settings**, click **Connect Spotify Downloader**, then open Spotify and select **OnTheSpot** from the device picker. Once that connects, try the download again.

Downloaded tracks are saved to the download folder set under **Settings → Downloader**. The downloader starts when you use it and closes itself after five minutes with nothing downloading. Only download files you have the rights or permission to use. See the [Legal / Terms / Privacy](legal.html) page.
