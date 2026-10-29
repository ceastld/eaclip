//! Device form factor and local presentation preferences for LAN peers.
//! Hardware detection never uses a device alias or battery presence. Call the
//! detector on a worker; DTO reads use only the previously detected value.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{json, Value};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DeviceKind {
    #[default]
    Unknown,
    Desktop,
    Laptop,
    Phone,
}

impl DeviceKind {
    pub fn from_metadata(value: Option<&Value>) -> Self {
        match value.and_then(Value::as_str) {
            Some("desktop") => Self::Desktop,
            Some("laptop") => Self::Laptop,
            Some("phone") => Self::Phone,
            _ => Self::Unknown,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Desktop => "desktop",
            Self::Laptop => "laptop",
            Self::Phone => "phone",
        }
    }

    /// Unknown/old peers keep the generic computer icon while the original
    /// detection value remains available separately in the DTO.
    fn display_fallback(self) -> Self {
        if self == Self::Unknown {
            Self::Desktop
        } else {
            self
        }
    }
}

impl<'de> Deserialize<'de> for DeviceKind {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // New peers may add more kinds. Unknown and malformed optional metadata
        // must not make an otherwise compatible discovery/peer document fail.
        let value = Value::deserialize(deserializer)?;
        Ok(Self::from_metadata(Some(&value)))
    }
}

fn parse_override(value: &Value) -> Result<Option<DeviceKind>, String> {
    match value.as_str() {
        Some("auto") => Ok(None),
        Some("desktop") => Ok(Some(DeviceKind::Desktop)),
        Some("laptop") => Ok(Some(DeviceKind::Laptop)),
        Some("phone") => Ok(Some(DeviceKind::Phone)),
        _ => Err("设备图标必须是 auto、desktop、laptop 或 phone。".into()),
    }
}

/// Overrides are local display preferences, separate from the peer's reported
/// kind, certificate and pairing. Detection is intentionally never persisted.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DevicePresentation {
    detected: DeviceKind,
    local_override: Option<DeviceKind>,
    peer_overrides: BTreeMap<String, DeviceKind>,
}

impl DevicePresentation {
    pub fn detected(&self) -> DeviceKind {
        self.detected
    }

    pub fn set_detected(&mut self, detected: DeviceKind) -> bool {
        let changed = self.detected != detected;
        self.detected = detected;
        changed
    }

    pub fn display_kind(&self) -> DeviceKind {
        self.local_override
            .unwrap_or(self.detected)
            .display_fallback()
    }

    pub fn override_name(&self) -> &'static str {
        self.local_override
            .map(DeviceKind::as_str)
            .unwrap_or("auto")
    }

    pub fn peer_display_kind(&self, id: &str, detected: DeviceKind) -> DeviceKind {
        self.peer_overrides
            .get(id)
            .copied()
            .unwrap_or(detected)
            .display_fallback()
    }

    pub fn peer_override_name(&self, id: &str) -> &'static str {
        self.peer_overrides
            .get(id)
            .copied()
            .map(DeviceKind::as_str)
            .unwrap_or("auto")
    }

    pub(crate) fn apply_patch(
        &mut self,
        params: &Value,
        paired_ids: &BTreeSet<String>,
    ) -> Result<(), String> {
        let mut next = self.clone();
        if let Some(value) = params.get("deviceKindOverride") {
            next.local_override = parse_override(value)?;
        }
        if let Some(value) = params.get("peerDeviceKindOverrides") {
            let patch = value
                .as_object()
                .ok_or("peerDeviceKindOverrides 必须是对象。")?;
            for (id, value) in patch {
                if !paired_ids.contains(id) {
                    return Err("只能修改已配对设备的显示图标。".into());
                }
                match parse_override(value)? {
                    Some(kind) => {
                        next.peer_overrides.insert(id.clone(), kind);
                    }
                    None => {
                        next.peer_overrides.remove(id);
                    }
                }
            }
        }
        *self = next;
        Ok(())
    }

    pub(crate) fn load_preferences(&mut self, settings: &Value) {
        self.local_override = settings
            .get("deviceKindOverride")
            .and_then(|value| parse_override(value).ok())
            .flatten();
        self.peer_overrides = settings
            .get("peerDeviceKindOverrides")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
            .filter_map(|(id, value)| {
                parse_override(value)
                    .ok()
                    .flatten()
                    .map(|kind| (id.clone(), kind))
            })
            .collect();
    }

    pub(crate) fn preferences(&self) -> Value {
        json!({"deviceKindOverride":self.override_name(), "peerDeviceKindOverrides":self.peer_overrides})
    }
}

