use crate as struct_diff;
#[doc(hidden)]
pub use serde as _serde;
use serde::{
    de,
    ser::{self, SerializeSeq},
    Deserialize, Serialize, Serializer,
};
use std::borrow::Cow;
pub use struct_diff_derive::SerdeDiffable;

// NEXT STEPS:
// - Decouple from serde_json as much as possible. We might need to use a "stream" format with
//   well-defined data order to be able to use serde Deserializer trait. DONE
// - Make all fields work again. DONE
// - Make it work via proc macro. DONE
// - Blanket impl or impl-via-macro common std types (i.e f32, i32, String). DONE
// - Handle containers. DONE
// - Ignore type mismatches instead of propagating the error. IMPOSSIBLE??

//TODO: Currently we store data as a command list that encodes the hierarchy, i.e.
// [{"Enter":{"Field":"a"}},{"Value":3.0},{"Enter":{"Field":"c"}},{"Enter":{"Field":"x"}},{"Value":39.0}]
// Value is decoded as an implicit Exit to avoid excessive Exits in the data stream.
// It could probably be made smaller and more readable in a text-based format.
//
// A problem occurs when encoding the command stream for bincode:
// We need to know the size of the list before we start serializing.
// To do so, we need to implement the serde::ser::Serializer trait and
// make the implementation only count up every time an element is serialized, doing nothing else.
// This is implemented as CountingSerializer

/// Anything diffable implements this trait
pub trait SerdeDiffable {
    /// Recursively walk the struct, invoking serialize_element on each member if the element is
    /// different. Returns whether anything changed.
    fn diff<'a, S: SerializeSeq>(
        &self,
        ctx: &mut DiffContext<'a, S>,
        other: &Self,
    ) -> Result<bool, S::Error>;

    fn apply<'de, A>(
        &mut self,
        seq: &mut A,
        ctx: &mut ApplyContext,
    ) -> Result<bool, <A as de::SeqAccess<'de>>::Error>
    where
        A: de::SeqAccess<'de>;
}

/// Used during a diff operation for transient data used during the diff
#[doc(hidden)]
pub struct DiffContext<'a, S: SerializeSeq> {
    field_stack: Vec<DiffPathElementValue<'a>>,
    /// Reference to the serializer used to save the data
    serializer: &'a mut S,
    /// some commands are implicit Exit to save space, so we set a flag to avoid writing the next Exit
    implicit_exit_written: bool,
}

impl<'a, S: SerializeSeq> DiffContext<'a, S> {
    /// Called when we visit a field. If the structure is recursive (i.e. struct within struct,
    /// elements within an array) this may be called more than once before a corresponding pop_path_element
    /// is called. See `pop_path_element`
    pub fn push_field(&mut self, field_name: &'static str) {
        self.field_stack
            .push(DiffPathElementValue::Field(Cow::Borrowed(field_name)));
    }
    pub fn push_collection_index(&mut self, idx: usize) {
        self.field_stack
            .push(DiffPathElementValue::CollectionIndex(idx));
    }
    pub fn push_collection_add(&mut self) {
        self.field_stack.push(DiffPathElementValue::AddToCollection);
    }
    /// Called when we finish visiting a field. See `push_field` for details
    pub fn pop_path_element(&mut self) -> Result<(), S::Error> {
        if self.field_stack.is_empty() {
            // if we don't have any buffered fields, we just write Exit command directly to the serializer
            // if we've just written a field, skip the Exit
            if !self.implicit_exit_written {
                let cmd = DiffCommandRef::<()>::Exit;
                self.serializer.serialize_element(&cmd)
            } else {
                self.implicit_exit_written = false;
                Ok(())
            }
        } else {
            self.field_stack.pop();
            Ok(())
        }
    }

    /// Stores a value for an element that has previously been pushed using push_field.
    pub fn save_value<T: Serialize>(&mut self, value: &T) -> Result<(), S::Error> {
        if !self.field_stack.is_empty() {
            // flush buffered fields as Enter commands
            for field in self.field_stack.drain(0..self.field_stack.len()) {
                self.serializer
                    .serialize_element(&DiffCommandRef::<()>::Enter(field))?;
            }
        }
        self.implicit_exit_written = true;
        let cmd = DiffCommandRef::Value(value);
        self.serializer.serialize_element(&cmd)
    }
    /// Stores an arbitrary DiffCommand to be handled by the type.
    /// Any custom sequence of DiffCommands must be followed by Exit.
    pub fn save_command<'b, T: Serialize>(
        &mut self,
        value: &DiffCommandRef<'b, T>,
        implicit_exit: bool,
    ) -> Result<(), S::Error> {
        if !self.field_stack.is_empty() {
            // flush buffered fields as Enter commands
            for field in self.field_stack.drain(0..self.field_stack.len()) {
                self.serializer
                    .serialize_element(&DiffCommandRef::<()>::Enter(field))?;
            }
        }
        self.implicit_exit_written = implicit_exit;
        self.serializer.serialize_element(value)
    }
}

