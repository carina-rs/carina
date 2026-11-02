//! Retain JSON field order as persistence metadata, without retaining values.

use serde::de::{DeserializeSeed, EnumAccess, MapAccess, SeqAccess, VariantAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

/// Field-order evidence captured while decoding a persisted JSON document.
/// It contains only object keys and container structure. Typed data remains
/// authoritative; newly introduced keys use JSON's canonical map order.
#[derive(Debug, Clone, Default)]
pub struct JsonFieldOrder(Order);

#[derive(Debug, Clone, Default)]
enum Order {
    #[default]
    Scalar,
    Object(Vec<(String, Order)>),
    Array(Vec<Order>),
}

impl JsonFieldOrder {
    /// Decode through the original deserializer, preserving its errors and paths.
    pub fn deserialize<'de, T: Deserialize<'de>, D: Deserializer<'de>>(
        &mut self,
        deserializer: D,
    ) -> Result<T, D::Error> {
        T::deserialize(RecordingDeserializer {
            inner: deserializer,
            order: &mut self.0,
            key: None,
        })
    }

    /// Capture field order after a document has passed its checked loader.
    pub fn from_json_str(content: &str) -> serde_json::Result<Self> {
        let mut order = Self::default();
        let mut deserializer = serde_json::Deserializer::from_str(content);
        let _: serde_json::Value = order.deserialize(&mut deserializer)?;
        deserializer.end()?;
        Ok(order)
    }

    /// Capture field order from checked JSON bytes.
    pub fn from_json_slice(bytes: &[u8]) -> serde_json::Result<Self> {
        let mut order = Self::default();
        let mut deserializer = serde_json::Deserializer::from_slice(bytes);
        let _: serde_json::Value = order.deserialize(&mut deserializer)?;
        deserializer.end()?;
        Ok(order)
    }

    /// Write current typed data using the loaded document's object order.
    pub fn serialize<T: Serialize, S: Serializer>(
        &self,
        value: &T,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let value = serde_json::to_value(value).map_err(serde::ser::Error::custom)?;
        OrderedValue {
            value: &value,
            order: &self.0,
        }
        .serialize(serializer)
    }
}

struct OrderedValue<'a> {
    value: &'a serde_json::Value,
    order: &'a Order,
}

impl Serialize for OrderedValue<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match (self.value, self.order) {
            (serde_json::Value::Object(values), Order::Object(keys)) => {
                let mut map = serializer.serialize_map(Some(values.len()))?;
                for (key, order) in keys {
                    if let Some(value) = values.get(key) {
                        map.serialize_entry(key, &OrderedValue { value, order })?;
                    }
                }
                for (key, value) in values {
                    if !keys.iter().any(|(known, _)| known == key) {
                        map.serialize_entry(key, value)?;
                    }
                }
                map.end()
            }
            (serde_json::Value::Array(values), Order::Array(orders)) => {
                use serde::ser::SerializeSeq;
                let mut seq = serializer.serialize_seq(Some(values.len()))?;
                for (index, value) in values.iter().enumerate() {
                    seq.serialize_element(&OrderedValue {
                        value,
                        order: orders.get(index).unwrap_or(&Order::Scalar),
                    })?;
                }
                seq.end()
            }
            _ => self.value.serialize(serializer),
        }
    }
}

struct RecordingDeserializer<'a, D> {
    inner: D,
    order: &'a mut Order,
    key: Option<&'a mut Option<String>>,
}

macro_rules! record_method {
    ($method:ident $(, $arg:ident: $ty:ty)*) => {
        fn $method<V: Visitor<'de>>(self, $($arg: $ty,)* visitor: V) -> Result<V::Value, Self::Error> {
            self.inner.$method($($arg,)* RecordingVisitor { visitor, order: self.order, key: self.key })
        }
    };
}

