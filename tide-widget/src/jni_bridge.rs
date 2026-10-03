//! The whole JNI surface: one call. Kotlin passes the instant and the widget's pixel size plus a w*h IntArray; Rust fills it with row-major ARGB.

use jni::objects::{JClass, JIntArray};
use jni::sys::{jboolean, jint, jlong};

use crate::render::Edition;
use jni::JNIEnv;

/// `TideNative.render(unix: Long, w: Int, h: Int, hourly: Boolean, out: IntArray)` — `out` must hold at least w*h ints. `hourly` picks the Arabic-numeral edition, else dozenal.
#[no_mangle]
pub extern "system" fn Java_com_inksurf_tide_TideNative_render<'local>(mut env: JNIEnv<'local>, _class: JClass<'local>, unix: jlong, w: jint, h: jint, hourly: jboolean, out: JIntArray<'local>) {
    let (w, h) = (w.max(1) as usize, h.max(1) as usize);
    let edition = if hourly != 0 { Edition::Hourly } else { Edition::Dozenal };
    let px = crate::render::render_argb(unix, w, h, edition);
    if let Err(e) = env.set_int_array_region(&out, 0, &px) {
        let _ = env.throw_new("java/lang/IllegalArgumentException", format!("tide render: {e}"));
    }
}
