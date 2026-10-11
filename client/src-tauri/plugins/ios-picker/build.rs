// Compiles and links ios/ (a Swift package) into the app on iOS targets. No
// webview-facing commands: the app calls the plugin from Rust
// (commands/ios_picker.rs), so there is nothing to grant in a capability.
const COMMANDS: &[&str] = &[];

fn main() {
    tauri_plugin::Builder::new(COMMANDS).ios_path("ios").build();
}