/// Serializes the difference between two values of a type
pub struct Diff<'a, 'b, T> {
    old: &'a T,
    new: &'b T,
}
impl<'a, 'b, T: SerdeDiffable + 'a + 'b> Diff<'a, 'b, T> {
    pub fn serializable(old: &'a T, new: &'b T) -> Self {
        Self { old, new }
    }
    pub fn diff<S: Serializer>(serializer: S, old: &'a T, new: &'b T) -> Result<S::Ok, S::Error> {
        Self::serializable(old, new).serialize(serializer)
    }
}
impl<'a, 'b, T: SerdeDiffable> Serialize for Diff<'a, 'b, T> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let num_elements = {
            let mut serializer = CountingSerializer { num_elements: 0 };
            let mut seq = serializer.serialize_seq(None).unwrap();
            let mut ctx = DiffContext {
                field_stack: Vec::new(),
                serializer: &mut seq,
                implicit_exit_written: false,
            };
            self.old.diff(&mut ctx, &self.new).unwrap();
            seq.end().unwrap();
            serializer.num_elements
        };
        let mut seq = serializer.serialize_seq(Some(num_elements))?;
        let mut ctx = DiffContext {
            field_stack: Vec::new(),
            serializer: &mut seq,
            implicit_exit_written: false,
        };
        self.old.diff(&mut ctx, &self.new)?;
        Ok(seq.end()?)
    }
}

/// Deserializes a [Diff]
pub struct Apply<'a, T: SerdeDiffable> {
    target: &'a mut T,
}
impl<'a, 'de, T: SerdeDiffable> Apply<'a, T> {
    pub fn deserializable(target: &'a mut T) -> Self {
        Self { target }
    }
    pub fn apply<D>(
        deserializer: D,
        target: &mut T,
    ) -> Result<(), <D as de::Deserializer<'de>>::Error>
    where
        D: de::Deserializer<'de>,
    {
        deserializer.deserialize_seq(Apply { target })
    }
}
impl<'a, 'de, T: SerdeDiffable> de::DeserializeSeed<'de> for Apply<'a, T> {
    type Value = ();
    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: de::Deserializer<'de>,
    {
        deserializer.deserialize_seq(self)
    }
}

impl<'a, 'de, T: SerdeDiffable> de::Visitor<'de> for Apply<'a, T> {
    type Value = ();
    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(formatter, "a sequence containing DiffCommands")
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, <A as de::SeqAccess<'de>>::Error>
    where
        A: de::SeqAccess<'de>,
    {
        let mut ctx = ApplyContext {};
        self.target.apply(&mut seq, &mut ctx)?;
        Ok(())
    }
}

