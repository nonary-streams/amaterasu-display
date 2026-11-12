use crate::model::{DisplayError, DisplayPath, DisplayResult};
use crate::profile_transaction::ProfileAdapter;
use amaterasu_contracts::display_color_profile::{
    ColorProfileState, HdrProfileDescriptor, HdrProfileSelection,
};
use sha2::{Digest, Sha256};
use std::{
    ffi::{CString, c_void},
    fs,
    fs::File,
    path::{Path, PathBuf},
};
use windows_sys::Win32::Foundation::{FreeLibrary, GetLastError, LUID, LocalFree};
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryExW};
use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;

const MAX_PROFILE_BYTES: usize = 32 * 1024 * 1024;
const COLOR_PROFILE_CURRENT_USER: i32 = 0;
const COLOR_PROFILE_ICC: u32 = 0;
const COLOR_PROFILE_EXTENDED_DISPLAY_COLOR_MODE: u32 = 8;
const ERROR_ALREADY_EXISTS: u32 = 183;
const ERROR_FILE_EXISTS: u32 = 80;

#[derive(Clone, Copy)]
pub struct Target {
    pub adapter: LUID,
    pub source_id: u32,
}

impl From<&DisplayPath> for Target {
    fn from(path: &DisplayPath) -> Self {
        Self {
            adapter: LUID {
                LowPart: path.target_address.adapter_low,
                HighPart: path.target_address.adapter_high,
            },
            source_id: path.source_address.id,
        }
    }
}

pub fn resolve(selection: &HdrProfileSelection) -> DisplayResult<HdrProfileDescriptor> {
    selection
        .validate()
        .map_err(|code| DisplayError::new(code, "invalid HDR profile catalog selection"))?;
    let path = catalog_path(selection)?;
    let bytes = read_profile(&path)?;
    let revision = format!("sha256:{}", hex(&Sha256::digest(&bytes)));
    let resolved_selection = HdrProfileSelection {
        id: path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| {
                DisplayError::new(
                    "hdr_profile_not_found",
                    "profile catalog filename is invalid",
                )
            })?
            .to_owned(),
    };
    Ok(HdrProfileDescriptor {
        selection: resolved_selection,
        revision,
        peak_luminance_nits: mhc2_peak_luminance(&bytes),
    })
}

pub fn observed_profile(path: &DisplayPath) -> DisplayResult<ColorProfileState> {
    let Ok(api) = AdvancedColorApi::load() else {
        return Ok(ColorProfileState::Unknown);
    };
    let profile = match api.default_profile(Target::from(path)) {
        Ok(profile) => profile,
        Err(_) => return Ok(ColorProfileState::Unknown),
    };
    let Some(filename) = profile else {
        return Ok(ColorProfileState::SystemDefault);
    };
    let selection = HdrProfileSelection { id: filename };
    match resolve(&selection) {
        Ok(descriptor) => Ok(ColorProfileState::Selected(descriptor)),
        Err(_) => Ok(ColorProfileState::Unknown),
    }
}

pub fn set_profile(path: &DisplayPath, profile: &ColorProfileState) -> DisplayResult<()> {
    let api = AdvancedColorApi::load()?;
    let mut adapter = TargetProfileAdapter {
        api,
        target: Target::from(path),
    };
    crate::profile_transaction::apply(&mut adapter, profile).map_err(|failure| {
        if failure.recovery.is_some() {
            DisplayError::after_mutation(DisplayError::new(
                "profile_rollback_failed",
                "color profile mutation could not restore the exact prior association",
            ))
        } else {
            *failure.error
        }
    })?;
    Ok(())
}

pub fn force_restore(path: &DisplayPath, profile: &ColorProfileState) -> DisplayResult<()> {
    if matches!(
        profile,
        ColorProfileState::Unknown | ColorProfileState::Unspecified
    ) {
        return Err(DisplayError::new(
            "profile_state_unknown",
            "exact saved profile state is required for restoration",
        ));
    }
    let api = AdvancedColorApi::load()?;
    let mut adapter = TargetProfileAdapter {
        api,
        target: Target::from(path),
    };
    adapter
        .write(profile)
        .map_err(DisplayError::after_mutation)?;
    let observed = adapter.read().map_err(DisplayError::after_mutation)?;
    if observed != *profile {
        return Err(DisplayError::after_mutation(DisplayError::new(
            "profile_restore_mismatch",
            "restored profile did not match exact readback",
        )));
    }
    Ok(())
}

