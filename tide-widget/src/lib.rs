//! tide-widget — the e-ink panel's tide chart for an Android home-screen widget. Rust does all the drawing; Kotlin only sizes the widget, copies the ARGB buffer into a Bitmap, and pushes it to the launcher.

pub mod render;

#[cfg(target_os = "android")]
mod jni_bridge;