impl<'de, D: Deserializer<'de>> Deserializer<'de> for RecordingDeserializer<'_, D> {
    type Error = D::Error;
    record_method!(deserialize_any);
    record_method!(deserialize_bool);
    record_method!(deserialize_i8);
    record_method!(deserialize_i16);
    record_method!(deserialize_i32);
    record_method!(deserialize_i64);
    record_method!(deserialize_u8);
    record_method!(deserialize_u16);
    record_method!(deserialize_u32);
    record_method!(deserialize_u64);
    record_method!(deserialize_f32);
    record_method!(deserialize_f64);
    record_method!(deserialize_char);
    record_method!(deserialize_str);
    record_method!(deserialize_string);
    record_method!(deserialize_bytes);
    record_method!(deserialize_byte_buf);
    record_method!(deserialize_option);
    record_method!(deserialize_unit);
    record_method!(deserialize_unit_struct, name: &'static str);
    record_method!(deserialize_newtype_struct, name: &'static str);
    record_method!(deserialize_seq);
    record_method!(deserialize_tuple, len: usize);
    record_method!(deserialize_tuple_struct, name: &'static str, len: usize);
    record_method!(deserialize_map);
    record_method!(deserialize_struct, name: &'static str, fields: &'static [&'static str]);
    record_method!(deserialize_enum, name: &'static str, variants: &'static [&'static str]);
    record_method!(deserialize_identifier);
    record_method!(deserialize_ignored_any);
    fn is_human_readable(&self) -> bool {
        self.inner.is_human_readable()
    }
}

struct RecordingVisitor<'a, V> {
    visitor: V,
    order: &'a mut Order,
    key: Option<&'a mut Option<String>>,
}

macro_rules! visit_scalar {
    ($method:ident, $ty:ty) => {
        fn $method<E: serde::de::Error>(self, value: $ty) -> Result<Self::Value, E> {
            if let Some(key) = self.key {
                *key = Some(value.to_string());
            }
            self.visitor.$method(value)
        }
    };
}

impl<'de, V: Visitor<'de>> Visitor<'de> for RecordingVisitor<'_, V> {
    type Value = V::Value;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.visitor.expecting(f)
    }
    visit_scalar!(visit_bool, bool);
    visit_scalar!(visit_i8, i8);
    visit_scalar!(visit_i16, i16);
    visit_scalar!(visit_i32, i32);
    visit_scalar!(visit_i64, i64);
    visit_scalar!(visit_u8, u8);
    visit_scalar!(visit_u16, u16);
    visit_scalar!(visit_u32, u32);
    visit_scalar!(visit_u64, u64);
    visit_scalar!(visit_f32, f32);
    visit_scalar!(visit_f64, f64);
    visit_scalar!(visit_char, char);
    visit_scalar!(visit_str, &str);
    visit_scalar!(visit_borrowed_str, &'de str);
    visit_scalar!(visit_string, String);
    fn visit_bytes<E: serde::de::Error>(self, value: &[u8]) -> Result<Self::Value, E> {
        self.visitor.visit_bytes(value)
    }
    fn visit_borrowed_bytes<E: serde::de::Error>(self, value: &'de [u8]) -> Result<Self::Value, E> {
        self.visitor.visit_borrowed_bytes(value)
    }
    fn visit_byte_buf<E: serde::de::Error>(self, value: Vec<u8>) -> Result<Self::Value, E> {
        self.visitor.visit_byte_buf(value)
    }
    fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
        self.visitor.visit_unit()
    }
    fn visit_none<E: serde::de::Error>(self) -> Result<Self::Value, E> {
        self.visitor.visit_none()
    }
    fn visit_some<D: Deserializer<'de>>(self, inner: D) -> Result<Self::Value, D::Error> {
        self.visitor.visit_some(RecordingDeserializer {
            inner,
            order: self.order,
            key: self.key,
        })
    }
    fn visit_newtype_struct<D: Deserializer<'de>>(self, inner: D) -> Result<Self::Value, D::Error> {
        self.visitor.visit_newtype_struct(RecordingDeserializer {
            inner,
            order: self.order,
            key: self.key,
        })
    }
    fn visit_map<A: MapAccess<'de>>(self, inner: A) -> Result<Self::Value, A::Error> {
        *self.order = Order::Object(Vec::new());
        let Order::Object(fields) = self.order else {
            unreachable!()
        };
        self.visitor.visit_map(RecordingMap {
            inner,
            fields,
            key: None,
        })
    }
    fn visit_seq<A: SeqAccess<'de>>(self, inner: A) -> Result<Self::Value, A::Error> {
        *self.order = Order::Array(Vec::new());
        let Order::Array(items) = self.order else {
            unreachable!()
        };
        self.visitor.visit_seq(RecordingSeq { inner, items })
    }
    fn visit_enum<A: EnumAccess<'de>>(self, inner: A) -> Result<Self::Value, A::Error> {
        self.visitor.visit_enum(RecordingEnum {
            inner,
            order: self.order,
            key: self.key,
        })
    }
}

