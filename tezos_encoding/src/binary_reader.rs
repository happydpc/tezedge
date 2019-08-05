use bitvec::{Bits, BitVec};
use bytes::{Buf, IntoBuf};
use serde::de::Error as SerdeError;

use crate::bit_utils::{BitReverse, BitTrim, ToBytes};
use crate::de::Error;
use crate::encoding::{Encoding, Field, SchemaType};
use crate::types::{self, Value};

pub struct BinaryReader {}

impl BinaryReader {

    pub fn new() -> Self {
        BinaryReader {}
    }

    pub fn read(&self, buf: Vec<u8>, encoding: &Encoding) -> Result<Value, Error> {
        match encoding {
            Encoding::Obj(schema) => self.decode_record(&mut buf.into_buf(), schema),
            _ => self.decode_value(&mut buf.into_buf(), encoding)
        }
    }

    fn decode_record(&self, buf: &mut dyn Buf, schema: &[Field]) -> Result<Value, Error> {
        let mut values = vec![];
        for field in schema {
            let name = field.get_name();
            let encoding = field.get_encoding();
            values.push((name.clone(), self.decode_value(buf, encoding)?))
        }
        Ok(Value::Record(values))
    }

    fn decode_value(&self, buf: &mut dyn Buf, encoding: &Encoding) -> Result<Value, Error> {
        match encoding {
            Encoding::Split(inner_encoding) => {
                let inner_encoding = inner_encoding(SchemaType::Binary);
                self.decode_value(buf, &inner_encoding)
            }
            Encoding::Unit => Ok(Value::Unit),
            Encoding::Int8 => Ok(Value::Int8(buf.get_i8())),
            Encoding::Uint8 => Ok(Value::Uint8(buf.get_u8())),
            Encoding::Int16 => Ok(Value::Int16(buf.get_i16_be())),
            Encoding::Uint16 => Ok(Value::Uint16(buf.get_u16_be())),
            Encoding::Int31 => Ok(Value::Int31(buf.get_i32_be())),
            Encoding::Int32 => Ok(Value::Int32(buf.get_i32_be())),
            Encoding::Int64 |
            Encoding::Timestamp => Ok(Value::Int64(buf.get_i64_be())),
            Encoding::Float => Ok(Value::Float(buf.get_f64_be())),
            Encoding::Bool => {
                let b = buf.get_u8();
                match b {
                    types::BYTE_VAL_TRUE => Ok(Value::Bool(true)),
                    types::BYTE_VAL_FALSE => Ok(Value::Bool(false)),
                    _ => Err(Error::custom(format!("Vas expecting 0xFF or 0x00 but instead got {:X}", b)))
                }
            }
            Encoding::String => {
                let bytes_sz = buf.get_u32_be() as usize;
                let mut str_buf = vec![0u8; bytes_sz].into_boxed_slice();
                buf.copy_to_slice(&mut str_buf);
                let str_buf = str_buf.into_vec();
                Ok(Value::String(String::from_utf8(str_buf)?))
            }
            Encoding::Enum => Ok(Value::Enum(None, Some(u32::from(buf.get_u8())))),
            Encoding::Dynamic(dynamic_encoding) => {
                let bytes_sz = buf.get_u32_be() as usize;
                let mut buf_slice = buf.take(bytes_sz);
                self.decode_value(&mut buf_slice, dynamic_encoding)
            }
            Encoding::Sized(sized_size, sized_encoding) => {
                let mut buf_slice = buf.take(*sized_size);
                self.decode_value(&mut buf_slice, sized_encoding)
            }
            Encoding::Greedy(un_sized_encoding) => {
                let bytes_sz = buf.remaining();
                let mut buf_slice = buf.take(bytes_sz);
                self.decode_value(&mut buf_slice, un_sized_encoding)
            }
            Encoding::Tags(tag_sz, ref tag_map) => {
                let tag_id = match tag_sz  {
                    1 => Ok(u16::from(buf.get_u8())),
                    2 => Ok(buf.get_u16_be()),
                    _ => Err(Error::custom(format!("Unsupported tag size {}", tag_sz)))
                }?;

                match tag_map.find_by_id(tag_id) {
                    Some(tag) => {
                        let tag_value = self.decode_value(buf, tag.get_encoding())?;
                        Ok(Value::Tag(tag.get_variant().to_string(), Box::new(tag_value)))
                    },
                    None => Err(Error::custom(format!("No tag found for id: 0x{:X}", tag_id)))
                }
            }
            Encoding::List(encoding_inner) => {
                let bytes_sz = buf.remaining();

                let mut buf_slice = buf.take(bytes_sz);

                let mut values = vec![];
                while buf_slice.remaining() > 0 {
                    values.push(self.decode_value(&mut buf_slice, encoding_inner)?);
                }

                Ok(Value::List(values))
            }
            Encoding::Option(_) => {
                let is_present_byte = buf.get_u8();
                match is_present_byte {
                    types::BYTE_VAL_SOME => {
                        let v = self.decode_value(buf, encoding.try_unwrap_option_encoding())?;
                        Ok(Value::Option(Some(Box::new(v))))
                    }
                    types::BYTE_VAL_NONE => Ok(Value::Option(None)),
                    _ => Err(Error::custom(format!("Unexpected option value {:X}", is_present_byte)))
                }
            }
            Encoding::Obj(schema_inner) => {
                Ok(self.decode_record(buf, schema_inner)?)
            }
            Encoding::Z => {
                // read first byte
                let byte = buf.get_u8();
                let negative = byte.get(6);
                if byte <= 0x3F {
                    let mut num = i32::from(byte);
                    if negative {
                        num *= -1;
                    }
                    Ok(Value::String(format!("{:x}", num)))
                } else {
                    let mut bits: BitVec<bitvec::BigEndian, u8> = BitVec::new();
                    for bit_idx in 0..6 {
                        bits.push(byte.get(bit_idx));
                    }

                    let mut has_next_byte = true;
                    while has_next_byte {
                        let byte = buf.get_u8();
                        for bit_idx in 0..7 {
                            bits.push(byte.get(bit_idx))
                        }

                        has_next_byte = byte.get(7);
                    }

                    let bytes = bits.reverse().trim_left().to_byte_vec();

                    let mut str_num = bytes.iter().enumerate()
                        .map(|(idx, b)| {
                            match idx {
                                0 => format!("{:x}", *b),
                                _ => format!("{:02x}", *b)
                            }
                        })
                        .fold(String::new(), |mut str_num, val| {
                            str_num.push_str(&val);
                            str_num
                        });
                    if negative {
                        str_num = String::from("-") + &str_num;
                    }

                    Ok(Value::String(str_num))
                }
            }
            Encoding::Bytes => {
                let bytes_sz = buf.remaining();
                let mut buf_slice = vec![0u8; bytes_sz].into_boxed_slice();
                buf.copy_to_slice(&mut buf_slice);
                Ok(Value::List(buf_slice.into_vec().iter().map(|&byte| Value::Uint8(byte)).collect()))
            }
            Encoding::Hash(hash_encoding) => {
                let mut buf_slice = vec![0u8; hash_encoding.get_bytes_size()].into_boxed_slice();
                buf.copy_to_slice(&mut buf_slice);
                Ok(Value::List(buf_slice.into_vec().iter().map(|&byte| Value::Uint8(byte)).collect()))
            }
            _ => Err(Error::custom(format!("Unsupported encoding {:?}", encoding)))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::mem::size_of;

    use serde::{Deserialize, Serialize};

    use crate::binary_writer::BinaryWriter;
    use crate::de;
    use crate::encoding::{Tag, TagMap};
    use crate::ser::Serializer;
    use crate::types::BigInt;

    use super::*;

    #[test]
    fn can_deserialize_z_from_binary() {
        #[derive(Deserialize, Debug)]
        struct Record {
            a: BigInt
        }
        let record_schema = vec![
            Field::new("a", Encoding::Z)
        ];

        let record_buf = hex::decode("9e9ed49d01").unwrap();
        let reader = BinaryReader::new();
        let value = reader.read(record_buf, &Encoding::Obj(record_schema)).unwrap();
        assert_eq!(Value::Record(vec![("a".to_string(), Value::String("9da879e".to_string()))]), value)
    }

    #[test]
    fn can_deserialize_tag_from_binary() {

        #[derive(Deserialize, Debug, PartialEq)]
        struct GetHeadRecord {
            chain_id: Vec<u8>,
        }

        let get_head_record_schema = vec![
            Field::new("chain_id", Encoding::Sized(4, Box::new(Encoding::Bytes)))
        ];

        #[derive(Deserialize, Debug, PartialEq)]
        enum Message {
            GetHead(GetHeadRecord)
        }

        #[derive(Deserialize, Debug, PartialEq)]
        struct Response {
            messages: Vec<Message>,
        }

        let response_schema = vec![
            Field::new("messages",  Encoding::dynamic(Encoding::list(
                Encoding::Tags(
                    size_of::<u16>(),
                    TagMap::new(&vec![Tag::new(0x10, "GetHead", Encoding::Obj(get_head_record_schema))])
                )
           )))
        ];

        // deserialize to value
        let record_buf = hex::decode("0000000600108eceda2f").unwrap();
        let reader = BinaryReader::new();
        let value = reader.read(record_buf, &Encoding::Obj(response_schema)).unwrap();
        // convert value to actual data structure
        let value: Response = de::from_value(&value).unwrap();
        let expected_value = Response {
            messages: vec![Message::GetHead(GetHeadRecord { chain_id: hex::decode("8eceda2f").unwrap() })]
        };
        assert_eq!(expected_value, value)
    }

    #[test]
    fn can_deserialize_z_range() {
        #[derive(Serialize, Deserialize, Debug)]
        struct Record {
            a: BigInt
        }
        let record_schema = vec![
            Field::new("a", Encoding::Z)
        ];
        let record_encoding = Encoding::Obj(record_schema);

        for num in -100..=100 {
            let num_mul = num * 1000;
            let record = Record {
                a: num_bigint::BigInt::from(num_mul).into()
            };

            let mut writer = BinaryWriter::new();
            let mut serializer = Serializer::default();

            let value_serialized = record.serialize(&mut serializer).unwrap();
            let record_bytes = writer.write(&record, &record_encoding).unwrap();

            let reader = BinaryReader::new();
            let value_deserialized = reader.read(record_bytes, &record_encoding).unwrap();

            assert_eq!(value_serialized, value_deserialized)
        }
    }

    #[test]
    fn can_deserialize_connection_message() {
        #[derive(Deserialize, Debug, PartialEq)]
        struct Version {
            name: String,
            major: u16,
            minor: u16,
        }

        #[derive(Deserialize, Debug, PartialEq)]
        struct ConnectionMessage {
            port: u16,
            versions: Vec<Version>,
            public_key: Vec<u8>,
            proof_of_work_stamp: Vec<u8>,
            message_nonce: Vec<u8>,
        }

        let version_schema = vec![
            Field::new("name", Encoding::String),
            Field::new("major", Encoding::Uint16),
            Field::new("minor", Encoding::Uint16)
        ];

        let connection_message_schema = vec![
            Field::new("port", Encoding::Uint16),
            Field::new("public_key", Encoding::sized(32, Encoding::Bytes)),
            Field::new("proof_of_work_stamp", Encoding::sized(24, Encoding::Bytes)),
            Field::new("message_nonce", Encoding::sized(24, Encoding::Bytes)),
            Field::new("versions", Encoding::list(Encoding::Obj(version_schema)))
        ];
        let connection_message_encoding = Encoding::Obj(connection_message_schema);

        let connection_message = ConnectionMessage {
            port: 3001,
            versions: vec![Version { name: "A".to_string(), major: 1, minor: 1 }, Version { name: "B".to_string(), major: 2, minor: 0 }],
            public_key: hex::decode("eaef40186db19fd6f56ed5b1af57f9d9c8a1eed85c29f8e4daaa7367869c0f0b").unwrap(),
            proof_of_work_stamp: hex::decode("000000000000000000000000000000000000000000000000").unwrap(),
            message_nonce: hex::decode("000000000000000000000000000000000000000000000000").unwrap(),
        };

        let connection_message_buf = hex::decode("0bb9eaef40186db19fd6f56ed5b1af57f9d9c8a1eed85c29f8e4daaa7367869c0f0b000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000014100010001000000014200020000").unwrap();
        let reader = BinaryReader::new();
        let value = reader.read(connection_message_buf, &connection_message_encoding).unwrap();

        let connection_message_deserialized: ConnectionMessage = de::from_value(&value).unwrap();
        assert_eq!(connection_message, connection_message_deserialized);
    }
}