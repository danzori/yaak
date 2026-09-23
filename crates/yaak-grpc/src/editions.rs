use crate::error::Result;
use prost::Message;
use prost_reflect::{DescriptorPool, DynamicMessage, MessageDescriptor, Value};
use std::sync::LazyLock;

/// `google/protobuf/descriptor.proto` from protoc 33.1, which knows about `edition` and
/// `features`. Regenerate with:
/// `yaakprotoc -I <include> --descriptor_set_out=resources/descriptor.binpb google/protobuf/descriptor.proto`
static DESCRIPTOR_POOL: LazyLock<DescriptorPool> = LazyLock::new(|| {
    DescriptorPool::decode(include_bytes!("../resources/descriptor.binpb").as_ref())
        .expect("embedded descriptor.proto is valid")
});

const FIELD_PRESENCE_LEGACY_REQUIRED: i32 = 3;
const REPEATED_FIELD_ENCODING_PACKED: i32 = 1;
const MESSAGE_ENCODING_DELIMITED: i32 = 2;
const TYPE_STRING: i32 = 9;
const TYPE_GROUP: i32 = 10;
const TYPE_MESSAGE: i32 = 11;
const TYPE_BYTES: i32 = 12;
const NON_PACKABLE_TYPES: [i32; 4] = [TYPE_STRING, TYPE_GROUP, TYPE_MESSAGE, TYPE_BYTES];
const LABEL_REQUIRED: i32 = 2;
const LABEL_REPEATED: i32 = 3;

#[derive(Clone, Copy)]
struct Features {
    field_presence: i32,
    repeated_field_encoding: i32,
    message_encoding: i32,
}

impl Features {
    const EDITION_DEFAULTS: Features =
        Features { field_presence: 1, repeated_field_encoding: 1, message_encoding: 1 };

    fn inherit(self, descriptor: &DynamicMessage) -> Features {
        let options = descriptor.get_field_by_name("options");
        let Some(features) = options
            .as_deref()
            .and_then(Value::as_message)
            .filter(|options| options.has_field_by_name("features"))
            .and_then(|options| options.get_field_by_name("features"))
        else {
            return self;
        };
        let Some(features) = features.as_message() else {
            return self;
        };

        let get = |name: &str, inherited: i32| {
            if features.has_field_by_name(name) {
                features
                    .get_field_by_name(name)
                    .and_then(|v| v.as_enum_number())
                    .unwrap_or(inherited)
            } else {
                inherited
            }
        };
        Features {
            field_presence: get("field_presence", self.field_presence),
            repeated_field_encoding: get("repeated_field_encoding", self.repeated_field_encoding),
            message_encoding: get("message_encoding", self.message_encoding),
        }
    }
}

pub(crate) fn lower_file_descriptor_set(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut set = DynamicMessage::decode(message("FileDescriptorSet"), bytes)?;
    for file in repeated_mut(&mut set, "file") {
        lower_file(file);
    }
    Ok(set.encode_to_vec())
}

pub(crate) fn lower_file_descriptor_proto(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut file = DynamicMessage::decode(message("FileDescriptorProto"), bytes)?;
    lower_file(&mut file);
    Ok(file.encode_to_vec())
}

fn message(name: &str) -> MessageDescriptor {
    DESCRIPTOR_POOL
        .get_message_by_name(&format!("google.protobuf.{name}"))
        .expect("message exists in descriptor.proto")
}

fn lower_file(file: &mut DynamicMessage) {
    let is_editions =
        file.get_field_by_name("syntax").as_deref().and_then(Value::as_str) == Some("editions");
    if !is_editions {
        return;
    }

    let features = Features::EDITION_DEFAULTS.inherit(file);
    for message in repeated_mut(file, "message_type") {
        lower_message(message, features);
    }
    for extension in repeated_mut(file, "extension") {
        lower_field(extension, features);
    }

    file.set_field_by_name("syntax", Value::String("proto2".to_string()));
    file.clear_field_by_name("edition");
}

fn lower_message(message: &mut DynamicMessage, parent: Features) {
    let features = parent.inherit(message);
    let oneof_features: Vec<Features> =
        repeated_mut(message, "oneof_decl").map(|oneof| features.inherit(oneof)).collect();

    for field in repeated_mut(message, "field") {
        // Fields inside a oneof inherit from the oneof rather than the message.
        let oneof = field
            .has_field_by_name("oneof_index")
            .then(|| field.get_field_by_name("oneof_index").and_then(|v| v.as_i32()))
            .flatten()
            .and_then(|i| oneof_features.get(i as usize).copied());
        lower_field(field, oneof.unwrap_or(features));
    }
    for extension in repeated_mut(message, "extension") {
        lower_field(extension, features);
    }
    for nested in repeated_mut(message, "nested_type") {
        lower_message(nested, features);
    }
}