struct RecordingSeed<'a, T> {
    seed: T,
    order: &'a mut Order,
    key: Option<&'a mut Option<String>>,
}

impl<'de, T: DeserializeSeed<'de>> DeserializeSeed<'de> for RecordingSeed<'_, T> {
    type Value = T::Value;
    fn deserialize<D: Deserializer<'de>>(self, inner: D) -> Result<Self::Value, D::Error> {
        self.seed.deserialize(RecordingDeserializer {
            inner,
            order: self.order,
            key: self.key,
        })
    }
}

struct RecordingMap<'a, A> {
    inner: A,
    fields: &'a mut Vec<(String, Order)>,
    key: Option<String>,
}

impl<'de, A: MapAccess<'de>> MapAccess<'de> for RecordingMap<'_, A> {
    type Error = A::Error;
    fn next_key_seed<T: DeserializeSeed<'de>>(
        &mut self,
        seed: T,
    ) -> Result<Option<T::Value>, Self::Error> {
        self.key = None;
        self.inner.next_key_seed(RecordingSeed {
            seed,
            order: &mut Order::Scalar,
            key: Some(&mut self.key),
        })
    }
    fn next_value_seed<T: DeserializeSeed<'de>>(
        &mut self,
        seed: T,
    ) -> Result<T::Value, Self::Error> {
        let key = self.key.take().unwrap_or_default();
        let index = match self.fields.iter().position(|(name, _)| name == &key) {
            Some(index) => index,
            None => {
                self.fields.push((key, Order::Scalar));
                self.fields.len() - 1
            }
        };
        self.inner.next_value_seed(RecordingSeed {
            seed,
            order: &mut self.fields[index].1,
            key: None,
        })
    }
    fn size_hint(&self) -> Option<usize> {
        self.inner.size_hint()
    }
}

struct RecordingSeq<'a, A> {
    inner: A,
    items: &'a mut Vec<Order>,
}
impl<'de, A: SeqAccess<'de>> SeqAccess<'de> for RecordingSeq<'_, A> {
    type Error = A::Error;
    fn next_element_seed<T: DeserializeSeed<'de>>(
        &mut self,
        seed: T,
    ) -> Result<Option<T::Value>, Self::Error> {
        self.items.push(Order::Scalar);
        let value = self.inner.next_element_seed(RecordingSeed {
            seed,
            order: self.items.last_mut().unwrap(),
            key: None,
        })?;
        if value.is_none() {
            self.items.pop();
        }
        Ok(value)
    }
    fn size_hint(&self) -> Option<usize> {
        self.inner.size_hint()
    }
}

struct RecordingEnum<'a, A> {
    inner: A,
    order: &'a mut Order,
    key: Option<&'a mut Option<String>>,
}
impl<'a, 'de, A: EnumAccess<'de>> EnumAccess<'de> for RecordingEnum<'a, A> {
    type Error = A::Error;
    type Variant = RecordingVariant<'a, A::Variant>;
    fn variant_seed<T: DeserializeSeed<'de>>(
        self,
        seed: T,
    ) -> Result<(T::Value, Self::Variant), Self::Error> {
        let mut name = None;
        let (value, inner) = self.inner.variant_seed(RecordingSeed {
            seed,
            order: &mut Order::Scalar,
            key: Some(&mut name),
        })?;
        let name = name.unwrap_or_default();
        if let Some(key) = self.key {
            *key = Some(name.clone());
        }
        *self.order = Order::Object(vec![(name, Order::Scalar)]);
        let Order::Object(fields) = self.order else {
            unreachable!()
        };
        Ok((
            value,
            RecordingVariant {
                inner,
                order: &mut fields[0].1,
            },
        ))
    }
}

