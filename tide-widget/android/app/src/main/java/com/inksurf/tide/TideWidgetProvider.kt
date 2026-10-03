package com.inksurf.tide

import android.app.AlarmManager
import android.app.PendingIntent
import android.appwidget.AppWidgetManager
import android.appwidget.AppWidgetProvider
import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.content.res.Configuration
import android.graphics.Bitmap
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.widget.RemoteViews
import java.io.File
import java.util.TimeZone
import kotlin.math.max
import kotlin.math.roundToInt

/**
 * Renders each widget at its exact pixel size via Rust, writes a PNG, and points the launcher at it by URI. This provider is the dozenal edition; [TideHourlyWidgetProvider] is the Arabic-numeral one, so the widget picker offers both.
 * Refresh: one self-re-arming alarm at every local :X5 (when the now time's 10-minute rounding flips) redraws every placed widget of both editions, plus resize, time/zone changes, boot, reinstall, and tap.
 */
open class TideWidgetProvider : AppWidgetProvider() {

    /** Which edition this provider's widgets draw; [TideHourlyWidgetProvider] overrides it. */
    protected open val hourly = false

    override fun onReceive(context: Context, intent: Intent) {
        when (intent.action) {
            ACTION_TICK,
            Intent.ACTION_TIME_CHANGED,
            Intent.ACTION_TIMEZONE_CHANGED,
            Intent.ACTION_BOOT_COMPLETED,
            Intent.ACTION_MY_PACKAGE_REPLACED,
            AlarmManager.ACTION_SCHEDULE_EXACT_ALARM_PERMISSION_STATE_CHANGED -> updateAll(context)
            else -> super.onReceive(context, intent)
        }
    }

    override fun onUpdate(context: Context, appWidgetManager: AppWidgetManager, appWidgetIds: IntArray) {
        appWidgetIds.forEach { update(context, appWidgetManager, it, hourly) }
        scheduleNext(context)
    }

    override fun onAppWidgetOptionsChanged(context: Context, appWidgetManager: AppWidgetManager, appWidgetId: Int, newOptions: Bundle) {
        update(context, appWidgetManager, appWidgetId, hourly)
    }

    override fun onDeleted(context: Context, appWidgetIds: IntArray) {
        appWidgetIds.forEach { File(context.filesDir, pngName(it)).delete() }
    }

    /** This edition's last widget is gone; stop the tick only if the other edition has none placed either. */
    override fun onDisabled(context: Context) {
        if (placed(context).isEmpty()) context.getSystemService(AlarmManager::class.java).cancel(tickIntent(context))
    }

    companion object {
        const val ACTION_TICK = "com.inksurf.tide.TICK"
        private const val PERIOD_MS = 10 * 60_000L
        private const val PHASE_MS = 5 * 60_000L // :X5 — when the now time's nearest-10-minute rounding flips

        fun pngName(id: Int) = "tide_$id.png"

        /** Every placed widget of both editions, as (widget id, hourly). */
        private fun placed(ctx: Context): List<Pair<Int, Boolean>> {
            val mgr = AppWidgetManager.getInstance(ctx)
            return listOf(TideWidgetProvider::class.java to false, TideHourlyWidgetProvider::class.java to true)
                .flatMap { (cls, hourly) -> mgr.getAppWidgetIds(ComponentName(ctx, cls)).map { it to hourly } }
        }

        fun updateAll(ctx: Context) {
            val mgr = AppWidgetManager.getInstance(ctx)
            val all = placed(ctx)
            all.forEach { (id, hourly) -> update(ctx, mgr, id, hourly) }
            if (all.isNotEmpty()) scheduleNext(ctx)
        }

        fun update(ctx: Context, mgr: AppWidgetManager, id: Int, hourly: Boolean) {
            val (w, h) = widgetPx(ctx, mgr, id)
            val unix = System.currentTimeMillis() / 1000
            val bmp = TideNative.bitmap(unix, w, h, hourly)
            val file = File(ctx.filesDir, pngName(id))
            val tmp = File(ctx.filesDir, pngName(id) + ".tmp")
            tmp.outputStream().use { bmp.compress(Bitmap.CompressFormat.PNG, 100, it) }
            tmp.renameTo(file)
            bmp.recycle()
            // Fresh URI per frame: the launcher caches by URI, so reusing one would keep showing the old chart.
            val uri = Uri.parse("content://${TideImageProvider.AUTHORITY}/$id/$unix")
            val views = RemoteViews(ctx.packageName, R.layout.tide_widget).apply {
                setImageViewUri(R.id.tide_image, uri)
                setOnClickPendingIntent(R.id.tide_image, tickIntent(ctx))
            }
            mgr.updateAppWidget(id, views)
        }

        /** Widget size in device pixels. Launchers report a dp range: portrait is (min width × max height), landscape (max width × min height). */
        fun widgetPx(ctx: Context, mgr: AppWidgetManager, id: Int): Pair<Int, Int> {
            val o = mgr.getAppWidgetOptions(id)
            val landscape = ctx.resources.configuration.orientation == Configuration.ORIENTATION_LANDSCAPE
            var wDp = o.getInt(if (landscape) AppWidgetManager.OPTION_APPWIDGET_MAX_WIDTH else AppWidgetManager.OPTION_APPWIDGET_MIN_WIDTH)
            var hDp = o.getInt(if (landscape) AppWidgetManager.OPTION_APPWIDGET_MIN_HEIGHT else AppWidgetManager.OPTION_APPWIDGET_MAX_HEIGHT)
            if (wDp <= 0 || hDp <= 0) {
                wDp = 250 // options not populated yet: fall back to the declared minWidth/minHeight
                hDp = 110
            }
            val d = ctx.resources.displayMetrics.density
            return Pair(max(1, (wDp * d).roundToInt()), max(1, (hDp * d).roundToInt()))
        }

        /** Arm the next local :X5. Exact if the user allowed it (Alarms & reminders), else setWindow — which Android 12+ stretches to a 10-min window. RTC (non-wakeup): a sleeping phone isn't woken; the pending tick fires as soon as the screen comes on. */
        fun scheduleNext(ctx: Context) {
            val am = ctx.getSystemService(AlarmManager::class.java)
            val now = System.currentTimeMillis()
            val off = TimeZone.getDefault().getOffset(now)
            val local = now + off
            val next = (Math.floorDiv(local - PHASE_MS, PERIOD_MS) + 1) * PERIOD_MS + PHASE_MS - off
            val pi = tickIntent(ctx)
            if (Build.VERSION.SDK_INT < Build.VERSION_CODES.S || am.canScheduleExactAlarms()) {
                am.setExact(AlarmManager.RTC, next, pi)
            } else {
                am.setWindow(AlarmManager.RTC, next, PERIOD_MS, pi)
            }
        }

        private fun tickIntent(ctx: Context): PendingIntent = PendingIntent.getBroadcast(
            ctx, 0,
            Intent(ctx, TideWidgetProvider::class.java).setAction(ACTION_TICK),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )
    }
}
