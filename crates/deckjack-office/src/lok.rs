//! LibreOfficeKit, by hand.
//!
//! LOK is LibreOffice loaded into this process: `libsofficeapp.so` exports
//! `libreofficekit_hook_2`, which boots the office and hands back a pointer to
//! a struct whose first field is a pointer to a table of functions. Documents
//! work the same way. That is the whole C surface this app needs, and it is
//! small enough that a vendored FFI is clearer than a crate: five function
//! pointers, laid out below in the order `LibreOfficeKit.h` declares them.
//!
//! Why LOK and not `soffice --convert-to`: the binary hard-codes its
//! single-instance pipe in `/tmp` and ignores `TMPDIR`, and an app's Landlock
//! grant has no `/tmp`, so `soffice` exits 1 under the sandbox. LOK is the same
//! engine with no pipe (docs/lanes/6-office.md, "What the spike found").
//!
//! Only ever called from the worker process (`--worker`), never from the app
//! process that serves commands: a document LibreOffice chokes on takes the
//! worker down, not the app, and the worker carries the memory and CPU limits.
//!
//! # Layout safety
//!
//! Every table starts with `nSize`, its own size in bytes, so a newer header
//! can add functions without breaking an older library. A field is read only
//! when `nSize` says the library's table reaches it; [`field`] is that check.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::path::Path;

type Office = c_void;
type Document = c_void;

#[repr(C)]
struct OfficeHandle {
    class: *const OfficeClass,
}

/// `LibreOfficeKitClass`, up to the last member this file uses.
#[repr(C)]
struct OfficeClass {
    size: usize,
    destroy: Option<unsafe extern "C" fn(*mut Office)>,
    document_load: Option<unsafe extern "C" fn(*mut Office, *const c_char) -> *mut Document>,
    get_error: Option<unsafe extern "C" fn(*mut Office) -> *mut c_char>,
    document_load_with_options:
        Option<unsafe extern "C" fn(*mut Office, *const c_char, *const c_char) -> *mut Document>,
    free_error: Option<unsafe extern "C" fn(*mut c_char)>,
    register_callback: Option<unsafe extern "C" fn()>,
    get_filter_types: Option<unsafe extern "C" fn()>,
    set_optional_features: Option<unsafe extern "C" fn()>,
    set_document_password: Option<unsafe extern "C" fn()>,
    get_version_info: Option<unsafe extern "C" fn(*mut Office) -> *mut c_char>,
}

#[repr(C)]
struct DocumentHandle {
    class: *const DocumentClass,
}

/// `LibreOfficeKitDocumentClass`, up to the last member this file uses.
#[repr(C)]
struct DocumentClass {
    size: usize,
    destroy: Option<unsafe extern "C" fn(*mut Document)>,
    save_as: Option<
        unsafe extern "C" fn(*mut Document, *const c_char, *const c_char, *const c_char) -> c_int,
    >,
}

/// The function at `offset` in a table of `size` bytes, if the table reaches
/// that far. `None` both for a short table and for a null slot.
fn field<F: Copy>(size: usize, offset: usize, value: Option<F>) -> Option<F> {
    if size >= offset + std::mem::size_of::<Option<F>>() {
        value
    } else {
        None
    }
}

/// The shared library LOK lives in, inside a LibreOffice program directory.
pub const LIBRARY: &str = "libsofficeapp.so";

/// The program directory of the distribution package on Arch, Debian and
/// Fedora alike.
pub const DEFAULT_INSTALL: &str = "/usr/lib/libreoffice/program";

#[derive(Debug, thiserror::Error)]
pub enum LokError {
    #[error("cannot load {library}: {reason}")]
    Load { library: String, reason: String },

    #[error("{library} has no libreofficekit_hook_2; this LibreOffice is too old for LOK")]
    NoHook { library: String },

    #[error("LibreOffice did not start: {0}")]
    Init(String),

    #[error("LibreOffice could not open the document: {0}")]
    Open(String),

    #[error("LibreOffice could not save the document as {format}: {reason}")]
    Save { format: String, reason: String },

    #[error("a path holds a NUL byte, which no URL can carry")]
    Nul,
}

/// A running office. One per process: LOK cannot be booted twice.
pub struct Lok {
    office: *mut OfficeHandle,
}

