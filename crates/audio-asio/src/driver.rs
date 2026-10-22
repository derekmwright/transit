//! The safe side of a driver: registry entries, what a loaded driver reports ([`DriverInfo`]), the
//! sample-type mapping and the `ASIOError` to [`ab::OpenError`] mapping. The calls themselves go
//! through [`crate::com::Driver`]; nothing here is `unsafe`.

use crate::abi;
use crate::com::Driver;
use transit_audio_backend as ab;
use windows_core::GUID;

/// The sample rates enumeration asks `canSampleRate` about.
pub const PROBE_RATES: [u32; 4] = [44_100, 48_000, 88_200, 96_000];

/// One installed driver as the registry lists it (`HKLM\SOFTWARE\ASIO\<name>`), nothing loaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriverEntry {
    /// The registry key name: what hosts show and save (cpal's `asio-sys` uses the same names).
    pub name: String,
    pub clsid: GUID,
    /// The `CLSID` value as written in the registry.
    pub clsid_text: String,
    /// The `Description` value, where there is one.
    pub description: Option<String>,
    /// `HKCR\CLSID\{clsid}\InprocServer32`'s default value, expanded.
    pub dll: Option<String>,
    /// Whether that file exists.
    pub dll_exists: bool,
    /// The `ThreadingModel` of the COM registration ("Apartment" for every driver seen so far).
    pub threading_model: Option<String>,
}

/// The sample type of a driver's channels (`ASIOSampleType`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SampleType {
    Int16Lsb,
    /// Packed 24-bit, three bytes per sample.
    Int24Lsb,
    Int32Lsb,
    Float32Lsb,
    Float64Lsb,
    /// A 32-bit container with 16, 18, 20 or 24 valid bits aligned to the **least** significant end
    /// (unlike WASAPI's 24-in-32, which is left-justified).
    Int32LsbAligned(u8),
    /// Any other type (big-endian, DSD, unknown): reported, not opened.
    Unsupported(i32),
}

impl SampleType {
    pub fn from_code(code: i32) -> Self {
        match code {
            abi::ASIOST_INT16_LSB => Self::Int16Lsb,
            abi::ASIOST_INT24_LSB => Self::Int24Lsb,
            abi::ASIOST_INT32_LSB => Self::Int32Lsb,
            abi::ASIOST_FLOAT32_LSB => Self::Float32Lsb,
            abi::ASIOST_FLOAT64_LSB => Self::Float64Lsb,
            abi::ASIOST_INT32_LSB16 => Self::Int32LsbAligned(16),
            abi::ASIOST_INT32_LSB18 => Self::Int32LsbAligned(18),
            abi::ASIOST_INT32_LSB20 => Self::Int32LsbAligned(20),
            abi::ASIOST_INT32_LSB24 => Self::Int32LsbAligned(24),
            other => Self::Unsupported(other),
        }
    }

    /// The SDK's name of the type.
    pub fn name(self) -> String {
        match self {
            Self::Int16Lsb => "ASIOSTInt16LSB".into(),
            Self::Int24Lsb => "ASIOSTInt24LSB".into(),
            Self::Int32Lsb => "ASIOSTInt32LSB".into(),
            Self::Float32Lsb => "ASIOSTFloat32LSB".into(),
            Self::Float64Lsb => "ASIOSTFloat64LSB".into(),
            Self::Int32LsbAligned(bits) => format!("ASIOSTInt32LSB{bits}"),
            Self::Unsupported(code) => match code {
                abi::ASIOST_INT16_MSB => "ASIOSTInt16MSB".into(),
                abi::ASIOST_INT24_MSB => "ASIOSTInt24MSB".into(),
                abi::ASIOST_INT32_MSB => "ASIOSTInt32MSB".into(),
                abi::ASIOST_FLOAT32_MSB => "ASIOSTFloat32MSB".into(),
                abi::ASIOST_FLOAT64_MSB => "ASIOSTFloat64MSB".into(),
                abi::ASIOST_INT32_MSB16..=abi::ASIOST_INT32_MSB24 => format!("ASIOSTInt32MSB{}", 16 + 2 * (code - abi::ASIOST_INT32_MSB16)),
                abi::ASIOST_DSD_INT8_LSB1 => "ASIOSTDSDInt8LSB1".into(),
                abi::ASIOST_DSD_INT8_MSB1 => "ASIOSTDSDInt8MSB1".into(),
                abi::ASIOST_DSD_INT8_NER8 => "ASIOSTDSDInt8NER8".into(),
                other => format!("ASIOSampleType {other}"),
            },
        }
    }