/// Used during an apply operation for transient data used during the apply
#[doc(hidden)]
pub struct ApplyContext {}
impl ApplyContext {
    /// Returns the next element if it is a path. If it is a Value or Exit, it returns None.
    pub fn next_path_element<'de, A>(
        &mut self,
        seq: &mut A,
    ) -> Result<Option<DiffPathElementValue<'de>>, <A as de::SeqAccess<'de>>::Error>
    where
        A: de::SeqAccess<'de>,
    {
        use DiffCommandValue::*;
        let element = match seq.next_element_seed(DiffCommandIgnoreValue {})? {
            Some(Enter(element)) => Ok(Some(element)),
            Some(Value(_)) | Some(Remove(_)) => panic!("unexpected DiffCommand Value or Remove"),
            Some(Exit) | Some(Nothing) | Some(DeserializedValue) | None => Ok(None),
        };
        element
    }
    /// To be called after next_path_element returns a path, but the path is not recognized.
    pub fn skip_value<'de, A>(
        &mut self,
        seq: &mut A,
    ) -> Result<(), <A as de::SeqAccess<'de>>::Error>
    where
        A: de::SeqAccess<'de>,
    {
        self.skip_value_internal(seq, 1)
    }
    fn skip_value_internal<'de, A>(
        &mut self,
        seq: &mut A,
        mut depth: i32,
    ) -> Result<(), <A as de::SeqAccess<'de>>::Error>
    where
        A: de::SeqAccess<'de>,
    {
        // this tries to skip the value without knowing the type - not possible for some formats..
        while let Some(cmd) = seq.next_element_seed(DiffCommandIgnoreValue {})? {
            match cmd {
                DiffCommandValue::Enter(_) => depth += 1,
                DiffCommandValue::Exit => depth -= 1,
                DiffCommandValue::Value(_) | DiffCommandValue::Remove(_) => depth -= 1, // ignore value, but reduce depth, as it is an implicit Exit
                DiffCommandValue::Nothing | DiffCommandValue::DeserializedValue => {
                    panic!("should never serialize cmd Nothing or DeserializedValue")
                }
            }
            if depth == 0 {
                break;
            }
        }
        if depth != 0 {
            panic!("mismatched DiffCommand::Enter/Exit ")
        }
        Ok(())
    }
    /// Attempts to deserialize a value
    pub fn read_value<'de, A, T: for<'c> Deserialize<'c>>(
        &mut self,
        seq: &mut A,
        val: &mut T,
    ) -> Result<bool, <A as de::SeqAccess<'de>>::Error>
    where
        A: de::SeqAccess<'de>,
    {
        // The visitor for DiffCommandDeserWrapper handles enum cases and returns
        // a command if the next element was not a Value
        let cmd = seq.next_element_seed::<DiffCommandDeserWrapper<T>>(DiffCommandDeserWrapper {
            val_wrapper: DeserWrapper { val },
        })?;
        match cmd {
            Some(DiffCommandValue::DeserializedValue) => return Ok(true),
            Some(DiffCommandValue::Enter(_)) => {
                self.skip_value_internal(seq, 1)?;
            }
            Some(DiffCommandValue::Exit) => panic!("unexpected Exit command"),
            _ => {}
        }

        Ok(false)
    }
    /// Returns the next command in the stream. Make sure you know what you're doing!
    pub fn read_next_command<'de, A, T: for<'c> Deserialize<'c>>(
        &mut self,
        seq: &mut A,
    ) -> Result<Option<DiffCommandValue<'de, T>>, <A as de::SeqAccess<'de>>::Error>
    where
        A: de::SeqAccess<'de>,
    {
        // The visitor for DiffCommandDeserWrapper handles enum cases and returns
        // a command if the next element was not a Value
        let cmd = seq.next_element::<DiffCommandValue<'de, T>>()?;
        Ok(match cmd {
            cmd @ Some(DiffCommandValue::Remove(_))
            | cmd @ Some(DiffCommandValue::Value(_))
            | cmd @ Some(DiffCommandValue::Enter(_))
            | cmd @ Some(DiffCommandValue::Exit) => cmd,
            _ => None,
        })
    }
}

struct DeserWrapper<'a, T> {
    val: &'a mut T,
}
struct DiffCommandDeserWrapper<'a, T> {
    val_wrapper: DeserWrapper<'a, T>,
}

