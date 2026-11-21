//! Optional HEIC/HEIF decoding through a runtime-loaded `libheif`.
//!
//! The image crate does not decode HEIC. Rather than requiring libheif at
//! build time, Pixsort loads it dynamically; if it is not installed, HEIC
//! files are indexed as failed with a clear message instead of crashing.
//!
//! The C API is used only for the well-established decode path:
//! context → primary image → RGB(A) interleaved plane. No editing APIs are used.

use std::ffi::{c_char, c_int, c_void, CStr};
use std::sync::OnceLock;

use anyhow::{anyhow, bail, Result};
use image::{DynamicImage, RgbImage, RgbaImage};
use libloading::Library;

const HEIF_COLORSPACE_RGB: c_int = 1;
const HEIF_CHROMA_INTERLEAVED_RGB: c_int = 10;
const HEIF_CHROMA_INTERLEAVED_RGBA: c_int = 11;
const HEIF_CHANNEL_INTERLEAVED: c_int = 10;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct HeifError {
    code: c_int,
    subcode: c_int,
    message: *const c_char,
}

type FnContextAlloc = unsafe extern "C" fn() -> *mut c_void;
type FnContextFree = unsafe extern "C" fn(*mut c_void);
type FnReadFromMemory =
    unsafe extern "C" fn(*mut c_void, *const u8, usize, *mut c_void) -> HeifError;
type FnGetPrimaryHandle = unsafe extern "C" fn(*mut c_void, *mut *mut c_void) -> HeifError;
type FnHandleWidth = unsafe extern "C" fn(*const c_void) -> c_int;
type FnHandleHeight = unsafe extern "C" fn(*const c_void) -> c_int;
type FnDecodeImage =
    unsafe extern "C" fn(*const c_void, *mut *mut c_void, c_int, c_int, *const c_void) -> HeifError;
type FnGetPlane = unsafe extern "C" fn(*const c_void, c_int, *mut c_int) -> *const u8;
type FnImageRelease = unsafe extern "C" fn(*const c_void);
type FnHandleRelease = unsafe extern "C" fn(*const c_void);

struct LibHeif {
    _library: Library,
    context_alloc: FnContextAlloc,
    context_free: FnContextFree,
    read_from_memory: FnReadFromMemory,
    get_primary_handle: FnGetPrimaryHandle,
    handle_width: FnHandleWidth,
    handle_height: FnHandleHeight,
    decode_image: FnDecodeImage,
    get_plane: FnGetPlane,
    image_release: FnImageRelease,
    handle_release: FnHandleRelease,
}

// Function pointers and a dlopen handle are safe to share; decoding itself
// only uses per-call context objects.
unsafe impl Send for LibHeif {}
unsafe impl Sync for LibHeif {}

fn library() -> Result<&'static LibHeif> {
    static LIB: OnceLock<Result<LibHeif, String>> = OnceLock::new();
    LIB.get_or_init(load_library)
        .as_ref()
        .map_err(|message| anyhow!(message.clone()))
}

fn load_library() -> Result<LibHeif, String> {
    let mut candidates: Vec<String> = Vec::new();
    if let Ok(path) = std::env::var("PIXSORT_LIBHEIF") {
        if !path.is_empty() {
            candidates.push(path);
        }
    }
    #[cfg(target_os = "windows")]
    candidates.extend(["heif.dll".to_string(), "libheif.dll".to_string()]);
    #[cfg(target_os = "macos")]
    candidates.extend([
        "libheif.dylib".to_string(),
        "/opt/homebrew/lib/libheif.dylib".to_string(),
        "/usr/local/lib/libheif.dylib".to_string(),
    ]);
    #[cfg(all(unix, not(target_os = "macos")))]
    candidates.extend([
        "libheif.so.1".to_string(),
        "libheif.so".to_string(),
        "libheif.so.2".to_string(),
    ]);

    let mut last_error = String::new();
    for name in candidates {
        // SAFETY: the library is kept alive for the whole process inside LibHeif.
        let lib = match unsafe { Library::new(&name) } {
            Ok(lib) => lib,
            Err(err) => {
                last_error = format!("{name}: {err}");
                continue;
            }
        };
        // SAFETY: these symbol names/signatures match the stable libheif C API.
        unsafe {
            macro_rules! sym {
                ($name:literal) => {
                    match lib.get::<*const c_void>($name) {
                        Ok(symbol) => *symbol as *const (),
                        Err(err) => {
                            return Err(format!(
                                "{} missing symbol {}: {err}",
                                name,
                                String::from_utf8_lossy($name)
                            ))
                        }
                    }
                };
            }
            let context_alloc = sym!(b"heif_context_alloc\0");
            let context_free = sym!(b"heif_context_free\0");
            let read_from_memory = sym!(b"heif_context_read_from_memory_without_copy\0");
            let get_primary_handle = sym!(b"heif_context_get_primary_image_handle\0");
            let handle_width = sym!(b"heif_image_handle_get_width\0");
            let handle_height = sym!(b"heif_image_handle_get_height\0");
            let decode_image = sym!(b"heif_decode_image\0");
            let get_plane = sym!(b"heif_image_get_plane_readonly\0");
            let image_release = sym!(b"heif_image_release\0");
            let handle_release = sym!(b"heif_image_handle_release\0");

            return Ok(LibHeif {
                _library: lib,
                context_alloc: std::mem::transmute::<*const (), FnContextAlloc>(context_alloc),
                context_free: std::mem::transmute::<*const (), FnContextFree>(context_free),
                read_from_memory: std::mem::transmute::<*const (), FnReadFromMemory>(
                    read_from_memory,
                ),
                get_primary_handle: std::mem::transmute::<*const (), FnGetPrimaryHandle>(
                    get_primary_handle,
                ),
                handle_width: std::mem::transmute::<*const (), FnHandleWidth>(handle_width),
                handle_height: std::mem::transmute::<*const (), FnHandleHeight>(handle_height),
                decode_image: std::mem::transmute::<*const (), FnDecodeImage>(decode_image),
                get_plane: std::mem::transmute::<*const (), FnGetPlane>(get_plane),
                image_release: std::mem::transmute::<*const (), FnImageRelease>(image_release),
                handle_release: std::mem::transmute::<*const (), FnHandleRelease>(handle_release),
            });
        }
    }
    Err(format!(
        "HEIC support requires libheif (set PIXSORT_LIBHEIF to a library path). {}",
        last_error
    ))
}