struct TargetProfileAdapter {
    api: AdvancedColorApi,
    target: Target,
}

impl crate::profile_transaction::ProfileAdapter for TargetProfileAdapter {
    fn read(&mut self) -> DisplayResult<ColorProfileState> {
        let Some(filename) = self.api.default_profile(self.target)? else {
            return Ok(ColorProfileState::SystemDefault);
        };
        let selection = HdrProfileSelection { id: filename };
        match resolve(&selection) {
            Ok(descriptor) => Ok(ColorProfileState::Selected(descriptor)),
            Err(_) => Ok(ColorProfileState::Unknown),
        }
    }

    fn write(&mut self, profile: &ColorProfileState) -> DisplayResult<()> {
        if let Some(existing) = self.api.default_profile(self.target)? {
            self.api.remove(self.target, &existing)?;
        }
        match profile {
            ColorProfileState::Selected(descriptor) => {
                let current = resolve(&descriptor.selection)?;
                if current != *descriptor {
                    return Err(DisplayError::new(
                        "hdr_profile_changed",
                        "resolved profile revision changed before association",
                    ));
                }
                let path = catalog_path(&descriptor.selection)?;
                self.api.install(&path)?;
                self.api.add(self.target, &descriptor.selection.id)
            }
            ColorProfileState::SystemDefault => Ok(()),
            ColorProfileState::Unspecified | ColorProfileState::Unknown => Err(DisplayError::new(
                "invalid_color_profile_state",
                "profile writes require a concrete selected or default state",
            )),
        }
    }
}

fn catalog_path(selection: &HdrProfileSelection) -> DisplayResult<PathBuf> {
    selection
        .validate()
        .map_err(|code| DisplayError::new(code, "invalid HDR profile catalog selection"))?;
    let requested_name = Path::new(&selection.id);
    if requested_name.file_name().and_then(|name| name.to_str()) != Some(selection.id.as_str()) {
        return Err(DisplayError::new(
            "hdr_profile_not_found",
            "selected profile is not a catalog filename",
        ));
    }
    let has_extension = requested_name.extension().is_some();
    if has_extension
        && !matches!(
            requested_name
                .extension()
                .and_then(|ext| ext.to_str())
                .map(str::to_ascii_lowercase)
                .as_deref(),
            Some("icc" | "icm")
        )
    {
        return Err(DisplayError::new(
            "hdr_profile_not_found",
            "selected profile must use the ICC or ICM catalog extension",
        ));
    }
    let mut system_directory = vec![0u16; 32_768];
    // SAFETY: the output buffer is writable for its advertised capacity and
    // the API writes a NUL-terminated path bounded by that capacity.
    let length = unsafe {
        GetSystemDirectoryW(system_directory.as_mut_ptr(), system_directory.len() as u32)
    } as usize;
    if length == 0 || length >= system_directory.len() {
        return Err(DisplayError::new(
            "unsupported_hdr_profile",
            "Windows system directory is unavailable",
        ));
    }
    let system_directory = String::from_utf16(&system_directory[..length]).map_err(|_| {
        DisplayError::new(
            "unsupported_hdr_profile",
            "Windows system directory is invalid",
        )
    })?;
    let root = PathBuf::from(system_directory)
        .join("spool")
        .join("drivers")
        .join("color");
    let root = fs::canonicalize(&root).map_err(|_| {
        DisplayError::new(
            "unsupported_hdr_profile",
            "system color profile catalog is unavailable",
        )
    })?;
    let mut candidates = vec![selection.id.clone()];
    if !has_extension {
        candidates.push(format!("{}.icm", selection.id));
        candidates.push(format!("{}.icc", selection.id));
    }
    for candidate_name in candidates {
        let Ok(candidate) = fs::canonicalize(root.join(candidate_name)) else {
            continue;
        };
        if candidate.parent() == Some(root.as_path()) && candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(DisplayError::new(
        "hdr_profile_not_found",
        "selected color profile is not installed in the system catalog",
    ))
}

fn read_profile(path: &Path) -> DisplayResult<Vec<u8>> {
    let file = File::open(path).map_err(|_| {
        DisplayError::new(
            "hdr_profile_not_found",
            "selected color profile cannot be read",
        )
    })?;
    crate::profile_reader::read_bounded_profile(file, MAX_PROFILE_BYTES, 132).map_err(|error| {
        match error {
            crate::profile_reader::ProfileReadError::Io(_) => DisplayError::new(
                "hdr_profile_not_found",
                "selected color profile cannot be read",
            ),
            crate::profile_reader::ProfileReadError::TooLarge
            | crate::profile_reader::ProfileReadError::TooSmall => DisplayError::new(
                "invalid_hdr_profile",
                "profile size is outside the supported bound",
            ),
        }
    })
}

fn mhc2_peak_luminance(bytes: &[u8]) -> Option<u32> {
    if bytes.len() < 132 || bytes.len() > MAX_PROFILE_BYTES {
        return None;
    }
    let tag_count = read_be_u32(bytes, 128)? as usize;
    if tag_count > (bytes.len() - 132) / 12 {
        return None;
    }
    for index in 0..tag_count {
        let entry = 132usize.checked_add(index.checked_mul(12)?)?;
        if bytes.get(entry..entry + 4)? != b"MHC2" {
            continue;
        }
        let offset = read_be_u32(bytes, entry + 4)? as usize;
        let length = read_be_u32(bytes, entry + 8)? as usize;
        let end = offset.checked_add(length)?;
        if length < 20 || end > bytes.len() || bytes.get(offset..offset + 4)? != b"MHC2" {
            return None;
        }
        let fixed = read_be_u32(bytes, offset + 16)? as i32;
        if fixed <= 0 {
            return None;
        }
        let nits = ((i64::from(fixed) + 32768) / 65536) as u32;
        return (nits > 0 && nits <= 100_000).then_some(nits);
    }
    None
}

fn read_be_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_be_bytes(
        bytes.get(offset..offset.checked_add(4)?)?.try_into().ok()?,
    ))
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(DIGITS[(byte >> 4) as usize] as char);
        output.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    output
}

