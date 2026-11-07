//! Minimal CoreFoundation / IOKit / IOPMLib FFI shared by `battery`, `lid` and `power`.
//!
//! Only the handful of calls the hardware modules need are declared here, so the crate needs no
//! new dependencies. Every owned CF object is wrapped in [`CfOwned`] (released on drop) and every
//! IOKit object in [`IoObject`] (`IOObjectRelease` on drop). Converting CF values goes through
//! [`to_json`], which yields plain `serde_json` values so the parsing logic stays pure and testable.

#![allow(non_upper_case_globals)]

use std::ffi::{c_char, c_void, CString};
use std::ptr;

use serde_json::{Map, Number, Value};

pub type CFTypeRef = *const c_void;
pub type CFStringRef = *const c_void;
pub type CFDictionaryRef = *const c_void;
pub type CFMutableDictionaryRef = *mut c_void;
pub type CFAllocatorRef = *const c_void;
pub type CFIndex = isize;
pub type CFTypeID = usize;
pub type Boolean = u8;
pub type MachPort = u32;
pub type IoObjectT = u32;
pub type KernReturn = i32;
pub type IoReturn = i32;
pub type IOPMAssertionID = u32;

const kCFStringEncodingUTF8: u32 = 0x0800_0100;
const kCFNumberSInt64Type: CFIndex = 4;
const kCFNumberFloat64Type: CFIndex = 6;
/// `kIOMainPortDefault` / `kIOMasterPortDefault` are both `MACH_PORT_NULL`.
const kIOMainPortDefault: MachPort = 0;
pub const kIOPMAssertionLevelOn: u32 = 255;
pub const kIOPMUserActiveLocal: u32 = 0;
pub const kIOReturnSuccess: IoReturn = 0;

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRelease(cf: CFTypeRef);
    fn CFGetTypeID(cf: CFTypeRef) -> CFTypeID;
    fn CFStringGetTypeID() -> CFTypeID;
    fn CFNumberGetTypeID() -> CFTypeID;
    fn CFBooleanGetTypeID() -> CFTypeID;
    fn CFDictionaryGetTypeID() -> CFTypeID;
    fn CFArrayGetTypeID() -> CFTypeID;
    fn CFDataGetTypeID() -> CFTypeID;
    fn CFStringCreateWithCString(
        alloc: CFAllocatorRef,
        c_str: *const c_char,
        encoding: u32,
    ) -> CFStringRef;
    fn CFStringGetLength(s: CFStringRef) -> CFIndex;
    fn CFStringGetMaximumSizeForEncoding(length: CFIndex, encoding: u32) -> CFIndex;
    fn CFStringGetCString(
        s: CFStringRef,
        buffer: *mut c_char,
        size: CFIndex,
        encoding: u32,
    ) -> Boolean;
    fn CFNumberIsFloatType(n: CFTypeRef) -> Boolean;
    fn CFNumberGetValue(n: CFTypeRef, the_type: CFIndex, value: *mut c_void) -> Boolean;
    fn CFBooleanGetValue(b: CFTypeRef) -> Boolean;
    fn CFDictionaryGetValue(dict: CFDictionaryRef, key: *const c_void) -> *const c_void;
    fn CFDictionaryGetCount(dict: CFDictionaryRef) -> CFIndex;
    fn CFDictionaryGetKeysAndValues(
        dict: CFDictionaryRef,
        keys: *mut *const c_void,
        values: *mut *const c_void,
    );
    fn CFArrayGetCount(array: CFTypeRef) -> CFIndex;
    fn CFArrayGetValueAtIndex(array: CFTypeRef, idx: CFIndex) -> *const c_void;
    fn CFDataGetLength(data: CFTypeRef) -> CFIndex;
    fn CFDataGetBytePtr(data: CFTypeRef) -> *const u8;
}

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IOServiceMatching(name: *const c_char) -> CFMutableDictionaryRef;
    fn IOServiceGetMatchingService(main_port: MachPort, matching: CFDictionaryRef) -> IoObjectT;
    fn IORegistryEntryFromPath(main_port: MachPort, path: *const c_char) -> IoObjectT;
    fn IORegistryEntryCreateCFProperty(
        entry: IoObjectT,
        key: CFStringRef,
        allocator: CFAllocatorRef,
        options: u32,
    ) -> CFTypeRef;
    fn IORegistryEntryCreateCFProperties(
        entry: IoObjectT,
        properties: *mut CFMutableDictionaryRef,
        allocator: CFAllocatorRef,
        options: u32,
    ) -> KernReturn;
    fn IOObjectRelease(object: IoObjectT) -> KernReturn;
    fn IOPMAssertionCreateWithName(
        assertion_type: CFStringRef,
        level: u32,
        name: CFStringRef,
        assertion_id: *mut IOPMAssertionID,
    ) -> IoReturn;
    fn IOPMAssertionRelease(assertion_id: IOPMAssertionID) -> IoReturn;
    fn IOPMAssertionDeclareUserActivity(
        name: CFStringRef,
        user_type: u32,
        assertion_id: *mut IOPMAssertionID,
    ) -> IoReturn;
}

