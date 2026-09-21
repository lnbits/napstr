package net.napstr.nostrfy

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Intent
import android.graphics.Bitmap
import android.graphics.BitmapFactory
import android.media.AudioManager
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.IBinder
import android.os.Looper
import android.os.PowerManager
import android.support.v4.media.MediaMetadataCompat
import android.support.v4.media.session.MediaSessionCompat
import android.support.v4.media.session.PlaybackStateCompat
import androidx.core.app.NotificationCompat
import androidx.core.app.NotificationManagerCompat
import androidx.media.app.NotificationCompat.MediaStyle
import androidx.media.VolumeProviderCompat
import java.net.HttpURLConnection
import java.net.URL
import java.util.concurrent.Executors

class MediaNotificationService : Service() {
  private lateinit var mediaSession: MediaSessionCompat
  private var title = "Napstrfy"
  private var artist = ""
  private var previousLabel = "Previous"
  private var playLabel = "Play"
  private var pauseLabel = "Pause"
  private var nextLabel = "Next"
  private var rewindLabel = "Back 15 seconds"
  private var forwardLabel = "Forward 15 seconds"
  private var channelLabel = "Media playback"
  // Like and repeat are session custom actions, so their labels travel with
  // the state too rather than living here in English.
  private var likeLabelText = "Add to Liked Songs"
  private var unlikeLabelText = "Remove from Liked Songs"
  private var repeatLabelText = "Repeat"
  private var repeatOffLabelText = "Turn repeat off"
  private var playing = false
  private var position = 0L
  private var duration = 0L
  private var canPrevious = false
  private var canNext = false
  private var canSeek = false
  private var liked = false
  private var looping = false
  /** True while the session describes the computer's player, not this phone's. */
  private var remote = false
  private var volumeLevel = 0
  /**
   * Where the system's volume keys go while the computer is the player. Relative
   * control means Android asks for a step and this app decides its size; the
   * webview turns the step into a volume command for the computer.
   */
  private val remoteVolume = object : VolumeProviderCompat(
    VolumeProviderCompat.VOLUME_CONTROL_RELATIVE,
    100,
    0
  ) {
    override fun onAdjustVolume(direction: Int) {
      // ADJUST_SAME (0) means "show the level, do not change it", and the system
      // sends it whenever the volume panel is redrawn - which includes the
      // redraw straight after a real press. Treating anything that is not a
      // raise as a step down made Volume Up undo itself: the level rose, then
      // the redraw stepped it back. Volume Down looked correct only because
      // "lower" and "same" happened to point the same way.
      when (direction) {
        AudioManager.ADJUST_RAISE -> MediaControlBridge.dispatch("volumeUp")
        AudioManager.ADJUST_LOWER -> MediaControlBridge.dispatch("volumeDown")
      }
    }
  }
  private var foregroundStarted = false
  private var screenWakeLock: PowerManager.WakeLock? = null
  private var artworkUrl = ""
  private var artwork: Bitmap? = null
  private var artworkRequest = 0
  private val mainHandler = Handler(Looper.getMainLooper())
  private val artworkLoader = Executors.newSingleThreadExecutor { runnable ->
    Thread(runnable, "napstrfy-artwork").apply { isDaemon = true }
  }

  override fun onCreate() {
    super.onCreate()
    createChannel()
    mediaSession = MediaSessionCompat(this, "NapstrfyPlayback").apply {
      setCallback(object : MediaSessionCompat.Callback() {
        override fun onPlay() = dispatch(ACTION_PLAY)
        override fun onPause() = dispatch(ACTION_PAUSE)
        override fun onSkipToPrevious() = dispatch(ACTION_PREVIOUS)
        override fun onSkipToNext() = dispatch(ACTION_NEXT)
        override fun onRewind() = dispatch(ACTION_REWIND)
        override fun onFastForward() = dispatch(ACTION_FORWARD)
        override fun onSeekTo(pos: Long) {
          if (canSeek) MediaControlBridge.dispatch("seek:${pos.coerceAtLeast(0L)}")
        }

        /**
         * Buttons the system media controls draw itself - the lock screen, the
         * quick settings player, Android Auto - arrive here, not through the
         * notification's pending intents, because they come from the session.
         */
        override fun onCustomAction(action: String?, extras: Bundle?) {
          when (action) {
            ACTION_LIKE, ACTION_REPEAT, ACTION_REWIND, ACTION_FORWARD -> dispatch(action)
          }
        }
      })
      isActive = true
    }
  }