impl Lok {
    /// Load the library and boot the office.
    ///
    /// `profile` is the user installation directory (what `soffice` calls
    /// `-env:UserInstallation`); the caller seeds it before this runs, so
    /// macros, Java and the update check are off before any document loads.
    pub fn boot(install: &Path, profile: &Path) -> Result<Lok, LokError> {
        let library = install.join(LIBRARY);
        let library_c = c_path(&library)?;
        // SAFETY: dlopen on a path we built; the handle is deliberately never
        // closed, because LibreOffice cannot be unloaded and the process
        // exits when the job is done.
        let handle =
            unsafe { libc::dlopen(library_c.as_ptr(), libc::RTLD_LAZY | libc::RTLD_GLOBAL) };
        if handle.is_null() {
            return Err(LokError::Load {
                library: library.display().to_string(),
                reason: dl_error(),
            });
        }
        // SAFETY: dlsym on a live handle with a NUL-terminated name.
        let hook = unsafe { libc::dlsym(handle, c"libreofficekit_hook_2".as_ptr()) };
        if hook.is_null() {
            return Err(LokError::NoHook {
                library: library.display().to_string(),
            });
        }
        // SAFETY: the symbol's C signature is
        // `LibreOfficeKit* libreofficekit_hook_2(const char*, const char*)`.
        let hook: unsafe extern "C" fn(*const c_char, *const c_char) -> *mut OfficeHandle =
            unsafe { std::mem::transmute(hook) };
        let install_c = c_path(install)?;
        let profile_c = CString::new(file_url(profile)).map_err(|_| LokError::Nul)?;
        // SAFETY: both arguments are valid C strings for the call's duration.
        let office = unsafe { hook(install_c.as_ptr(), profile_c.as_ptr()) };
        if office.is_null() {
            return Err(LokError::Init(
                "libreofficekit_hook_2 returned nothing (a broken install or a profile it cannot \
                 write)"
                    .to_string(),
            ));
        }
        // SAFETY: a non-null office always carries its class table.
        if unsafe { (*office).class.is_null() } {
            return Err(LokError::Init(
                "the office has no function table".to_string(),
            ));
        }
        Ok(Lok { office })
    }

    fn class(&self) -> &OfficeClass {
        // SAFETY: checked non-null in `boot`; the table lives as long as the
        // process.
        unsafe { &*(*self.office).class }
    }

    /// LibreOffice's own version description, JSON. Read once, as the layout
    /// check: a table read at the wrong offsets does not return JSON.
    pub fn version_info(&self) -> Option<String> {
        let class = self.class();
        let get = field(
            class.size,
            std::mem::offset_of!(OfficeClass, get_version_info),
            class.get_version_info,
        )?;
        // SAFETY: a valid office pointer; the result is a malloc'd C string
        // or null.
        let raw = unsafe { get(self.office.cast()) };
        self.take_string(raw)
    }

    /// The last error LibreOffice recorded, as text.
    fn error(&self) -> String {
        let class = self.class();
        let Some(get) = field(
            class.size,
            std::mem::offset_of!(OfficeClass, get_error),
            class.get_error,
        ) else {
            return "no detail available".to_string();
        };
        // SAFETY: as in `version_info`.
        let raw = unsafe { get(self.office.cast()) };
        self.take_string(raw)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "no detail available".to_string())
    }

    fn take_string(&self, raw: *mut c_char) -> Option<String> {
        if raw.is_null() {
            return None;
        }
        // SAFETY: LOK returns NUL-terminated strings.
        let text = unsafe { CStr::from_ptr(raw) }
            .to_string_lossy()
            .into_owned();
        let class = self.class();
        match field(
            class.size,
            std::mem::offset_of!(OfficeClass, free_error),
            class.free_error,
        ) {
            // SAFETY: freeError is the matching deallocator.
            Some(free) => unsafe { free(raw) },
            // SAFETY: before freeError existed the strings were malloc'd.
            None => unsafe { libc::free(raw.cast()) },
        }
        Some(text)
    }

    /// Open a document by path.
    pub fn open(&self, path: &Path) -> Result<Doc<'_>, LokError> {
        let url = CString::new(file_url(path)).map_err(|_| LokError::Nul)?;
        let class = self.class();
        let document = if let Some(load) = field(
            class.size,
            std::mem::offset_of!(OfficeClass, document_load_with_options),
            class.document_load_with_options,
        ) {
            // SAFETY: valid office and C strings. The empty options string is
            // what `documentLoad` itself passes.
            unsafe { load(self.office.cast(), url.as_ptr(), c"".as_ptr()) }
        } else if let Some(load) = field(
            class.size,
            std::mem::offset_of!(OfficeClass, document_load),
            class.document_load,
        ) {
            // SAFETY: as above.
            unsafe { load(self.office.cast(), url.as_ptr()) }
        } else {
            return Err(LokError::Open(
                "this LibreOffice cannot load documents".to_string(),
            ));
        };
        if document.is_null() {
            return Err(LokError::Open(self.error()));
        }
        let document = document.cast::<DocumentHandle>();
        // SAFETY: a non-null document carries its class table.
        if unsafe { (*document).class.is_null() } {
            return Err(LokError::Open(
                "the document has no function table".to_string(),
            ));
        }
        Ok(Doc {
            office: self,
            document,
        })
    }
}

