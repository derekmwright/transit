//! The ASIO ABI, written by hand: the `#[repr(C)]` structures, the `IASIO` vtable, the error codes
//! and the sample types. No code here is `unsafe`; the calls through the vtable are in [`crate::com`].
//!
//! Sources (the public ASIO SDK 2.3 headers, whose layout has been stable since ASIO 2.0):
//! - `common/asio.h`: every structure below and the `ASIOError`, `ASIOSampleType` and
//!   `AsioTimeInfoFlags` values. On Windows the header wraps its declarations in
//!   `#pragma pack(push,4)` (under `_MSC_VER`), so structures holding a `double` or a pointer are
//!   4-byte packed; that is why several types are `packed(4)` and why `ASIODriverInfo::sysRef`
//!   sits at offset 164, not 168.
//! - `common/asiosys.h`: on Windows `IEEE754_64FLOAT` is 1, so `ASIOSampleRate` is a plain `double`
//!   (passed by value in a floating-point register), and `NATIVE_INT64` is 0, so `ASIOSamples` and
//!   `ASIOTimeStamp` are `{ unsigned long hi; unsigned long lo; }` pairs. `long` is 32 bits on
//!   Windows (LLP64).
//! - `common/iasiodrv.h`: `interface IASIO : public IUnknown` and the order of its 21 methods.
//!
//! The sizes and offsets are checked at compile time below. They were cross-checked against the
//! layout tests that bindgen generates from the same headers in cpal's `asio-sys` (used only as a
//! reference; this crate does not depend on it).
//!
//! Calling convention: the methods are C++ virtual functions (`thiscall` on 32-bit x86). On 64-bit
//! Windows (x64 and ARM64) there is one C calling convention and `this` is the first argument, so
//! the vtable entries are `extern "C"` functions taking the object pointer first. The crate is
//! compiled for 64-bit Windows only.
#![allow(non_snake_case)]

use std::ffi::c_void;
use std::mem::{offset_of, size_of};

/// `long` on Windows.
pub type AsioLong = i32;
/// `ASIOBool`: `long`, `ASIOFalse` = 0, `ASIOTrue` = 1.
pub type AsioBool = i32;
/// `ASIOError`: `long`.
pub type AsioErrorCode = i32;
/// `ASIOSampleRate` on Windows (`IEEE754_64FLOAT`).
pub type AsioSampleRate = f64;
/// `ASIOSampleType`: `long`.
pub type AsioSampleTypeCode = i32;

pub const ASIO_FALSE: AsioBool = 0;

// `ASIOError` values (`asio.h`, `enum { ASE_OK = 0, ASE_SUCCESS = 0x3f4847a0, ASE_NotPresent = -1000, ... }`).
pub const ASE_OK: AsioErrorCode = 0;
/// The success value of `ASIOFuture` calls.
pub const ASE_SUCCESS: AsioErrorCode = 0x3f48_47a0;
pub const ASE_NOT_PRESENT: AsioErrorCode = -1000;
pub const ASE_HW_MALFUNCTION: AsioErrorCode = -999;
pub const ASE_INVALID_PARAMETER: AsioErrorCode = -998;
pub const ASE_INVALID_MODE: AsioErrorCode = -997;
pub const ASE_SP_NOT_ADVANCING: AsioErrorCode = -996;
pub const ASE_NO_CLOCK: AsioErrorCode = -995;
pub const ASE_NO_MEMORY: AsioErrorCode = -994;

