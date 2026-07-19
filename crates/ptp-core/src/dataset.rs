//! PTP dataset encoders: the structured payloads carried in data phases —
//! `ObjectInfo`, `DeviceInfo`, `StorageInfo`, and `DevicePropDesc`. Field order
//! and widths follow ISO 15740. These are syntax only; which properties or
//! formats a given camera supports is manifest data.

use crate::datatype::{Reader, Writer};
use crate::error::{DecodeError, EncodeError};

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ObjectInfo {
    pub storage_id: u32,
    pub object_format: u16,
    pub protection_status: u16,
    pub object_compressed_size: u32,
    pub thumb_format: u16,
    pub thumb_compressed_size: u32,
    pub thumb_pix_width: u32,
    pub thumb_pix_height: u32,
    pub image_pix_width: u32,
    pub image_pix_height: u32,
    pub image_bit_depth: u32,
    pub parent_object: u32,
    pub association_type: u16,
    pub association_desc: u32,
    pub sequence_number: u32,
    pub filename: String,
    pub capture_date: String,
    pub modification_date: String,
    pub keywords: String,
}

impl ObjectInfo {
    pub fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        let mut r = Reader::new(bytes);
        Ok(Self {
            storage_id: r.u32()?,
            object_format: r.u16()?,
            protection_status: r.u16()?,
            object_compressed_size: r.u32()?,
            thumb_format: r.u16()?,
            thumb_compressed_size: r.u32()?,
            thumb_pix_width: r.u32()?,
            thumb_pix_height: r.u32()?,
            image_pix_width: r.u32()?,
            image_pix_height: r.u32()?,
            image_bit_depth: r.u32()?,
            parent_object: r.u32()?,
            association_type: r.u16()?,
            association_desc: r.u32()?,
            sequence_number: r.u32()?,
            filename: r.ptp_string()?,
            capture_date: r.ptp_string()?,
            modification_date: r.ptp_string()?,
            keywords: r.ptp_string()?,
        })
    }

    pub fn encode(&self, w: &mut Writer) -> Result<(), EncodeError> {
        w.u32(self.storage_id);
        w.u16(self.object_format);
        w.u16(self.protection_status);
        w.u32(self.object_compressed_size);
        w.u16(self.thumb_format);
        w.u32(self.thumb_compressed_size);
        w.u32(self.thumb_pix_width);
        w.u32(self.thumb_pix_height);
        w.u32(self.image_pix_width);
        w.u32(self.image_pix_height);
        w.u32(self.image_bit_depth);
        w.u32(self.parent_object);
        w.u16(self.association_type);
        w.u32(self.association_desc);
        w.u32(self.sequence_number);
        w.ptp_string(&self.filename)?;
        w.ptp_string(&self.capture_date)?;
        w.ptp_string(&self.modification_date)?;
        w.ptp_string(&self.keywords)?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DeviceInfo {
    pub standard_version: u16,
    pub vendor_extension_id: u32,
    pub vendor_extension_version: u16,
    pub vendor_extension_desc: String,
    pub functional_mode: u16,
    pub operations_supported: Vec<u16>,
    pub events_supported: Vec<u16>,
    pub device_properties_supported: Vec<u16>,
    pub capture_formats: Vec<u16>,
    pub image_formats: Vec<u16>,
    pub manufacturer: String,
    pub model: String,
    pub device_version: String,
    pub serial_number: String,
}

impl DeviceInfo {
    pub fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        let mut r = Reader::new(bytes);
        Ok(Self {
            standard_version: r.u16()?,
            vendor_extension_id: r.u32()?,
            vendor_extension_version: r.u16()?,
            vendor_extension_desc: r.ptp_string()?,
            functional_mode: r.u16()?,
            operations_supported: r.ptp_array(|r| r.u16())?,
            events_supported: r.ptp_array(|r| r.u16())?,
            device_properties_supported: r.ptp_array(|r| r.u16())?,
            capture_formats: r.ptp_array(|r| r.u16())?,
            image_formats: r.ptp_array(|r| r.u16())?,
            manufacturer: r.ptp_string()?,
            model: r.ptp_string()?,
            device_version: r.ptp_string()?,
            serial_number: r.ptp_string()?,
        })
    }

    pub fn encode(&self, w: &mut Writer) -> Result<(), EncodeError> {
        w.u16(self.standard_version);
        w.u32(self.vendor_extension_id);
        w.u16(self.vendor_extension_version);
        w.ptp_string(&self.vendor_extension_desc)?;
        w.u16(self.functional_mode);
        w.ptp_array(&self.operations_supported, |w, v| w.u16(*v));
        w.ptp_array(&self.events_supported, |w, v| w.u16(*v));
        w.ptp_array(&self.device_properties_supported, |w, v| w.u16(*v));
        w.ptp_array(&self.capture_formats, |w, v| w.u16(*v));
        w.ptp_array(&self.image_formats, |w, v| w.u16(*v));
        w.ptp_string(&self.manufacturer)?;
        w.ptp_string(&self.model)?;
        w.ptp_string(&self.device_version)?;
        w.ptp_string(&self.serial_number)?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StorageInfo {
    pub storage_type: u16,
    pub filesystem_type: u16,
    pub access_capability: u16,
    pub max_capacity: u64,
    pub free_space_bytes: u64,
    pub free_space_images: u32,
    pub storage_description: String,
    pub volume_label: String,
}

impl StorageInfo {
    pub fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        let mut r = Reader::new(bytes);
        Ok(Self {
            storage_type: r.u16()?,
            filesystem_type: r.u16()?,
            access_capability: r.u16()?,
            max_capacity: r.u64()?,
            free_space_bytes: r.u64()?,
            free_space_images: r.u32()?,
            storage_description: r.ptp_string()?,
            volume_label: r.ptp_string()?,
        })
    }

    pub fn encode(&self, w: &mut Writer) -> Result<(), EncodeError> {
        w.u16(self.storage_type);
        w.u16(self.filesystem_type);
        w.u16(self.access_capability);
        w.u64(self.max_capacity);
        w.u64(self.free_space_bytes);
        w.u32(self.free_space_images);
        w.ptp_string(&self.storage_description)?;
        w.ptp_string(&self.volume_label)?;
        Ok(())
    }
}

/// A typed scalar PTP property value. DevicePropDesc values use the standard
/// signed and unsigned integer widths or a PTP string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PropValue {
    I8(i8),
    U8(u8),
    I16(i16),
    U16(u16),
    I32(i32),
    U32(u32),
    I64(i64),
    U64(u64),
    Str(String),
}

