package com.inksurf.tide

import android.app.Activity
import android.app.AlarmManager
import android.content.Intent
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.provider.Settings
import android.view.Gravity
import android.widget.Button
import android.widget.ImageView
import android.widget.LinearLayout
import android.widget.TextView

/** Bare-bones host screen: a full-width preview from the same renderer, how to add the widget, and the exact-alarm grant. Opening it also refreshes every placed widget and re-arms the tick. */
class MainActivity : Activity() {
    private lateinit var preview: ImageView
    private lateinit var exactBtn: Button

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val pad = (16 * resources.displayMetrics.density).toInt()
        preview = ImageView(this).apply { adjustViewBounds = true }
        val hint = TextView(this).apply {
            text = "Long-press the home screen → Widgets → Tide. Resize it to any size; it re-renders pixel-for-pixel."
            setPadding(0, pad, 0, pad)
        }
        exactBtn = Button(this).apply {
            text = "Allow exact :X5 updates"
            setOnClickListener {
                startActivity(Intent(Settings.ACTION_REQUEST_SCHEDULE_EXACT_ALARM, Uri.parse("package:$packageName")))
            }
        }
        setContentView(LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            gravity = Gravity.CENTER_HORIZONTAL
            setPadding(pad, pad * 3, pad, pad)
            addView(preview)
            addView(hint)
            addView(exactBtn)
        })
    }

    override fun onResume() {
        super.onResume()
        val w = resources.displayMetrics.widthPixels - 2 * (16 * resources.displayMetrics.density).toInt()
        val h = w * 180 / 384 // panel aspect
        preview.setImageBitmap(TideNative.bitmap(System.currentTimeMillis() / 1000, w, h))
        val exactOk = Build.VERSION.SDK_INT < Build.VERSION_CODES.S || getSystemService(AlarmManager::class.java).canScheduleExactAlarms()
        exactBtn.visibility = if (exactOk) Button.GONE else Button.VISIBLE
        TideWidgetProvider.updateAll(this)
    }
}
