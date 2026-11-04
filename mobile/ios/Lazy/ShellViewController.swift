import UIKit
import WebKit

/// The phone app (D49): the server's web client in a web view. Screens and application logic live
/// in `lazy-web`, so a deploy of the web client reaches this app without a store release. This file
/// holds only what a page cannot do: choosing a server, the screen for when it cannot be reached,
/// JavaScript's dialogs (a web view has none of its own), and downloads.
///
/// The page knows it is in the app from `LazyShell/1` in the user agent and asks for a native
/// screen by going to a `lazy-shell://` address. There are deliberately no script message
/// handlers: a page gets no native calls.
final class ShellViewController: UIViewController, WKNavigationDelegate, WKUIDelegate, WKDownloadDelegate,
    UITextFieldDelegate
{
    private var webView: WKWebView?
    private var server: URL?
    /// A screen over the web view: the server screen or the no-connection one.
    private var cover: UIView?
    private var serverField: UITextField?
    private var serverStatus: UILabel?
    private var connectButton: UIButton?
    private var downloaded: [ObjectIdentifier: URL] = [:]
    private var lastPageCrash: Date?

    private let surface = UIColor { traits in
        traits.userInterfaceStyle == .dark
            ? UIColor(red: 0x1F / 255, green: 0x3A / 255, blue: 0x31 / 255, alpha: 1)
            : UIColor(red: 0xFF / 255, green: 0xFD / 255, blue: 0xFA / 255, alpha: 1)
    }

    override func viewDidLoad() {
        super.viewDidLoad()
        view.backgroundColor = surface
        // A launch argument (`-server <address>`) lands here too, which is how the UI test starts.
        if let saved = UserDefaults.standard.string(forKey: "server"), let address = Self.normalize(saved) {
            open(address)
        } else {
            showServerScreen(canCancel: false)
        }
    }

    // MARK: The server

    /// An address a person typed, as an origin; nil if it cannot be one.
    static func normalize(_ typed: String) -> URL? {
        var text = typed.trimmingCharacters(in: .whitespacesAndNewlines)
        while text.hasSuffix("/") { text.removeLast() }
        if text.isEmpty { return nil }
        if !text.contains("://") { text = "https://" + text }
        guard let parts = URLComponents(string: text), let host = parts.host, !host.isEmpty else { return nil }
        var allowed = parts.scheme == "https"
        #if DEBUG
            allowed = allowed || parts.scheme == "http"
        #endif
        if !allowed { return nil }
        var origin = URLComponents()
        origin.scheme = parts.scheme
        origin.host = host
        origin.port = parts.port
        return origin.url
    }

    /// Asks the server whether it is a Lazy server offering the web client; what went wrong if not.
    private func check(_ address: URL) async -> String? {
        do {
            let (health, healthResponse) = try await URLSession.shared.data(
                from: address.appendingPathComponent("healthz"))
            guard (healthResponse as? HTTPURLResponse)?.statusCode == 200,
                String(decoding: health, as: UTF8.self).contains("protocol_version")
            else {
                return "That address answered, but it isn't a Lazy server."
            }
            let (_, pageResponse) = try await URLSession.shared.data(
                from: address.appendingPathComponent("web/"))
            guard (pageResponse as? HTTPURLResponse)?.statusCode == 200 else {
                return "This Lazy server doesn't offer the web client the app needs."
            }
            return nil
        } catch {
            return "Could not reach \(address.host ?? "that address"). Check the address and your connection."
        }
    }

    private func showServerScreen(canCancel: Bool) {
        let title = UILabel()
        title.text = "Lazy"
        title.font = .systemFont(ofSize: 34, weight: .bold)

        let label = UILabel()
        label.text = "Your server's address"

        let field = UITextField()
        field.placeholder = "chat.example.com"
        field.borderStyle = .roundedRect
        field.autocapitalizationType = .none
        field.autocorrectionType = .no
        field.keyboardType = .URL
        field.returnKeyType = .go
        field.textContentType = .URL
        field.accessibilityLabel = "Server address"
        field.delegate = self
        if let server { field.text = server.absoluteString.replacingOccurrences(of: "https://", with: "") }

        let status = UILabel()
        status.numberOfLines = 0
        status.font = .preferredFont(forTextStyle: .footnote)

        let connect = UIButton(configuration: .borderedProminent())
        connect.setTitle("Connect", for: .normal)
        connect.addAction(UIAction { [weak self] _ in self?.submitServer() }, for: .touchUpInside)

        serverField = field
        serverStatus = status
        connectButton = connect

        var views: [UIView] = [title, label, field, status, connect]
        if canCancel {
            let cancel = UIButton(configuration: .bordered())
            cancel.setTitle("Cancel", for: .normal)
            cancel.addAction(UIAction { [weak self] _ in self?.removeCover() }, for: .touchUpInside)
            views.append(cancel)
        }
        setCover(views)
    }

    func textFieldShouldReturn(_ textField: UITextField) -> Bool {
        submitServer()
        return true
    }

    private func submitServer() {
        guard let address = Self.normalize(serverField?.text ?? "") else {
            serverStatus?.text = "Enter an address like chat.example.com. It has to use HTTPS."
            return
        }
        connectButton?.isEnabled = false
        serverStatus?.text = "Connecting…"
        Task { @MainActor in
            let failure = await check(address)
            connectButton?.isEnabled = true
            if let failure {
                serverStatus?.text = failure
            } else {
                UserDefaults.standard.set(address.absoluteString, forKey: "server")
                open(address)
            }
        }
    }

    private func showOffline() {
        let title = UILabel()
        title.text = "Can't reach \(server?.host ?? "the server")"
        title.numberOfLines = 0
        title.font = .systemFont(ofSize: 24, weight: .bold)

        let body = UILabel()
        body.numberOfLines = 0
        body.text = "Check your connection, or try again in a moment."

        let retry = UIButton(configuration: .borderedProminent())
        retry.setTitle("Try again", for: .normal)
        retry.addAction(
            UIAction { [weak self] _ in
                guard let self else { return }
                self.removeCover()
                if let server = self.server {
                    self.webView?.load(URLRequest(url: server.appendingPathComponent("web/")))
                }
            }, for: .touchUpInside)

        let change = UIButton(configuration: .bordered())
        change.setTitle("Change server", for: .normal)
        change.addAction(UIAction { [weak self] _ in self?.showServerScreen(canCancel: false) }, for: .touchUpInside)

        setCover([title, body, retry, change])
    }

    private func setCover(_ views: [UIView]) {
        removeCover()
        let screen = UIView()
        screen.backgroundColor = surface
        screen.translatesAutoresizingMaskIntoConstraints = false
        view.addSubview(screen)

        let stack = UIStackView(arrangedSubviews: views)
        stack.axis = .vertical
        stack.spacing = 12
        stack.translatesAutoresizingMaskIntoConstraints = false
        screen.addSubview(stack)

        NSLayoutConstraint.activate([
            screen.topAnchor.constraint(equalTo: view.topAnchor),
            screen.bottomAnchor.constraint(equalTo: view.bottomAnchor),
            screen.leadingAnchor.constraint(equalTo: view.leadingAnchor),
            screen.trailingAnchor.constraint(equalTo: view.trailingAnchor),
            stack.leadingAnchor.constraint(equalTo: screen.safeAreaLayoutGuide.leadingAnchor, constant: 24),
            stack.trailingAnchor.constraint(equalTo: screen.safeAreaLayoutGuide.trailingAnchor, constant: -24),
            stack.topAnchor.constraint(equalTo: screen.safeAreaLayoutGuide.topAnchor, constant: 48),
        ])
        cover = screen
    }

    private func removeCover() {
        cover?.removeFromSuperview()
        cover = nil
    }

    // MARK: The page

    private func open(_ address: URL) {
        removeCover()
        let changed = server != address
        server = address
        let page = webView ?? makeWebView()
        if changed { page.load(URLRequest(url: address.appendingPathComponent("web/"))) }
    }

    private func makeWebView() -> WKWebView {
        let configuration = WKWebViewConfiguration()
        configuration.applicationNameForUserAgent = "LazyShell/1"
        configuration.allowsInlineMediaPlayback = true
        let page = WKWebView(frame: .zero, configuration: configuration)
        page.navigationDelegate = self
        page.uiDelegate = self
        page.allowsLinkPreview = false
        page.isOpaque = false
        page.backgroundColor = surface
        page.scrollView.backgroundColor = surface
        page.scrollView.bounces = false
        // The page handles the safe areas itself, with env(safe-area-inset-*).
        page.scrollView.contentInsetAdjustmentBehavior = .never
        #if DEBUG
            page.isInspectable = true
        #endif
        page.translatesAutoresizingMaskIntoConstraints = false
        view.insertSubview(page, at: 0)
        NSLayoutConstraint.activate([
            page.topAnchor.constraint(equalTo: view.topAnchor),
            page.bottomAnchor.constraint(equalTo: view.bottomAnchor),
            page.leadingAnchor.constraint(equalTo: view.leadingAnchor),
            page.trailingAnchor.constraint(equalTo: view.trailingAnchor),
        ])
        webView = page
        return page
    }

    private func isSameServer(_ url: URL) -> Bool {
        guard let server else { return false }
        return url.scheme == server.scheme && url.host == server.host && url.port == server.port
    }

    func webView(
        _ webView: WKWebView,
        decidePolicyFor navigationAction: WKNavigationAction,
        decisionHandler: @escaping (WKNavigationActionPolicy) -> Void
    ) {
        guard let url = navigationAction.request.url else {
            decisionHandler(.cancel)
            return
        }
        if url.scheme == "lazy-shell" {
            if url.host == "change-server" { showServerScreen(canCancel: true) }
            decisionHandler(.cancel)
            return
        }
        // A frame that is not the main one stays with the page's content security policy.
        if let frame = navigationAction.targetFrame, !frame.isMainFrame {
            decisionHandler(.allow)
            return
        }
        if navigationAction.shouldPerformDownload {
            decisionHandler(isSameServer(url) ? .download : .cancel)
            return
        }
        if isSameServer(url) {
            // The app stays on its page; anything else of the server's is a file.
            decisionHandler(url.path.hasPrefix("/web") ? .allow : .download)
            return
        }
        // Other sites open in the person's browser. Nothing else is followed.
        if let scheme = url.scheme, ["http", "https", "mailto"].contains(scheme) {
            UIApplication.shared.open(url)
        }
        decisionHandler(.cancel)
    }

    func webView(
        _ webView: WKWebView,
        decidePolicyFor navigationResponse: WKNavigationResponse,
        decisionHandler: @escaping (WKNavigationResponsePolicy) -> Void
    ) {
        let response = navigationResponse.response as? HTTPURLResponse
        if navigationResponse.isForMainFrame, let response, response.statusCode >= 400,
            response.url?.path.hasPrefix("/web") == true
        {
            decisionHandler(.cancel)
            showOffline()
            return
        }
        let attachment =
            response?.value(forHTTPHeaderField: "Content-Disposition")?.lowercased().hasPrefix("attachment") == true
        decisionHandler(navigationResponse.canShowMIMEType && !attachment ? .allow : .download)
    }

    func webView(_ webView: WKWebView, navigationAction: WKNavigationAction, didBecome download: WKDownload) {
        download.delegate = self
    }

    func webView(_ webView: WKWebView, navigationResponse: WKNavigationResponse, didBecome download: WKDownload) {
        download.delegate = self
    }

    func webView(
        _ webView: WKWebView, didFailProvisionalNavigation navigation: WKNavigation!, withError error: Error
    ) {
        let failure = error as NSError
        // Cancelled by this class: a download, or a link sent elsewhere.
        if failure.domain == NSURLErrorDomain && failure.code == NSURLErrorCancelled { return }
        if failure.domain == "WebKitErrorDomain" && failure.code == 102 { return }
        showOffline()
    }

    // The page's process died, which iOS also does to save memory: start it again. If it dies again
    // straight away, stop and say so, since reloading would only repeat it forever.
    func webViewWebContentProcessDidTerminate(_ webView: WKWebView) {
        let now = Date()
        let again = lastPageCrash.map { now.timeIntervalSince($0) < 60 } ?? false
        lastPageCrash = now
        if again {
            showOffline()
        } else {
            webView.reload()
        }
    }

    // MARK: JavaScript's dialogs, which do nothing in a web view unless the app shows them

    func webView(
        _ webView: WKWebView, runJavaScriptAlertPanelWithMessage message: String,
        initiatedByFrame frame: WKFrameInfo, completionHandler: @escaping () -> Void
    ) {
        let alert = UIAlertController(title: nil, message: message, preferredStyle: .alert)
        alert.addAction(UIAlertAction(title: "OK", style: .default) { _ in completionHandler() })
        present(alert, animated: true)
    }

    func webView(
        _ webView: WKWebView, runJavaScriptConfirmPanelWithMessage message: String,
        initiatedByFrame frame: WKFrameInfo, completionHandler: @escaping (Bool) -> Void
    ) {
        let alert = UIAlertController(title: nil, message: message, preferredStyle: .alert)
        alert.addAction(UIAlertAction(title: "Cancel", style: .cancel) { _ in completionHandler(false) })
        alert.addAction(UIAlertAction(title: "OK", style: .default) { _ in completionHandler(true) })
        present(alert, animated: true)
    }

    func webView(
        _ webView: WKWebView, runJavaScriptTextInputPanelWithPrompt prompt: String, defaultText: String?,
        initiatedByFrame frame: WKFrameInfo, completionHandler: @escaping (String?) -> Void
    ) {
        let alert = UIAlertController(title: nil, message: prompt, preferredStyle: .alert)
        alert.addTextField { $0.text = defaultText }
        alert.addAction(UIAlertAction(title: "Cancel", style: .cancel) { _ in completionHandler(nil) })
        alert.addAction(
            UIAlertAction(title: "OK", style: .default) { [weak alert] _ in
                completionHandler(alert?.textFields?.first?.text)
            })
        present(alert, animated: true)
    }

    // MARK: Downloads

    func download(
        _ download: WKDownload, decideDestinationUsing response: URLResponse, suggestedFilename: String,
        completionHandler: @escaping (URL?) -> Void
    ) {
        let folder = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        do {
            try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
        } catch {
            completionHandler(nil)
            return
        }
        let name = (suggestedFilename as NSString).lastPathComponent
        let file = folder.appendingPathComponent(name.isEmpty ? "download" : name)
        downloaded[ObjectIdentifier(download)] = file
        completionHandler(file)
    }

    func downloadDidFinish(_ download: WKDownload) {
        guard let file = downloaded.removeValue(forKey: ObjectIdentifier(download)) else { return }
        // The system's share sheet is where a person saves it to Files or Photos, or opens it.
        let sheet = UIActivityViewController(activityItems: [file], applicationActivities: nil)
        sheet.popoverPresentationController?.sourceView = view
        sheet.popoverPresentationController?.sourceRect = CGRect(
            x: view.bounds.midX, y: view.bounds.midY, width: 0, height: 0)
        present(sheet, animated: true)
    }

    func download(_ download: WKDownload, didFailWithError error: Error, resumeData: Data?) {
        downloaded.removeValue(forKey: ObjectIdentifier(download))
        let alert = UIAlertController(title: nil, message: "The file could not be downloaded.", preferredStyle: .alert)
        alert.addAction(UIAlertAction(title: "OK", style: .default))
        present(alert, animated: true)
    }
}
