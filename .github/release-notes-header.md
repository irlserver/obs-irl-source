## Installation

One download per platform (Windows also gets an installer), and it works on OBS 32.1 and newer. The plugin bundles its own FFmpeg and libsrt, so there is no longer a separate download per OBS version.

### Windows

Run `-windows-x64-setup.exe`. It locates OBS Studio from the registry, closes it if it is running, and registers an entry in Add or Remove Programs. The installer is unsigned, so SmartScreen asks for a confirmation the first time.

Prefer to do it by hand? Use the zip instead:

1. Close OBS.
2. Extract the zip into your OBS Studio install folder (usually `C:\Program Files\obs-studio`). The DLL lands in `obs-plugins\64bit`.
3. Start OBS.

Upgrading from 1.x by hand: the payload no longer contains `w32-pthreads.dll`, so delete `obs-plugins\64bit\w32-pthreads.dll` if a previous release left one there. The installer removes it for you.

### macOS (Apple Silicon)

1. Close OBS.
2. Extract the zip into `~/Library/Application Support/obs-studio/plugins/`. It contains an `obs-irl-source.plugin` bundle; if an `obs-irl-source` folder from an older release is still there, delete it (that flat layout is invisible to OBS on macOS).
3. The binary is not signed or notarized, so clear the quarantine flag once:

   ```
   xattr -dr com.apple.quarantine "$HOME/Library/Application Support/obs-studio/plugins/obs-irl-source.plugin"
   ```

4. Start OBS.

### Linux

1. Close OBS.
2. Extract the tarball into `~/.config/obs-studio/plugins/` — for the Flatpak, `~/.var/app/com.obsproject.Studio/config/obs-studio/plugins/`.
3. Start OBS.

The build bundles its own media stack and resolves libobs symbols from the OBS process at load time, so the OBS version does not matter. It is compiled against Ubuntu 22.04's glibc 2.35 for Flatpak and older-distribution compatibility; on a still-older distribution, or if OBS does not load it, build from source instead (see the README).

Verify downloads against `sha256sums.txt`.

---