/// An owned (+1 retained) CoreFoundation object, released on drop.
pub struct CfOwned(CFTypeRef);

impl CfOwned {
    /// Takes ownership of a reference returned by a CF "Create"/"Copy" function.
    fn from_create(raw: CFTypeRef) -> Option<Self> {
        (!raw.is_null()).then_some(Self(raw))
    }

    pub fn as_ptr(&self) -> CFTypeRef {
        self.0
    }

    /// Creates a `CFString` from UTF-8 text. `None` if the text contains a NUL byte.
    pub fn string(text: &str) -> Option<Self> {
        let c = CString::new(text).ok()?;
        // SAFETY: `c` is a valid NUL-terminated buffer that outlives the call; a null allocator
        // selects the default allocator. The result is a +1 reference owned by `CfOwned`.
        Self::from_create(unsafe {
            CFStringCreateWithCString(ptr::null(), c.as_ptr(), kCFStringEncodingUTF8)
        })
    }
}

impl Drop for CfOwned {
    fn drop(&mut self) {
        // SAFETY: `self.0` is a non-null +1 reference that this wrapper exclusively owns.
        unsafe { CFRelease(self.0) }
    }
}

/// An IOKit registry entry / service handle, released on drop.
pub struct IoObject(IoObjectT);

impl IoObject {
    /// First registered service whose class matches `class_name`.
    pub fn matching_service(class_name: &str) -> Option<Self> {
        let c = CString::new(class_name).ok()?;
        // SAFETY: `c` is NUL-terminated and outlives the call. `IOServiceMatching` returns a +1
        // dictionary that `IOServiceGetMatchingService` consumes (releases), even on failure,
        // so it must not be released here. A null dictionary is checked before passing it on.
        let service = unsafe {
            let matching = IOServiceMatching(c.as_ptr());
            if matching.is_null() {
                return None;
            }
            IOServiceGetMatchingService(kIOMainPortDefault, matching as CFDictionaryRef)
        };
        (service != 0).then_some(Self(service))
    }

    /// Registry entry at a plane path such as `IODeviceTree:/product`.
    pub fn from_path(path: &str) -> Option<Self> {
        let c = CString::new(path).ok()?;
        // SAFETY: `c` is NUL-terminated and outlives the call; the returned handle (or 0) is
        // owned by `IoObject` and released on drop.
        let entry = unsafe { IORegistryEntryFromPath(kIOMainPortDefault, c.as_ptr()) };
        (entry != 0).then_some(Self(entry))
    }

    /// Copies one property (`IORegistryEntryCreateCFProperty`).
    pub fn property(&self, key: &str) -> Option<CfOwned> {
        let key = CfOwned::string(key)?;
        // SAFETY: `self.0` is a live registry handle and `key` a valid CFString for the duration
        // of the call. The result is a +1 reference (or null) owned by `CfOwned`.
        CfOwned::from_create(unsafe {
            IORegistryEntryCreateCFProperty(self.0, key.as_ptr(), ptr::null(), 0)
        })
    }

    /// Copies the full property dictionary (`IORegistryEntryCreateCFProperties`).
    pub fn properties(&self) -> Option<CfOwned> {
        let mut dict: CFMutableDictionaryRef = ptr::null_mut();
        // SAFETY: `self.0` is a live registry handle and `dict` is a valid out-pointer. On
        // success the dictionary is a +1 reference owned by `CfOwned`.
        let kr = unsafe { IORegistryEntryCreateCFProperties(self.0, &mut dict, ptr::null(), 0) };
        if kr != 0 {
            return None;
        }
        CfOwned::from_create(dict as CFTypeRef)
    }
}

impl Drop for IoObject {
    fn drop(&mut self) {
        // SAFETY: `self.0` is a non-zero handle this wrapper exclusively owns.
        unsafe {
            IOObjectRelease(self.0);
        }
    }
}

/// Looks up `key` in a CF dictionary and converts the value. Borrowed values are not released.
pub fn dict_get(dict: &CfOwned, key: &str) -> Option<Value> {
    let key = CfOwned::string(key)?;
    // SAFETY: `dict` is a live CFDictionary (callers pass the result of `properties()`), and
    // `key` is a valid CFString. `CFDictionaryGetValue` returns a borrowed reference (or null)
    // that stays valid while `dict` is alive, which covers the `to_json` call below.
    let value = unsafe {
        if CFGetTypeID(dict.as_ptr()) != CFDictionaryGetTypeID() {
            return None;
        }
        CFDictionaryGetValue(dict.as_ptr(), key.as_ptr())
    };
    if value.is_null() {
        return None;
    }
    Some(to_json(value))
}