  override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
    when (intent?.action) {
      ACTION_UPDATE -> readState(intent)
      ACTION_CLEAR -> {
        stopForeground(STOP_FOREGROUND_REMOVE)
        foregroundStarted = false
        stopSelf()
        return START_NOT_STICKY
      }
      ACTION_PLAY, ACTION_PAUSE, ACTION_PREVIOUS, ACTION_NEXT, ACTION_LIKE,
      ACTION_REPEAT, ACTION_REWIND, ACTION_FORWARD -> {
        dispatch(intent.action!!)
        return START_NOT_STICKY
      }
      else -> return START_NOT_STICKY
    }

    updateSession()
    val notification = buildNotification()
    startForeground(NOTIFICATION_ID, notification)
    foregroundStarted = true
    return START_NOT_STICKY
  }

  private fun readState(intent: Intent) {
    title = intent.getStringExtra(EXTRA_TITLE)?.ifBlank { "Napstrfy" } ?: "Napstrfy"
    artist = intent.getStringExtra(EXTRA_ARTIST).orEmpty()
    playing = intent.getBooleanExtra(EXTRA_PLAYING, false)
    position = intent.getLongExtra(EXTRA_POSITION, 0L).coerceAtLeast(0L)
    duration = intent.getLongExtra(EXTRA_DURATION, 0L).coerceAtLeast(0L)
    canPrevious = intent.getBooleanExtra(EXTRA_CAN_PREVIOUS, false)
    canSeek = intent.getBooleanExtra(EXTRA_CAN_SEEK, false)
    previousLabel = intent.getStringExtra("label_previous")?.ifBlank { "Previous" } ?: "Previous"
    playLabel = intent.getStringExtra("label_play")?.ifBlank { "Play" } ?: "Play"
    pauseLabel = intent.getStringExtra("label_pause")?.ifBlank { "Pause" } ?: "Pause"
    nextLabel = intent.getStringExtra("label_next")?.ifBlank { "Next" } ?: "Next"
    rewindLabel = intent.getStringExtra("label_rewind")?.ifBlank { "Back 15 seconds" } ?: "Back 15 seconds"
    forwardLabel = intent.getStringExtra("label_forward")?.ifBlank { "Forward 15 seconds" } ?: "Forward 15 seconds"
    likeLabelText = intent.getStringExtra("label_like")?.ifBlank { "Add to Liked Songs" } ?: "Add to Liked Songs"
    unlikeLabelText = intent.getStringExtra("label_unlike")?.ifBlank { "Remove from Liked Songs" } ?: "Remove from Liked Songs"
    repeatLabelText = intent.getStringExtra("label_repeat")?.ifBlank { "Repeat" } ?: "Repeat"
    repeatOffLabelText = intent.getStringExtra("label_repeatOff")?.ifBlank { "Turn repeat off" } ?: "Turn repeat off"
    val nextChannelLabel = intent.getStringExtra("label_channel")?.ifBlank { "Media playback" } ?: "Media playback"
    if (nextChannelLabel != channelLabel) {
      channelLabel = nextChannelLabel
      createChannel()
    }
    liked = intent.getBooleanExtra(EXTRA_LIKED, false)
    looping = intent.getBooleanExtra(EXTRA_LOOPING, false)
    remote = intent.getBooleanExtra(EXTRA_REMOTE, false)
    volumeLevel = intent.getIntExtra(EXTRA_VOLUME, 0).coerceIn(0, 100)
    updateArtwork(intent.getStringExtra(EXTRA_ARTWORK).orEmpty())
    updateScreenWakeLock()
  }

  /**
   * The webview pushes the cover URL on every state change, so only a genuine
   * change is worth a fetch; anything already decoded is reused from the cache.
   */
  private fun updateArtwork(url: String) {
    if (url == artworkUrl) return
    artworkUrl = url
    artwork = ArtworkCache.peek(url)
    artworkRequest += 1
    if (artwork != null) return updateSession()
    if (url.isEmpty()) return updateSession()
    val request = artworkRequest
    artworkLoader.execute {
      val loaded = ArtworkCache.load(url)
      mainHandler.post {
        if (loaded == null || request != artworkRequest) return@post
        artwork = loaded
        updateSession()
        refreshNotification()
      }
    }
  }

  /** Re-posts the notification once artwork arrives, without alerting again. */
  private fun refreshNotification() {
    if (!foregroundStarted) return
    NotificationManagerCompat.from(this).notify(NOTIFICATION_ID, buildNotification())
  }

  @Suppress("DEPRECATION")
  private fun updateScreenWakeLock() {
    // Only this phone's own audio is a reason to keep the screen on: a track
    // playing on the computer must not hold this phone's display awake.
    if (playing && !remote) {
      if (screenWakeLock == null) {
        val powerManager = getSystemService(POWER_SERVICE) as PowerManager
        screenWakeLock = powerManager.newWakeLock(
          PowerManager.SCREEN_DIM_WAKE_LOCK,
          "$packageName:playback-screen"
        ).apply { setReferenceCounted(false) }
      }
      if (screenWakeLock?.isHeld != true) screenWakeLock?.acquire()
    } else {
      releaseScreenWakeLock()
    }
  }

  private fun releaseScreenWakeLock() {
    screenWakeLock?.takeIf { it.isHeld }?.release()
  }

  private fun updateSession() {
    val metadata = MediaMetadataCompat.Builder()
      .putString(MediaMetadataCompat.METADATA_KEY_TITLE, title)
      .putString(MediaMetadataCompat.METADATA_KEY_ARTIST, artist)
      .putLong(MediaMetadataCompat.METADATA_KEY_DURATION, duration)
    // The lock screen, media output picker and Android Auto read the session.
    artwork?.let {
      metadata.putBitmap(MediaMetadataCompat.METADATA_KEY_ALBUM_ART, it)
      metadata.putBitmap(MediaMetadataCompat.METADATA_KEY_ART, it)
    }
    mediaSession.setMetadata(metadata.build())
    if (remote) {
      // The volume keys and the system's own volume panel belong to the
      // computer's player while it is the one playing. The phone has no local
      // audio to turn down, so nothing is lost by handing them over.
      if (remoteVolume.currentVolume != volumeLevel) remoteVolume.currentVolume = volumeLevel
      mediaSession.setPlaybackToRemote(remoteVolume)
    } else {
      mediaSession.setPlaybackToLocal(AudioManager.STREAM_MUSIC)
    }
    var actions = PlaybackStateCompat.ACTION_PLAY or
      PlaybackStateCompat.ACTION_PAUSE or
      PlaybackStateCompat.ACTION_PLAY_PAUSE
    if (canSeek) actions = actions or PlaybackStateCompat.ACTION_SEEK_TO or
      PlaybackStateCompat.ACTION_REWIND or PlaybackStateCompat.ACTION_FAST_FORWARD
    if (canPrevious) actions = actions or PlaybackStateCompat.ACTION_SKIP_TO_PREVIOUS
    if (canNext) actions = actions or PlaybackStateCompat.ACTION_SKIP_TO_NEXT
    // The notification's own buttons are only drawn in the shade. Everything
    // that renders the session itself - the lock screen, Android Auto, the
    // media output picker - only ever sees what is published here, so like and
    // repeat have to be declared as custom actions as well.
    mediaSession.setPlaybackState(
      PlaybackStateCompat.Builder()
        .setActions(actions)
        .addCustomAction(customAction(ACTION_LIKE, likeLabel(), likeIcon()))
        .addCustomAction(
          customAction(ACTION_REPEAT, repeatLabel(), R.drawable.ic_napstrfy_loop)
        )
        .apply {
          // Android 13+ derives lock-screen buttons from session custom actions.
          if (canSeek) {
            addCustomAction(ACTION_REWIND, rewindLabel, R.drawable.ic_replay_15)
            addCustomAction(ACTION_FORWARD, forwardLabel, R.drawable.ic_forward_15)
          }
        }
        .setState(
          if (playing) PlaybackStateCompat.STATE_PLAYING else PlaybackStateCompat.STATE_PAUSED,
          position,
          if (playing) 1f else 0f
        )
        .build()
    )
  }

  private fun likeLabel() = if (liked) unlikeLabelText else likeLabelText

  private fun likeIcon() = if (liked) R.drawable.ic_napstrfy_liked else R.drawable.ic_napstrfy_like

  private fun repeatLabel() = if (looping) repeatOffLabelText else repeatLabelText

  private fun customAction(
    action: String,
    name: String,
    icon: Int
  ): PlaybackStateCompat.CustomAction = PlaybackStateCompat.CustomAction.Builder(action, name, icon).build()

  private fun buildNotification(): Notification {
    val previous = actionPendingIntent(ACTION_PREVIOUS, 1)
    val playPause = actionPendingIntent(if (playing) ACTION_PAUSE else ACTION_PLAY, 2)
    val next = actionPendingIntent(ACTION_NEXT, 3)
    // The shade carries the five transport buttons: previous, back 15,
    // play/pause, forward 15, next. Like and repeat stay session custom
    // actions, which is where the lock screen and Android Auto read buttons from.
    val rewind = if (canSeek) actionPendingIntent(ACTION_REWIND, 4) else null
    val forward = if (canSeek) actionPendingIntent(ACTION_FORWARD, 5) else null
    val launch = packageManager.getLaunchIntentForPackage(packageName)?.let {
      PendingIntent.getActivity(this, 0, it, pendingFlags())
    }
    return NotificationCompat.Builder(this, CHANNEL_ID)
      .setSmallIcon(R.drawable.ic_stat_napstrfy)
      .setContentTitle(title)
      .setContentText(artist)
      .setContentIntent(launch)
      .apply { artwork?.let { setLargeIcon(it) } }
      .setOnlyAlertOnce(true)
      .setSilent(true)
      .setVisibility(NotificationCompat.VISIBILITY_PUBLIC)
      .setCategory(NotificationCompat.CATEGORY_TRANSPORT)
      .setOngoing(playing)
      .addAction(android.R.drawable.ic_media_previous, previousLabel, previous)
      .addAction(R.drawable.ic_replay_15, rewindLabel, rewind)
      .addAction(
        if (playing) android.R.drawable.ic_media_pause else android.R.drawable.ic_media_play,
        if (playing) pauseLabel else playLabel,
        playPause
      )
      .addAction(R.drawable.ic_forward_15, forwardLabel, forward)
      .addAction(android.R.drawable.ic_media_next, nextLabel, next)
      .setStyle(MediaStyle().setMediaSession(mediaSession.sessionToken).setShowActionsInCompactView(1, 2, 3))
      .build()
  }

  private fun actionPendingIntent(action: String, requestCode: Int): PendingIntent =
    PendingIntent.getService(
      this,
      requestCode,
      Intent(this, MediaNotificationService::class.java).setAction(action),
      pendingFlags()
    )

  private fun pendingFlags(): Int =
    PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE

  private fun dispatch(action: String) {
    when (action) {
      ACTION_PLAY -> MediaControlBridge.dispatch("play")
      ACTION_PAUSE -> MediaControlBridge.dispatch("pause")
      ACTION_PREVIOUS -> if (canPrevious) MediaControlBridge.dispatch("previous")
      ACTION_NEXT -> if (canNext) MediaControlBridge.dispatch("next")
      // The system's own buttons toggle like and repeat; the three-way repeat
      // choice stays in the app.
      ACTION_LIKE -> MediaControlBridge.dispatch("like")
      ACTION_REPEAT -> MediaControlBridge.dispatch("repeat")
      ACTION_REWIND -> if (canSeek) MediaControlBridge.dispatch("rewind")
      ACTION_FORWARD -> if (canSeek) MediaControlBridge.dispatch("forward")
    }
  }

  private fun createChannel() {
    if (Build.VERSION.SDK_INT < Build.VERSION_CODES.O) return
    val channel = NotificationChannel(
      CHANNEL_ID,
      channelLabel,
      NotificationManager.IMPORTANCE_LOW
    ).apply {
      description = channelLabel
      setShowBadge(false)
    }
    getSystemService(NotificationManager::class.java).createNotificationChannel(channel)
  }

  override fun onTaskRemoved(rootIntent: Intent?) {
    releaseScreenWakeLock()
    stopForeground(STOP_FOREGROUND_REMOVE)
    stopSelf()
    super.onTaskRemoved(rootIntent)
  }

  override fun onDestroy() {
    artworkLoader.shutdownNow()
    releaseScreenWakeLock()
    mediaSession.isActive = false
    mediaSession.release()
    super.onDestroy()
  }

  override fun onBind(intent: Intent?): IBinder? = null

  companion object {
    const val ACTION_UPDATE = "net.napstr.nostrfy.media.UPDATE"
    const val ACTION_CLEAR = "net.napstr.nostrfy.media.CLEAR"
    const val ACTION_PLAY = "net.napstr.nostrfy.media.PLAY"
    const val ACTION_PAUSE = "net.napstr.nostrfy.media.PAUSE"
    const val ACTION_PREVIOUS = "net.napstr.nostrfy.media.PREVIOUS"
    const val ACTION_NEXT = "net.napstr.nostrfy.media.NEXT"
    const val ACTION_LIKE = "net.napstr.nostrfy.media.LIKE"
    const val ACTION_REPEAT = "net.napstr.nostrfy.media.REPEAT"
    const val ACTION_REWIND = "net.napstr.nostrfy.media.REWIND"
    const val ACTION_FORWARD = "net.napstr.nostrfy.media.FORWARD"
    const val EXTRA_TITLE = "title"
    const val EXTRA_ARTIST = "artist"
    const val EXTRA_ARTWORK = "artwork"
    const val EXTRA_PLAYING = "playing"
    const val EXTRA_POSITION = "position"
    const val EXTRA_DURATION = "duration"
    const val EXTRA_CAN_PREVIOUS = "canPrevious"
    const val EXTRA_CAN_NEXT = "canNext"
    const val EXTRA_CAN_SEEK = "canSeek"
    const val EXTRA_LIKED = "liked"
    const val EXTRA_LOOPING = "looping"
    const val EXTRA_REMOTE = "remote"
    const val EXTRA_VOLUME = "volume"
    private const val CHANNEL_ID = "napstrfy_playback"
    private const val NOTIFICATION_ID = 7302
  }
}