struct AdvancedColorApi {
    module: *mut c_void,
    get_default: GetDefault,
    add: AddAssociation,
    remove: RemoveAssociation,
    install: Option<InstallProfile>,
}

type GetDefault = unsafe extern "system" fn(i32, LUID, u32, u32, u32, *mut *mut u16) -> i32;
type AddAssociation = unsafe extern "system" fn(i32, *const u16, LUID, u32, i32, i32) -> i32;
type RemoveAssociation = unsafe extern "system" fn(i32, *const u16, LUID, u32, i32) -> i32;
type InstallProfile = unsafe extern "system" fn(*const u16, *const u16) -> i32;

impl AdvancedColorApi {
    fn load() -> DisplayResult<Self> {
        let name: Vec<u16> = "Mscms.dll\0".encode_utf16().collect();
        // SAFETY: the DLL name is a valid, nul-terminated UTF-16 string and Windows owns the returned module handle.
        let module = unsafe { LoadLibraryExW(name.as_ptr(), std::ptr::null_mut(), 0x0000_0800) };
        if module.is_null() {
            return Err(DisplayError::new(
                "unsupported_hdr_profile",
                "Windows Advanced Color profile APIs are unavailable",
            ));
        }
        // SAFETY: each symbol is checked for presence before transmuting to the documented Windows ABI signature.
        let get_default =
            unsafe { resolve_symbol::<GetDefault>(module, b"ColorProfileGetDisplayDefault\0") };
        // SAFETY: symbol lookup validates the symbol exists and the target ABI matches the documented Mscms export.
        let add = unsafe {
            resolve_symbol::<AddAssociation>(module, b"ColorProfileAddDisplayAssociation\0")
        };
        // SAFETY: symbol lookup validates the symbol exists and the target ABI matches the documented Mscms export.
        let remove = unsafe {
            resolve_symbol::<RemoveAssociation>(module, b"ColorProfileRemoveDisplayAssociation\0")
        };
        // SAFETY: profile installation is optional for already installed catalog entries.
        let install =
            unsafe { resolve_symbol::<InstallProfile>(module, b"InstallColorProfileW\0") };
        match (get_default, add, remove) {
            (Some(get_default), Some(add), Some(remove)) => Ok(Self {
                module,
                get_default,
                add,
                remove,
                install,
            }),
            _ => {
                // SAFETY: no function pointer escaped this constructor.
                unsafe {
                    FreeLibrary(module);
                }
                Err(DisplayError::new(
                    "unsupported_hdr_profile",
                    "Windows Advanced Color profile APIs are unavailable",
                ))
            }
        }
    }

