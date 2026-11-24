//! Render UniFFI library metadata as a surface snapshot.
//!
//! The Swift and Kotlin bindings are both generated from this metadata, the
//! crate's `uniffi.toml`, and the UniFFI generator version, so the snapshot
//! records all three. Names are the Rust names; each binding derives its own
//! names from them by fixed case conversion. Docstrings, checksums and FFI
//! symbol details are omitted because consumers do not compile against them.
//!
//! The matches below are exhaustive on purpose: a UniFFI upgrade that adds
//! metadata stops this tool from compiling until the new shape is rendered.

use std::collections::BTreeMap;

use uniffi_meta::{
    DefaultValueMetadata, EnumShape, FieldMetadata, FnParamMetadata, LiteralMetadata, Metadata,
    ObjectImpl, Type, UniffiTraitMetadata,
};

use crate::compare::{key, Item};

/// Item order in the snapshot file. Items of one kind sort by key.
fn rank(header: &str) -> u8 {
    match header.split_whitespace().next().unwrap_or("") {
        "generator" => 0,
        "config" => 1,
        "namespace" => 2,
        "custom" => 3,
        "record" => 4,
        "enum" => 5,
        "object" => 6,
        "callback_interface" => 7,
        "trait_impl" => 8,
        "uniffi_trait" => 9,
        "fn" => 10,
        _ => 11,
    }
}