struct RecordingVariant<'a, A> {
    inner: A,
    order: &'a mut Order,
}
impl<'de, A: VariantAccess<'de>> VariantAccess<'de> for RecordingVariant<'_, A> {
    type Error = A::Error;
    fn unit_variant(self) -> Result<(), Self::Error> {
        self.inner.unit_variant()
    }
    fn newtype_variant_seed<T: DeserializeSeed<'de>>(
        self,
        seed: T,
    ) -> Result<T::Value, Self::Error> {
        self.inner.newtype_variant_seed(RecordingSeed {
            seed,
            order: self.order,
            key: None,
        })
    }
    fn tuple_variant<V: Visitor<'de>>(
        self,
        len: usize,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.inner.tuple_variant(
            len,
            RecordingVisitor {
                visitor,
                order: self.order,
                key: None,
            },
        )
    }
    fn struct_variant<V: Visitor<'de>>(
        self,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.inner.struct_variant(
            fields,
            RecordingVisitor {
                visitor,
                order: self.order,
                key: None,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[derive(Debug, Serialize, Deserialize)]
    struct Document {
        version: u32,
        #[serde(flatten)]
        fields: HashMap<String, Node>,
    }

    #[derive(Debug, Serialize, Deserialize)]
    enum Node {
        Int(i64),
        Map(HashMap<String, Node>),
        List(Vec<Node>),
    }

    fn ordered_json<T: Serialize>(value: &T, order: &JsonFieldOrder) -> String {
        struct Persisted<'a, T> {
            value: &'a T,
            order: &'a JsonFieldOrder,
        }
        impl<T: Serialize> Serialize for Persisted<'_, T> {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                self.order.serialize(self.value, serializer)
            }
        }
        serde_json::to_string(&Persisted { value, order }).unwrap()
    }

    #[test]
    fn json_field_order_preserves_flattened_fields_and_nested_enum_payloads() {
        let input = r#"{"node":{"Map":{"z":{"Int":1},"a":{"List":[{"Map":{"y":{"Int":2},"x":{"Int":3}}}]}}},"version":39}"#;
        let mut order = JsonFieldOrder::default();
        let document: Document = order
            .deserialize(&mut serde_json::Deserializer::from_str(input))
            .unwrap();
        assert_eq!(ordered_json(&document, &order), input);
        let separately_captured = JsonFieldOrder::from_json_str(input).unwrap();
        assert_eq!(ordered_json(&document, &separately_captured), input);
    }

    #[test]
    fn json_field_order_serializes_current_values_after_mutation() {
        let input = r#"{"removed":{"Int":7},"node":{"List":[{"Map":{"z":{"Int":1},"a":{"Int":2}}}]},"version":21}"#;
        let mut order = JsonFieldOrder::default();
        let mut document: Document = order
            .deserialize(&mut serde_json::Deserializer::from_str(input))
            .unwrap();
        document.version = 22;
        document.fields.remove("removed");
        document.fields.insert("added".into(), Node::Int(4));
        let Node::List(items) = document.fields.get_mut("node").unwrap() else {
            panic!("expected a list");
        };
        let Node::Map(fields) = &mut items[0] else {
            panic!("expected a map");
        };
        fields.insert("z".into(), Node::Int(3));
        assert_eq!(
            ordered_json(&document, &order),
            r#"{"node":{"List":[{"Map":{"z":{"Int":3},"a":{"Int":2}}}]},"version":22,"added":{"Int":4}}"#,
        );
    }
}