// This monstrosity is based off the output of the derive macro for DiffCommand.
// The justification for this is that we want to use Deserialize::deserialize_in_place
// for DiffCommand::Value in order to support zero-copy deserialization of T.
// This is achieved by passing &mut T through the DiffCommandDeserWrapper, which parsers the enum
// to the DeserWrapper which calls Deserialize::deserialize_in_place.
#[allow(non_camel_case_types)]
enum DiffCommandField {
    Enter,
    Value,
    Remove,
    Exit,
}
struct DiffCommandFieldVisitor;
const VARIANTS: &'static [&'static str] = &["Enter", "Value", "Remove", "Exit"];
impl<'de> de::Visitor<'de> for DiffCommandFieldVisitor {
    type Value = DiffCommandField;
    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        std::fmt::Formatter::write_str(formatter, "variant identifier")
    }
    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        match value {
            0u64 => Ok(DiffCommandField::Enter),
            1u64 => Ok(DiffCommandField::Value),
            2u64 => Ok(DiffCommandField::Remove),
            3u64 => Ok(DiffCommandField::Exit),
            _ => Err(de::Error::invalid_value(
                de::Unexpected::Unsigned(value),
                &"variant index 0 <= i < 4",
            )),
        }
    }
    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        match value {
            "Enter" => Ok(DiffCommandField::Enter),
            "Value" => Ok(DiffCommandField::Value),
            "Remove" => Ok(DiffCommandField::Remove),
            "Exit" => Ok(DiffCommandField::Exit),
            _ => Err(de::Error::unknown_variant(value, VARIANTS)),
        }
    }
    fn visit_bytes<E>(self, value: &[u8]) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        match value {
            b"Enter" => Ok(DiffCommandField::Enter),
            b"Value" => Ok(DiffCommandField::Value),
            b"Remove" => Ok(DiffCommandField::Remove),
            b"Exit" => Ok(DiffCommandField::Exit),
            _ => {
                let value = &serde::export::from_utf8_lossy(value);
                Err(de::Error::unknown_variant(value, VARIANTS))
            }
        }
    }
}
impl<'de> Deserialize<'de> for DiffCommandField {
    #[inline]
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: de::Deserializer<'de>,
    {
        de::Deserializer::deserialize_identifier(deserializer, DiffCommandFieldVisitor)
    }
}
impl<'a, 'de, T> de::DeserializeSeed<'de> for DiffCommandDeserWrapper<'a, T>
where
    T: de::Deserialize<'de>,
{
    type Value = DiffCommandValue<'de, T>;
    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: de::Deserializer<'de>,
    {
        struct Visitor<'de, 'a, T>
        where
            T: de::Deserialize<'de>,
        {
            seed: DeserWrapper<'a, T>,
            lifetime: std::marker::PhantomData<&'de ()>,
        }
        impl<'de, 'a, T> de::Visitor<'de> for Visitor<'de, 'a, T>
        where
            T: de::Deserialize<'de>,
        {
            type Value = DiffCommandValue<'de, T>;
            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                std::fmt::Formatter::write_str(formatter, "enum DiffCommandValueTest")
            }
            fn visit_enum<A>(self, data: A) -> Result<Self::Value, A::Error>
            where
                A: de::EnumAccess<'de>,
            {
                match de::EnumAccess::variant(data)? {
                    (DiffCommandField::Enter, variant) => {
                        let enter =
                            de::VariantAccess::newtype_variant::<DiffPathElementValue>(variant)?;
                        Ok(DiffCommandValue::Enter(enter))
                    }
                    (DiffCommandField::Value, variant) => {
                        de::VariantAccess::newtype_variant_seed::<DeserWrapper<T>>(
                            variant, self.seed,
                        )?;
                        Ok(DiffCommandValue::DeserializedValue)
                    }
                    (DiffCommandField::Remove, variant) => {
                        let num_elements = de::VariantAccess::newtype_variant::<usize>(variant)?;
                        Ok(DiffCommandValue::Remove(num_elements))
                    }
                    (DiffCommandField::Exit, variant) => {
                        de::VariantAccess::unit_variant(variant)?;
                        Ok(DiffCommandValue::Exit)
                    }
                }
            }
        }
        de::Deserializer::deserialize_enum(
            deserializer,
            "DiffCommandValueTest",
            VARIANTS,
            Visitor {
                seed: self.val_wrapper,
                lifetime: std::marker::PhantomData,
            },
        )
    }
}

impl<'a, 'de, T: Deserialize<'de>> de::DeserializeSeed<'de> for DeserWrapper<'a, T> {
    type Value = Self;
    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: de::Deserializer<'de>,
    {
        Deserialize::deserialize_in_place(deserializer, self.val)?;
        Ok(self)
    }
}