/// Build snapshot items from library metadata, binding config entries and the
/// generator version.
pub fn items(
    metadata: Vec<Metadata>,
    config: &[(String, String)],
    generator_version: &str,
) -> Result<Vec<Item>, String> {
    let mut headers: BTreeMap<String, String> = BTreeMap::new();
    // Fields and cases keep declaration order; other members are sorted.
    let mut ordered: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut unordered: BTreeMap<String, Vec<String>> = BTreeMap::new();
    // Methods and constructors are attached to their type after all types
    // are known.
    let mut attached: Vec<(String, String)> = Vec::new();

    let mut add = |header: String, members: Vec<String>| -> Result<(), String> {
        let item_key = key(&header);
        if headers.insert(item_key.clone(), header).is_some() {
            return Err(format!("metadata defines `{item_key}` twice"));
        }
        ordered.insert(item_key, members);
        Ok(())
    };

    add(format!("generator uniffi {generator_version}"), Vec::new())?;
    for (name, value) in config {
        add(format!("config {name} = {value}"), Vec::new())?;
    }

    for item in metadata {
        match item {
            Metadata::Namespace(meta) => add(format!("namespace {}", meta.name), Vec::new())?,
            Metadata::UdlFile(meta) => {
                return Err(format!(
                    "UDL file `{}` found; UDL-defined items are not in library metadata and this snapshot cannot cover them",
                    meta.file_stub
                ));
            }
            Metadata::Func(meta) => add(
                format!(
                    "fn {}",
                    callable(
                        &meta.name,
                        &meta.inputs,
                        meta.is_async,
                        meta.throws.as_ref(),
                        meta.return_type.as_ref()
                    )
                ),
                Vec::new(),
            )?,
            Metadata::Object(meta) => {
                let qualifier = match meta.imp {
                    ObjectImpl::Struct => "",
                    ObjectImpl::Trait => " trait",
                    ObjectImpl::CallbackTrait => " callback_trait",
                };
                add(format!("object {}{qualifier}", meta.name), Vec::new())?;
            }
            Metadata::CallbackInterface(meta) => {
                add(format!("callback_interface {}", meta.name), Vec::new())?
            }
            Metadata::Record(meta) => {
                let fields = meta
                    .fields
                    .iter()
                    .map(|f| format!("field {}", field(f)))
                    .collect();
                add(format!("record {}", meta.name), fields)?;
            }
            Metadata::Enum(meta) => {
                let mut header = format!("enum {}", meta.name);
                if let Some(discr_type) = &meta.discr_type {
                    header.push_str(&format!(" repr={}", ty(discr_type)));
                }
                let flat_error = meta.shape == EnumShape::Error { flat: true };
                match meta.shape {
                    EnumShape::Enum => {}
                    EnumShape::Error { flat: false } => header.push_str(" error"),
                    EnumShape::Error { flat: true } => header.push_str(" flat_error"),
                }
                if meta.non_exhaustive {
                    header.push_str(" non_exhaustive");
                }
                let cases = meta
                    .variants
                    .iter()
                    .map(|variant| {
                        let mut case = format!("case {}", variant.name);
                        // Flat errors expose only the case name and a message.
                        if !flat_error && !variant.fields.is_empty() {
                            let fields: Vec<String> = variant.fields.iter().map(field).collect();
                            case.push_str(&format!("({})", fields.join(", ")));
                        }
                        if let Some(discr) = &variant.discr {
                            case.push_str(&format!(" = {}", literal(discr)));
                        }
                        case
                    })
                    .collect();
                add(header, cases)?;
            }
            Metadata::Constructor(meta) => attached.push((
                meta.self_name.clone(),
                format!(
                    "constructor {}",
                    callable(
                        &meta.name,
                        &meta.inputs,
                        meta.is_async,
                        meta.throws.as_ref(),
                        None
                    )
                ),
            )),
            Metadata::Method(meta) => attached.push((
                meta.self_name.clone(),
                format!(
                    "method {}",
                    callable(
                        &meta.name,
                        &meta.inputs,
                        meta.is_async,
                        meta.throws.as_ref(),
                        meta.return_type.as_ref()
                    )
                ),
            )),
            Metadata::TraitMethod(meta) => attached.push((
                meta.trait_name.clone(),
                format!(
                    "method {}",
                    callable(
                        &meta.name,
                        &meta.inputs,
                        meta.is_async,
                        meta.throws.as_ref(),
                        meta.return_type.as_ref()
                    )
                ),
            )),
            Metadata::CustomType(meta) => add(
                format!("custom {} = {}", meta.name, ty(&meta.builtin)),
                Vec::new(),
            )?,
            Metadata::UniffiTrait(meta) => {
                let name = match &meta {
                    UniffiTraitMetadata::Debug { .. } => "Debug",
                    UniffiTraitMetadata::Display { .. } => "Display",
                    UniffiTraitMetadata::Eq { .. } => "Eq",
                    UniffiTraitMetadata::Hash { .. } => "Hash",
                    UniffiTraitMetadata::Ord { .. } => "Ord",
                };
                add(
                    format!("uniffi_trait {}.{name}", meta.self_name()),
                    Vec::new(),
                )?;
            }
            Metadata::ObjectTraitImpl(meta) => add(
                format!("trait_impl {}.{}", ty(&meta.ty), ty(&meta.trait_ty)),
                Vec::new(),
            )?,
        }
    }

    // Attach constructors and methods to the type that declares them.
    let owners: BTreeMap<String, String> = headers
        .keys()
        .filter_map(|item_key| {
            let (kind, name) = item_key.split_once(' ')?;
            matches!(kind, "object" | "callback_interface" | "record" | "enum")
                .then(|| (name.to_string(), item_key.clone()))
        })
        .collect();
    for (self_name, member) in attached {
        let owner = owners.get(&self_name).ok_or_else(|| {
            format!("`{member}` belongs to `{self_name}`, which the metadata does not define")
        })?;
        unordered.entry(owner.clone()).or_default().push(member);
    }

    let mut items: Vec<Item> = headers
        .into_iter()
        .map(|(item_key, header)| {
            let mut members = ordered.remove(&item_key).unwrap_or_default();
            let mut rest = unordered.remove(&item_key).unwrap_or_default();
            rest.sort_by(|a, b| {
                (!a.starts_with("constructor "), a).cmp(&(!b.starts_with("constructor "), b))
            });
            members.extend(rest);
            Item { header, members }
        })
        .collect();
    items.sort_by_key(|item| (rank(&item.header), key(&item.header)));
    Ok(items)
}

/// Snapshot file text: the preamble followed by every item.
pub fn text(preamble: &str, items: &[Item]) -> String {
    let mut out = String::from(preamble);
    for item in items {
        out.push('\n');
        out.push_str(&item.header);
        out.push('\n');
        for member in &item.members {
            out.push_str("  ");
            out.push_str(member);
            out.push('\n');
        }
    }
    out
}

