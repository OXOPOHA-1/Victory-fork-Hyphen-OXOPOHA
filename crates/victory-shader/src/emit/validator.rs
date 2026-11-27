//! The DXIL validator in `dxil.dll` (`IDxcValidator`), for tests and the
//! offline batch. Translated shaders do not need it: they carry the
//! container hash we compute (`container::hash`).
//!
//! Use the newest installed Windows SDK validator. `VICTORY_DXIL` may
//! select another installed SDK version by its full path.

use std::ffi::{OsString, c_void};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use windows_sys::Win32::Foundation::{FreeLibrary, HMODULE, MAX_PATH};
use windows_sys::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};
use windows_sys::Win32::UI::Shell::{CSIDL_PROGRAM_FILESX86, SHGetFolderPathW};

type Hresult = i32;

#[repr(C)]
struct Guid(u32, u16, u16, [u8; 8]);

const CLSID_DXC_VALIDATOR: Guid = Guid(
    0x8ca3_e215,
    0xf728,
    0x4cf3,
    [0x8c, 0xdd, 0x88, 0xaf, 0x91, 0x75, 0x87, 0xa1],
);
const IID_IDXC_VALIDATOR: Guid = Guid(
    0xa6e8_2bd2,
    0x1fd7,
    0x4826,
    [0x98, 0x11, 0x28, 0x57, 0xe7, 0x97, 0xf4, 0x9a],
);
/// `DxcValidatorFlags_InPlaceEdit`: sign the container we pass.
const IN_PLACE_EDIT: u32 = 1;

type CreateInstance =
    unsafe extern "system" fn(*const Guid, *const Guid, *mut *mut c_void) -> Hresult;

/// COM object: a pointer to a table of method pointers.
type Object = *mut *const usize;

unsafe fn method<F: Copy>(obj: Object, slot: usize) -> F {
    // SAFETY: the caller names a slot of the object's interface.
    unsafe { std::mem::transmute_copy(&*(*obj).add(slot)) }
}

unsafe fn release(obj: Object) {
    if !obj.is_null() {
        // SAFETY: slot 2 of every COM interface is Release.
        unsafe { method::<unsafe extern "system" fn(Object) -> u32>(obj, 2)(obj) };
    }
}

/// An `IDxcBlob` over our bytes: QueryInterface, AddRef, Release,
/// GetBufferPointer, GetBufferSize. Lives on the caller's stack.
#[repr(C)]
struct Blob {
    vtable: *const BlobVtable,
    data: *mut u8,
    len: usize,
}

/// IUnknown and IDxcBlob, as raw GUID bytes.
const IUNKNOWN: [u8; 16] = [0, 0, 0, 0, 0, 0, 0, 0, 0xc0, 0, 0, 0, 0, 0, 0, 0x46];
const IDXC_BLOB: [u8; 16] = [
    0x08, 0xfb, 0xa5, 0x8b, 0x95, 0x51, 0xe2, 0x40, 0xac, 0x58, 0x0d, 0x98, 0x9c, 0x3a, 0x01, 0x02,
];

unsafe extern "system" fn blob_qi(
    this: *mut Blob,
    iid: *const [u8; 16],
    out: *mut *mut c_void,
) -> Hresult {
    // SAFETY: the validator passes valid pointers.
    unsafe {
        if *iid == IUNKNOWN || *iid == IDXC_BLOB {
            *out = this.cast();
            0
        } else {
            *out = std::ptr::null_mut();
            0x8000_4002_u32 as i32 // E_NOINTERFACE
        }
    }
}
unsafe extern "system" fn blob_ref(_: *mut Blob) -> u32 {
    1
}
unsafe extern "system" fn blob_ptr(this: *mut Blob) -> *mut u8 {
    // SAFETY: `this` is our blob.
    unsafe { (*this).data }
}
unsafe extern "system" fn blob_len(this: *mut Blob) -> usize {
    // SAFETY: `this` is our blob.
    unsafe { (*this).len }
}