fn lower_field(field: &mut DynamicMessage, parent: Features) {
    let features = parent.inherit(field);
    let enum_field = |field: &DynamicMessage, name: &str| {
        field.get_field_by_name(name).and_then(|v| v.as_enum_number()).unwrap_or_default()
    };
    let label = enum_field(field, "label");
    let ty = enum_field(field, "type");

    if features.field_presence == FIELD_PRESENCE_LEGACY_REQUIRED {
        field.set_field_by_name("label", Value::EnumNumber(LABEL_REQUIRED));
    }

    if features.message_encoding == MESSAGE_ENCODING_DELIMITED && ty == TYPE_MESSAGE {
        field.set_field_by_name("type", Value::EnumNumber(TYPE_GROUP));
    }

    // Proto2 defaults to expanded, so packing has to be spelled out explicitly.
    if label == LABEL_REPEATED && !NON_PACKABLE_TYPES.contains(&ty) {
        let packed = features.repeated_field_encoding == REPEATED_FIELD_ENCODING_PACKED;
        if let Some(options) =
            field.get_field_by_name_mut("options").and_then(Value::as_message_mut)
        {
            options.set_field_by_name("packed", Value::Bool(packed));
        }
    }
}

fn repeated_mut<'a>(
    message: &'a mut DynamicMessage,
    name: &str,
) -> impl Iterator<Item = &'a mut DynamicMessage> {
    message
        .get_field_by_name_mut(name)
        .and_then(Value::as_list_mut)
        .into_iter()
        .flatten()
        .filter_map(Value::as_message_mut)
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost_reflect::{Cardinality, Kind};

    fn compile(source: &str) -> Vec<u8> {
        let protoc_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../crates-tauri/yaak-app-client/vendored/protoc");
        let dir = std::env::temp_dir().join(format!("yaak-editions-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("test.proto"), source).unwrap();
        let out = dir.join("out.binpb");
        let status = std::process::Command::new(protoc_dir.join("yaakprotoc"))
            .arg("-I")
            .arg(protoc_dir.join("include"))
            .arg("-I")
            .arg(&dir)
            .arg("--include_imports")
            .arg("-o")
            .arg(&out)
            .arg("test.proto")
            .status()
            .unwrap();
        assert!(status.success());
        let bytes = std::fs::read(&out).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        bytes
    }

    #[test]
    fn lowers_edition_2024_features() {
        let bytes = compile(
            r#"
            edition = "2024";
            package test;
            option features.field_presence = IMPLICIT;

            message Inner { int32 value = 1; }
            message Outer {
              int32 id = 1 [features.field_presence = LEGACY_REQUIRED];
              repeated int32 packed = 2;
              repeated int32 expanded = 3 [features.repeated_field_encoding = EXPANDED];
              Inner delimited = 4 [features.message_encoding = DELIMITED];
              Inner normal = 5;
              repeated string names = 6;
            }
            service Svc { rpc Call(Outer) returns (Inner); }
            "#,
        );

        let pool =
            DescriptorPool::decode(lower_file_descriptor_set(&bytes).unwrap().as_ref()).unwrap();
        let outer = pool.get_message_by_name("test.Outer").unwrap();
        let field = |name: &str| outer.get_field_by_name(name).unwrap();

        assert_eq!(field("id").cardinality(), Cardinality::Required);
        assert!(field("packed").is_packed());
        assert!(!field("expanded").is_packed());
        assert!(field("delimited").is_group());
        assert!(matches!(field("normal").kind(), Kind::Message(_)));
        assert!(!field("normal").is_group());
        assert_eq!(pool.services().count(), 1);

        let json = r#"{"id": 1, "packed": [1, 2], "expanded": [3], "delimited": {"value": 4}, "names": ["a"]}"#;
        let mut de = serde_json::Deserializer::from_str(json);
        let message = DynamicMessage::deserialize(outer.clone(), &mut de).unwrap();
        let decoded =
            DynamicMessage::decode(outer, prost::Message::encode_to_vec(&message).as_slice())
                .unwrap();
        assert_eq!(decoded, message);
    }

    #[test]
    fn leaves_proto3_untouched() {
        let bytes =
            compile(r#"syntax = "proto3"; package test; message M { repeated int32 v = 1; }"#);
        assert_eq!(
            DescriptorPool::decode(lower_file_descriptor_set(&bytes).unwrap().as_ref())
                .unwrap()
                .get_message_by_name("test.M")
                .unwrap()
                .parent_file()
                .syntax(),
            prost_reflect::Syntax::Proto3
        );
    }
}