// `ASIOSampleType` values (`asio.h`, `enum { ASIOSTInt16MSB = 0, ... }`).
pub const ASIOST_INT16_MSB: AsioSampleTypeCode = 0;
pub const ASIOST_INT24_MSB: AsioSampleTypeCode = 1;
pub const ASIOST_INT32_MSB: AsioSampleTypeCode = 2;
pub const ASIOST_FLOAT32_MSB: AsioSampleTypeCode = 3;
pub const ASIOST_FLOAT64_MSB: AsioSampleTypeCode = 4;
pub const ASIOST_INT32_MSB16: AsioSampleTypeCode = 8;
pub const ASIOST_INT32_MSB18: AsioSampleTypeCode = 9;
pub const ASIOST_INT32_MSB20: AsioSampleTypeCode = 10;
pub const ASIOST_INT32_MSB24: AsioSampleTypeCode = 11;
pub const ASIOST_INT16_LSB: AsioSampleTypeCode = 16;
/// Packed 24-bit, three bytes per sample (also used for 20 bits).
pub const ASIOST_INT24_LSB: AsioSampleTypeCode = 17;
pub const ASIOST_INT32_LSB: AsioSampleTypeCode = 18;
pub const ASIOST_FLOAT32_LSB: AsioSampleTypeCode = 19;
pub const ASIOST_FLOAT64_LSB: AsioSampleTypeCode = 20;
/// 32-bit container, 16 valid bits aligned to the least significant end (`ASIOSTInt32LSB16`).
pub const ASIOST_INT32_LSB16: AsioSampleTypeCode = 24;
pub const ASIOST_INT32_LSB18: AsioSampleTypeCode = 25;
pub const ASIOST_INT32_LSB20: AsioSampleTypeCode = 26;
pub const ASIOST_INT32_LSB24: AsioSampleTypeCode = 27;
pub const ASIOST_DSD_INT8_LSB1: AsioSampleTypeCode = 32;
pub const ASIOST_DSD_INT8_MSB1: AsioSampleTypeCode = 33;
pub const ASIOST_DSD_INT8_NER8: AsioSampleTypeCode = 40;

/// `ASIOSamples` (`asio.h`, `NATIVE_INT64` 0): a 64-bit sample count as two 32-bit halves.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AsioSamples {
    pub hi: u32,
    pub lo: u32,
}

impl AsioSamples {
    pub const fn value(self) -> u64 {
        ((self.hi as u64) << 32) | self.lo as u64
    }
}

/// `ASIOTimeStamp` (`asio.h`, `NATIVE_INT64` 0): system time in nanoseconds as two halves.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AsioTimeStamp {
    pub hi: u32,
    pub lo: u32,
}

/// `ASIODriverInfo` (`asio.h`): filled by the SDK's `ASIOInit`; a direct `IASIO` host passes only
/// `sysRef` (to `IASIO::init`) and reads the rest through the vtable.
#[repr(C, packed(4))]
#[derive(Clone, Copy)]
pub struct AsioDriverInfo {
    pub asioVersion: AsioLong,
    pub driverVersion: AsioLong,
    pub name: [u8; 32],
    pub errorMessage: [u8; 124],
    /// The host's window handle on Windows.
    pub sysRef: *mut c_void,
}

/// `ASIOClockSource` (`asio.h`).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct AsioClockSource {
    pub index: AsioLong,
    pub associatedChannel: AsioLong,
    pub associatedGroup: AsioLong,
    pub isCurrentSource: AsioBool,
    pub name: [u8; 32],
}

/// `ASIOChannelInfo` (`asio.h`): `channel` and `isInput` are set by the host, the rest by the driver.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct AsioChannelInfo {
    pub channel: AsioLong,
    pub isInput: AsioBool,
    pub isActive: AsioBool,
    pub channelGroup: AsioLong,
    pub type_: AsioSampleTypeCode,
    pub name: [u8; 32],
}

impl AsioChannelInfo {
    pub const fn query(channel: AsioLong, input: bool) -> Self {
        Self { channel, isInput: input as AsioBool, isActive: 0, channelGroup: 0, type_: -1, name: [0; 32] }
    }
}

/// `ASIOBufferInfo` (`asio.h`): one channel's double buffer, filled by `createBuffers`.
#[repr(C, packed(4))]
#[derive(Clone, Copy)]
pub struct AsioBufferInfo {
    pub isInput: AsioBool,
    pub channelNum: AsioLong,
    pub buffers: [*mut c_void; 2],
}

/// `ASIOTimeCode` (`asio.h`).
#[repr(C, packed(4))]
#[derive(Clone, Copy)]
pub struct AsioTimeCode {
    pub speed: f64,
    pub timeCodeSamples: AsioSamples,
    pub flags: u32,
    pub future: [u8; 64],
}

/// `AsioTimeInfo` (`asio.h`).
#[repr(C, packed(4))]
#[derive(Clone, Copy)]
pub struct AsioTimeInfo {
    pub speed: f64,
    pub systemTime: AsioTimeStamp,
    pub samplePosition: AsioSamples,
    pub sampleRate: AsioSampleRate,
    pub flags: u32,
    pub reserved: [u8; 12],
}

