package expo.modules.connectionservice

import android.content.Context
import android.content.Intent
import android.util.Log
import androidx.core.content.ContextCompat
import expo.modules.kotlin.exception.Exceptions
import expo.modules.kotlin.modules.Module
import expo.modules.kotlin.modules.ModuleDefinition

class ConnectionServiceModule : Module() {
  private val context: Context
    get() = appContext.reactContext ?: throw Exceptions.ReactContextLost()

  // Set by start, cleared by stop: update() never revives a service JS stopped.
  private var started = false

  override fun definition() = ModuleDefinition {
    Name("ConnectionService")

    Function("start") { title: String, text: String -> start(title, text) }
    Function("update") { text: String -> update(text) }
    Function("stop") { stop() }
    Function("isRunning") { ConnectionForegroundService.running }
  }

  private fun intent(action: String) =
    Intent(context, ConnectionForegroundService::class.java).setAction(action)

  /** JS calls this only while the app is in the foreground. */
  private fun start(title: String, text: String) {
    started = try {
      ContextCompat.startForegroundService(
        context,
        intent(ConnectionForegroundService.ACTION_START)
          .putExtra(ConnectionForegroundService.EXTRA_TITLE, title)
          .putExtra(ConnectionForegroundService.EXTRA_TEXT, text)
      )
      true
    } catch (e: IllegalStateException) {
      // ForegroundServiceStartNotAllowedException (Android 12+, app in background) extends it.
      Log.w(ConnectionForegroundService.TAG, "foreground service start refused", e)
      false
    }
  }

  // Plain startService is allowed here: an app with a foreground service counts as foreground.
  private fun update(text: String) {
    if (!started) return
    context.startService(
      intent(ConnectionForegroundService.ACTION_UPDATE)
        .putExtra(ConnectionForegroundService.EXTRA_TEXT, text)
    )
  }

  // Never a bare stopService(): one that reaches the system before onStartCommand has run
  // startForeground() crashes the app ("Context.startForegroundService() did not then call
  // Service.startForeground()"), e.g. a revoked pairing denied right after start(). STOP goes
  // through the service's own start queue, behind any pending START.
  private fun stop() {
    val wasStarted = started
    started = false
    // `running` too: a reloaded JS instance gets a fresh module while the service still runs
    if (!wasStarted && !ConnectionForegroundService.running) return
    try {
      context.startService(intent(ConnectionForegroundService.ACTION_STOP))
    } catch (e: IllegalStateException) {
      // Background start refused: no foreground service runs, so no START is pending either.
      context.stopService(Intent(context, ConnectionForegroundService::class.java))
    }
  }
}
