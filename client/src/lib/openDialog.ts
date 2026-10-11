// plugin-dialog's `open()`, except on iOS.
//
// On iOS plugin-dialog copies every picked file into the sandbox first (a
// multi-GB game image would be duplicated before the upload even starts) and
// can't pick folders at all. There the app's own document picker
// (tauri-plugin-ios-picker, via the `ios_pick_paths` command) opens the Files
// app instead: files AND folders, from iCloud Drive, a USB drive or a file
// provider, opened in place with security-scoped access the in-process engine
// can stream from. Same call shape and result as plugin-dialog's open(), so
// callers just swap the import.

import { open as pluginOpen, type OpenDialogOptions } from "@tauri-apps/plugin-dialog";

import { invoke } from "./invokeLogged";
import { isIOS } from "./platform";

export async function openDialog(
  options: OpenDialogOptions = {},
): Promise<string | string[] | null> {
  if (isIOS()) {
    const paths = await invoke<string[]>("ios_pick_paths", {
      directory: options.directory === true,
      multiple: options.multiple === true,
    });
    if (paths.length === 0) return null;
    return options.multiple ? paths : paths[0];
  }
  return pluginOpen(options);
}