/// `ASIOTime` (`asio.h`): the argument of `bufferSwitchTimeInfo`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct AsioTime {
    pub reserved: [AsioLong; 4],
    pub timeInfo: AsioTimeInfo,
    pub timeCode: AsioTimeCode,
}

/// `ASIOCallbacks` (`asio.h`): the host's four callbacks, passed to `createBuffers` (slice b).
#[repr(C, packed(4))]
#[derive(Clone, Copy)]
pub struct AsioCallbacks {
    pub bufferSwitch: Option<unsafe extern "C" fn(doubleBufferIndex: AsioLong, directProcess: AsioBool)>,
    pub sampleRateDidChange: Option<unsafe extern "C" fn(sRate: AsioSampleRate)>,
    pub asioMessage: Option<unsafe extern "C" fn(selector: AsioLong, value: AsioLong, message: *mut c_void, opt: *mut f64) -> AsioLong>,
    pub bufferSwitchTimeInfo:
        Option<unsafe extern "C" fn(params: *mut AsioTime, doubleBufferIndex: AsioLong, directProcess: AsioBool) -> *mut AsioTime>,
}

/// An `IASIO` object: a pointer to its vtable, as for any COM object.
#[repr(C)]
pub struct IAsio {
    pub vtbl: *const IAsioVtbl,
}

/// The `IASIO` vtable (`iasiodrv.h`): `IUnknown`'s three methods, then the 21 ASIO methods in
/// declaration order. Every entry takes the object pointer first (see the module documentation).
#[repr(C)]
pub struct IAsioVtbl {
    pub QueryInterface: unsafe extern "C" fn(this: *mut IAsio, riid: *const [u8; 16], object: *mut *mut c_void) -> i32,
    pub AddRef: unsafe extern "C" fn(this: *mut IAsio) -> u32,
    pub Release: unsafe extern "C" fn(this: *mut IAsio) -> u32,
    /// `ASIOBool init(void *sysHandle)`: `ASIOTrue` on success.
    pub init: unsafe extern "C" fn(this: *mut IAsio, sys_handle: *mut c_void) -> AsioBool,
    /// `void getDriverName(char *name)`: at most 32 bytes with the terminator.
    pub getDriverName: unsafe extern "C" fn(this: *mut IAsio, name: *mut u8),
    pub getDriverVersion: unsafe extern "C" fn(this: *mut IAsio) -> AsioLong,
    /// `void getErrorMessage(char *string)`: at most 124 bytes with the terminator.
    pub getErrorMessage: unsafe extern "C" fn(this: *mut IAsio, string: *mut u8),
    pub start: unsafe extern "C" fn(this: *mut IAsio) -> AsioErrorCode,
    pub stop: unsafe extern "C" fn(this: *mut IAsio) -> AsioErrorCode,
    pub getChannels: unsafe extern "C" fn(this: *mut IAsio, inputs: *mut AsioLong, outputs: *mut AsioLong) -> AsioErrorCode,
    pub getLatencies: unsafe extern "C" fn(this: *mut IAsio, input: *mut AsioLong, output: *mut AsioLong) -> AsioErrorCode,
    pub getBufferSize: unsafe extern "C" fn(
        this: *mut IAsio,
        min: *mut AsioLong,
        max: *mut AsioLong,
        preferred: *mut AsioLong,
        granularity: *mut AsioLong,
    ) -> AsioErrorCode,
    pub canSampleRate: unsafe extern "C" fn(this: *mut IAsio, rate: AsioSampleRate) -> AsioErrorCode,
    pub getSampleRate: unsafe extern "C" fn(this: *mut IAsio, rate: *mut AsioSampleRate) -> AsioErrorCode,
    pub setSampleRate: unsafe extern "C" fn(this: *mut IAsio, rate: AsioSampleRate) -> AsioErrorCode,
    pub getClockSources: unsafe extern "C" fn(this: *mut IAsio, clocks: *mut AsioClockSource, count: *mut AsioLong) -> AsioErrorCode,
    pub setClockSource: unsafe extern "C" fn(this: *mut IAsio, reference: AsioLong) -> AsioErrorCode,
    pub getSamplePosition: unsafe extern "C" fn(this: *mut IAsio, position: *mut AsioSamples, stamp: *mut AsioTimeStamp) -> AsioErrorCode,
    pub getChannelInfo: unsafe extern "C" fn(this: *mut IAsio, info: *mut AsioChannelInfo) -> AsioErrorCode,
    pub createBuffers: unsafe extern "C" fn(
        this: *mut IAsio,
        infos: *mut AsioBufferInfo,
        channels: AsioLong,
        frames: AsioLong,
        callbacks: *mut AsioCallbacks,
    ) -> AsioErrorCode,
    pub disposeBuffers: unsafe extern "C" fn(this: *mut IAsio) -> AsioErrorCode,
    pub controlPanel: unsafe extern "C" fn(this: *mut IAsio) -> AsioErrorCode,
    pub future: unsafe extern "C" fn(this: *mut IAsio, selector: AsioLong, opt: *mut c_void) -> AsioErrorCode,
    pub outputReady: unsafe extern "C" fn(this: *mut IAsio) -> AsioErrorCode,
}

