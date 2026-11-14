## Installation

One download per platform (Windows also gets an installer), and it works on OBS 32.1 and newer. The plugin bundles its own FFmpeg and libsrt, so there is no longer a separate download per OBS version.

### Windows

Run `-windows-x64-setup.exe`. It installs into `C:\ProgramData\obs-studio\plugins`, checks the OBS version it finds from the registry, closes OBS if it is running, and registers an entry in Add or Remove Programs. It also removes the copy that releases up to 2.2 put in the OBS folder. The installer is unsigned, so SmartScreen asks for a confirmation the first time.

Prefer to do it by hand? Use the zip instead:

1. Close OBS.
2. If an earlier release is installed, delete `obs-plugins\64bit\obs-irl-source.dll` and `data\obs-plugins\obs-irl-source` from your OBS Studio folder (usually `C:\Program Files\obs-studio`).
3. Extract the zip into `C:\ProgramData\obs-studio\plugins\`. For a portable OBS 33, extract it into the `plugins` folder of the portable install instead.
4. Start OBS.

The folder holds the DLL twice: OBS 33 loads the one at the top, and OBS 32 loads the one in `bin\64bit`. In the OBS folder, OBS 33 flags the plugin as Legacy and OBS 34 will not load it.

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
2. Extract the tarball into the plugin folder for your OBS version:
   - OBS 33 and newer: `~/.local/share/obs-studio/plugins/`, or for the Flatpak `~/.var/app/com.obsproject.Studio/data/obs-studio/plugins/`.
   - OBS 32: `~/.config/obs-studio/plugins/`, or for the Flatpak `~/.var/app/com.obsproject.Studio/config/obs-studio/plugins/`.
3. Start OBS.

After an upgrade to OBS 33, delete the old `obs-irl-source` folder under `~/.config`. OBS 33 flags a plugin there as Legacy, and OBS 34 will not load it.

The build bundles its own media stack and resolves libobs symbols from the OBS process at load time, so the OBS version does not matter. It is compiled against Ubuntu 22.04's glibc 2.35 for Flatpak and older-distribution compatibility; on a still-older distribution, or if OBS does not load it, build from source instead (see the README).

Verify downloads against `sha256sums.txt`.

---