/// Cached once per process; the host invokes this from its background worker
/// and publishes the result to its own runtime, without blocking UI/status reads.
pub fn detect_local_device_kind() -> DeviceKind {
    static DETECTED: std::sync::OnceLock<DeviceKind> = std::sync::OnceLock::new();
    *DETECTED.get_or_init(detect_platform_kind)
}

#[cfg(target_os = "android")]
fn detect_platform_kind() -> DeviceKind {
    DeviceKind::Phone
}

#[cfg(all(not(windows), not(target_os = "android")))]
fn detect_platform_kind() -> DeviceKind {
    DeviceKind::Unknown
}

#[cfg(windows)]
fn detect_platform_kind() -> DeviceKind {
    // https://learn.microsoft.com/windows/win32/api/sysinfoapi/nf-sysinfoapi-getsystemfirmwaretable
    // Classify from chassis/profile bytes only; never decode/store firmware
    // serials or manufacturer strings. No WMI, COM provider or network probe.
    let smbios = read_firmware(u32::from_be_bytes(*b"RSMB"), 0)
        .map(|bytes| device_kind_from_raw_smbios(&bytes))
        .unwrap_or_default();
    if smbios != DeviceKind::Unknown {
        return smbios;
    }
    // Read the OEM's FADT profile directly. PowerDeterminePlatformRoleEx would
    // silently fall back to battery presence when firmware has no profile.
    read_firmware(u32::from_be_bytes(*b"ACPI"), u32::from_le_bytes(*b"FACP"))
        .map(|bytes| device_kind_from_fadt(&bytes))
        .unwrap_or_default()
}

#[cfg(windows)]
fn read_firmware(provider: u32, table: u32) -> Option<Vec<u8>> {
    #[link(name = "kernel32")]
    extern "system" {
        fn GetSystemFirmwareTable(
            provider: u32,
            table: u32,
            buffer: *mut std::ffi::c_void,
            size: u32,
        ) -> u32;
    }
    const MAX_FIRMWARE_BYTES: u32 = 4 * 1024 * 1024;
    let required = unsafe { GetSystemFirmwareTable(provider, table, std::ptr::null_mut(), 0) };
    if !(8..=MAX_FIRMWARE_BYTES).contains(&required) {
        return None;
    }
    let mut bytes = vec![0u8; required as usize];
    let written =
        unsafe { GetSystemFirmwareTable(provider, table, bytes.as_mut_ptr().cast(), required) };
    if written < 8 || written > required {
        return None;
    }
    bytes.truncate(written as usize);
    Some(bytes)
}

fn device_kind_from_fadt(raw: &[u8]) -> DeviceKind {
    // ACPI FADT Preferred_PM_Profile is byte 45. Require the expected signature,
    // declared length and checksum before using an OEM role.
    // https://uefi.org/htmlspecs/ACPI_Spec_6_4_html/05_ACPI_Software_Programming_Model/ACPI_Software_Programming_Model.html
    if raw.get(..4) != Some(b"FACP".as_slice()) {
        return DeviceKind::Unknown;
    }
    let Some(length) = raw
        .get(4..8)
        .and_then(|bytes| <[u8; 4]>::try_from(bytes).ok())
    else {
        return DeviceKind::Unknown;
    };
    let Some(table) = raw.get(..u32::from_le_bytes(length) as usize) else {
        return DeviceKind::Unknown;
    };
    if table.len() < 46
        || table[8] < 2
        || table.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte)) != 0
    {
        return DeviceKind::Unknown;
    }
    match table[45] {
        2 => DeviceKind::Laptop,
        1 | 3..=7 => DeviceKind::Desktop,
        _ => DeviceKind::Unknown,
    }
}

fn chassis_device_kind(value: u8) -> DeviceKind {
    // SMBIOS Type 3 bit 7 is the chassis lock flag, not part of the type.
    // https://learn.microsoft.com/windows/win32/cimwin32prov/win32-systemenclosure
    // SMBIOS DSP0134 3.9.0, 7.4 / Tables 16–17:
    // https://www.dmtf.org/sites/default/files/standards/documents/DSP0134_3.9.0.pdf
    match value & 0x7f {
        8 | 9 | 10 | 14 | 31 | 32 => DeviceKind::Laptop,
        3..=7 | 13 | 15..=17 | 23 | 24 | 35 | 36 => DeviceKind::Desktop,
        // A Windows tablet/hand-held is not evidence that this is a phone.
        _ => DeviceKind::Unknown,
    }
}