impl PropValue {
    pub fn datatype_code(&self) -> u16 {
        use crate::codes::datatype_code as dt;
        match self {
            PropValue::I8(_) => dt::INT8,
            PropValue::U8(_) => dt::UINT8,
            PropValue::I16(_) => dt::INT16,
            PropValue::U16(_) => dt::UINT16,
            PropValue::I32(_) => dt::INT32,
            PropValue::U32(_) => dt::UINT32,
            PropValue::I64(_) => dt::INT64,
            PropValue::U64(_) => dt::UINT64,
            PropValue::Str(_) => dt::STR,
        }
    }

    pub fn decode(r: &mut Reader, datatype: u16) -> Result<Self, DecodeError> {
        use crate::codes::datatype_code as dt;
        Ok(match datatype {
            dt::INT8 => PropValue::I8(r.i8()?),
            dt::UINT8 => PropValue::U8(r.u8()?),
            dt::INT16 => PropValue::I16(r.i16()?),
            dt::UINT16 => PropValue::U16(r.u16()?),
            dt::INT32 => PropValue::I32(r.i32()?),
            dt::UINT32 => PropValue::U32(r.u32()?),
            dt::INT64 => PropValue::I64(r.i64()?),
            dt::UINT64 => PropValue::U64(r.u64()?),
            dt::STR => PropValue::Str(r.ptp_string()?),
            _ => return Err(DecodeError::InvalidString("unsupported prop datatype")),
        })
    }