    /// The trait's sample format and valid bits, or `None` for a type this backend does not open.
    /// The 32-bit aligned types keep their alignment in [`SampleType`] itself: the conversion of
    /// slice (b) must shift them, which `valid_bits` alone does not say.
    pub fn format(self) -> Option<(ab::SampleFormat, Option<u8>)> {
        match self {
            Self::Int16Lsb => Some((ab::SampleFormat::I16, None)),
            Self::Int24Lsb => Some((ab::SampleFormat::I24, None)),
            Self::Int32Lsb => Some((ab::SampleFormat::I32, None)),
            Self::Float32Lsb => Some((ab::SampleFormat::F32, None)),
            Self::Float64Lsb => Some((ab::SampleFormat::F64, None)),
            Self::Int32LsbAligned(bits) => Some((ab::SampleFormat::I32, Some(bits))),
            Self::Unsupported(_) => None,
        }
    }
}

/// The sample type of one direction of a driver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DirectionType {
    /// The driver has no channels in this direction.
    NoChannels,
    /// Every channel reports this type.
    Uniform(SampleType),
    /// Channels report different types (not opened).
    Mixed(Vec<SampleType>),
    /// `getChannelInfo` failed on a channel.
    QueryFailed { channel: i32, code: i32 },
}

impl DirectionType {
    /// The trait format of the direction when every channel has one supported type.
    pub fn format(&self) -> Option<(ab::SampleFormat, Option<u8>)> {
        match self {
            Self::Uniform(t) => t.format(),
            _ => None,
        }
    }

    /// A short description for logs and diagnostics.
    pub fn describe(&self) -> String {
        match self {
            Self::NoChannels => "no channels".into(),
            Self::Uniform(t) if t.format().is_some() => t.name(),
            Self::Uniform(t) => format!("{} (unsupported)", t.name()),
            Self::Mixed(types) => format!("mixed types {:?} (unsupported)", types.iter().map(|t| t.name()).collect::<Vec<_>>()),
            Self::QueryFailed { channel, code } => format!("getChannelInfo({channel}) failed: {}", abi::error_text(*code)),
        }
    }
}

/// `getBufferSize`: frames, as the driver reports them. `granularity` -1 means powers of two
/// between `min` and `max`, 0 one fixed size, a positive value steps from `min`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BufferSize {
    pub min: i32,
    pub max: i32,
    pub preferred: i32,
    pub granularity: i32,
}

/// What a loaded and initialized driver reports (no buffers created).
#[derive(Debug, Clone, PartialEq)]
pub struct DriverInfo {
    /// The registry name it was loaded by.
    pub name: String,
    /// `getDriverName`.
    pub driver_name: String,
    pub driver_version: i32,
    pub inputs: u16,
    pub outputs: u16,
    pub buffer: BufferSize,
    /// The [`PROBE_RATES`] `canSampleRate` accepts.
    pub rates: Vec<u32>,
    /// `getSampleRate`, where it answers.
    pub current_rate: Option<f64>,
    pub input_type: DirectionType,
    pub output_type: DirectionType,
    /// `getLatencies` (input, output) in frames, where it answers.
    pub latencies: Option<(u32, u32)>,
}

impl DriverInfo {
    /// The sample type of one direction.
    pub fn sample_type(&self, direction: ab::Direction) -> &DirectionType {
        match direction {
            ab::Direction::Input => &self.input_type,
            ab::Direction::Output => &self.output_type,
        }
    }

    /// The channel count of one direction.
    pub fn channels(&self, direction: ab::Direction) -> u16 {
        match direction {
            ab::Direction::Input => self.inputs,
            ab::Direction::Output => self.outputs,
        }
    }

    /// The format ranges of one direction: one per accepted rate (or the current rate when none
    /// of the probed ones is accepted), empty when the direction has no channels or no supported
    /// sample type.
    pub fn format_ranges(&self, direction: ab::Direction) -> Vec<ab::FormatRange> {
        let channels = self.channels(direction);
        let Some((sample, _)) = self.sample_type(direction).format() else { return Vec::new() };
        if channels == 0 {
            return Vec::new();
        }
        let mut rates = self.rates.clone();
        if rates.is_empty()
            && let Some(rate) = self.current_rate.filter(|r| r.is_finite() && *r >= 1.0)
        {
            rates.push(rate.round() as u32);
        }
        let period = (self.buffer.min > 0 && self.buffer.max >= self.buffer.min).then_some((self.buffer.min as u32, self.buffer.max as u32));
        rates.into_iter().map(|rate| ab::FormatRange { sample, channels, min_rate: rate, max_rate: rate, period }).collect()
    }
}

