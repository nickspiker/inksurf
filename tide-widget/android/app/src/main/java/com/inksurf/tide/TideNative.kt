package com.inksurf.tide

import android.graphics.Bitmap

/** The whole JNI surface: Rust renders the tide chart centred on a Unix instant into a row-major ARGB buffer. */
object TideNative {
    init {
        System.loadLibrary("tide_widget")
    }

    /** Fills [out] (at least w*h ints) with ARGB pixels; [hourly] picks the Arabic-numeral edition, else dozenal. */
    @JvmStatic
    external fun render(unix: Long, w: Int, h: Int, hourly: Boolean, out: IntArray)

    fun bitmap(unix: Long, w: Int, h: Int, hourly: Boolean): Bitmap {
        val px = IntArray(w * h)
        render(unix, w, h, hourly, px)
        return Bitmap.createBitmap(px, w, h, Bitmap.Config.ARGB_8888)
    }
}