    pub fn encode(&self, w: &mut Writer) -> Result<(), EncodeError> {
        match self {
            PropValue::I8(v) => w.i8(*v),
            PropValue::U8(v) => w.u8(*v),
            PropValue::I16(v) => w.i16(*v),
            PropValue::U16(v) => w.u16(*v),
            PropValue::I32(v) => w.i32(*v),
            PropValue::U32(v) => w.u32(*v),
            PropValue::I64(v) => w.i64(*v),
            PropValue::U64(v) => w.u64(*v),
            PropValue::Str(s) => w.ptp_string(s)?,
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PropForm {
    None,
    Range {
        min: PropValue,
        max: PropValue,
        step: PropValue,
    },
    Enum(Vec<PropValue>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DevicePropDesc {
    pub code: u16,
    pub datatype: u16,
    pub get_set: u8,
    pub factory_default: PropValue,
    pub current: PropValue,
    pub form: PropForm,
}

impl DevicePropDesc {
    pub fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        let mut r = Reader::new(bytes);
        let code = r.u16()?;
        let datatype = r.u16()?;
        let get_set = r.u8()?;
        let factory_default = PropValue::decode(&mut r, datatype)?;
        let current = PropValue::decode(&mut r, datatype)?;
        let form_flag = r.u8()?;
        let form = match form_flag {
            0 => PropForm::None,
            1 => PropForm::Range {
                min: PropValue::decode(&mut r, datatype)?,
                max: PropValue::decode(&mut r, datatype)?,
                step: PropValue::decode(&mut r, datatype)?,
            },
            2 => {
                let count = r.u16()?;
                let mut values = Vec::with_capacity(count as usize);
                for _ in 0..count {
                    values.push(PropValue::decode(&mut r, datatype)?);
                }
                PropForm::Enum(values)
            }
            _ => return Err(DecodeError::InvalidString("unknown prop form flag")),
        };
        Ok(Self {
            code,
            datatype,
            get_set,
            factory_default,
            current,
            form,
        })
    }

    pub fn encode(&self, w: &mut Writer) -> Result<(), EncodeError> {
        w.u16(self.code);
        w.u16(self.datatype);
        w.u8(self.get_set);
        self.factory_default.encode(w)?;
        self.current.encode(w)?;
        match &self.form {
            PropForm::None => w.u8(0),
            PropForm::Range { min, max, step } => {
                w.u8(1);
                min.encode(w)?;
                max.encode(w)?;
                step.encode(w)?;
            }
            PropForm::Enum(values) => {
                w.u8(2);
                w.u16(values.len() as u16);
                for v in values {
                    v.encode(w)?;
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codes::{datatype_code, format};

    fn rt_objectinfo(oi: &ObjectInfo) {
        let mut w = Writer::new();
        oi.encode(&mut w).unwrap();
        assert_eq!(&ObjectInfo::decode(w.as_slice()).unwrap(), oi);
    }

    #[test]
    fn object_info_round_trips() {
        rt_objectinfo(&ObjectInfo {
            storage_id: 0x0001_0001,
            object_format: format::EXIF_JPEG,
            object_compressed_size: 4_289_912,
            image_pix_width: 8256,
            image_pix_height: 6192,
            parent_object: 0x10,
            filename: "DSCF1494.JPG".into(),
            capture_date: "20260518T114931".into(),
            ..Default::default()
        });
    }

    #[test]
    fn object_info_oversize_ceiling() {
        // The 32-bit size field cannot represent >4 GiB; the ceiling is 0xffffffff.
        rt_objectinfo(&ObjectInfo {
            object_format: 0x300d,
            object_compressed_size: 0xffff_ffff,
            filename: "DSCF1495.MOV".into(),
            ..Default::default()
        });
    }

    #[test]
    fn device_info_round_trips() {
        let di = DeviceInfo {
            standard_version: 100,
            vendor_extension_id: 0x0000_00ff,
            model: "GFX100 II".into(),
            manufacturer: "FUJIFILM".into(),
            operations_supported: vec![0x1001, 0x1002, 0x101b, 0x9054],
            device_properties_supported: vec![0x5007, 0xd02a],
            image_formats: vec![format::EXIF_JPEG],
            ..Default::default()
        };
        let mut w = Writer::new();
        di.encode(&mut w).unwrap();
        assert_eq!(DeviceInfo::decode(w.as_slice()).unwrap(), di);
    }

    #[test]
    fn storage_info_round_trips() {
        let si = StorageInfo {
            storage_type: 3,
            filesystem_type: 2,
            access_capability: 0,
            max_capacity: 512u64 * 1024 * 1024 * 1024,
            free_space_bytes: 100u64 * 1024 * 1024 * 1024,
            free_space_images: 9999,
            storage_description: "SD1".into(),
            volume_label: "".into(),
        };
        let mut w = Writer::new();
        si.encode(&mut w).unwrap();
        assert_eq!(StorageInfo::decode(w.as_slice()).unwrap(), si);
    }

    #[test]
    fn prop_desc_enum_round_trips() {
        // Aperture (FNumber): u16 enum, read/write.
        let desc = DevicePropDesc {
            code: 0x5007,
            datatype: datatype_code::UINT16,
            get_set: 1,
            factory_default: PropValue::U16(400),
            current: PropValue::U16(560),
            form: PropForm::Enum(vec![
                PropValue::U16(280),
                PropValue::U16(400),
                PropValue::U16(560),
                PropValue::U16(800),
                PropValue::U16(65535),
            ]),
        };
        let mut w = Writer::new();
        desc.encode(&mut w).unwrap();
        assert_eq!(DevicePropDesc::decode(w.as_slice()).unwrap(), desc);
    }

    #[test]
    fn prop_desc_range_round_trips() {
        let desc = DevicePropDesc {
            code: 0xd02a,
            datatype: datatype_code::UINT32,
            get_set: 1,
            factory_default: PropValue::U32(100),
            current: PropValue::U32(800),
            form: PropForm::Range {
                min: PropValue::U32(100),
                max: PropValue::U32(12800),
                step: PropValue::U32(1),
            },
        };
        let mut w = Writer::new();
        desc.encode(&mut w).unwrap();
        assert_eq!(DevicePropDesc::decode(w.as_slice()).unwrap(), desc);
    }

    #[test]
    fn signed_prop_desc_round_trips() {
        let desc = DevicePropDesc {
            code: 0x5001,
            datatype: datatype_code::INT16,
            get_set: 1,
            factory_default: PropValue::I16(-1),
            current: PropValue::I16(-3),
            form: PropForm::Range {
                min: PropValue::I16(-10),
                max: PropValue::I16(10),
                step: PropValue::I16(1),
            },
        };
        let mut w = Writer::new();
        desc.encode(&mut w).unwrap();
        assert_eq!(DevicePropDesc::decode(w.as_slice()).unwrap(), desc);
    }
}
