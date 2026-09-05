package net.napstr.nostrfy

import android.content.Intent
import android.os.Build
import android.webkit.JavascriptInterface
import android.webkit.WebView
import java.lang.ref.WeakReference
import org.json.JSONObject

class MediaControlBridge(private val activity: MainActivity) {
  @JavascriptInterface
  fun update(payload: String) {
    val state = try {
      JSONObject(payload)
    } catch (_: Exception) {
      return
    }

    activity.runOnUiThread { activity.ensureMediaNotificationPermission() }
    val intent = Intent(activity, MediaNotificationService::class.java).apply {
      action = MediaNotificationService.ACTION_UPDATE
      putExtra(MediaNotificationService.EXTRA_TITLE, safeText(state.optString("title"), 300))
      putExtra(MediaNotificationService.EXTRA_ARTIST, safeText(state.optString("artist"), 300))
      putExtra(MediaNotificationService.EXTRA_PLAYING, state.optBoolean("playing"))
      putExtra(MediaNotificationService.EXTRA_POSITION, state.optDouble("position").coerceIn(0.0, MAX_SECONDS).toLong() * 1000L)
      putExtra(MediaNotificationService.EXTRA_DURATION, state.optDouble("duration").coerceIn(0.0, MAX_SECONDS).toLong() * 1000L)
      putExtra(MediaNotificationService.EXTRA_CAN_PREVIOUS, state.optBoolean("canPrevious"))
      putExtra(MediaNotificationService.EXTRA_CAN_NEXT, state.optBoolean("canNext"))
    }
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) activity.startForegroundService(intent)
    else activity.startService(intent)
  }

  @JavascriptInterface
  fun clear() {
    activity.stopService(Intent(activity, MediaNotificationService::class.java))
  }

  private fun safeText(value: String, limit: Int): String =
    value.filter { it >= ' ' && it != '\u007f' }.take(limit)

  companion object {
    private const val MAX_SECONDS = 60.0 * 60.0 * 24.0 * 14.0
    private var webView = WeakReference<WebView>(null)

    fun attach(next: WebView) {
      webView = WeakReference(next)
    }

    fun detach() {
      webView.clear()
    }

    fun dispatch(action: String) {
      if (action !in setOf("play", "pause", "previous", "next") && !action.startsWith("seek:")) return
      webView.get()?.post {
        val encoded = JSONObject.quote(action)
        webView.get()?.evaluateJavascript(
          "window.dispatchEvent(new CustomEvent('napstrfy-media-action',{detail:$encoded}))",
          null
        )
      }
    }
  }
}