/**
 * Decoded covers for the notification, kept in a tiny LRU so scrolling through
 * tracks does not re-download the same five albums.
 */
private object ArtworkCache {
  private const val MAX_ENTRIES = 4
  private const val MAX_EDGE = 512
  private const val CONNECT_TIMEOUT_MS = 5_000
  private const val READ_TIMEOUT_MS = 8_000
  private val entries = LinkedHashMap<String, Bitmap>(MAX_ENTRIES, 0.75f, true)

  @Synchronized fun peek(url: String): Bitmap? = entries[url]

  fun load(url: String): Bitmap? {
    // Covers are published as HTTPS URLs; anything else is not ours to fetch.
    if (!url.startsWith("https://")) return null
    peek(url)?.let { return it }
    val decoded = try {
      val connection = (URL(url).openConnection() as HttpURLConnection).apply {
        connectTimeout = CONNECT_TIMEOUT_MS
        readTimeout = READ_TIMEOUT_MS
        instanceFollowRedirects = true
        setRequestProperty("Accept", "image/*")
      }
      try {
        connection.inputStream.use { stream -> BitmapFactory.decodeStream(stream) }
      } finally {
        connection.disconnect()
      }
    } catch (_: Exception) {
      null
    } ?: return null
    return remember(url, decoded)
  }

  @Synchronized private fun remember(url: String, decoded: Bitmap): Bitmap {
    val longest = maxOf(decoded.width, decoded.height)
    if (longest <= MAX_EDGE) {
      store(url, decoded)
      return decoded
    }
    val ratio = MAX_EDGE.toFloat() / longest
    val scaled = Bitmap.createScaledBitmap(
      decoded,
      (decoded.width * ratio).toInt().coerceAtLeast(1),
      (decoded.height * ratio).toInt().coerceAtLeast(1),
      true
    )
    if (scaled != decoded) decoded.recycle()
    store(url, scaled)
    return scaled
  }

  @Synchronized private fun store(url: String, bitmap: Bitmap) {
    entries[url] = bitmap
    while (entries.size > MAX_ENTRIES) {
      val oldest = entries.keys.firstOrNull() ?: return
      entries.remove(oldest)
    }
  }
}