/// An open document, borrowed from its office.
pub struct Doc<'a> {
    office: &'a Lok,
    document: *mut DocumentHandle,
}

impl Doc<'_> {
    fn class(&self) -> &DocumentClass {
        // SAFETY: checked non-null in `Lok::open`.
        unsafe { &*(*self.document).class }
    }

    /// Save a copy as `format` (a LibreOffice short name such as `pdf`).
    pub fn save_as(&self, path: &Path, format: &str) -> Result<(), LokError> {
        let url = CString::new(file_url(path)).map_err(|_| LokError::Nul)?;
        let format_c = CString::new(format).map_err(|_| LokError::Nul)?;
        let class = self.class();
        let Some(save) = field(
            class.size,
            std::mem::offset_of!(DocumentClass, save_as),
            class.save_as,
        ) else {
            return Err(LokError::Save {
                format: format.to_string(),
                reason: "this LibreOffice has no saveAs".to_string(),
            });
        };
        // SAFETY: valid document and C strings; null filter options mean the
        // filter's defaults.
        let ok = unsafe {
            save(
                self.document.cast(),
                url.as_ptr(),
                format_c.as_ptr(),
                std::ptr::null(),
            )
        };
        if ok == 0 {
            return Err(LokError::Save {
                format: format.to_string(),
                reason: self.office.error(),
            });
        }
        Ok(())
    }
}

impl Drop for Doc<'_> {
    fn drop(&mut self) {
        let class = self.class();
        if let Some(destroy) = field(
            class.size,
            std::mem::offset_of!(DocumentClass, destroy),
            class.destroy,
        ) {
            // SAFETY: destroyed exactly once, here.
            unsafe { destroy(self.document.cast()) };
        }
    }
}

fn c_path(path: &Path) -> Result<CString, LokError> {
    use std::os::unix::ffi::OsStrExt as _;
    CString::new(path.as_os_str().as_bytes()).map_err(|_| LokError::Nul)
}

fn dl_error() -> String {
    // SAFETY: dlerror returns a thread-local C string or null.
    let raw = unsafe { libc::dlerror() };
    if raw.is_null() {
        "unknown dlopen failure".to_string()
    } else {
        // SAFETY: non-null and NUL-terminated.
        unsafe { CStr::from_ptr(raw) }
            .to_string_lossy()
            .into_owned()
    }
}

/// A `file://` URL for an absolute path, percent-encoding every byte that is
/// not unreserved. LibreOffice parses URLs strictly: a space or a `#` left
/// raw names a different file or none.
pub fn file_url(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt as _;
    let mut url = String::from("file://");
    for &byte in path.as_os_str().as_bytes() {
        let keep = byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'.' | b'_' | b'~');
        if keep {
            url.push(byte as char);
        } else {
            url.push_str(&format!("%{byte:02X}"));
        }
    }
    url
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_url_escapes_what_a_url_would_misread() {
        // `#` would start a fragment and `%` an escape; a space is not a URL
        // character at all. Each would load a different file or none.
        let url = file_url(Path::new("/home/a b/#1 100%/Résumé.docx"));
        assert_eq!(
            url,
            "file:///home/a%20b/%231%20100%25/R%C3%A9sum%C3%A9.docx"
        );
    }

    #[test]
    fn a_table_field_is_read_only_when_the_table_reaches_it() {
        let f: Option<fn()> = Some(|| {});
        let width = std::mem::size_of::<Option<fn()>>();
        assert!(field(8 + width, 8, f).is_some());
        assert!(field(8 + width - 1, 8, f).is_none());
    }
}