/// The size of the name buffer `getDriverName` writes (32 in the SDK); we lend more in case a
/// driver writes past it.
pub const DRIVER_NAME_BUFFER: usize = 128;
/// The size of the message buffer `getErrorMessage` writes (124 in the SDK), with the same slack.
pub const ERROR_MESSAGE_BUFFER: usize = 256;

// Layout assertions. Sizes and offsets on 64-bit Windows; each matches the `asio.h` declaration
// under `#pragma pack(push,4)` and the bindgen-generated layout tests of the same header.
const _: () = {
    const PTR: usize = size_of::<*mut c_void>();
    assert!(PTR == 8, "the crate is built for 64-bit Windows only");

    assert!(size_of::<AsioSamples>() == 8);
    assert!(offset_of!(AsioSamples, hi) == 0);
    assert!(offset_of!(AsioSamples, lo) == 4);
    assert!(size_of::<AsioTimeStamp>() == 8);
    assert!(offset_of!(AsioTimeStamp, hi) == 0);
    assert!(offset_of!(AsioTimeStamp, lo) == 4);

    assert!(size_of::<AsioDriverInfo>() == 172);
    assert!(offset_of!(AsioDriverInfo, asioVersion) == 0);
    assert!(offset_of!(AsioDriverInfo, driverVersion) == 4);
    assert!(offset_of!(AsioDriverInfo, name) == 8);
    assert!(offset_of!(AsioDriverInfo, errorMessage) == 40);
    assert!(offset_of!(AsioDriverInfo, sysRef) == 164);

    assert!(size_of::<AsioClockSource>() == 48);
    assert!(offset_of!(AsioClockSource, isCurrentSource) == 12);
    assert!(offset_of!(AsioClockSource, name) == 16);

    assert!(size_of::<AsioChannelInfo>() == 52);
    assert!(offset_of!(AsioChannelInfo, channel) == 0);
    assert!(offset_of!(AsioChannelInfo, isInput) == 4);
    assert!(offset_of!(AsioChannelInfo, isActive) == 8);
    assert!(offset_of!(AsioChannelInfo, channelGroup) == 12);
    assert!(offset_of!(AsioChannelInfo, type_) == 16);
    assert!(offset_of!(AsioChannelInfo, name) == 20);

    assert!(size_of::<AsioBufferInfo>() == 24);
    assert!(offset_of!(AsioBufferInfo, isInput) == 0);
    assert!(offset_of!(AsioBufferInfo, channelNum) == 4);
    assert!(offset_of!(AsioBufferInfo, buffers) == 8);

    assert!(size_of::<AsioTimeCode>() == 84);
    assert!(offset_of!(AsioTimeCode, speed) == 0);
    assert!(offset_of!(AsioTimeCode, timeCodeSamples) == 8);
    assert!(offset_of!(AsioTimeCode, flags) == 16);
    assert!(offset_of!(AsioTimeCode, future) == 20);

    assert!(size_of::<AsioTimeInfo>() == 48);
    assert!(offset_of!(AsioTimeInfo, speed) == 0);
    assert!(offset_of!(AsioTimeInfo, systemTime) == 8);
    assert!(offset_of!(AsioTimeInfo, samplePosition) == 16);
    assert!(offset_of!(AsioTimeInfo, sampleRate) == 24);
    assert!(offset_of!(AsioTimeInfo, flags) == 32);
    assert!(offset_of!(AsioTimeInfo, reserved) == 36);

    assert!(size_of::<AsioTime>() == 148);
    assert!(offset_of!(AsioTime, reserved) == 0);
    assert!(offset_of!(AsioTime, timeInfo) == 16);
    assert!(offset_of!(AsioTime, timeCode) == 64);

    assert!(size_of::<AsioCallbacks>() == 32);
    assert!(offset_of!(AsioCallbacks, bufferSwitch) == 0);
    assert!(offset_of!(AsioCallbacks, sampleRateDidChange) == 8);
    assert!(offset_of!(AsioCallbacks, asioMessage) == 16);
    assert!(offset_of!(AsioCallbacks, bufferSwitchTimeInfo) == 24);

    // The vtable: slot n at n pointers. IUnknown is 0..=2, `init` is 3, `outputReady` is 23.
    assert!(size_of::<IAsioVtbl>() == 24 * PTR);
    assert!(offset_of!(IAsioVtbl, QueryInterface) == 0);
    assert!(offset_of!(IAsioVtbl, Release) == 2 * PTR);
    assert!(offset_of!(IAsioVtbl, init) == 3 * PTR);
    assert!(offset_of!(IAsioVtbl, getDriverName) == 4 * PTR);
    assert!(offset_of!(IAsioVtbl, getDriverVersion) == 5 * PTR);
    assert!(offset_of!(IAsioVtbl, getErrorMessage) == 6 * PTR);
    assert!(offset_of!(IAsioVtbl, start) == 7 * PTR);
    assert!(offset_of!(IAsioVtbl, stop) == 8 * PTR);
    assert!(offset_of!(IAsioVtbl, getChannels) == 9 * PTR);
    assert!(offset_of!(IAsioVtbl, getLatencies) == 10 * PTR);
    assert!(offset_of!(IAsioVtbl, getBufferSize) == 11 * PTR);
    assert!(offset_of!(IAsioVtbl, canSampleRate) == 12 * PTR);
    assert!(offset_of!(IAsioVtbl, getSampleRate) == 13 * PTR);
    assert!(offset_of!(IAsioVtbl, setSampleRate) == 14 * PTR);
    assert!(offset_of!(IAsioVtbl, getClockSources) == 15 * PTR);
    assert!(offset_of!(IAsioVtbl, setClockSource) == 16 * PTR);
    assert!(offset_of!(IAsioVtbl, getSamplePosition) == 17 * PTR);
    assert!(offset_of!(IAsioVtbl, getChannelInfo) == 18 * PTR);
    assert!(offset_of!(IAsioVtbl, createBuffers) == 19 * PTR);
    assert!(offset_of!(IAsioVtbl, disposeBuffers) == 20 * PTR);
    assert!(offset_of!(IAsioVtbl, controlPanel) == 21 * PTR);
    assert!(offset_of!(IAsioVtbl, future) == 22 * PTR);
    assert!(offset_of!(IAsioVtbl, outputReady) == 23 * PTR);
};

/// The text of an `ASIOError` code (the comments of `asio.h`).
pub fn error_text(code: AsioErrorCode) -> &'static str {
    match code {
        ASE_OK | ASE_SUCCESS => "success",
        ASE_NOT_PRESENT => "hardware input or output is not present or available (ASE_NotPresent)",
        ASE_HW_MALFUNCTION => "hardware is malfunctioning (ASE_HWMalfunction)",
        ASE_INVALID_PARAMETER => "input parameter invalid (ASE_InvalidParameter)",
        ASE_INVALID_MODE => "hardware is in a bad mode or used in a bad mode (ASE_InvalidMode)",
        ASE_SP_NOT_ADVANCING => "hardware is not running when sample position is inquired (ASE_SPNotAdvancing)",
        ASE_NO_CLOCK => "sample clock or rate cannot be determined or is not present (ASE_NoClock)",
        ASE_NO_MEMORY => "not enough memory for completing the request (ASE_NoMemory)",
        _ => "unknown ASIO error code",
    }
}

/// The bytes of a NUL-terminated ANSI string a driver wrote, up to the terminator (or the end).
pub fn c_string(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).trim_end().to_owned()
}
