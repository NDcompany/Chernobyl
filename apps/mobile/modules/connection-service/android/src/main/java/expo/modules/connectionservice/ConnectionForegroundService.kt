package expo.modules.connectionservice

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.IBinder
import android.util.Log
import androidx.core.app.NotificationCompat
import com.facebook.react.ReactApplication
import com.facebook.react.bridge.Arguments
import com.facebook.react.bridge.ReactContext
import com.facebook.react.jstasks.HeadlessJsTaskConfig
import com.facebook.react.jstasks.HeadlessJsTaskContext
import java.lang.ref.WeakReference

/**
 * Keeps the app process, and with it the E2E WebSocket and JS timers, alive while the app is
 * in the background (ADR-011 section 4). Holds no wake lock (unlike RN's HeadlessJsTaskService)
 * and is START_NOT_STICKY: nothing restarts it after the process dies.
 */
class ConnectionForegroundService : Service() {
  private var title = ""
  private var text = ""
  private var inForeground = false

  // RN pauses JS timers when the activity pauses unless a headless JS task is active.
  private var taskContext: WeakReference<ReactContext>? = null
  private var taskId = 0

  override fun onBind(intent: Intent?): IBinder? = null

  override fun onCreate() {
    super.onCreate()
    running = true
  }

  override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
    text = intent?.getStringExtra(EXTRA_TEXT) ?: text
    when (intent?.action) {
      ACTION_START -> {
        title = intent?.getStringExtra(EXTRA_TITLE) ?: title
        try {
          goForeground()
        } catch (e: Exception) {
          // ForegroundServiceStartNotAllowedException (Android 12+ background start): log, no crash.
          Log.w(TAG, "startForeground refused", e)
          stopSelf(startId)
          return START_NOT_STICKY
        }
        inForeground = true
        startTaskOnce()
      }
      ACTION_UPDATE -> if (inForeground) {
        notificationManager()?.notify(NOTIFICATION_ID, notification())
      } else {
        // An update reaching a fresh instance must not leave a plain background service behind.
        stopSelf(startId)
      }
      // Runs after any START queued before it. stopSelf(startId), never stopSelf(): it is a no-op
      // while a newer START is queued (stop then start on re-pair), so that START is never
      // brought down before it calls startForeground().
      ACTION_STOP -> stopSelf(startId)
    }
    return START_NOT_STICKY
  }

  override fun onDestroy() {
    finishTask()
    running = false
    super.onDestroy()
  }

  private fun notificationManager() = getSystemService(NotificationManager::class.java)

  private fun goForeground() {
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
      notificationManager()?.createNotificationChannel(
        NotificationChannel(CHANNEL_ID, "Background connection", NotificationManager.IMPORTANCE_LOW)
          .apply { setShowBadge(false) }
      )
    }
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
      startForeground(NOTIFICATION_ID, notification(), ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE)
    } else {
      startForeground(NOTIFICATION_ID, notification())
    }
  }

  private fun notification(): Notification {
    val open = packageManager.getLaunchIntentForPackage(packageName)?.let {
      PendingIntent.getActivity(this, 0, it, PendingIntent.FLAG_IMMUTABLE)
    }
    return NotificationCompat.Builder(this, CHANNEL_ID)
      // alpha-only glyph: the full-colour launcher icon draws as a white blob in the status bar
      .setSmallIcon(R.drawable.ic_stat_chernobyl)
      .setContentTitle(title)
      .setContentText(text)
      .setContentIntent(open)
      .setOngoing(true)
      .setOnlyAlertOnce(true)
      .setPriority(NotificationCompat.PRIORITY_LOW)
      .build()
  }

  /** Starts ChernobylConnection on the current React instance, once (again after a reload). */
  private fun startTaskOnce() {
    // No React instance: keep only the foreground service (process alive); never start one.
    val context = (application as? ReactApplication)?.reactHost?.currentReactContext ?: return
    if (taskContext?.get() === context) return
    finishTask()
    taskId = HeadlessJsTaskContext.getInstance(context).startTask(
      HeadlessJsTaskConfig(TASK_KEY, Arguments.createMap(), timeout = 0, isAllowedInForeground = true)
    )
    taskContext = WeakReference(context)
  }

  private fun finishTask() {
    taskContext?.get()?.let { HeadlessJsTaskContext.getInstance(it).finishTask(taskId) }
    taskContext = null
  }

  companion object {
    const val TAG = "ConnectionService"
    const val ACTION_START = "expo.modules.connectionservice.START"
    const val ACTION_UPDATE = "expo.modules.connectionservice.UPDATE"
    const val ACTION_STOP = "expo.modules.connectionservice.STOP"
    const val EXTRA_TITLE = "title"
    const val EXTRA_TEXT = "text"
    private const val CHANNEL_ID = "chernobyl-connection"
    private const val NOTIFICATION_ID = 1 // startForeground rejects 0
    private const val TASK_KEY = "ChernobylConnection"

    @Volatile
    var running = false
      private set
  }
}
