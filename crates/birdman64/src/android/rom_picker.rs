//! The ROM picker on Android: a Storage Access Framework file picker
//! (`ACTION_OPEN_DOCUMENT`) opened through JNI. Its result is forwarded by
//! the APK's one Java class (`packaging/android/java/app/birdman64/
//! PwActivity.java`, a `NativeActivity` subclass) into the JNI export below
//! — `onActivityResult` cannot reach native code through the framework's
//! `NativeActivity`, and a static `native` method resolves in our loaded
//! `.so`. The picked document is copied into the app's `rom/` folder under
//! its display name (content URIs have no file path, and the copy makes the
//! pick survive: no SAF permission to keep), then the ordinary ROM
//! resolution verifies and remembers it (`rom_setup::use_rom_file`).
//!
//! The picker never blocks the window: `start_pick` only launches the
//! system screen, the ROM screen polls [`take_picked`] every frame while it
//! is up, and a cancel simply lands back on the ROM screen.

use jni::errors::LogErrorAndDefault;
use jni::objects::{JClass, JObject, JString};
use jni::{Env, EnvUnowned, JValue, jni_sig, jni_str};
use std::io::Read as _;
use std::os::fd::FromRawFd as _;
use std::path::PathBuf;
use std::sync::Mutex;

/// The request code the native side starts the picker with and
/// `PwActivity.onActivityResult` forwards (keep in sync with the Java).
const ROM_PICK_REQUEST: i32 = 4001;

/// The last picker result: `Some(None)` = cancelled, `Some(Some(uri))` =
/// a picked document URI, `None` = nothing new. Taken by the ROM screen.
static PICKED: Mutex<Option<Option<String>>> = Mutex::new(None);

/// What a picked document read produced.
struct PickedFile {
    name: String,
    bytes: Vec<u8>,
}

/// Why a picked document couldn't be read (`Missing`: Android said so —
/// the document is gone; `Jni`/`Io`: the usual transport failures).
enum ReadFail {
    Missing,
    Jni(jni::errors::Error),
    Io(std::io::Error),
}

impl From<jni::errors::Error> for ReadFail {
    fn from(e: jni::errors::Error) -> Self {
        Self::Jni(e)
    }
}

/// Opens the system file picker (non-blocking; the result arrives later
/// through [`take_picked`]). The `startActivityForResult` call runs on the
/// Java UI thread.
pub fn start_pick() {
    let app = crate::android::app();
    let ui = app.clone();
    app.run_on_java_main_thread(Box::new(move || {
        // SAFETY: the process's JavaVM, valid for its lifetime.
        let vm = unsafe { jni::JavaVM::from_raw(ui.vm_as_ptr().cast()) };
        let r = vm.attach_current_thread(|env| -> jni::errors::Result<()> {
            // SAFETY: the activity's global reference, alive while the
            // activity is (this runs from its own UI thread).
            let activity = unsafe { JObject::from_raw(env, ui.activity_as_ptr().cast()) };
            start_picker_intent(env, &activity)
        });
        if let Err(e) = r {
            eprintln!("[android] ROM picker: {e}");
        }
    }));
}

/// `ACTION_OPEN_DOCUMENT`, openable documents only, no type filter: ROM
/// files usually have no registered MIME type, so filtering could hide the
/// very file the player wants (a wrong pick is reported after, by the
/// verification).
fn start_picker_intent(env: &mut Env, activity: &JObject) -> jni::errors::Result<()> {
    let action = env.new_string("android.intent.action.OPEN_DOCUMENT")?;
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
    env.call_method(
        activity,
        jni_str!("startActivityForResult"),
        jni_sig!("(Landroid/content/Intent;I)V"),
        &[JValue::Object(&intent), JValue::Int(ROM_PICK_REQUEST)],
    )?;
    Ok(())
}

/// The Java side (`PwActivity.onActivityResult`) delivers the picked
/// document URI here, "" on cancel. Runs on the Java UI thread: store and
/// return, never block.
#[unsafe(no_mangle)]
pub extern "system" fn Java_app_birdman64_PwActivity_nativeRomPicked<'caller>(
    mut env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    uri: JString<'caller>,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        let text = uri.try_to_string(env)?;
        if text.is_empty() {
            eprintln!("[android] ROM picker cancelled");
            *PICKED.lock().unwrap() = Some(None);
        } else {
            eprintln!("[android] ROM picker picked {text}");
            *PICKED.lock().unwrap() = Some(Some(text));
        }
        Ok(())
    })
    // A failure here only means the result is lost; throwing on the UI
    // thread would kill the process for nothing.
    .resolve::<LogErrorAndDefault>()
}

