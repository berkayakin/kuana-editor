; Windows installer (Inno Setup 6), built by .github/workflows/release.yml:
;   ISCC.exe /DVersion=<version> scripts\kuana.iss
; Installs per user (no admin prompt) so the updater can replace kuana.exe in place.
; It takes the `dist` build; /DProfile=release takes `cargo build --release`'s. For Arm:
; /DArch=aarch64 /DArchitectures=arm64 /DSource=..\target\aarch64-pc-windows-msvc\dist

#ifndef Version
  #define Version "0.0.0"
#endif
#ifndef Profile
  #define Profile "dist"
#endif
#ifndef Source
  #define Source "..\target\" + Profile
#endif
#ifndef Arch
  #define Arch "x86_64"
#endif
#ifndef Architectures
  #define Architectures "x64compatible"
#endif

[Setup]
AppId={{8E4C1D2B-6F0A-4B57-9C3E-2A7D5F1B9E64}
AppName=Kuana
AppVersion={#Version}
AppVerName=Kuana {#Version}
AppPublisher=Kuana
AppPublisherURL=https://github.com/berkayakin/kuana-editor
DefaultDirName={autopf}\Kuana
DefaultGroupName=Kuana
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed={#Architectures}
ArchitecturesInstallIn64BitMode={#Architectures}
SetupIconFile=..\assets\app-icon\windows\kuana.ico
UninstallDisplayIcon={app}\kuana.exe
OutputDir=..\dist
OutputBaseFilename=Kuana-{#Version}-windows-{#Arch}-setup
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
CloseApplications=yes
; Tells Explorer the file associations below changed, so Open With lists Kuana without a sign-out.
ChangesAssociations=yes

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked
Name: "foldermenu"; Description: "Add ""Open with Kuana"" to folders' context menu in Explorer"; GroupDescription: "Explorer:"; Flags: unchecked

[Files]
Source: "{#Source}\kuana.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\LICENSE-MIT"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\LICENSE-APACHE"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\assets\fonts\OFL.txt"; DestDir: "{app}"; DestName: "Vidaloka-OFL.txt"; Flags: ignoreversion
Source: "..\assets\fonts\JetBrainsMono-OFL.txt"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\Kuana"; Filename: "{app}\kuana.exe"
Name: "{autodesktop}\Kuana"; Filename: "{app}\kuana.exe"; Tasks: desktopicon

; Open With: Kuana is offered for text, docs and code files, never made their default app (the
; user picks that with "Always"). HKA is HKCU here, as the install is per user. Each extension
; lists the Kuana.File ProgID under OpenWithProgids (Kuana among the suggested apps) and in
; SupportedTypes (Kuana among "Choose another app"'s).
#dim Extensions[110] { \
  "txt", "text", "log", "md", "markdown", "mdown", "mkd", "rst", "adoc", "tex", "json", \
  "jsonc", "json5", "yaml", "yml", "toml", "ini", "cfg", "conf", "properties", "env", \
  "xml", "csv", "tsv", "rs", "go", "py", "pyw", "pyi", "rb", "php", "phtml", "js", "mjs", \
  "cjs", "jsx", "ts", "mts", "cts", "tsx", "vue", "svelte", "astro", "html", "htm", "xhtml", \
  "css", "scss", "sass", "less", "c", "h", "cc", "cpp", "cxx", "hpp", "hh", "hxx", "m", \
  "mm", "cs", "java", "kt", "kts", "swift", "dart", "scala", "groovy", "gradle", "lua", \
  "r", "jl", "zig", "nim", "ex", "exs", "erl", "hrl", "hs", "ml", "mli", "clj", "cljs", \
  "el", "vim", "sh", "bash", "zsh", "fish", "ps1", "psm1", "psd1", "bat", "cmd", "sql", \
  "graphql", "gql", "proto", "tf", "tfvars", "hcl", "cmake", "mk", "diff", "patch", "lock", \
  "gitignore", "gitattributes", "editorconfig", "dockerignore" }
#define I 0

#sub OpenWithEntries
Root: HKA; Subkey: "Software\Classes\.{#Extensions[I]}\OpenWithProgids"; ValueType: string; ValueName: "Kuana.File"; ValueData: ""; Flags: uninsdeletevalue
Root: HKA; Subkey: "Software\Classes\Applications\kuana.exe\SupportedTypes"; ValueType: string; ValueName: ".{#Extensions[I]}"; ValueData: ""
#endsub

[Registry]
Root: HKA; Subkey: "Software\Classes\Kuana.File"; ValueType: string; ValueName: ""; ValueData: "Kuana Document"; Flags: uninsdeletekey
Root: HKA; Subkey: "Software\Classes\Kuana.File\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: "{app}\kuana.exe,0"
Root: HKA; Subkey: "Software\Classes\Kuana.File\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\kuana.exe"" ""%1"""
Root: HKA; Subkey: "Software\Classes\Applications\kuana.exe"; ValueType: string; ValueName: "FriendlyAppName"; ValueData: "Kuana"; Flags: uninsdeletekey
Root: HKA; Subkey: "Software\Classes\Applications\kuana.exe\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: "{app}\kuana.exe,0"
Root: HKA; Subkey: "Software\Classes\Applications\kuana.exe\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\kuana.exe"" ""%1"""
#for {I = 0; I < DimOf(Extensions); I++} OpenWithEntries
; Right-click on a folder, and on a folder's empty space, opens it as a project.
Root: HKA; Subkey: "Software\Classes\Directory\shell\Kuana"; ValueType: string; ValueName: ""; ValueData: "Open with Kuana"; Tasks: foldermenu; Flags: uninsdeletekey
Root: HKA; Subkey: "Software\Classes\Directory\shell\Kuana"; ValueType: string; ValueName: "Icon"; ValueData: "{app}\kuana.exe"; Tasks: foldermenu
Root: HKA; Subkey: "Software\Classes\Directory\shell\Kuana\command"; ValueType: string; ValueName: ""; ValueData: """{app}\kuana.exe"" ""%1"""; Tasks: foldermenu
Root: HKA; Subkey: "Software\Classes\Directory\Background\shell\Kuana"; ValueType: string; ValueName: ""; ValueData: "Open with Kuana"; Tasks: foldermenu; Flags: uninsdeletekey
Root: HKA; Subkey: "Software\Classes\Directory\Background\shell\Kuana"; ValueType: string; ValueName: "Icon"; ValueData: "{app}\kuana.exe"; Tasks: foldermenu
Root: HKA; Subkey: "Software\Classes\Directory\Background\shell\Kuana\command"; ValueType: string; ValueName: ""; ValueData: """{app}\kuana.exe"" ""%V"""; Tasks: foldermenu

[Run]
Filename: "{app}\kuana.exe"; Description: "{cm:LaunchProgram,Kuana}"; Flags: nowait postinstall skipifsilent

[InstallDelete]
; Left by installs from when Kuana was called Kin (same AppId, so same folder).
Type: files; Name: "{app}\kin.exe"
Type: files; Name: "{autoprograms}\Kin.lnk"
Type: files; Name: "{autodesktop}\Kin.lnk"

[UninstallDelete]
; What the updater leaves beside the executable.
Type: files; Name: "{app}\kuana.exe.old"
Type: filesandordirs; Name: "{app}\.kuana-update"