// Deserializes a DiffCommand but ignores values
struct DiffCommandIgnoreValue;
impl<'de> de::DeserializeSeed<'de> for DiffCommandIgnoreValue {
    type Value = DiffCommandValue<'de, ()>;
    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: de::Deserializer<'de>,
    {
        struct Visitor<'de> {
            lifetime: std::marker::PhantomData<&'de ()>,
        }
        impl<'de> de::Visitor<'de> for Visitor<'de> {
            type Value = DiffCommandValue<'de, ()>;
            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                std::fmt::Formatter::write_str(formatter, "enum DiffCommandValueTest")
            }
            fn visit_enum<A>(self, data: A) -> Result<Self::Value, A::Error>
            where
                A: de::EnumAccess<'de>,
            {
                match de::EnumAccess::variant(data)? {
                    (DiffCommandField::Enter, variant) => {
                        let enter =
                            de::VariantAccess::newtype_variant::<DiffPathElementValue>(variant)?;
                        Ok(DiffCommandValue::Enter(enter))
                    }
                    (DiffCommandField::Value, variant) => {
                        de::VariantAccess::newtype_variant::<de::IgnoredAny>(variant)?;
                        Ok(DiffCommandValue::Value(()))
                    }
                    (DiffCommandField::Remove, variant) => {
                        let num_elements = de::VariantAccess::newtype_variant::<usize>(variant)?;
                        Ok(DiffCommandValue::Remove(num_elements))
                    }
                    (DiffCommandField::Exit, variant) => {
                        de::VariantAccess::unit_variant(variant)?;
                        Ok(DiffCommandValue::Exit)
                    }
                }
            }
        }
        de::Deserializer::deserialize_enum(
            deserializer,
            "DiffCommandValueTest",
            VARIANTS,
            Visitor {
                lifetime: std::marker::PhantomData,
            },
        )
    }
}

#[doc(hidden)]
#[derive(Serialize, Debug)]
pub enum DiffCommandRef<'a, T: Serialize> {
    /// Enter a path element
    Enter(DiffPathElementValue<'a>),
    /// A value to be deserialized.
    /// For collections, this implies "add to end" if not preceded by UpdateIndex.
    Value(&'a T),
    /// Remove N items from end of collection
    Remove(usize),
    /// Exit a path element
    Exit,
}
#[doc(hidden)]
#[derive(Deserialize, Debug)]
pub enum DiffCommandValue<'a, T> {
    // Enter a path element
    #[serde(borrow)]
    Enter(DiffPathElementValue<'a>),
    /// A value to be deserialized.
    Value(T),
    /// Remove N items from end of collection
    Remove(usize),
    // Exit a path element
    Exit,
    // Never serialized
    Nothing,
    // Never serialized, used to indicate that deserializer wrote a value into supplied reference
    DeserializedValue,
}

#[doc(hidden)]
#[derive(Serialize, Deserialize, Debug)]
pub enum DiffPathElementValue<'a> {
    /// A struct field
    #[serde(borrow)]
    Field(Cow<'a, str>),
    CollectionIndex(usize),
    AddToCollection,
}
impl<T: SerdeDiffable + Serialize + for<'a> Deserialize<'a>> SerdeDiffable for Vec<T> {
    fn diff<'a, S: SerializeSeq>(
        &self,
        ctx: &mut DiffContext<'a, S>,
        other: &Self,
    ) -> Result<bool, S::Error> {
        let mut self_iter = self.iter();
        let mut other_iter = other.iter();
        let mut idx = 0;
        let mut need_exit = false;
        let mut changed = false;
        loop {
            let self_item = self_iter.next();
            let other_item = other_iter.next();
            match (self_item, other_item) {
                (None, None) => break,
                (Some(_), None) => {
                    let mut num_to_remove = 1;
                    while self_iter.next().is_some() {
                        num_to_remove += 1;
                    }
                    ctx.save_command::<()>(&DiffCommandRef::Remove(num_to_remove), true)?;
                    changed = true;
                }
                (None, Some(other_item)) => {
                    ctx.save_command::<()>(
                        &DiffCommandRef::Enter(DiffPathElementValue::AddToCollection),
                        false,
                    )?;
                    ctx.save_command(&DiffCommandRef::Value(other_item), true)?;
                    need_exit = true;
                    changed = true;
                }
                (Some(self_item), Some(other_item)) => {
                    ctx.push_collection_index(idx);
                    if <T as SerdeDiffable>::diff(self_item, ctx, other_item)? {
                        need_exit = true;
                        changed = true;
                    }
                    ctx.pop_path_element()?;
                }
            }
            idx += 1;
        }
        if need_exit {
            ctx.save_command::<()>(&DiffCommandRef::Exit, true)?;
        }
        Ok(changed)
    }

    fn apply<'de, A>(
        &mut self,
        seq: &mut A,
        ctx: &mut ApplyContext,
    ) -> Result<bool, <A as de::SeqAccess<'de>>::Error>
    where
        A: de::SeqAccess<'de>,
    {
        let mut changed = false;
        while let Some(cmd) = ctx.read_next_command::<A, T>(seq)? {
            use DiffCommandValue::*;
            use DiffPathElementValue::*;
            match cmd {
                // we should not be getting fields when reading collection commands
                Enter(Field(_)) => {
                    ctx.skip_value(seq)?;
                    break;
                }
                Enter(CollectionIndex(idx)) => {
                    if let Some(value_ref) = self.get_mut(idx) {
                        changed |= <T as SerdeDiffable>::apply(value_ref, seq, ctx)?;
                    } else {
                        ctx.skip_value(seq)?;
                    }
                }
                Enter(AddToCollection) => {
                    if let Value(v) = ctx
                        .read_next_command(seq)?
                        .expect("Expected value after AddToCollection")
                    {
                        changed = true;
                        self.push(v);
                    } else {
                        panic!("Expected value after AddToCollection");
                    }
                }
                Remove(num_elements) => {
                    let new_length = self.len().saturating_sub(num_elements);
                    self.truncate(new_length);
                    changed = true;
                    break;
                }
                _ => break,
            }
        }
        Ok(changed)
    }
}
/// Implements SerdeDiffable on a type given that it impls Serialize + Deserialize + PartialEq.
/// This makes the type a "terminal" type in the SerdeDiffable hierarchy, meaning deeper inspection
/// will not be possible. Use the SerdeDiffable derive macro for
#[macro_export]
macro_rules! simple_serde_diffable {
    ($t:ty) => {
        impl SerdeDiffable for $t {
            fn diff<'a, S: struct_diff::_serde::ser::SerializeSeq>(
                &self,
                ctx: &mut struct_diff::DiffContext<'a, S>,
                other: &Self,
            ) -> Result<bool, S::Error> {
                if self != other {
                    ctx.save_value(other)?;
                    Ok(true)
                } else {
                    Ok(false)
                }
            }

            fn apply<'de, A>(
                &mut self,
                seq: &mut A,
                ctx: &mut struct_diff::ApplyContext,
            ) -> Result<bool, <A as struct_diff::_serde::de::SeqAccess<'de>>::Error>
            where
                A: struct_diff::_serde::de::SeqAccess<'de>,
            {
                ctx.read_value(seq, self)
            }
        }
    };
}

