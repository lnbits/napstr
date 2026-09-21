package net.napstr.nostrfy

import android.webkit.JavascriptInterface
import android.webkit.WebView
import java.lang.ref.WeakReference

/**
 * Lets the page own the hardware back button.
 *
 * Kotlin cannot ask the page whether it has anything for back to close, so the
 * webview pushes that flag on every transition and after every press it handles.
 * While it is set, a back press is handed to the page and consumed; otherwise the
 * press goes back to the system, so back still leaves the app when the page has
 * nothing open.
 */
class BackBridge {
  @JavascriptInterface
  fun setBackAvailable(available: Boolean) {
    backAvailable = available
  }

  /** The older name for the same flag, for a page that predates this one. */
  @JavascriptInterface
  fun setDrawerOpen(open: Boolean) {
    backAvailable = open
  }

  companion object {
    private const val BACK_EVENT = "napstrfy-back"
    // Written from the webview's JS bridge thread and read from the UI thread.
    @Volatile private var backAvailable = false
    private var webView = WeakReference<WebView>(null)

    fun attach(next: WebView) {
      webView = WeakReference(next)
    }

    fun detach() {
      backAvailable = false
      webView.clear()
    }

    /**
     * True when the press was handed to the page rather than the system.
     *
     * Clearing the flag here is what makes one press one answer: the page
     * publishes again as soon as it has moved, which is why a page that still
     * has somewhere to go - the search tab under the liked page - is not left
     * with a flag that says otherwise and then goes unheard.
     */
    fun consumeBack(): Boolean {
      if (!backAvailable) return false
      backAvailable = false
      webView.get()?.post {
        webView.get()?.evaluateJavascript(
          "window.dispatchEvent(new CustomEvent('$BACK_EVENT'))",
          null
        )
      }
      return true
    }
  }
}