    fn default_profile(&self, target: Target) -> DisplayResult<Option<String>> {
        let mut name = std::ptr::null_mut();
        // SAFETY: function and output pointer follow the Mscms contract; returned memory is released with LocalFree.
        let result = unsafe {
            (self.get_default)(
                COLOR_PROFILE_CURRENT_USER,
                target.adapter,
                target.source_id,
                COLOR_PROFILE_ICC,
                COLOR_PROFILE_EXTENDED_DISPLAY_COLOR_MODE,
                &mut name,
            )
        };
        if result < 0 {
            if name.is_null() && matches!(result as u32, 0x8007_0002 | 0x8007_0490) {
                return Ok(None);
            }
            if !name.is_null() {
                // SAFETY: Mscms may set the output even when it reports an error.
                unsafe {
                    LocalFree(name.cast());
                }
            }
            return Err(DisplayError::new(
                "profile_query_failed",
                "Windows could not read the per-display Advanced Color profile",
            ));
        }
        if name.is_null() {
            return Ok(None);
        }
        // SAFETY: successful Mscms call returns a NUL-terminated UTF-16 buffer owned by the caller.
        let mut length = 0usize;
        // SAFETY: walk the API-owned NUL-terminated UTF-16 buffer; LocalFree is called below.
        unsafe {
            while *name.add(length) != 0 {
                length += 1;
            }
        }
        // SAFETY: the slice covers the measured, API-owned UTF-16 string before its terminator.
        let wide = unsafe { std::slice::from_raw_parts(name, length) };
        let decoded = String::from_utf16(wide).map_err(|_| {
            DisplayError::new(
                "profile_readback_failed",
                "Windows returned invalid profile text",
            )
        });
        // SAFETY: Mscms allocates this output with the Windows local allocator.
        unsafe {
            LocalFree(name.cast());
        }
        let decoded = decoded?;
        let filename = Path::new(&decoded)
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| {
                DisplayError::new(
                    "profile_readback_failed",
                    "Windows profile identity is not a catalog filename",
                )
            })?;
        Ok(Some(filename.to_owned()))
    }

    fn add(&self, target: Target, name: &str) -> DisplayResult<()> {
        let wide = wide(name);
        // SAFETY: pointers remain valid during call and target identifies the one CCD target.
        let result = unsafe {
            (self.add)(
                COLOR_PROFILE_CURRENT_USER,
                wide.as_ptr(),
                target.adapter,
                target.source_id,
                1,
                1,
            )
        };
        hresult(
            result,
            "Windows rejected the per-display profile association",
        )
    }

    fn remove(&self, target: Target, name: &str) -> DisplayResult<()> {
        let wide = wide(name);
        // SAFETY: pointers remain valid during call and target identifies the one CCD target.
        let result = unsafe {
            (self.remove)(
                COLOR_PROFILE_CURRENT_USER,
                wide.as_ptr(),
                target.adapter,
                target.source_id,
                1,
            )
        };
        hresult(result, "Windows rejected the per-display profile removal")
    }

    fn install(&self, path: &Path) -> DisplayResult<()> {
        let Some(install) = self.install else {
            return Ok(());
        };
        let path = path.to_string_lossy();
        let path_wide = wide(&path);
        // SAFETY: a null machine name means the current machine; the path is canonical and remains alive during call.
        let result = unsafe { install(std::ptr::null(), path_wide.as_ptr()) };
        if result == 0 {
            // Already-installed catalog files are valid; any other installation error is surfaced.
            let code = unsafe { GetLastError() };
            if code != ERROR_ALREADY_EXISTS && code != ERROR_FILE_EXISTS {
                return Err(DisplayError::new(
                    "profile_install_failed",
                    "Windows could not install the selected color profile",
                ));
            }
        }
        Ok(())
    }
}

impl Drop for AdvancedColorApi {
    fn drop(&mut self) {
        // SAFETY: module was loaded by this object and is no longer used after Drop.
        unsafe {
            FreeLibrary(self.module);
        }
    }
}

unsafe fn resolve_symbol<T: Copy>(module: *mut c_void, name: &[u8]) -> Option<T> {
    let symbol = CString::new(name.strip_suffix(&[0])?).ok()?;
    // SAFETY: module is a valid LoadLibraryExW handle; symbol is a valid nul-terminated name.
    let address = unsafe { GetProcAddress(module, symbol.as_ptr().cast()) }?;
    // SAFETY: caller supplies the documented function type for each explicitly named Mscms export.
    Some(unsafe { std::mem::transmute_copy(&address) })
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

fn hresult(result: i32, reason: &str) -> DisplayResult<()> {
    if result < 0 {
        Err(DisplayError::after_mutation(DisplayError::new(
            "profile_association_failed",
            reason,
        )))
    } else {
        Ok(())
    }
}