/// Converts a borrowed CF property-list value into JSON. `CFData` becomes `Null`; it is read
/// separately with [`data_bytes`] where needed.
pub fn to_json(value: CFTypeRef) -> Value {
    if value.is_null() {
        return Value::Null;
    }
    // SAFETY: `value` is a non-null, live CF object borrowed from a container the caller holds.
    // Each branch only calls accessors valid for the type ID it checked first, with correctly
    // sized out-buffers. Nested values are borrowed from `value` and outlive the recursion.
    unsafe {
        let type_id = CFGetTypeID(value);
        if type_id == CFBooleanGetTypeID() {
            Value::Bool(CFBooleanGetValue(value) != 0)
        } else if type_id == CFNumberGetTypeID() {
            if CFNumberIsFloatType(value) != 0 {
                let mut out = 0f64;
                CFNumberGetValue(
                    value,
                    kCFNumberFloat64Type,
                    &mut out as *mut f64 as *mut c_void,
                );
                Number::from_f64(out).map_or(Value::Null, Value::Number)
            } else {
                let mut out = 0i64;
                CFNumberGetValue(
                    value,
                    kCFNumberSInt64Type,
                    &mut out as *mut i64 as *mut c_void,
                );
                Value::Number(out.into())
            }
        } else if type_id == CFStringGetTypeID() {
            cf_string(value).map_or(Value::Null, Value::String)
        } else if type_id == CFArrayGetTypeID() {
            let count = CFArrayGetCount(value);
            (0..count)
                .map(|i| to_json(CFArrayGetValueAtIndex(value, i)))
                .collect()
        } else if type_id == CFDictionaryGetTypeID() {
            let count = CFDictionaryGetCount(value).max(0) as usize;
            let mut keys = vec![ptr::null::<c_void>(); count];
            let mut values = vec![ptr::null::<c_void>(); count];
            CFDictionaryGetKeysAndValues(value, keys.as_mut_ptr(), values.as_mut_ptr());
            let mut map = Map::new();
            for (k, v) in keys.into_iter().zip(values) {
                if CFGetTypeID(k) == CFStringGetTypeID() {
                    if let Some(key) = cf_string(k) {
                        map.insert(key, to_json(v));
                    }
                }
            }
            Value::Object(map)
        } else {
            Value::Null
        }
    }
}

/// Bytes of a `CFData` value; `None` for any other type.
pub fn data_bytes(value: &CfOwned) -> Option<Vec<u8>> {
    // SAFETY: `value` is a live CF object; the byte pointer is only read after confirming the
    // type is CFData, and only for `CFDataGetLength` bytes while `value` is alive.
    unsafe {
        if CFGetTypeID(value.as_ptr()) != CFDataGetTypeID() {
            return None;
        }
        let len = CFDataGetLength(value.as_ptr()).max(0) as usize;
        let ptr = CFDataGetBytePtr(value.as_ptr());
        if ptr.is_null() || len == 0 {
            return Some(Vec::new());
        }
        Some(std::slice::from_raw_parts(ptr, len).to_vec())
    }
}

/// Copies a borrowed `CFString` into a Rust `String`.
///
/// # Safety
/// `s` must be a live `CFString`.
unsafe fn cf_string(s: CFTypeRef) -> Option<String> {
    let len = CFStringGetLength(s);
    let cap = CFStringGetMaximumSizeForEncoding(len, kCFStringEncodingUTF8) + 1;
    if cap <= 0 {
        return None;
    }
    let mut buf = vec![0u8; cap as usize];
    if CFStringGetCString(
        s,
        buf.as_mut_ptr() as *mut c_char,
        cap,
        kCFStringEncodingUTF8,
    ) == 0
    {
        return None;
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    buf.truncate(end);
    String::from_utf8(buf).ok()
}

/// `IOPMAssertionCreateWithName(type, kIOPMAssertionLevelOn, name)`.
pub fn assertion_create(assertion_type: &str, name: &str) -> Result<IOPMAssertionID, IoReturn> {
    let kind = CfOwned::string(assertion_type).ok_or(-1)?;
    let name = CfOwned::string(name).ok_or(-1)?;
    let mut id: IOPMAssertionID = 0;
    // SAFETY: both CFStrings are live for the call and `id` is a valid out-pointer.
    let rc = unsafe {
        IOPMAssertionCreateWithName(kind.as_ptr(), kIOPMAssertionLevelOn, name.as_ptr(), &mut id)
    };
    if rc == kIOReturnSuccess {
        Ok(id)
    } else {
        Err(rc)
    }
}

/// `IOPMAssertionDeclareUserActivity(name, kIOPMUserActiveLocal)`, which wakes the display.
pub fn declare_user_activity(name: &str) -> Result<IOPMAssertionID, IoReturn> {
    let name = CfOwned::string(name).ok_or(-1)?;
    let mut id: IOPMAssertionID = 0;
    // SAFETY: `name` is live for the call and `id` is a valid out-pointer.
    let rc =
        unsafe { IOPMAssertionDeclareUserActivity(name.as_ptr(), kIOPMUserActiveLocal, &mut id) };
    if rc == kIOReturnSuccess {
        Ok(id)
    } else {
        Err(rc)
    }
}

/// `IOPMAssertionRelease(id)`.
pub fn assertion_release(id: IOPMAssertionID) -> IoReturn {
    // SAFETY: releasing an assertion ID is safe even if it is stale; the kernel validates it.
    unsafe { IOPMAssertionRelease(id) }
}