/// Parse Windows RawSMBIOSData (8-byte header followed by SMBIOS structures).
/// Formatted lengths and double-NUL string terminators are checked before use.
fn device_kind_from_raw_smbios(raw: &[u8]) -> DeviceKind {
    let Some(length) = raw
        .get(4..8)
        .and_then(|bytes| <[u8; 4]>::try_from(bytes).ok())
    else {
        return DeviceKind::Unknown;
    };
    let Some(table) = raw.get(8..8usize.saturating_add(u32::from_le_bytes(length) as usize)) else {
        return DeviceKind::Unknown;
    };
    if raw[1] < 2 {
        return DeviceKind::Unknown;
    }
    let chassis_minimum = if raw[1] == 2 && raw[2] == 0 { 9 } else { 13 };
    let mut offset = 0usize;
    let mut kind = DeviceKind::Unknown;
    while offset < table.len() {
        let Some(header) = table.get(offset..offset.saturating_add(4)) else {
            return DeviceKind::Unknown;
        };
        let length = header[1] as usize;
        if length < 4 {
            return DeviceKind::Unknown;
        }
        let Some(formatted) = table.get(offset..offset.saturating_add(length)) else {
            return DeviceKind::Unknown;
        };
        if header[0] == 127 {
            break;
        }
        if header[0] == 3 {
            if length < chassis_minimum {
                return DeviceKind::Unknown;
            }
            let Some(chassis) = formatted.get(5) else {
                return DeviceKind::Unknown;
            };
            let detected = chassis_device_kind(*chassis);
            // A laptop can also expose a docking/auxiliary desktop enclosure.
            if detected == DeviceKind::Laptop || kind == DeviceKind::Unknown {
                kind = detected;
            }
        }
        let string_start = offset + length;
        let Some(end) = table[string_start..]
            .windows(2)
            .position(|bytes| bytes == [0, 0])
        else {
            return DeviceKind::Unknown;
        };
        offset = string_start + end + 2;
    }
    kind
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw_chassis(types: &[u8]) -> Vec<u8> {
        let mut table = Vec::new();
        for (handle, chassis) in types.iter().enumerate() {
            table.extend_from_slice(&[
                3,
                13,
                handle as u8,
                0,
                0,
                *chassis,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
            ]);
        }
        table.extend_from_slice(&[127, 4, 0xff, 0xff, 0, 0]);
        let mut raw = vec![0, 3, 5, 0];
        raw.extend_from_slice(&(table.len() as u32).to_le_bytes());
        raw.extend_from_slice(&table);
        raw
    }

    #[test]
    fn firmware_chassis_distinguishes_portable_pc_without_using_battery_presence() {
        for value in [8, 9, 10, 14, 31, 32] {
            assert_eq!(
                device_kind_from_raw_smbios(&raw_chassis(&[value])),
                DeviceKind::Laptop
            );
            assert_eq!(
                device_kind_from_raw_smbios(&raw_chassis(&[value | 0x80])),
                DeviceKind::Laptop
            );
        }
        for value in [3, 4, 5, 6, 7, 13, 15, 16, 17, 23, 24, 35, 36] {
            assert_eq!(
                device_kind_from_raw_smbios(&raw_chassis(&[value])),
                DeviceKind::Desktop
            );
        }
        assert_eq!(
            device_kind_from_raw_smbios(&raw_chassis(&[3, 9])),
            DeviceKind::Laptop
        );
        assert_eq!(
            device_kind_from_raw_smbios(&raw_chassis(&[9, 3])),
            DeviceKind::Laptop
        );
    }

    #[test]
    fn unknown_and_truncated_firmware_fall_back_without_claiming_a_phone() {
        for value in [0, 1, 2, 11, 12, 30, 0xff] {
            assert_eq!(
                device_kind_from_raw_smbios(&raw_chassis(&[value])),
                DeviceKind::Unknown
            );
        }
        let valid = raw_chassis(&[9]);
        for length in 0..valid.len() {
            assert_eq!(
                device_kind_from_raw_smbios(&valid[..length]),
                DeviceKind::Unknown
            );
        }
        let mut bad_length = valid.clone();
        bad_length[9] = 3;
        assert_eq!(
            device_kind_from_raw_smbios(&bad_length),
            DeviceKind::Unknown
        );
        let mut no_strings_end = valid;
        no_strings_end[21..].fill(1);
        assert_eq!(
            device_kind_from_raw_smbios(&no_strings_end),
            DeviceKind::Unknown
        );
    }

    #[test]
    fn acpi_oem_role_is_validated_without_battery_based_guessing() {
        let table = |profile: u8| {
            let mut bytes = vec![0u8; 116];
            bytes[..4].copy_from_slice(b"FACP");
            bytes[4..8].copy_from_slice(&116u32.to_le_bytes());
            bytes[8] = 2;
            bytes[45] = profile;
            bytes[9] =
                0u8.wrapping_sub(bytes.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte)));
            bytes
        };
        assert_eq!(device_kind_from_fadt(&table(2)), DeviceKind::Laptop);
        for role in [1, 3, 4, 5, 6, 7] {
            assert_eq!(device_kind_from_fadt(&table(role)), DeviceKind::Desktop);
        }
        for role in [0, 8, 255] {
            assert_eq!(device_kind_from_fadt(&table(role)), DeviceKind::Unknown);
        }
        let mut invalid = table(2);
        invalid[45] = 1;
        assert_eq!(device_kind_from_fadt(&invalid), DeviceKind::Unknown);
        assert_eq!(device_kind_from_fadt(&table(2)[..45]), DeviceKind::Unknown);
    }

    #[test]
    fn unknown_metadata_is_compatible_but_invalid_user_override_is_rejected_atomically() {
        for value in [json!("future-tablet"), json!(false), Value::Null, json!({})] {
            assert_eq!(
                serde_json::from_value::<DeviceKind>(value).unwrap(),
                DeviceKind::Unknown
            );
        }
        let paired = BTreeSet::from(["peer-a".to_string(), "peer-b".to_string()]);
        let mut presentation = DevicePresentation::default();
        presentation.set_detected(DeviceKind::Laptop);
        presentation.apply_patch(&json!({"deviceKindOverride":"phone", "peerDeviceKindOverrides":{"peer-a":"laptop"}}), &paired).unwrap();
        let before = presentation.clone();
        for params in [
            json!({"deviceKindOverride":"desktop", "peerDeviceKindOverrides":{"peer-b":"invalid"}}),
            json!({"deviceKindOverride":"desktop", "peerDeviceKindOverrides":{"unpaired":"phone"}}),
            json!({"deviceKindOverride":null}),
            json!({"peerDeviceKindOverrides":[]}),
        ] {
            assert!(presentation.apply_patch(&params, &paired).is_err());
            assert_eq!(presentation, before);
        }
        presentation
            .apply_patch(&json!({"peerDeviceKindOverrides":{}}), &paired)
            .unwrap();
        assert_eq!(presentation, before);
        presentation
            .apply_patch(
                &json!({"deviceKindOverride":"auto", "peerDeviceKindOverrides":{"peer-b":"phone"}}),
                &paired,
            )
            .unwrap();
        assert_eq!(presentation.display_kind(), DeviceKind::Laptop);
        assert_eq!(presentation.peer_override_name("peer-a"), "laptop");
        presentation
            .apply_patch(
                &json!({"peerDeviceKindOverrides":{"peer-a":"auto"}}),
                &paired,
            )
            .unwrap();
        assert_eq!(
            presentation.peer_display_kind("peer-a", DeviceKind::Phone),
            DeviceKind::Phone
        );
        assert_eq!(presentation.peer_override_name("peer-b"), "phone");
        assert_eq!(presentation.detected(), DeviceKind::Laptop);
    }

    #[test]
    fn preferences_roundtrip_keeps_detection_local_and_auto_restores_latest_report() {
        let paired = BTreeSet::from(["peer".to_string()]);
        let mut original = DevicePresentation::default();
        original.set_detected(DeviceKind::Desktop);
        original.apply_patch(&json!({"deviceKindOverride":"phone", "peerDeviceKindOverrides":{"peer":"desktop"}}), &paired).unwrap();
        let encoded = original.preferences();
        assert!(encoded.get("detectedDeviceKind").is_none());
        let mut reopened = DevicePresentation::default();
        reopened.load_preferences(&encoded);
        reopened.set_detected(DeviceKind::Laptop);
        assert_eq!(reopened.display_kind(), DeviceKind::Phone);
        assert_eq!(
            reopened.peer_display_kind("peer", DeviceKind::Laptop),
            DeviceKind::Desktop
        );
        reopened
            .apply_patch(
                &json!({"deviceKindOverride":"auto", "peerDeviceKindOverrides":{"peer":"auto"}}),
                &paired,
            )
            .unwrap();
        assert_eq!(reopened.display_kind(), DeviceKind::Laptop);
        assert_eq!(
            reopened.peer_display_kind("peer", DeviceKind::Laptop),
            DeviceKind::Laptop
        );
    }
}