/// A driver that could not be listed, with why (the per-driver diagnostic of enumeration).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriverDiagnostic {
    /// The registry name.
    pub driver: String,
    pub problem: String,
}

impl std::fmt::Display for DriverDiagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.driver, self.problem)
    }
}

/// Maps an `ASIOError` returned by `what` on `driver` to the trait's error.
pub fn asio_error(driver: &str, what: &str, code: i32) -> ab::OpenError {
    let text = format!("ASIO driver \"{driver}\": {what} failed: {} ({code})", abi::error_text(code));
    match code {
        abi::ASE_NOT_PRESENT => ab::OpenError::DeviceNotAvailable(text),
        _ => ab::OpenError::Other(text),
    }
}

/// Parses a registry CLSID (`{XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX}`, braces optional).
pub fn parse_guid(text: &str) -> Option<GUID> {
    let text = text.trim();
    let inner = text.strip_prefix('{').and_then(|t| t.strip_suffix('}')).unwrap_or(text);
    let parts: Vec<&str> = inner.split('-').collect();
    let lengths = [8, 4, 4, 4, 12];
    if parts.len() != 5 || parts.iter().zip(lengths).any(|(p, n)| p.len() != n || !p.bytes().all(|b| b.is_ascii_hexdigit())) {
        return None;
    }
    let hex = |s: &str| u64::from_str_radix(s, 16).ok();
    let d1 = u32::try_from(hex(parts[0])?).ok()?;
    let d2 = u16::try_from(hex(parts[1])?).ok()?;
    let d3 = u16::try_from(hex(parts[2])?).ok()?;
    let tail = (hex(parts[3])? << 48) | hex(parts[4])?;
    Some(GUID::from_values(d1, d2, d3, tail.to_be_bytes()))
}

/// Queries a loaded, initialized driver: channels and buffer sizes are required, the rest is
/// reported where the driver answers. Runs on the host thread.
pub(crate) fn query(name: &str, driver: &mut Driver) -> Result<DriverInfo, ab::OpenError> {
    let (inputs, outputs) = driver.channels().map_err(|code| asio_error(name, "getChannels", code))?;
    let buffer = driver.buffer_size().map_err(|code| asio_error(name, "getBufferSize", code))?;
    let rates = PROBE_RATES.into_iter().filter(|rate| driver.can_sample_rate(f64::from(*rate)).is_ok()).collect();
    let current_rate = driver.sample_rate().ok();
    let latencies = driver.latencies().ok().map(|(i, o)| (i.max(0) as u32, o.max(0) as u32));
    let input_type = direction_type(driver, inputs, true);
    let output_type = direction_type(driver, outputs, false);
    let clamp = |n: i32| u16::try_from(n.max(0)).unwrap_or(u16::MAX);
    Ok(DriverInfo {
        name: name.to_owned(),
        driver_name: driver.driver_name(),
        driver_version: driver.driver_version(),
        inputs: clamp(inputs),
        outputs: clamp(outputs),
        buffer,
        rates,
        current_rate,
        input_type,
        output_type,
        latencies,
    })
}