fn callable(
    name: &str,
    inputs: &[FnParamMetadata],
    is_async: bool,
    throws: Option<&Type>,
    return_type: Option<&Type>,
) -> String {
    let params: Vec<String> = inputs
        .iter()
        .map(|param| {
            let mut out = format!("{}: {}", param.name, ty(&param.ty));
            if let Some(default) = &param.default {
                out.push_str(&format!(" = {}", default_value(default)));
            }
            out
        })
        .collect();
    let mut out = format!("{name}({})", params.join(", "));
    if is_async {
        out.push_str(" async");
    }
    if let Some(error) = throws {
        out.push_str(&format!(" throws({})", ty(error)));
    }
    if let Some(returned) = return_type {
        out.push_str(&format!(" -> {}", ty(returned)));
    }
    out
}

fn field(field: &FieldMetadata) -> String {
    let mut out = format!("{}: {}", field.name, ty(&field.ty));
    if let Some(default) = &field.default {
        out.push_str(&format!(" = {}", default_value(default)));
    }
    out
}

fn ty(ty: &Type) -> String {
    match ty {
        Type::UInt8 => "u8".into(),
        Type::Int8 => "i8".into(),
        Type::UInt16 => "u16".into(),
        Type::Int16 => "i16".into(),
        Type::UInt32 => "u32".into(),
        Type::Int32 => "i32".into(),
        Type::UInt64 => "u64".into(),
        Type::Int64 => "i64".into(),
        Type::Float32 => "f32".into(),
        Type::Float64 => "f64".into(),
        Type::Boolean => "bool".into(),
        Type::String => "String".into(),
        Type::Bytes => "Bytes".into(),
        Type::Timestamp => "Timestamp".into(),
        Type::Duration => "Duration".into(),
        Type::Object { name, .. }
        | Type::Record { name, .. }
        | Type::Enum { name, .. }
        | Type::CallbackInterface { name, .. }
        | Type::Custom { name, .. } => name.clone(),
        Type::Optional { inner_type } => format!("Option<{}>", self::ty(inner_type)),
        Type::Sequence { inner_type } => format!("Vec<{}>", self::ty(inner_type)),
        Type::Map {
            key_type,
            value_type,
        } => {
            format!("Map<{}, {}>", self::ty(key_type), self::ty(value_type))
        }
    }
}

fn default_value(value: &DefaultValueMetadata) -> String {
    match value {
        DefaultValueMetadata::Default => "default".into(),
        DefaultValueMetadata::Literal(literal) => self::literal(literal),
    }
}

fn literal(literal: &LiteralMetadata) -> String {
    match literal {
        LiteralMetadata::Boolean(value) => value.to_string(),
        LiteralMetadata::String(value) => format!("{value:?}"),
        LiteralMetadata::UInt(value, _, _) => value.to_string(),
        LiteralMetadata::Int(value, _, _) => value.to_string(),
        LiteralMetadata::Float(value, _) => value.clone(),
        LiteralMetadata::Enum(case, enum_type) => format!("{}.{case}", ty(enum_type)),
        LiteralMetadata::EmptySequence => "[]".into(),
        LiteralMetadata::EmptyMap => "{}".into(),
        LiteralMetadata::None => "None".into(),
        LiteralMetadata::Some { inner } => format!("Some({})", default_value(inner)),
    }
}

/// Flatten a TOML table into sorted `(path, value)` entries, each of which
/// renders as one snapshot line with a unique key.
///
/// A path segment is written bare when it is a non-empty TOML bare key
/// (`A-Za-z0-9_-`) and JSON-quoted otherwise, so distinct paths stay distinct
/// and a quoted segment can hold `.`, ` = ` or a newline. Values use a
/// one-line encoding in which strings are JSON-quoted.
pub fn config_entries(table: &toml::Table) -> Vec<(String, String)> {
    fn walk(prefix: &str, table: &toml::Table, out: &mut Vec<(String, String)>) {
        for (name, value) in table {
            let segment = config_segment(name);
            let path = if prefix.is_empty() {
                segment
            } else {
                format!("{prefix}.{segment}")
            };
            match value {
                toml::Value::Table(inner) => walk(&path, inner, out),
                other => out.push((path, config_value(other))),
            }
        }
    }
    let mut out = Vec::new();
    walk("", table, &mut out);
    out.sort();
    out
}