// Implement `SerdeDiffable` for primitive types and types defined in the standard library.
simple_serde_diffable!(bool);
simple_serde_diffable!(isize);
simple_serde_diffable!(i8);
simple_serde_diffable!(i16);
simple_serde_diffable!(i32);
simple_serde_diffable!(i64);
simple_serde_diffable!(usize);
simple_serde_diffable!(u8);
simple_serde_diffable!(u16);
simple_serde_diffable!(u32);
simple_serde_diffable!(u64);
simple_serde_diffable!(i128);
simple_serde_diffable!(u128);
simple_serde_diffable!(f32);
simple_serde_diffable!(f64);
simple_serde_diffable!(char);
simple_serde_diffable!(String);
simple_serde_diffable!(std::ffi::CString);
simple_serde_diffable!(std::ffi::OsString);
simple_serde_diffable!(std::num::NonZeroU8);
simple_serde_diffable!(std::num::NonZeroU16);
simple_serde_diffable!(std::num::NonZeroU32);
simple_serde_diffable!(std::num::NonZeroU64);
simple_serde_diffable!(std::time::Duration);
simple_serde_diffable!(std::time::SystemTime);
simple_serde_diffable!(std::net::IpAddr);
simple_serde_diffable!(std::net::Ipv4Addr);
simple_serde_diffable!(std::net::Ipv6Addr);
simple_serde_diffable!(std::net::SocketAddr);
simple_serde_diffable!(std::net::SocketAddrV4);
simple_serde_diffable!(std::net::SocketAddrV6);
simple_serde_diffable!(std::path::PathBuf);

#[allow(dead_code)]
type Unit = ();
simple_serde_diffable!(Unit);

struct CountingSerializer {
    num_elements: usize,
}

#[derive(Debug)]
struct SerializerError;
impl std::fmt::Display for SerializerError {
    fn fmt(&self, _f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        unimplemented!()
    }
}
impl std::error::Error for SerializerError {
    fn description(&self) -> &str {
        ""
    }
    fn cause(&self) -> Option<&dyn std::error::Error> {
        None
    }
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        None
    }
}
impl ser::Error for SerializerError {
    fn custom<T>(_msg: T) -> Self
    where
        T: std::fmt::Display,
    {
        SerializerError
    }
}

impl<'a> ser::Serializer for &'a mut CountingSerializer {
    type Ok = ();
    type Error = SerializerError;