fn direction_type(driver: &mut Driver, channels: i32, input: bool) -> DirectionType {
    let mut types: Vec<SampleType> = Vec::new();
    for channel in 0..channels.max(0) {
        match driver.channel_info(channel, input) {
            Ok(info) => {
                let t = SampleType::from_code(info.type_);
                if !types.contains(&t) {
                    types.push(t);
                }
            }
            Err(code) => return DirectionType::QueryFailed { channel, code },
        }
    }
    match types.len() {
        0 => DirectionType::NoChannels,
        1 => DirectionType::Uniform(types[0]),
        _ => DirectionType::Mixed(types),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_types_map_to_trait_formats_and_others_are_unsupported() {
        let cases = [
            (abi::ASIOST_INT16_LSB, Some((ab::SampleFormat::I16, None)), "ASIOSTInt16LSB"),
            (abi::ASIOST_INT24_LSB, Some((ab::SampleFormat::I24, None)), "ASIOSTInt24LSB"),
            (abi::ASIOST_INT32_LSB, Some((ab::SampleFormat::I32, None)), "ASIOSTInt32LSB"),
            (abi::ASIOST_FLOAT32_LSB, Some((ab::SampleFormat::F32, None)), "ASIOSTFloat32LSB"),
            (abi::ASIOST_FLOAT64_LSB, Some((ab::SampleFormat::F64, None)), "ASIOSTFloat64LSB"),
            (abi::ASIOST_INT32_LSB16, Some((ab::SampleFormat::I32, Some(16))), "ASIOSTInt32LSB16"),
            (abi::ASIOST_INT32_LSB18, Some((ab::SampleFormat::I32, Some(18))), "ASIOSTInt32LSB18"),
            (abi::ASIOST_INT32_LSB20, Some((ab::SampleFormat::I32, Some(20))), "ASIOSTInt32LSB20"),
            (abi::ASIOST_INT32_LSB24, Some((ab::SampleFormat::I32, Some(24))), "ASIOSTInt32LSB24"),
            (abi::ASIOST_INT24_MSB, None, "ASIOSTInt24MSB"),
            (abi::ASIOST_INT32_MSB20, None, "ASIOSTInt32MSB20"),
            (abi::ASIOST_FLOAT32_MSB, None, "ASIOSTFloat32MSB"),
            (abi::ASIOST_DSD_INT8_NER8, None, "ASIOSTDSDInt8NER8"),
            (99, None, "ASIOSampleType 99"),
        ];
        for (code, format, name) in cases {
            let t = SampleType::from_code(code);
            assert_eq!(t.format(), format, "{name}");
            assert_eq!(t.name(), name);
        }
    }

    #[test]
    fn asio_errors_map_to_open_errors() {
        assert!(
            matches!(asio_error("D", "init", abi::ASE_NOT_PRESENT), ab::OpenError::DeviceNotAvailable(t) if t.contains("ASE_NotPresent") && t.contains("\"D\""))
        );
        for code in [abi::ASE_HW_MALFUNCTION, abi::ASE_INVALID_PARAMETER, abi::ASE_INVALID_MODE, abi::ASE_NO_CLOCK, abi::ASE_NO_MEMORY, -1] {
            let e = asio_error("D", "getChannels", code);
            assert!(matches!(&e, ab::OpenError::Other(t) if t.contains("getChannels") && t.contains(&code.to_string())), "{code}: {e:?}");
        }
        assert!(asio_error("D", "x", abi::ASE_HW_MALFUNCTION).to_string().contains("ASE_HWMalfunction"));
    }

    #[test]
    fn guids_parse_from_registry_text() {
        let g = parse_guid("{AC4D0455-50D7-4498-B3CD-9A41D130B759}").expect("parses");
        assert_eq!(g, GUID::from_u128(0xac4d0455_50d7_4498_b3cd_9a41d130b759));
        assert_eq!(parse_guid("ac4d0455-50d7-4498-b3cd-9a41d130b759"), Some(g));
        for bad in ["", "{}", "{AC4D0455-50D7-4498-B3CD}", "{AC4D0455-50D7-4498-B3CD-9A41D130B75G}", "{AC4D04550-50D7-4498-B3CD-9A41D130B75}"] {
            assert_eq!(parse_guid(bad), None, "{bad}");
        }
    }

    fn info(input_type: DirectionType, output_type: DirectionType) -> DriverInfo {
        DriverInfo {
            name: "D".into(),
            driver_name: "D".into(),
            driver_version: 1,
            inputs: 12,
            outputs: 10,
            buffer: BufferSize { min: 32, max: 2048, preferred: 256, granularity: -1 },
            rates: vec![44_100, 48_000],
            current_rate: Some(48_000.0),
            input_type,
            output_type,
            latencies: Some((300, 400)),
        }
    }

    #[test]
    fn format_ranges_list_each_accepted_rate_with_the_buffer_range() {
        let i = info(DirectionType::Uniform(SampleType::Int24Lsb), DirectionType::Uniform(SampleType::Unsupported(abi::ASIOST_INT24_MSB)));
        let ranges = i.format_ranges(ab::Direction::Input);
        assert_eq!(
            ranges,
            vec![
                ab::FormatRange { sample: ab::SampleFormat::I24, channels: 12, min_rate: 44_100, max_rate: 44_100, period: Some((32, 2048)) },
                ab::FormatRange { sample: ab::SampleFormat::I24, channels: 12, min_rate: 48_000, max_rate: 48_000, period: Some((32, 2048)) },
            ]
        );
        assert!(i.format_ranges(ab::Direction::Output).is_empty(), "an unsupported type lists no format");
        let mut none = info(DirectionType::Uniform(SampleType::Float32Lsb), DirectionType::NoChannels);
        none.rates.clear();
        none.outputs = 0;
        assert_eq!(none.format_ranges(ab::Direction::Input)[0].min_rate, 48_000, "the current rate when no probed rate is accepted");
        assert!(none.format_ranges(ab::Direction::Output).is_empty());
        let mixed = DirectionType::Mixed(vec![SampleType::Int24Lsb, SampleType::Int32Lsb]);
        assert!(mixed.format().is_none() && mixed.describe().contains("unsupported"));
    }
}