/// The picker result since the last call, if any: `None` = cancelled.
pub fn take_picked() -> Option<Option<String>> {
    PICKED.lock().unwrap().take()
}

/// Reads the picked document and installs it as the ROM: the bytes land in
/// the app's `rom/` folder under the document's display name, then
/// [`crate::rom_setup::use_rom_file`] verifies and remembers them. `Err`:
/// the (title, text) the ROM screen shows — the failed copy is removed, so
/// it can't be picked up by the folder scan on the next start.
pub fn install_picked(uri: &str) -> Result<PathBuf, (&'static str, String)> {
    let app = crate::android::app();
    let ui = app.clone();
    let uri = uri.to_string();
    // SAFETY: the process's JavaVM, valid for its lifetime.
    let vm = unsafe { jni::JavaVM::from_raw(ui.vm_as_ptr().cast()) };
    let read: Result<PickedFile, ReadFail> =
        vm.attach_current_thread(|env| read_document(env, &ui, &uri));
    let picked = read.map_err(|e| read_fail_message(&e))?;
    let dir = crate::paths::data_dir().join("rom");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return Err((
            "Couldn't read the file",
            format!("Birdman64 couldn't create its rom folder: {e}"),
        ));
    }
    let dest = dir.join(sanitize_name(&picked.name));
    if let Err(e) = std::fs::write(&dest, &picked.bytes) {
        return Err((
            "Couldn't read the file",
            format!("Birdman64 couldn't store the picked file: {e}"),
        ));
    }
    match crate::rom_setup::use_rom_file(&dest) {
        Ok(abs) => Ok(abs),
        Err((title, text)) => {
            let _ = std::fs::remove_file(&dest);
            Err((title, text))
        }
    }
}

/// The ROM screen's error box text for a read failure.
fn read_fail_message(e: &ReadFail) -> (&'static str, String) {
    match e {
        ReadFail::Missing => (
            "Couldn't read the file",
            "Birdman64 couldn't read the picked file: it is no longer where it was \
             picked from. Please pick it again."
                .to_string(),
        ),
        ReadFail::Jni(e) => (
            "Couldn't read the file",
            format!("Birdman64 couldn't read the picked file: {e}"),
        ),
        ReadFail::Io(e) => (
            "Couldn't read the file",
            format!("Birdman64 couldn't read the picked file: {e}"),
        ),
    }
}

/// Reads the document on this thread through the content resolver: the
/// display name (for the file name) via a query, the bytes via a
/// `ParcelFileDescriptor` detached into an ordinary `File`. Any thrown Java
/// exception is cleared before returning, so the UI thread never sees a
/// pending one.
fn read_document(
    env: &mut Env,
    ui: &winit::platform::android::activity::AndroidApp,
    uri: &str,
) -> Result<PickedFile, ReadFail> {
    match read_document_inner(env, ui, uri) {
        Ok(p) => Ok(p),
        Err(e) => {
            env.exception_clear();
            Err(e.into())
        }
    }
}

