//! Save export / import on Android: the Storage Access Framework's "create
//! document" and "open document" screens, started through JNI like the ROM
//! picker (rom_picker.rs) and answered through the same `PwActivity`
//! forwarding. The app's files are private, so this is the only way a save
//! gets in or out. The result is polled by `save_transfer.rs`.

use jni::errors::LogErrorAndDefault;
use jni::objects::{JClass, JObject, JString};
use jni::{Env, EnvUnowned, JValue, jni_sig, jni_str};
use std::io::Write as _;
use std::os::fd::FromRawFd as _;
use std::sync::Mutex;

/// `PwActivity.SAVE_EXPORT_REQUEST` / `SAVE_IMPORT_REQUEST`.
pub const EXPORT_REQUEST: i32 = 4002;
pub const IMPORT_REQUEST: i32 = 4003;

/// The last result: `(request, Some(uri))`, or `None` for a cancel.
static PICKED: Mutex<Option<(i32, Option<String>)>> = Mutex::new(None);

/// Opens the "save as" screen, suggesting `pw64.eep`.
pub fn start_export() {
    start(
        "android.intent.action.CREATE_DOCUMENT",
        EXPORT_REQUEST,
        Some("pw64.eep"),
    );
}

/// Opens the "pick a file" screen (no type filter: `.eep` has no MIME type).
pub fn start_import() {
    start("android.intent.action.OPEN_DOCUMENT", IMPORT_REQUEST, None);
}

fn start(action: &'static str, request: i32, title: Option<&'static str>) {
    let app = crate::android::app();
    let ui = app.clone();
    app.run_on_java_main_thread(Box::new(move || {
        // SAFETY: the process's JavaVM, valid for its lifetime.
        let vm = unsafe { jni::JavaVM::from_raw(ui.vm_as_ptr().cast()) };
        let r = vm.attach_current_thread(|env| -> jni::errors::Result<()> {
            // SAFETY: the activity's global reference, alive while the
            // activity is (this runs from its own UI thread).
            let activity = unsafe { JObject::from_raw(env, ui.activity_as_ptr().cast()) };
            start_intent(env, &activity, action, request, title)
        });
        if let Err(e) = r {
            eprintln!("[android] save picker: {e}");
            *PICKED.lock().unwrap() = Some((request, None));
        }
    }));
}

fn start_intent(
    env: &mut Env,
    activity: &JObject,
    action: &str,
    request: i32,
    title: Option<&str>,
) -> jni::errors::Result<()> {
    let action = env.new_string(action)?;
    let category = env.new_string("android.intent.category.OPENABLE")?;
    let mime = env.new_string("*/*")?;
    let intent = env.new_object(
        jni_str!("android/content/Intent"),
        jni_sig!("(Ljava/lang/String;)V"),
        &[JValue::Object(&action)],
    )?;
    env.call_method(
        &intent,
        jni_str!("addCategory"),
        jni_sig!("(Ljava/lang/String;)Landroid/content/Intent;"),
        &[JValue::Object(&category)],
    )?
    .l()?;
    env.call_method(
        &intent,
        jni_str!("setType"),
        jni_sig!("(Ljava/lang/String;)Landroid/content/Intent;"),
        &[JValue::Object(&mime)],
    )?
    .l()?;
    if let Some(title) = title {
        let key = env.new_string("android.intent.extra.TITLE")?;
        let value = env.new_string(title)?;
        env.call_method(
            &intent,
            jni_str!("putExtra"),
            jni_sig!("(Ljava/lang/String;Ljava/lang/String;)Landroid/content/Intent;"),
            &[JValue::Object(&key), JValue::Object(&value)],
        )?
        .l()?;
    }
    env.call_method(
        activity,
        jni_str!("startActivityForResult"),
        jni_sig!("(Landroid/content/Intent;I)V"),
        &[JValue::Object(&intent), JValue::Int(request)],
    )?;
    Ok(())
}

/// `PwActivity.onActivityResult` delivers the document URI here, "" on
/// cancel. UI thread: store and return.
#[unsafe(no_mangle)]
pub extern "system" fn Java_app_birdman64_PwActivity_nativeSavePicked<'caller>(
    mut env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    request: i32,
    uri: JString<'caller>,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        let text = uri.try_to_string(env)?;
        eprintln!("[android] save picker {request}: {text:?}");
        *PICKED.lock().unwrap() = Some((request, Some(text).filter(|t| !t.is_empty())));
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

/// The picker result since the last call, if any.
pub fn take_picked() -> Option<(i32, Option<String>)> {
    PICKED.lock().unwrap().take()
}

/// Writes `bytes` over the document (truncating: `"wt"`).
pub fn write_document(uri: &str, bytes: &[u8]) -> Result<(), String> {
    let app = crate::android::app();
    let ui = app.clone();
    // SAFETY: the process's JavaVM, valid for its lifetime.
    let vm = unsafe { jni::JavaVM::from_raw(ui.vm_as_ptr().cast()) };
    let r = vm.attach_current_thread(|env| -> jni::errors::Result<_> {
        let r = open_for_write(env, &ui, uri);
        if r.is_err() {
            env.exception_clear();
        }
        Ok(r)
    });
    let mut file = r.map_err(|e| format!("Birdman64 couldn't write the file: {e}"))??;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|e| format!("Birdman64 couldn't write the file: {e}"))
}

fn open_for_write(
    env: &mut Env,
    ui: &winit::platform::android::activity::AndroidApp,
    uri: &str,
) -> Result<std::fs::File, String> {
    let fail = |e: jni::errors::Error| format!("Birdman64 couldn't write the file: {e}");
    // SAFETY: the activity's global reference, alive while the activity is.
    let activity = unsafe { JObject::from_raw(env, ui.activity_as_ptr().cast()) };
    let cr = env
        .call_method(
            &activity,
            jni_str!("getContentResolver"),
            jni_sig!("()Landroid/content/ContentResolver;"),
            &[],
        )
        .and_then(|v| v.l())
        .map_err(fail)?;
    let uri_str = env.new_string(uri).map_err(fail)?;
    let uri = env
        .call_static_method(
            jni_str!("android/net/Uri"),
            jni_str!("parse"),
            jni_sig!("(Ljava/lang/String;)Landroid/net/Uri;"),
            &[JValue::Object(&uri_str)],
        )
        .and_then(|v| v.l())
        .map_err(fail)?;
    let mode = env.new_string("wt").map_err(fail)?;
    let pfd = env
        .call_method(
            &cr,
            jni_str!("openFileDescriptor"),
            jni_sig!("(Landroid/net/Uri;Ljava/lang/String;)Landroid/os/ParcelFileDescriptor;"),
            &[JValue::Object(&uri), JValue::Object(&mode)],
        )
        .and_then(|v| v.l())
        .map_err(|_| "Birdman64 couldn't open that file for writing.".to_string())?;
    let fd = env
        .call_method(&pfd, jni_str!("detachFd"), jni_sig!("()I"), &[])
        .and_then(|v| v.i())
        .map_err(fail)?;
    if fd < 0 {
        return Err("Birdman64 couldn't open that file for writing.".to_string());
    }
    // SAFETY: detachFd transferred the fd's ownership to us.
    Ok(unsafe { std::fs::File::from_raw_fd(fd) })
}
