; Lightify installer (Inno Setup 6).
;
; Build with scripts/build-installer.ps1, which compiles the release binary first
; and passes the payload switches below. Compiling this file on its own works too,
; as long as target\release\Lightify.exe exists.
;
; Payload switches (ISCC /D...):
;   /DIncludeDownloader=1   bundle the OnTheSpot bridge (downloads work offline)
;   /DIncludeFfmpeg=1       bundle ffmpeg.exe (the bridge needs it to convert)
; Both are optional: without them Lightify still installs and plays, and the
; downloader falls back to an ffmpeg already on PATH.

#define AppName      "Lightify"
#define AppExeName   "Lightify.exe"
#define AppPublisher "Lightify"
#ifndef AppVersion
  #define AppVersion "1.0.0"
#endif

#define SrcRoot   "..\.."
#define ShellRoot ".."
#define BinDir    ShellRoot + "\target\release"

[Setup]
AppId={{8F3C2A17-5D4E-4B9A-9E21-3C7A6B0D1E44}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher={#AppPublisher}
VersionInfoProductName={#AppName}
VersionInfoDescription={#AppName} Setup
VersionInfoVersion={#AppVersion}

DefaultDirName={autopf}\{#AppName}
DefaultGroupName={#AppName}
; Let the user pick per-machine (admin) or just-for-me; neither is forced.
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog

; The installer, its Add/Remove Programs entry and the shortcuts all carry the app icon.
SetupIconFile={#ShellRoot}\assets\lightify.ico
UninstallDisplayIcon={app}\{#AppExeName}
UninstallDisplayName={#AppName}

OutputDir={#ShellRoot}\target\installer
OutputBaseFilename={#AppName}-Setup-{#AppVersion}
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
DisableProgramGroupPage=yes
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
LicenseFile={#SrcRoot}\LICENSE.md

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "{#BinDir}\{#AppExeName}"; DestDir: "{app}"; Flags: ignoreversion

; The downloader bridge. Lightify looks for it next to its own executable first,
; so dropping it in {app} is all the wiring it needs.
#ifdef IncludeDownloader
Source: "{#SrcRoot}\lightify-tauri\src-tauri\target\pyinstaller\release\dist\lightify-onthespot.exe"; DestDir: "{app}"; Flags: ignoreversion
#endif

; ffmpeg, used by the bridge to convert downloads. Found the same way (next to the
; executable, then PATH), so this is a drop-in too.
#ifdef IncludeFfmpeg
Source: "{#SrcRoot}\lightify-tauri\src-tauri\target\release\ffmpeg.exe"; DestDir: "{app}"; Flags: ignoreversion
#endif

[Icons]
Name: "{group}\{#AppName}"; Filename: "{app}\{#AppExeName}"; IconFilename: "{app}\{#AppExeName}"
Name: "{group}\Uninstall {#AppName}"; Filename: "{uninstallexe}"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#AppExeName}"; IconFilename: "{app}\{#AppExeName}"; Tasks: desktopicon

[Run]
Filename: "{app}\{#AppExeName}"; Description: "{cm:LaunchProgram,{#StringChange(AppName, '&', '&&')}}"; Flags: nowait postinstall skipifsilent

[UninstallDelete]
; The bridge writes a log next to the app data; leave the user's own config and
; cached sign-in alone, they are shared with the full app.
Type: files; Name: "{app}\onthespot-web.log"
