import Network
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

class BrowseArgs: Decodable {
  var types: [String]
  var timeoutMs: Int?
}

struct BonjourHit: Encodable {
  let service: String
  let name: String
  let host: String
  let addresses: [String]
  let port: Int
}

struct BrowseResult: Encodable {
  let hits: [BonjourHit]
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
    DispatchQueue.main.async {
      self.webview = webview
      self.installNetBridge(webview)
      // Ask at launch, like the Android shell's requestLocalNetwork() in onCreate.
      self.startLocalNetworkProbe()
      NotificationCenter.default.addObserver(
        self, selector: #selector(self.appWillEnterForeground),
        name: UIApplication.willEnterForegroundNotification, object: nil)
    }
  }

  // MARK: Local network permission (the iOS side of Android's PS5UploadNet)
  //
  // iOS blocks every connection to the PS5 until the user allows "Local
  // Network" for the app, and has no API to read that setting. A Bonjour
  // browse both shows the prompt (at launch) and reveals a denial: it waits
  // with kDNSServiceErr_PolicyDenied. The page sees it through the same
  // window.PS5UploadNet { granted(), request() } bridge the Android shell
  // injects, so the banner and the Connection hint work unchanged.

  private weak var webview: WKWebView?
  private var netBrowser: NWBrowser?
  private var netDenied = false
  private var bonjourSessions: [BonjourSession] = []

  private static let netBridgeJS = """
    (function () {
      if (window.PS5UploadNet) return;
      window.PS5UploadNet = {
        granted: function () {
          try { window.webkit.messageHandlers.ps5uploadNet.postMessage("query"); } catch (e) {}
          return window.__ps5uploadNetDenied !== true;
        },
        request: function () {
          try { window.webkit.messageHandlers.ps5uploadNet.postMessage("request"); } catch (e) {}
        }
      };
    })();
    """

  private func installNetBridge(_ webview: WKWebView) {
    let ucc = webview.configuration.userContentController
    ucc.removeScriptMessageHandler(forName: "ps5uploadNet")
    ucc.add(NetBridgeHandler(plugin: self), name: "ps5uploadNet")
    ucc.addUserScript(
      WKUserScript(source: Self.netBridgeJS, injectionTime: .atDocumentStart, forMainFrameOnly: true))
    webview.evaluateJavaScript(Self.netBridgeJS, completionHandler: nil)
  }

  fileprivate func handleNetMessage(_ body: String) {
    if body == "request" {
      if netDenied, let url = URL(string: UIApplication.openSettingsURLString) {
        // iOS shows its prompt once; after a denial only Settings can allow it.
        UIApplication.shared.open(url, options: [:], completionHandler: nil)
      } else {
        startLocalNetworkProbe()
      }
    } else {
      pushNetState()
    }
  }

  private func pushNetState() {
    webview?.evaluateJavaScript(
      "window.__ps5uploadNetDenied = \(netDenied ? "true" : "false");", completionHandler: nil)
  }

  private func setNetDenied(_ denied: Bool) {
    netDenied = denied
    pushNetState()
  }

  private func startLocalNetworkProbe() {
    netBrowser?.cancel()
    let params = NWParameters()
    params.includePeerToPeer = false
    let browser = NWBrowser(for: .bonjour(type: "_sonic-loader._tcp", domain: nil), using: params)
    browser.stateUpdateHandler = { [weak self] state in
      switch state {
      case .ready:
        self?.setNetDenied(false)
      case .waiting(let error), .failed(let error):
        if case .dns(let code) = error, Int(code) == -65570 {  // kDNSServiceErr_PolicyDenied
          self?.setNetDenied(true)
        }
      default:
        break
      }
    }
    browser.browseResultsChangedHandler = { _, _ in }
    browser.start(queue: .main)
    netBrowser = browser
  }

  /// Settings may have changed while the app was in the background.
  @objc private func appWillEnterForeground() {
    startLocalNetworkProbe()
  }

  // MARK: Bonjour discovery
  //
  // The mdns-sd crate the other platforms use needs raw multicast sockets,
  // which iOS refuses without Apple's multicast entitlement. The system's
  // Bonjour (NetServiceBrowser) needs only the Local Network permission and
  // the NSBonjourServices list, so discover_ps5 asks here on iOS.

  @objc public func bonjourBrowse(_ invoke: Invoke) throws {
    let args = try invoke.parseArgs(BrowseArgs.self)
    DispatchQueue.main.async {
      let session = BonjourSession()
      self.bonjourSessions.append(session)
      session.start(types: args.types)
      let ms = max(500, min(args.timeoutMs ?? 3000, 10000))
      DispatchQueue.main.asyncAfter(deadline: .now() + .milliseconds(ms)) {
        session.stop()
        self.bonjourSessions.removeAll { $0 === session }
        invoke.resolve(BrowseResult(hits: session.hits))
      }
    }
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

/// Receives window.PS5UploadNet calls. A separate object so the content
/// controller's strong reference doesn't keep the plugin alive.
private final class NetBridgeHandler: NSObject, WKScriptMessageHandler {
  weak var plugin: IosPickerPlugin?

  init(plugin: IosPickerPlugin) {
    self.plugin = plugin
  }

  func userContentController(
    _ userContentController: WKUserContentController, didReceive message: WKScriptMessage
  ) {
    let body = message.body as? String ?? ""
    DispatchQueue.main.async { self.plugin?.handleNetMessage(body) }
  }
}

/// One discovery run: browses every type, resolves what it finds, and keeps
/// the numeric addresses. Lives on the main run loop.
private final class BonjourSession: NSObject, NetServiceBrowserDelegate, NetServiceDelegate {
  private var browsers: [NetServiceBrowser] = []
  private var services: [NetService] = []
  private(set) var hits: [BonjourHit] = []

  func start(types: [String]) {
    for type in types {
      let browser = NetServiceBrowser()
      browser.delegate = self
      browsers.append(browser)
      browser.searchForServices(ofType: type, inDomain: "local.")
    }
  }

  func stop() {
    browsers.forEach { $0.stop() }
    services.forEach { $0.stop() }
  }

  func netServiceBrowser(
    _ browser: NetServiceBrowser, didFind service: NetService, moreComing: Bool
  ) {
    services.append(service)
    service.delegate = self
    service.resolve(withTimeout: 2.0)
  }

  func netServiceDidResolveAddress(_ sender: NetService) {
    let addresses = (sender.addresses ?? []).compactMap(Self.numericHost)
    hits.append(
      BonjourHit(
        service: sender.type, name: sender.name, host: sender.hostName ?? "",
        addresses: addresses, port: sender.port))
  }

  private static func numericHost(_ data: Data) -> String? {
    data.withUnsafeBytes { (raw: UnsafeRawBufferPointer) -> String? in
      guard let base = raw.baseAddress else { return nil }
      let sa = base.assumingMemoryBound(to: sockaddr.self)
      var host = [CChar](repeating: 0, count: Int(NI_MAXHOST))
      let rc = getnameinfo(
        sa, socklen_t(data.count), &host, socklen_t(host.count), nil, 0, NI_NUMERICHOST)
      return rc == 0 ? String(cString: host) : nil
    }
  }
}

@_cdecl("init_plugin_ios_picker")
func initPlugin() -> Plugin {
  return IosPickerPlugin()
}
