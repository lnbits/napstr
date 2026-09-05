package net.napstr.nostrfy

import android.Manifest
import android.content.pm.PackageManager
import android.os.Build
import android.os.Bundle
import android.webkit.WebSettings
import android.webkit.WebView
import androidx.activity.enableEdgeToEdge
import androidx.core.app.ActivityCompat
import androidx.core.content.ContextCompat

class MainActivity : TauriActivity() {
  private var notificationPermissionRequested = false

  override fun onCreate(savedInstanceState: Bundle?) {
    enableEdgeToEdge()
    super.onCreate(savedInstanceState)
  }

  override fun onWebViewCreate(webView: WebView) {
    super.onWebViewCreate(webView)
    // The bundled UI is secure, while private audio is served by the
    // token-protected loopback server allowed by the CSP and network policy.
    webView.settings.mixedContentMode = WebSettings.MIXED_CONTENT_ALWAYS_ALLOW
    MediaControlBridge.attach(webView)
    webView.addJavascriptInterface(MediaControlBridge(this), "NapstrfyMedia")
  }

  fun ensureMediaNotificationPermission() {
    if (
      Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU &&
      ContextCompat.checkSelfPermission(this, Manifest.permission.POST_NOTIFICATIONS) !=
        PackageManager.PERMISSION_GRANTED &&
      !notificationPermissionRequested
    ) {
      notificationPermissionRequested = true
      ActivityCompat.requestPermissions(
        this,
        arrayOf(Manifest.permission.POST_NOTIFICATIONS),
        MEDIA_NOTIFICATION_PERMISSION
      )
    }
  }

  override fun onDestroy() {
    MediaControlBridge.detach()
    super.onDestroy()
  }

  companion object {
    private const val MEDIA_NOTIFICATION_PERMISSION = 7301
  }
}