pub fn is_available() -> bool {
    library().is_ok()
}

pub fn availability_message() -> String {
    match library() {
        Ok(_) => "libheif available".into(),
        Err(err) => err.to_string(),
    }
}

fn error_text(err: HeifError) -> String {
    let message = if err.message.is_null() {
        String::new()
    } else {
        // SAFETY: libheif returns a NUL-terminated static string.
        unsafe { CStr::from_ptr(err.message) }
            .to_string_lossy()
            .into_owned()
    };
    if message.is_empty() {
        format!("libheif error {} (subcode {})", err.code, err.subcode)
    } else {
        format!(
            "libheif error {} (subcode {}): {message}",
            err.code, err.subcode
        )
    }
}

struct ContextGuard<'a> {
    lib: &'a LibHeif,
    context: *mut c_void,
}
impl Drop for ContextGuard<'_> {
    fn drop(&mut self) {
        // SAFETY: context allocated by this library and not used afterwards.
        unsafe { (self.lib.context_free)(self.context) };
    }
}

struct HandleGuard<'a> {
    lib: &'a LibHeif,
    handle: *mut c_void,
}
impl Drop for HandleGuard<'_> {
    fn drop(&mut self) {
        // SAFETY: handle from this context and not used afterwards.
        unsafe { (self.lib.handle_release)(self.handle) };
    }
}

struct ImageGuard<'a> {
    lib: &'a LibHeif,
    image: *mut c_void,
}
impl Drop for ImageGuard<'_> {
    fn drop(&mut self) {
        // SAFETY: image from this library and not used afterwards.
        unsafe { (self.lib.image_release)(self.image) };
    }
}

/// Decodes the primary image of a HEIC/HEIF buffer.
pub fn decode(bytes: &[u8]) -> Result<DynamicImage> {
    if bytes.is_empty() {
        bail!("empty HEIC file");
    }
    let lib = library()?;

    // SAFETY: all pointers are checked, and RAII guards release libheif
    // objects. The input buffer outlives the context because
    // read_from_memory_without_copy only borrows it during decode.
    unsafe {
        let context = (lib.context_alloc)();
        if context.is_null() {
            bail!("libheif could not allocate a context");
        }
        let context = ContextGuard { lib, context };

        let read_err = (lib.read_from_memory)(
            context.context,
            bytes.as_ptr(),
            bytes.len(),
            std::ptr::null_mut(),
        );
        if read_err.code != 0 {
            bail!(error_text(read_err));
        }

        let mut handle: *mut c_void = std::ptr::null_mut();
        let handle_err = (lib.get_primary_handle)(context.context, &mut handle);
        if handle_err.code != 0 || handle.is_null() {
            bail!(error_text(handle_err));
        }
        let handle = HandleGuard { lib, handle };

        let width = (lib.handle_width)(handle.handle);
        let height = (lib.handle_height)(handle.handle);
        if width <= 0 || height <= 0 {
            bail!("HEIC image has invalid dimensions");
        }
        let (w, h) = (width as u32, height as u32);

        let mut image: *mut c_void = std::ptr::null_mut();
        let mut decode_err = (lib.decode_image)(
            handle.handle,
            &mut image,
            HEIF_COLORSPACE_RGB,
            HEIF_CHROMA_INTERLEAVED_RGBA,
            std::ptr::null(),
        );
        let mut channels = 4usize;
        if decode_err.code != 0 || image.is_null() {
            // Some HEIF files have no alpha plane; retry as RGB.
            image = std::ptr::null_mut();
            decode_err = (lib.decode_image)(
                handle.handle,
                &mut image,
                HEIF_COLORSPACE_RGB,
                HEIF_CHROMA_INTERLEAVED_RGB,
                std::ptr::null(),
            );
            channels = 3;
        }
        if decode_err.code != 0 || image.is_null() {
            bail!(error_text(decode_err));
        }
        let image = ImageGuard { lib, image };

        let mut stride: c_int = 0;
        let plane = (lib.get_plane)(image.image, HEIF_CHANNEL_INTERLEAVED, &mut stride);
        if plane.is_null() {
            bail!("libheif returned no pixel plane");
        }
        let stride = stride.max(0) as usize;
        let row_bytes = w as usize * channels;
        if stride < row_bytes {
            bail!("libheif returned an invalid stride");
        }

        let mut buffer = Vec::with_capacity(row_bytes * h as usize);
        for row in 0..h as usize {
            let start = plane.add(row * stride);
            buffer.extend_from_slice(std::slice::from_raw_parts(start, row_bytes));
        }

        if channels == 4 {
            let image = RgbaImage::from_raw(w, h, buffer)
                .ok_or_else(|| anyhow!("could not build RGBA image from libheif output"))?;
            Ok(DynamicImage::ImageRgba8(image))
        } else {
            let image = RgbImage::from_raw(w, h, buffer)
                .ok_or_else(|| anyhow!("could not build RGB image from libheif output"))?;
            Ok(DynamicImage::ImageRgb8(image))
        }
    }
}