#[repr(C)]
struct BlobVtable {
    qi: unsafe extern "system" fn(*mut Blob, *const [u8; 16], *mut *mut c_void) -> Hresult,
    add_ref: unsafe extern "system" fn(*mut Blob) -> u32,
    release: unsafe extern "system" fn(*mut Blob) -> u32,
    ptr: unsafe extern "system" fn(*mut Blob) -> *mut u8,
    len: unsafe extern "system" fn(*mut Blob) -> usize,
}

static BLOB_VTABLE: BlobVtable = BlobVtable {
    qi: blob_qi,
    add_ref: blob_ref,
    release: blob_ref,
    ptr: blob_ptr,
    len: blob_len,
};

pub struct Validator {
    object: Object,
    module: HMODULE,
}

// SAFETY: the validator object is free-threaded (DXC's validator has no
// thread affinity); calls are serialised by `&mut self`.
unsafe impl Send for Validator {}

/// Enumerate only installed SDKs under Windows' Program Files (x86).
/// Do not consult PATH, the current directory, or a user-supplied SDK root.
fn installed_validators() -> &'static [PathBuf] {
    static INSTALLED: OnceLock<Vec<PathBuf>> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        let mut buf = [0u16; MAX_PATH as usize];
        // SAFETY: the documented MAX_PATH output buffer, current user, no window.
        let hr = unsafe {
            SHGetFolderPathW(
                std::ptr::null_mut(),
                CSIDL_PROGRAM_FILESX86 as i32,
                std::ptr::null_mut(),
                0,
                buf.as_mut_ptr(),
            )
        };
        if hr < 0 {
            return Vec::new();
        }
        let Some(end) = buf.iter().position(|&c| c == 0) else {
            return Vec::new();
        };
        let root = PathBuf::from(OsString::from_wide(&buf[..end]));
        let bin = root.join(r"Windows Kits\10\bin");
        let (Ok(root), Ok(entries)) = (root.canonicalize(), bin.read_dir()) else {
            return Vec::new();
        };
        let mut versions: Vec<_> = entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let name = entry.file_name();
                let mut parts = name.to_str()?.split('.');
                let mut version = [0u32; 4];
                for field in &mut version {
                    *field = parts.next()?.parse().ok()?;
                }
                if parts.next().is_some() {
                    return None;
                }
                let tail = Path::new(r"Windows Kits\10\bin")
                    .join(name)
                    .join(r"x64\dxil.dll");
                let path = entry.path().join(r"x64\dxil.dll");
                // A junction or symlink into a writable folder is not an installed validator.
                (path.is_file() && path.canonicalize().ok()? == root.join(tail))
                    .then_some((version, path))
            })
            .collect();
        versions.sort_unstable();
        versions.into_iter().map(|(_, path)| path).collect()
    })
}

fn select<'a>(installed: &'a [PathBuf], requested: Option<&Path>) -> Option<&'a PathBuf> {
    match requested {
        Some(path) => installed
            .iter()
            .find(|p| p.as_os_str().eq_ignore_ascii_case(path.as_os_str())),
        None => installed.last(),
    }
}

impl Validator {
    pub fn default_path() -> Option<PathBuf> {
        let requested = std::env::var_os("VICTORY_DXIL");
        select(installed_validators(), requested.as_deref().map(Path::new)).cloned()
    }