fn read_document_inner(
    env: &mut Env,
    ui: &winit::platform::android::activity::AndroidApp,
    uri: &str,
) -> Result<PickedFile, ReadFail> {
    // SAFETY: the activity's global reference, alive while the activity is.
    let activity = unsafe { JObject::from_raw(env, ui.activity_as_ptr().cast()) };
    let cr = env
        .call_method(
            &activity,
            jni_str!("getContentResolver"),
            jni_sig!("()Landroid/content/ContentResolver;"),
            &[],
        )?
        .l()?;
    let uri_str = env.new_string(uri)?;
    let uri = env
        .call_static_method(
            jni_str!("android/net/Uri"),
            jni_str!("parse"),
            jni_sig!("(Ljava/lang/String;)Landroid/net/Uri;"),
            &[JValue::Object(&uri_str)],
        )?
        .l()?;
    let name = display_name(env, &cr, &uri).unwrap_or_else(|| "picked-rom.z64".to_string());
    let mode = env.new_string("r")?;
    let pfd = env.call_method(
        &cr,
        jni_str!("openFileDescriptor"),
        jni_sig!("(Landroid/net/Uri;Ljava/lang/String;)Landroid/os/ParcelFileDescriptor;"),
        &[JValue::Object(&uri), JValue::Object(&mode)],
    );
    let pfd = match pfd {
        Ok(p) => p.l()?,
        // The document is gone (deleted, moved, different account):
        // openFileDescriptor threw FileNotFoundException.
        Err(_) => return Err(ReadFail::Missing),
    };
    let fd = env.call_method(&pfd, jni_str!("detachFd"), jni_sig!("()I"), &[])?.i()?;
    if fd < 0 {
        return Err(ReadFail::Missing);
    }
    // SAFETY: detachFd transferred the fd's ownership to us.
    let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(ReadFail::Io)?;
    Ok(PickedFile { name, bytes })
}

/// The document's display name (`OpenableColumns.DISPLAY_NAME`), as the
/// player knows the file. `None`: the query found nothing useful.
fn display_name(env: &mut Env, cr: &JObject, uri: &JObject) -> Option<String> {
    let projection = env
        .new_object_array(1, jni_str!("java/lang/String"), JObject::null())
        .ok()?;
    let column = env.new_string("_display_name").ok()?;
    env.set_object_array_element(&projection, 0, &column).ok()?;
    let cursor = env
        .call_method(
            cr,
            jni_str!("query"),
            jni_sig!(
                "(Landroid/net/Uri;[Ljava/lang/String;Ljava/lang/String;\
                 [Ljava/lang/String;Ljava/lang/String;)Landroid/database/Cursor;"
            ),
            &[
                JValue::Object(uri),
                JValue::Object(&projection),
                JValue::Object(&JObject::null()),
                JValue::Object(&JObject::null()),
                JValue::Object(&JObject::null()),
            ],
        )
        .ok()?
        .l()
        .ok()?;
    if cursor.is_null() {
        return None;
    }
    let mut name = None;
    if env
        .call_method(&cursor, jni_str!("moveToFirst"), jni_sig!("()Z"), &[])
        .ok()?
        .z()
        .ok()?
    {
        let s = env
            .call_method(
                &cursor,
                jni_str!("getString"),
                jni_sig!("(I)Ljava/lang/String;"),
                &[JValue::Int(0)],
            )
            .ok()?
            .l()
            .ok();
        if let Some(s) = s {
            if let Ok(s) = env.cast_local::<JString>(s) {
                name = s.try_to_string(env).ok();
            }
        }
    }
    let _ = env.call_method(&cursor, jni_str!("close"), jni_sig!("()V"), &[]);
    // close can throw IOException; nothing of ours depends on it.
    env.exception_clear();
    name.filter(|n| !n.trim().is_empty())
}

/// A display name safe to use as a file name in the app's rom folder:
/// folder components and odd characters out, `.`/`..` out, never empty.
fn sanitize_name(raw: &str) -> String {
    let name = raw
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .trim();
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ' ' | '(' | ')') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let s = cleaned.trim().to_string();
    if s.is_empty() || s == "." || s == ".." {
        "picked-rom.z64".to_string()
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::sanitize_name;

    #[test]
    fn plain_name_is_kept() {
        assert_eq!(sanitize_name("Pilotwings 64 (USA).z64"), "Pilotwings 64 (USA).z64");
    }

    #[test]
    fn folder_parts_and_odd_chars_are_stripped() {
        assert_eq!(sanitize_name("rom/pw64 (USA).z64"), "pw64 (USA).z64");
        assert_eq!(sanitize_name("..\\..\\rom.zip"), "___rom.zip");
        assert_eq!(sanitize_name("bad/name\u{0}«x».n64"), "bad_name__x__.n64");
    }

    #[test]
    fn unusable_names_fall_back() {
        assert_eq!(sanitize_name(""), "picked-rom.z64");
        assert_eq!(sanitize_name("  "), "picked-rom.z64");
        assert_eq!(sanitize_name(".."), "picked-rom.z64");
    }
}
