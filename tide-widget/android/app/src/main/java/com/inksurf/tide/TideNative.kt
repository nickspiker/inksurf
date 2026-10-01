package com.inksurf.tide

import android.graphics.Bitmap

/** The whole JNI surface: Rust renders the tide chart centred on a Unix instant into a row-major ARGB buffer. */
object TideNative {
    init {
        System.loadLibrary("tide_widget")
    }

    /** Fills [out] (at least w*h ints) with ARGB pixels. */
    @JvmStatic
    external fun render(unix: Long, w: Int, h: Int, out: IntArray)

    fun bitmap(unix: Long, w: Int, h: Int): Bitmap {
        val px = IntArray(w * h)
        render(unix, w, h, px)
        return Bitmap.createBitmap(px, w, h, Bitmap.Config.ARGB_8888)
    }
}