    /// Refuse unknown paths before the Windows loader can run any DLL code.
    pub fn load(path: Option<&Path>) -> Result<Self, String> {
        let path = path.and_then(|p| select(installed_validators(), Some(p)))
            .ok_or("no installed x64 Windows SDK validator selected; install the Windows SDK or unset VICTORY_DXIL")?;
        let name: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
        // SAFETY: an installed DLL by absolute path; resolve dependencies in System32.
        unsafe {
            let module = LoadLibraryExW(
                name.as_ptr(),
                std::ptr::null_mut(),
                LOAD_LIBRARY_SEARCH_SYSTEM32,
            );
            if module.is_null() {
                return Err(format!(
                    "cannot load {}: {}",
                    path.display(),
                    std::io::Error::last_os_error()
                ));
            }
            let Some(create) = GetProcAddress(module, c"DxcCreateInstance".as_ptr().cast()) else {
                FreeLibrary(module);
                return Err("dxil.dll lacks DxcCreateInstance".into());
            };
            let create: CreateInstance = std::mem::transmute(create);
            let mut object = std::ptr::null_mut();
            let hr = create(&CLSID_DXC_VALIDATOR, &IID_IDXC_VALIDATOR, &mut object);
            if hr < 0 || object.is_null() {
                FreeLibrary(module);
                return Err(format!("DxcCreateInstance(validator) failed: {hr:#x}"));
            }
            Ok(Validator {
                object: object.cast(),
                module,
            })
        }
    }

    /// Validates `container` and signs it in place; the error text if it
    /// is rejected.
    pub fn validate(&mut self, container: &mut [u8]) -> Result<(), String> {
        let mut blob = Blob {
            vtable: &BLOB_VTABLE,
            data: container.as_mut_ptr(),
            len: container.len(),
        };
        // SAFETY: IDxcValidator::Validate (slot 3) with our blob; then
        // IDxcOperationResult GetStatus (3) and GetErrorBuffer (5), and
        // IDxcBlob GetBufferPointer (3) / GetBufferSize (4).
        unsafe {
            let mut result: Object = std::ptr::null_mut();
            let validate: unsafe extern "system" fn(
                Object,
                *mut Blob,
                u32,
                *mut Object,
            ) -> Hresult = method(self.object, 3);
            let hr = validate(self.object, &mut blob, IN_PLACE_EDIT, &mut result);
            if hr < 0 || result.is_null() {
                return Err(format!("Validate failed: {hr:#x}"));
            }
            let mut status = 0;
            let get_status: unsafe extern "system" fn(Object, *mut Hresult) -> Hresult =
                method(result, 3);
            get_status(result, &mut status);
            let mut text = String::new();
            if status < 0 {
                let mut errors: Object = std::ptr::null_mut();
                let get_errors: unsafe extern "system" fn(Object, *mut Object) -> Hresult =
                    method(result, 5);
                if get_errors(result, &mut errors) >= 0 && !errors.is_null() {
                    let ptr: unsafe extern "system" fn(Object) -> *const u8 = method(errors, 3);
                    let len: unsafe extern "system" fn(Object) -> usize = method(errors, 4);
                    let bytes = std::slice::from_raw_parts(ptr(errors), len(errors));
                    text = String::from_utf8_lossy(bytes)
                        .trim_end_matches('\0')
                        .trim()
                        .into();
                    release(errors);
                }
                if text.is_empty() {
                    text = format!("rejected ({status:#x})");
                }
            }
            release(result);
            if status < 0 { Err(text) } else { Ok(()) }
        }
    }
}

impl Drop for Validator {
    fn drop(&mut self) {
        // SAFETY: release the object while its vtable is still mapped.
        unsafe {
            release(self.object);
            FreeLibrary(self.module);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_installed_validator_can_be_selected() {
        let sdks = [
            PathBuf::from(r"C:\SDK\1\x64\dxil.dll"),
            PathBuf::from(r"C:\SDK\2\x64\dxil.dll"),
        ];
        assert_eq!(select(&sdks, None), Some(&sdks[1]));
        assert_eq!(
            select(&sdks, Some(Path::new(r"c:\sdk\1\x64\DXIL.DLL"))),
            Some(&sdks[0])
        );
        for p in [
            "dxil.dll",
            r".\dxil.dll",
            r"C:\Games\dxil.dll",
            r"\\server\share\dxil.dll",
        ] {
            assert_eq!(select(&sdks, Some(Path::new(p))), None);
            assert!(Validator::load(Some(Path::new(p))).is_err());
        }
        assert!(Validator::load(None).is_err());
    }
}