fn config_segment(name: &str) -> String {
    let bare = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'));
    if bare {
        name.to_string()
    } else {
        json_string(name)
    }
}

fn config_value(value: &toml::Value) -> String {
    match value {
        toml::Value::String(text) => json_string(text),
        toml::Value::Integer(number) => number.to_string(),
        toml::Value::Float(number) => format!("{number:?}"),
        toml::Value::Boolean(flag) => flag.to_string(),
        toml::Value::Datetime(datetime) => datetime.to_string(),
        toml::Value::Array(values) => {
            let values: Vec<String> = values.iter().map(config_value).collect();
            format!("[{}]", values.join(", "))
        }
        toml::Value::Table(table) => {
            let entries: Vec<String> = table
                .iter()
                .map(|(name, value)| format!("{} = {}", config_segment(name), config_value(value)))
                .collect();
            format!("{{{}}}", entries.join(", "))
        }
    }
}

/// A JSON string literal: quoted, with quotes, backslashes and control
/// characters escaped.
fn json_string(text: &str) -> String {
    let mut out = String::from("\"");
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The `uniffi_bindgen` version recorded in a Cargo.lock file.
pub fn generator_version(lockfile: &str) -> Result<String, String> {
    let mut versions = Vec::new();
    let mut lines = lockfile.lines();
    while let Some(line) = lines.next() {
        if line.trim() == "name = \"uniffi_bindgen\"" {
            let version = lines
                .next()
                .and_then(|next| next.trim().strip_prefix("version = \""))
                .and_then(|rest| rest.strip_suffix('"'))
                .ok_or("Cargo.lock: uniffi_bindgen entry has no version line")?;
            versions.push(version.to_string());
        }
    }
    match versions.as_slice() {
        [version] => Ok(version.clone()),
        [] => Err("Cargo.lock: no uniffi_bindgen package".into()),
        _ => Err(format!(
            "Cargo.lock: several uniffi_bindgen versions: {}",
            versions.join(", ")
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uniffi_meta::{
        CallbackInterfaceMetadata, EnumMetadata, FnMetadata, ObjectMetadata, RecordMetadata,
        TraitMethodMetadata, VariantMetadata,
    };

    const MODULE: &str = "camera_protocol_ffi";

    fn record_type(name: &str) -> Type {
        Type::Record {
            module_path: MODULE.into(),
            name: name.into(),
        }
    }

    fn field_of(name: &str, ty: Type) -> FieldMetadata {
        FieldMetadata {
            name: name.into(),
            ty,
            default: None,
            docstring: None,
        }
    }

    fn variant(name: &str, fields: Vec<FieldMetadata>) -> VariantMetadata {
        VariantMetadata {
            name: name.into(),
            discr: None,
            fields,
            docstring: None,
        }
    }

    fn enum_of(name: &str, shape: EnumShape, variants: Vec<VariantMetadata>) -> Metadata {
        Metadata::Enum(EnumMetadata {
            module_path: MODULE.into(),
            name: name.into(),
            shape,
            remote: false,
            variants,
            discr_type: None,
            non_exhaustive: false,
            docstring: Some("ignored".into()),
        })
    }

    fn render(metadata: Vec<Metadata>) -> String {
        text("", &items(metadata, &[], "0.31.1").unwrap())
    }

    #[test]
    fn renders_function_signature_with_async_throws_and_return() {
        let out = render(vec![Metadata::Func(FnMetadata {
            module_path: MODULE.into(),
            name: "decode".into(),
            is_async: true,
            inputs: vec![FnParamMetadata::simple("bytes", Type::Bytes)],
            return_type: Some(Type::Optional {
                inner_type: Box::new(record_type("Frame")),
            }),
            throws: Some(Type::Enum {
                module_path: MODULE.into(),
                name: "FfiError".into(),
            }),
            checksum: Some(1),
            docstring: None,
        })]);
        assert!(out.contains("\nfn decode(bytes: Bytes) async throws(FfiError) -> Option<Frame>\n"));
    }

    #[test]
    fn renders_record_fields_in_declaration_order() {
        let out = render(vec![Metadata::Record(RecordMetadata {
            module_path: MODULE.into(),
            name: "Descriptor".into(),
            remote: false,
            fields: vec![
                field_of("title", Type::String),
                field_of(
                    "id",
                    Type::Sequence {
                        inner_type: Box::new(Type::UInt8),
                    },
                ),
            ],
            docstring: None,
        })]);
        assert!(out.contains("\nrecord Descriptor\n  field title: String\n  field id: Vec<u8>\n"));
        assert!(!out.contains("ignored"));
    }

    #[test]
    fn renders_enum_case_fields_but_not_flat_error_fields() {
        let out = render(vec![
            enum_of(
                "Step",
                EnumShape::Enum,
                vec![variant("Mtu", vec![field_of("requested", Type::UInt16)])],
            ),
            enum_of(
                "FfiError",
                EnumShape::Error { flat: true },
                vec![variant("Io", vec![field_of("detail", Type::String)])],
            ),
        ]);
        assert!(out.contains("\nenum Step\n  case Mtu(requested: u16)\n"));
        assert!(out.contains("\nenum FfiError flat_error\n  case Io\n"));
    }

    #[test]
    fn attaches_trait_methods_to_callback_interfaces_and_objects() {
        let method = |owner: &str, name: &str, index| {
            Metadata::TraitMethod(TraitMethodMetadata {
                module_path: MODULE.into(),
                trait_name: owner.into(),
                index,
                name: name.into(),
                is_async: false,
                inputs: Vec::new(),
                return_type: None,
                throws: None,
                takes_self_by_arc: false,
                checksum: None,
                docstring: None,
            })
        };
        let out = render(vec![
            Metadata::Object(ObjectMetadata {
                module_path: MODULE.into(),
                name: "Transport".into(),
                remote: false,
                imp: ObjectImpl::CallbackTrait,
                docstring: None,
            }),
            Metadata::CallbackInterface(CallbackInterfaceMetadata {
                module_path: MODULE.into(),
                name: "Listener".into(),
                docstring: None,
            }),
            method("Transport", "write", 1),
            method("Transport", "read", 0),
            method("Listener", "changed", 0),
        ]);
        assert!(
            out.contains("\nobject Transport callback_trait\n  method read()\n  method write()\n")
        );
        assert!(out.contains("\ncallback_interface Listener\n  method changed()\n"));
    }

    #[test]
    fn method_without_owner_is_an_error() {
        let orphan = Metadata::TraitMethod(TraitMethodMetadata {
            module_path: MODULE.into(),
            trait_name: "Missing".into(),
            index: 0,
            name: "read".into(),
            is_async: false,
            inputs: Vec::new(),
            return_type: None,
            throws: None,
            takes_self_by_arc: false,
            checksum: None,
            docstring: None,
        });
        assert!(items(vec![orphan], &[], "0.31.1").is_err());
    }

    #[test]
    fn rendered_text_parses_back_to_the_same_items() {
        let rendered = items(
            vec![enum_of(
                "Step",
                EnumShape::Enum,
                vec![variant("Read", Vec::new())],
            )],
            &[("bindings.swift.module_name".into(), "\"M\"".into())],
            "0.31.1",
        )
        .unwrap();
        let parsed = crate::compare::parse(&text("# preamble\n", &rendered)).unwrap();
        assert_eq!(parsed, rendered);
    }

    #[test]
    fn config_entries_flatten_nested_tables() {
        let table: toml::Table = toml::from_str(
            "[bindings.swift]\nmodule_name = \"M\"\n[bindings.kotlin]\npackage_name = \"p\"\n",
        )
        .unwrap();
        assert_eq!(
            config_entries(&table),
            [
                (
                    "bindings.kotlin.package_name".to_string(),
                    "\"p\"".to_string()
                ),
                (
                    "bindings.swift.module_name".to_string(),
                    "\"M\"".to_string()
                ),
            ]
        );
    }

    /// `update` writes `text(PREAMBLE, items)`; `check` parses that file and
    /// compares it with freshly rendered items. The pair must always agree.
    fn assert_update_then_check_passes(config: &[(String, String)]) {
        let rendered = items(Vec::new(), config, "0.31.1").unwrap();
        let written = text("# preamble\n", &rendered);
        let parsed = crate::compare::parse(&written).unwrap();
        assert_eq!(parsed, rendered, "{written}");
        assert!(
            crate::compare::compare(&parsed, &rendered).is_empty(),
            "{written}"
        );
    }

    #[test]
    fn multi_line_config_value_renders_on_one_line() {
        let table: toml::Table =
            toml::from_str("[bindings.kotlin.custom_types.Foo]\nlift = \"\"\"\nx\ny\"\"\"\n")
                .unwrap();
        let entries = config_entries(&table);
        assert_eq!(
            entries,
            [(
                "bindings.kotlin.custom_types.Foo.lift".to_string(),
                "\"x\\ny\"".to_string()
            )]
        );
        assert_update_then_check_passes(&entries);
    }

    #[test]
    fn quoted_config_keys_stay_distinct() {
        let table: toml::Table = toml::from_str(
            "[bindings.swift.rename]\n\"a-b\" = \"x\"\n\"a-c\" = \"y\"\n\"a.b\" = \"z\"\n",
        )
        .unwrap();
        let entries = config_entries(&table);
        assert_eq!(entries.len(), 3);
        assert_update_then_check_passes(&entries);
    }

    /// Small deterministic generator for the round-trip property.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn pick<'a>(&mut self, options: &[&'a str]) -> &'a str {
            options[(self.next() % options.len() as u64) as usize]
        }
    }

    const KEYS: [&str; 14] = [
        "a",
        "a-b",
        "a-c",
        "a.b",
        "",
        " ",
        "a = b",
        "\"q\"",
        "x\\y",
        "\u{fc}n",
        "#h",
        "  lead",
        "tab\tkey",
        "line\nkey",
    ];
    const STRINGS: [&str; 9] = [
        "plain",
        "",
        "two\nlines",
        "crlf\r\nend",
        "\"\"\"",
        " = ",
        "tab\there",
        "\\",
        "\u{2028}",
    ];

    fn value(rng: &mut Rng, depth: u32) -> toml::Value {
        match rng.next() % if depth == 0 { 6 } else { 8 } {
            0 => toml::Value::String(rng.pick(&STRINGS).to_string()),
            1 => toml::Value::Integer(rng.next() as i64),
            2 => toml::Value::Float(match rng.next() % 4 {
                0 => f64::NAN,
                1 => f64::INFINITY,
                2 => -0.0,
                _ => (rng.next() % 1000) as f64 / 7.0,
            }),
            3 => toml::Value::Boolean(rng.next().is_multiple_of(2)),
            4 => toml::Value::Datetime("1979-05-27T07:32:00Z".parse().unwrap()),
            5 => toml::Value::Array(Vec::new()),
            6 => toml::Value::Array((0..3).map(|_| value(rng, depth - 1)).collect()),
            _ => toml::Value::Table(table(rng, depth - 1)),
        }
    }

    fn table(rng: &mut Rng, depth: u32) -> toml::Table {
        let mut out = toml::Table::new();
        for _ in 0..(rng.next() % 5) {
            out.insert(rng.pick(&KEYS).to_string(), value(rng, depth));
        }
        out
    }

    #[test]
    fn update_then_check_passes_for_generated_configs() {
        for seed in 1..=500_u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
            let config = table(&mut rng, 3);
            assert_update_then_check_passes(&config_entries(&config));
        }
    }

    #[test]
    fn generator_version_reads_the_single_uniffi_bindgen_entry() {
        let lock = "[[package]]\nname = \"uniffi_bindgen\"\nversion = \"0.31.1\"\n";
        assert_eq!(generator_version(lock).unwrap(), "0.31.1");
        assert!(generator_version("").is_err());
        assert!(generator_version(&format!("{lock}{lock}")).is_err());
    }
}
