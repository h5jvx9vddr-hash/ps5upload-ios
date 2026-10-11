import Tauri
import UIKit
import UniformTypeIdentifiers
import WebKit

class PickArgs: Decodable {
  var directory: Bool?
  var multiple: Bool?
}

struct PickResult: Encodable {
  let paths: [String]
}

/// Files-app picker: files or folders, opened in place (no copy into the
/// sandbox). Security-scoped access is started on every pick and held for the
/// life of the process; a bookmark per pick lets the next launch re-acquire it,
/// so a resumed upload can still read its source.
class IosPickerPlugin: Plugin, UIDocumentPickerDelegate {
  private static let bookmarksKey = "ps5upload.iosPicker.bookmarks"
  private static let maxBookmarks = 64

  private var pending: Invoke?
  /// URLs whose security-scoped access is currently held.
  private var held: [URL] = []

  @objc public override func load(webview: WKWebView) {
    restoreBookmarks()
  }

  @objc public func pick(_ invoke: Invoke) throws {
    let args = try invoke.parseArgs(PickArgs.self)
    DispatchQueue.main.async {
      // A picker still waiting (shouldn't happen: it is modal) ends as cancelled.
      self.pending?.resolve(PickResult(paths: []))
      self.pending = invoke

      let types: [UTType] = (args.directory ?? false) ? [.folder] : [.item]
      let picker = UIDocumentPickerViewController(forOpeningContentTypes: types, asCopy: false)
      picker.allowsMultipleSelection = args.multiple ?? false
      picker.shouldShowFileExtensions = true
      picker.delegate = self
      picker.modalPresentationStyle = .formSheet
      guard let presenter = self.manager.viewController else {
        self.pending = nil
        invoke.reject("no view controller to present the picker from")
        return
      }
      presenter.present(picker, animated: true, completion: nil)
    }
  }

  // MARK: UIDocumentPickerDelegate

  public func documentPicker(
    _ controller: UIDocumentPickerViewController, didPickDocumentsAt urls: [URL]
  ) {
    var paths: [String] = []
    for url in urls {
      hold(url)
      saveBookmark(url)
      paths.append(url.path)
    }
    pending?.resolve(PickResult(paths: paths))
    pending = nil
  }

  public func documentPickerWasCancelled(_ controller: UIDocumentPickerViewController) {
    pending?.resolve(PickResult(paths: []))
    pending = nil
  }

  // MARK: Security-scoped access

  private func hold(_ url: URL) {
    if held.contains(where: { $0.path == url.path }) { return }
    if url.startAccessingSecurityScopedResource() {
      held.append(url)
    }
  }

  private func loadBookmarks() -> [[String: Any]] {
    UserDefaults.standard.array(forKey: Self.bookmarksKey) as? [[String: Any]] ?? []
  }

  private func storeBookmarks(_ list: [[String: Any]]) {
    UserDefaults.standard.set(Array(list.suffix(Self.maxBookmarks)), forKey: Self.bookmarksKey)
  }

  private func saveBookmark(_ url: URL) {
    guard
      let data = try? url.bookmarkData(
        options: [], includingResourceValuesForKeys: nil, relativeTo: nil)
    else { return }
    var list = loadBookmarks().filter { ($0["path"] as? String) != url.path }
    list.append(["path": url.path, "data": data])
    storeBookmarks(list)
  }

  private func restoreBookmarks() {
    var kept: [[String: Any]] = []
    for entry in loadBookmarks() {
      guard let data = entry["data"] as? Data else { continue }
      var stale = false
      guard
        let url = try? URL(
          resolvingBookmarkData: data, options: [], relativeTo: nil,
          bookmarkDataIsStale: &stale)
      else { continue }
      hold(url)
      var fresh = data
      if stale,
        let renewed = try? url.bookmarkData(
          options: [], includingResourceValuesForKeys: nil, relativeTo: nil)
      {
        fresh = renewed
      }
      kept.append(["path": url.path, "data": fresh])
    }
    storeBookmarks(kept)
  }
}

@_cdecl("init_plugin_ios_picker")
func initPlugin() -> Plugin {
  return IosPickerPlugin()
}