    type SerializeSeq = Self;
    type SerializeTuple = ser::Impossible<(), Self::Error>;
    type SerializeTupleStruct = ser::Impossible<(), Self::Error>;
    type SerializeTupleVariant = ser::Impossible<(), Self::Error>;
    type SerializeMap = ser::Impossible<(), Self::Error>;
    type SerializeStruct = ser::Impossible<(), Self::Error>;
    type SerializeStructVariant = ser::Impossible<(), Self::Error>;

    fn serialize_bool(self, _v: bool) -> Result<(), Self::Error> {
        unimplemented!()
    }

    fn serialize_i8(self, _v: i8) -> Result<(), Self::Error> {
        unimplemented!()
    }

    fn serialize_i16(self, _v: i16) -> Result<(), Self::Error> {
        unimplemented!()
    }

    fn serialize_i32(self, _v: i32) -> Result<(), Self::Error> {
        unimplemented!()
    }

    fn serialize_i64(self, _v: i64) -> Result<(), Self::Error> {
        unimplemented!()
    }

    fn serialize_u8(self, _v: u8) -> Result<(), Self::Error> {
        unimplemented!()
    }

    fn serialize_u16(self, _v: u16) -> Result<(), Self::Error> {
        unimplemented!()
    }

    fn serialize_u32(self, _v: u32) -> Result<(), Self::Error> {
        unimplemented!()
    }

    fn serialize_u64(self, _v: u64) -> Result<(), Self::Error> {
        unimplemented!()
    }

    fn serialize_f32(self, _v: f32) -> Result<(), Self::Error> {
        unimplemented!()
    }

    fn serialize_f64(self, _v: f64) -> Result<(), Self::Error> {
        unimplemented!()
    }

    fn serialize_char(self, _v: char) -> Result<(), Self::Error> {
        unimplemented!()
    }

    fn serialize_str(self, _v: &str) -> Result<(), Self::Error> {
        unimplemented!()
    }

    fn serialize_bytes(self, _v: &[u8]) -> Result<(), Self::Error> {
        unimplemented!()
    }

    fn serialize_none(self) -> Result<(), Self::Error> {
        unimplemented!()
    }

    fn serialize_some<T>(self, _value: &T) -> Result<(), Self::Error>
    where
        T: ?Sized + Serialize,
    {
        unimplemented!()
    }

    fn serialize_unit(self) -> Result<(), Self::Error> {
        unimplemented!()
    }

    fn serialize_unit_struct(self, _name: &'static str) -> Result<(), Self::Error> {
        unimplemented!()
    }

    fn serialize_unit_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        _variant: &'static str,
    ) -> Result<(), Self::Error> {
        unimplemented!()
    }

    fn serialize_newtype_struct<T>(self, _name: &'static str, _value: &T) -> Result<(), Self::Error>
    where
        T: ?Sized + Serialize,
    {
        unimplemented!()
    }

    fn serialize_newtype_variant<T>(
        self,
        _name: &'static str,
        _variant_index: u32,
        _variant: &'static str,
        _value: &T,
    ) -> Result<(), Self::Error>
    where
        T: ?Sized + Serialize,
    {
        unimplemented!()
    }

    fn serialize_seq(self, _len: Option<usize>) -> Result<Self::SerializeSeq, Self::Error> {
        Ok(self)
    }

    fn serialize_tuple(self, _len: usize) -> Result<Self::SerializeTuple, Self::Error> {
        unimplemented!()
    }

    fn serialize_tuple_struct(
        self,
        _name: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeTupleStruct, Self::Error> {
        unimplemented!()
    }

    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        _variant: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeTupleVariant, Self::Error> {
        unimplemented!()
    }

    fn serialize_map(self, _len: Option<usize>) -> Result<Self::SerializeMap, Self::Error> {
        unimplemented!()
    }

    fn serialize_struct(
        self,
        _name: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeStruct, Self::Error> {
        unimplemented!()
    }

    fn serialize_struct_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        _variant: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeStructVariant, Self::Error> {
        unimplemented!()
    }
}

impl<'a> ser::SerializeSeq for &'a mut CountingSerializer {
    type Ok = ();
    type Error = SerializerError;

    fn serialize_element<T>(&mut self, _value: &T) -> Result<(), Self::Error>
    where
        T: ?Sized + Serialize,
    {
        self.num_elements += 1;
        Ok(())
    }

    fn end(self) -> Result<(), Self::Error> {
        Ok(())
    }
}
