package io.github.last_refuge.lazy

import android.app.Activity
import android.app.DownloadManager
import android.content.ActivityNotFoundException
import android.content.Intent
import android.content.pm.ApplicationInfo
import android.content.res.Configuration
import android.graphics.Color
import android.net.Uri
import android.net.http.SslError
import android.os.Build
import android.os.Bundle
import android.os.Environment
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import android.text.InputType
import android.view.Gravity
import android.view.View
import android.view.ViewGroup
import android.view.WindowInsets
import android.view.WindowInsetsController
import android.view.inputmethod.EditorInfo
import android.webkit.CookieManager
import android.webkit.RenderProcessGoneDetail
import android.webkit.SslErrorHandler
import android.webkit.URLUtil
import android.webkit.ValueCallback
import android.webkit.WebChromeClient
import android.webkit.WebResourceError
import android.webkit.WebResourceRequest
import android.webkit.WebResourceResponse
import android.webkit.WebView
import android.webkit.WebViewClient
import android.widget.Button
import android.widget.EditText
import android.widget.FrameLayout
import android.widget.LinearLayout
import android.widget.TextView
import android.widget.Toast
import java.net.HttpURLConnection
import java.net.URL

/**
 * The phone app (D49): the server's web client in a web view. Screens and application logic live
 * in `lazy-web`, so a deploy of the web client reaches this app without a store release. This file
 * holds only what a page cannot do: choosing a server, the screen for when it cannot be reached,
 * the file chooser, downloads, the back button, and fitting the page around the system's bars and
 * keyboard.
 *
 * The page knows it is in the app from `LazyShell/1` in the user agent, asks for a native screen
 * by going to a `lazy-shell://` address, and answers the back button through `window.lazyBack()`.
 * There is deliberately no JavaScript interface: a page gets no native calls.
 */
class MainActivity : Activity() {
    private lateinit var root: FrameLayout
    private var web: WebView? = null
    private var server: Uri? = null
    /** A screen over the web view (the server screen or the no-connection one). */
    private var cover: View? = null
    private var coverCanCancel = false
    private var pickingFiles: ValueCallback<Array<Uri>>? = null
    private var lastPageCrash = 0L
    private val main = Handler(Looper.getMainLooper())
    private val debuggable get() = applicationInfo.flags and ApplicationInfo.FLAG_DEBUGGABLE != 0

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        window.setDecorFitsSystemWindows(false)
        root = FrameLayout(this)
        // The bars and the keyboard take their space from the web view, so a page never sits under
        // them. That also keeps env(safe-area-inset-*) at zero here, whichever WebView is installed.
        root.setOnApplyWindowInsetsListener { view, insets ->
            val taken = insets.getInsets(
                WindowInsets.Type.systemBars() or WindowInsets.Type.displayCutout() or WindowInsets.Type.ime(),
            )
            view.setPadding(taken.left, taken.top, taken.right, taken.bottom)
            WindowInsets.CONSUMED
        }
        setContentView(root)
        applyTheme()
        if (Build.VERSION.SDK_INT >= 33) {
            onBackInvokedDispatcher.registerOnBackInvokedCallback(0) { goBack() }
        }
        WebView.setWebContentsDebuggingEnabled(debuggable)

        // Debug builds can be pointed at a server by the test harness.
        val given = if (debuggable) intent.getStringExtra("server") else null
        val saved = given ?: getSharedPreferences("lazy", MODE_PRIVATE).getString("server", null)
        val address = saved?.let { normalize(it) }
        if (address != null) open(address) else showServerScreen(canCancel = false, message = null)
    }

    private val night
        get() = resources.configuration.uiMode and Configuration.UI_MODE_NIGHT_MASK == Configuration.UI_MODE_NIGHT_YES

    private fun applyTheme() {
        root.setBackgroundColor(Color.parseColor(if (night) "#1F3A31" else "#FFFDFA"))
        val light = WindowInsetsController.APPEARANCE_LIGHT_STATUS_BARS or WindowInsetsController.APPEARANCE_LIGHT_NAVIGATION_BARS
        window.insetsController?.setSystemBarsAppearance(if (night) 0 else light, light)
    }

    override fun onConfigurationChanged(newConfig: Configuration) {
        super.onConfigurationChanged(newConfig)
        applyTheme()
    }

    // ----- The server -----

    /** An address a person typed, as an origin; null if it cannot be one. */
    private fun normalize(typed: String): Uri? {
        val text = typed.trim().trimEnd('/')
        if (text.isEmpty()) return null
        val uri = Uri.parse(if (text.contains("://")) text else "https://$text")
        val allowed = uri.scheme == "https" || (debuggable && uri.scheme == "http")
        if (!allowed || uri.host.isNullOrEmpty()) return null
        return uri.buildUpon().path("").clearQuery().fragment(null).build()
    }

    /** Asks the server whether it is a Lazy server offering the web client. */
    private fun check(address: Uri, done: (String?) -> Unit) {
        Thread {
            val failure = try {
                val health = get("$address/healthz")
                if (health.first != 200 || !health.second.contains("protocol_version")) {
                    "That address answered, but it isn't a Lazy server."
                } else if (get("$address/web/").first != 200) {
                    "This Lazy server doesn't offer the web client the app needs."
                } else {
                    null
                }
            } catch (_: Exception) {
                "Could not reach ${address.host}. Check the address and your connection."
            }
            main.post { done(failure) }
        }.start()
    }

    private fun get(url: String): Pair<Int, String> {
        val connection = URL(url).openConnection() as HttpURLConnection
        connection.connectTimeout = 10_000
        connection.readTimeout = 10_000
        try {
            val status = connection.responseCode
            val body = if (status == 200) connection.inputStream.bufferedReader().use { it.readText().take(4096) } else ""
            return status to body
        } finally {
            connection.disconnect()
        }
    }

    private fun showServerScreen(canCancel: Boolean, message: String?) {
        val column = screen()
        column.addView(title("Lazy"))
        column.addView(text("Your server's address"))
        val field = EditText(this).apply {
            hint = "chat.example.com"
            isSingleLine = true
            inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_VARIATION_URI
            imeOptions = EditorInfo.IME_ACTION_GO
            contentDescription = "Server address"
            setText(server?.toString()?.removePrefix("https://") ?: "")
        }
        column.addView(field)
        val status = text(message ?: "")
        column.addView(status)
        val connect = Button(this).apply { setText("Connect") }
        column.addView(connect)
        if (canCancel) {
            column.addView(Button(this).apply {
                setText("Cancel")
                setOnClickListener { removeCover() }
            })
        }
        val submit = {
            val address = normalize(field.text.toString())
            if (address == null) {
                status.text = "Enter an address like chat.example.com. It has to use HTTPS."
            } else {
                connect.isEnabled = false
                status.text = "Connecting…"
                check(address) { failure ->
                    connect.isEnabled = true
                    if (failure != null) {
                        status.text = failure
                    } else {
                        getSharedPreferences("lazy", MODE_PRIVATE).edit().putString("server", address.toString()).apply()
                        open(address)
                    }
                }
            }
        }
        connect.setOnClickListener { submit() }
        field.setOnEditorActionListener { _, action, _ ->
            if (action == EditorInfo.IME_ACTION_GO) {
                submit()
                true
            } else {
                false
            }
        }
        setCover(column, canCancel)
    }

    private fun showOffline() {
        val column = screen()
        column.addView(title("Can't reach ${server?.host ?: "the server"}"))
        column.addView(text("Check your connection, or try again in a moment."))
        column.addView(Button(this).apply {
            setText("Try again")
            setOnClickListener {
                removeCover()
                val view = web
                if (view != null) {
                    view.reload()
                } else {
                    server?.let { address ->
                        server = null
                        open(address)
                    }
                }
            }
        })
        column.addView(Button(this).apply {
            setText("Change server")
            setOnClickListener { showServerScreen(canCancel = false, message = null) }
        })
        setCover(column, canCancel = false)
    }

    private fun screen() = LinearLayout(this).apply {
        orientation = LinearLayout.VERTICAL
        gravity = Gravity.CENTER_VERTICAL
        val pad = (24 * resources.displayMetrics.density).toInt()
        setPadding(pad, pad, pad, pad)
        setBackgroundColor(Color.parseColor(if (night) "#1F3A31" else "#FFFDFA"))
        // Tapping the screen must not reach the page underneath.
        isClickable = true
    }

    private fun title(words: String) = TextView(this).apply {
        text = words
        textSize = 28f
        setPadding(0, 0, 0, (16 * resources.displayMetrics.density).toInt())
    }

    private fun text(words: String) = TextView(this).apply {
        text = words
        textSize = 16f
    }

    private fun setCover(view: View, canCancel: Boolean) {
        cover?.let { root.removeView(it) }
        cover = view
        coverCanCancel = canCancel
        root.addView(view, FrameLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.MATCH_PARENT))
    }

    private fun removeCover() {
        cover?.let { root.removeView(it) }
        cover = null
    }

    // ----- The page -----

    private fun open(address: Uri) {
        removeCover()
        val changed = server != address
        server = address
        val view = web ?: makeWebView().also {
            web = it
            root.addView(it, 0, FrameLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.MATCH_PARENT))
        }
        if (changed) view.loadUrl("$address/web/")
    }

    private fun makeWebView() = WebView(this).apply {
        setBackgroundColor(Color.TRANSPARENT)
        settings.apply {
            javaScriptEnabled = true
            domStorageEnabled = true
            allowFileAccess = false
            allowContentAccess = false
            setSupportMultipleWindows(false)
            userAgentString = "$userAgentString LazyShell/1"
        }
        webViewClient = Pages()
        webChromeClient = Chrome()
        setDownloadListener { url, _, disposition, mediaType, _ -> save(url, disposition, mediaType) }
    }

    private fun sameServer(uri: Uri): Boolean {
        val server = server ?: return false
        return uri.scheme == server.scheme && uri.host == server.host && uri.port == server.port
    }

    private inner class Pages : WebViewClient() {
        override fun shouldOverrideUrlLoading(view: WebView, request: WebResourceRequest): Boolean {
            val uri = request.url
            if (uri.scheme == "lazy-shell") {
                if (uri.host == "change-server") showServerScreen(canCancel = true, message = null)
                return true
            }
            if (!request.isForMainFrame) return false
            if (sameServer(uri)) {
                // The app stays on its page; anything else of the server's is a file.
                if (uri.path?.startsWith("/web") == true) return false
                save(uri.toString(), null, null)
                return true
            }
            // Other sites open in the person's browser. Nothing else is followed.
            if (uri.scheme in listOf("http", "https", "mailto")) {
                try {
                    startActivity(Intent(Intent.ACTION_VIEW, uri))
                } catch (_: ActivityNotFoundException) {
                    Toast.makeText(this@MainActivity, "No app can open that link.", Toast.LENGTH_SHORT).show()
                }
            }
            return true
        }

        override fun onReceivedError(view: WebView, request: WebResourceRequest, error: WebResourceError) {
            if (request.isForMainFrame) showOffline()
        }

        override fun onReceivedHttpError(view: WebView, request: WebResourceRequest, response: WebResourceResponse) {
            if (request.isForMainFrame) showOffline()
        }

        // A certificate that does not check out is never let through.
        override fun onReceivedSslError(view: WebView, handler: SslErrorHandler, error: SslError) {
            handler.cancel()
            if (error.url.startsWith(server.toString())) showOffline()
        }

        // The page's process died: start it again rather than taking the app down with it. If it
        // dies again straight away, stop and say so, since reloading would only repeat it forever.
        override fun onRenderProcessGone(view: WebView, detail: RenderProcessGoneDetail): Boolean {
            root.removeView(view)
            view.destroy()
            web = null
            val now = SystemClock.elapsedRealtime()
            val again = now - lastPageCrash < PAGE_CRASH_WINDOW_MS
            lastPageCrash = now
            if (again) {
                showOffline()
            } else {
                server?.let { address ->
                    server = null
                    open(address)
                }
            }
            return true
        }
    }

    private inner class Chrome : WebChromeClient() {
        override fun onShowFileChooser(
            view: WebView,
            callback: ValueCallback<Array<Uri>>,
            params: FileChooserParams,
        ): Boolean {
            pickingFiles?.onReceiveValue(null)
            pickingFiles = callback
            try {
                startActivityForResult(params.createIntent(), PICK_FILES)
            } catch (_: ActivityNotFoundException) {
                pickingFiles = null
                callback.onReceiveValue(null)
            }
            return true
        }
    }

    @Deprecated("The platform's file chooser still answers here.")
    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        super.onActivityResult(requestCode, resultCode, data)
        if (requestCode != PICK_FILES) return
        val picked = mutableListOf<Uri>()
        if (resultCode == RESULT_OK && data != null) {
            val several = data.clipData
            if (several != null) {
                for (index in 0 until several.itemCount) picked.add(several.getItemAt(index).uri)
            } else {
                data.data?.let { picked.add(it) }
            }
        }
        pickingFiles?.onReceiveValue(if (picked.isEmpty()) null else picked.toTypedArray())
        pickingFiles = null
    }

    /** Saves one of the server's files to Downloads, with the session, which only it may be sent. */
    private fun save(url: String, disposition: String?, mediaType: String?) {
        val uri = Uri.parse(url)
        if (!sameServer(uri)) return
        val cookies = CookieManager.getInstance().getCookie(url)
        val agent = web?.settings?.userAgentString
        Thread {
            // A link gives no name or type, so ask the server for them.
            var named = disposition
            var type = mediaType
            if (named.isNullOrEmpty()) {
                try {
                    val connection = URL(url).openConnection() as HttpURLConnection
                    connection.setRequestProperty("Cookie", cookies)
                    connection.connectTimeout = 10_000
                    connection.responseCode
                    named = connection.getHeaderField("Content-Disposition")
                    type = connection.contentType
                    connection.disconnect()
                } catch (_: Exception) {
                }
            }
            val name = URLUtil.guessFileName(url, named, type)
            main.post {
                val request = DownloadManager.Request(uri)
                    .addRequestHeader("Cookie", cookies ?: "")
                    .addRequestHeader("User-Agent", agent ?: "")
                    .setTitle(name)
                    .setMimeType(type)
                    .setNotificationVisibility(DownloadManager.Request.VISIBILITY_VISIBLE_NOTIFY_COMPLETED)
                    .setDestinationInExternalPublicDir(Environment.DIRECTORY_DOWNLOADS, name)
                getSystemService(DownloadManager::class.java).enqueue(request)
                Toast.makeText(this, "Saving $name to Downloads", Toast.LENGTH_SHORT).show()
            }
        }.start()
    }

    // ----- The back button -----

    private fun goBack() {
        if (cover != null) {
            if (coverCanCancel) removeCover() else moveTaskToBack(true)
            return
        }
        val view = web
        if (view == null) {
            moveTaskToBack(true)
            return
        }
        view.evaluateJavascript("window.lazyBack ? window.lazyBack() : false") { closed ->
            if (closed != "true") moveTaskToBack(true)
        }
    }

    @Deprecated("Android 13 and later use the callback registered in onCreate.")
    override fun onBackPressed() {
        goBack()
    }

    // ----- Lifecycle -----

    override fun onPause() {
        CookieManager.getInstance().flush()
        web?.onPause()
        super.onPause()
    }

    override fun onResume() {
        super.onResume()
        web?.onResume()
    }

    override fun onDestroy() {
        web?.destroy()
        web = null
        super.onDestroy()
    }

    private companion object {
        const val PICK_FILES = 1
        const val PAGE_CRASH_WINDOW_MS = 60_000L
    }
}
